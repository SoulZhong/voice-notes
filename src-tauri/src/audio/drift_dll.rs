//! Adriaensen 二阶 DLL(LAC 2005/2012)的 dt 感知变体:滤(累计样本数, host 时刻)
//! 测量点,输出实测速率 ppm 与相位误差。纯算法,无 I/O、无系统调用。
//!
//! # 为什么用二阶环
//! 一阶环(仅相位反馈)只能追相位、无法收敛频率——面对持续漂移的晶振会一直有稳态误差。
//! 二阶临界阻尼环同时维护"相位"(`t_pred`,预测的下一次 host 时刻)与"频率"
//! (`ratio`,实测/标称的秒数比)两个状态,相位误差 e 按比例项 b·e 修正相位、
//! 按积分项 c·e 修正频率,可零稳态误差地跟踪恒定速率漂移(时钟漂移在数十秒尺度上
//! 近似恒定,不需要三阶环追加速度项)。b、c 由环路带宽 bw 与本次测量的标称时长
//! dt_nom 推导(`w = 2π·bw·dt_nom`,临界阻尼:`b = √2·w`,`c = w²`)——这正是
//! "dt 感知"的含义:帧距不固定(掉帧、暂停恢复)时环路增益仍按实际测量间隔缩放,
//! 而不是假设固定帧率。
//!
//! # 符号约定
//! - `ratio`:实测秒数 / 标称秒数。`ratio > 1` 表示设备产出同样样本数花的实际时间更长
//!   ⇒ 设备实际采样率比标称慢。
//! - `rate_ppm = (1/ratio - 1) * 1e6`:设备实际速率相对标称的偏差,正号 = 设备偏快。
//! - `e = 实测 host 时刻 - 环路预测的 host 时刻`:相位误差,单位秒;e>0 表示实际发生
//!   得比预测晚(设备这段时间比模型预期慢)。
//!
//! 参数为调研共识初值,待 E1 标定校准(docs/2026-08-12-clock-drift-sensor-design.md 第一节)。

use std::f64::consts::{PI, SQRT_2};

pub struct DllConfig {
    /// 稳态环路带宽(Hz)—— 调研共识初值,待 E1 标定校准。
    pub steady_bw_hz: f64,
    /// warmup 期环路带宽(Hz,更宽以加快初始收敛)—— 调研共识初值,待 E1 标定校准。
    pub warmup_bw_hz: f64,
    /// warmup 时长(秒)—— 调研共识初值,待 E1 标定校准。
    pub warmup_secs: f64,
    /// 触发自动重锚的相位误差阈值(秒)—— 调研共识初值,待 E1 标定校准。
    pub reanchor_err_secs: f64,
}

impl Default for DllConfig {
    fn default() -> Self {
        Self {
            steady_bw_hz: 0.05,
            warmup_bw_hz: 0.5,
            warmup_secs: 4.0,
            reanchor_err_secs: 0.005,
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct DriftEstimate {
    /// 实测速率相对标称的偏差(+ = 设备实际比标称快)。
    pub rate_ppm: f64,
    /// 最近一次相位误差(微秒)。
    pub phase_err_us: f64,
    /// 过了 warmup 且最近 5 秒 |e| 持续 < 1ms。
    pub converged: bool,
    /// 含内部自动重锚与外部调用。
    pub reanchors: u32,
    pub updates: u64,
}

/// dt 感知二阶临界阻尼 DLL:喂 (累计样本数, host 时刻) 测量点,估计设备真实采样率
/// 相对标称值的偏差(ppm)。
pub struct DriftDll {
    cfg: DllConfig,
    nominal_hz: f64,
    /// 环路状态(相位):预测下一测量点的 host 时刻(秒,绝对)。
    t_pred: f64,
    /// 环路状态(频率):实测秒 / 标称秒 之比(1.0 = 无漂移;>1 = 设备慢)。
    ratio: f64,
    last_samples: u64,
    /// 本轮锚定(session 或最近一次 reanchor)起点的 host 时刻,用于 warmup 计时。
    started_at: Option<f64>,
    /// 最近一次测量的 host 时刻——独立于 `t_pred`(后者会被环路持续修正),
    /// 专供 `estimate()` 计算"最近稳定了多久"用,语义单纯、不受相位修正影响。
    last_t: f64,
    last_e: f64,
    /// warmup 结束后,e 首次连续满足 |e|<1ms 的起始时刻;任何一次超阈(含触发自动
    /// 重锚)都会清空,重新计时。
    stable_since: Option<f64>,
    reanchors: u32,
    updates: u64,
}

/// 判定"持续稳定"所需的最短观测窗口(秒)——收敛判据"最近 5 秒 |e| 持续 < 1ms"中的 5 秒。
const CONVERGE_WINDOW_SECS: f64 = 5.0;
/// 收敛判据的相位误差阈值(秒)—— 1ms。
const CONVERGE_ERR_SECS: f64 = 0.001;

impl DriftDll {
    pub fn new(nominal_hz: f64, cfg: DllConfig) -> Self {
        Self {
            cfg,
            nominal_hz,
            t_pred: 0.0,
            ratio: 1.0,
            last_samples: 0,
            started_at: None,
            last_t: 0.0,
            last_e: 0.0,
            stable_since: None,
            reanchors: 0,
            updates: 0,
        }
    }

    /// 喂一个测量点:该源自会话起累计样本数 + 首样本 host 时刻。任意帧距可用(dt 感知)。
    pub fn push(&mut self, total_samples: u64, host_time_ns: u64) {
        let t = host_time_ns as f64 / 1e9;
        let Some(start) = self.started_at else {
            // 首个点(或 reanchor 后的首个点):只锚定相位与样本计数基准,不构成一次
            // 有效的误差测量(没有"上一个预测点"可比)。
            self.started_at = Some(t);
            self.t_pred = t;
            self.last_t = t;
            self.last_samples = total_samples;
            return;
        };
        let ds = total_samples.saturating_sub(self.last_samples);
        if ds == 0 || t < self.t_pred - 10.0 {
            return; // 无新样本,或时间戳大幅倒退(非单调,丢弃)。
        }
        self.last_samples = total_samples;
        let dt_nom = ds as f64 / self.nominal_hz;

        let predicted = self.t_pred + dt_nom * self.ratio;
        let e = t - predicted;
        self.updates += 1;
        self.last_t = t;
        self.last_e = e;

        if e.abs() > self.cfg.reanchor_err_secs {
            // 自动重锚:相位跳变过大(掉帧、系统休眠恢复等),保留频率状态、只清相位——
            // 真正换设备(晶振不同)走外部 reanchor(false)。
            self.t_pred = t;
            self.reanchors += 1;
            self.stable_since = None;
            return;
        }

        let bw = if t - start < self.cfg.warmup_secs {
            self.cfg.warmup_bw_hz
        } else {
            self.cfg.steady_bw_hz
        };
        let w = 2.0 * PI * bw * dt_nom;
        let b = SQRT_2 * w;
        let c = w * w;
        self.t_pred = predicted + b * e;
        self.ratio += c * e / dt_nom;

        if t - start > self.cfg.warmup_secs && e.abs() < CONVERGE_ERR_SECS {
            self.stable_since.get_or_insert(t);
        } else {
            self.stable_since = None;
        }
    }

    /// 设备切换/长补零后调用:清相位重锚。keep_freq=true 保留频率状态(掉帧场景);
    /// false 全清(设备真换了,晶振不是同一颗)。
    pub fn reanchor(&mut self, keep_freq: bool) {
        self.started_at = None;
        self.stable_since = None;
        self.reanchors += 1;
        if !keep_freq {
            self.ratio = 1.0;
            self.updates = 0;
        }
    }

    pub fn estimate(&self) -> DriftEstimate {
        let converged = matches!(
            self.stable_since,
            Some(s) if self.last_t - s >= CONVERGE_WINDOW_SECS
        );
        DriftEstimate {
            rate_ppm: (1.0 / self.ratio - 1.0) * 1e6,
            phase_err_us: self.last_e * 1e6,
            converged,
            reanchors: self.reanchors,
            updates: self.updates,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成一个真实速率 = 标称×(1+ppm/1e6) 的设备:每 10ms 出一帧(480 样本@48k),
    /// 观测时刻 = 按真实速率换算的理想时刻 + jitter。
    fn run_synthetic(ppm: f64, jitter_ns: u64, secs: f64) -> DriftEstimate {
        let nominal = 48_000.0;
        let true_hz = nominal * (1.0 + ppm / 1e6);
        let mut dll = DriftDll::new(nominal, DllConfig::default());
        let frame = 480u64;
        let n = (secs * true_hz / frame as f64) as u64;
        // 确定性伪随机 jitter(不引 rand:线性同余),±jitter_ns 均匀。
        let mut seed = 0x9e3779b97f4a7c15u64;
        for k in 0..n {
            let total = k * frame;
            let ideal_ns = (total as f64 / true_hz * 1e9) as u64;
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let j = if jitter_ns == 0 {
                0i64
            } else {
                (seed % (2 * jitter_ns + 1)) as i64 - jitter_ns as i64
            };
            dll.push(total, (ideal_ns as i64 + j).max(0) as u64 + 1_000_000_000);
            // 任意起点偏移
        }
        dll.estimate()
    }

    #[test]
    fn zero_drift_stays_near_zero() {
        let e = run_synthetic(0.0, 0, 30.0);
        assert!(e.converged, "30s 无抖动必须收敛");
        assert!(
            e.rate_ppm.abs() < 0.1,
            "零漂移输入不得虚报: {} ppm",
            e.rate_ppm
        );
    }

    #[test]
    fn converges_to_injected_drift_with_hw_jitter() {
        // 硬件时间戳量级抖动(±10µs):60s 内收敛到 ±2ppm。
        let e = run_synthetic(200.0, 10_000, 60.0);
        assert!(e.converged);
        assert!((e.rate_ppm - 200.0).abs() < 2.0, "got {} ppm", e.rate_ppm);
    }

    #[test]
    fn tolerates_coarse_jitter_degraded() {
        // 到达墙钟量级抖动(±0.5ms):仍应估到量级正确(±30ppm)。
        let e = run_synthetic(-300.0, 500_000, 60.0);
        assert!((e.rate_ppm + 300.0).abs() < 30.0, "got {} ppm", e.rate_ppm);
    }

    #[test]
    fn big_phase_jump_triggers_auto_reanchor() {
        let nominal = 48_000.0;
        let mut dll = DriftDll::new(nominal, DllConfig::default());
        for k in 0..1000u64 {
            dll.push(k * 480, (k as f64 * 0.01 * 1e9) as u64);
        }
        let before = dll.estimate();
        // 时间轴突跳 +50ms(> 5ms 阈值)
        dll.push(1000 * 480, (10.0f64 * 1e9) as u64 + 50_000_000);
        let after = dll.estimate();
        assert_eq!(after.reanchors, before.reanchors + 1);
        // 重锚保频率:速率估计不因跳变被摧毁
        assert!((after.rate_ppm - before.rate_ppm).abs() < 5.0);
    }

    #[test]
    fn external_full_reanchor_resets_frequency() {
        let mut dll = DriftDll::new(48_000.0, DllConfig::default());
        for k in 0..3000u64 {
            let t = (k as f64 * 0.01 * (1.0 + 500.0 / 1e6) * 1e9) as u64;
            dll.push(k * 480, t);
        }
        assert!(dll.estimate().rate_ppm < -400.0); // 设备慢 ≈ -500ppm
        dll.reanchor(false);
        assert_eq!(dll.estimate().rate_ppm, 0.0, "全清后频率状态归零");
    }
}
