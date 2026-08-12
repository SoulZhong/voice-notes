# 时钟漂移传感器(一期)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给双轨录音装上"只测不动数据"的时钟漂移传感器:每源一个 Adriaensen 二阶 DLL 吃硬件时间戳,停录时落 `drift_report.json`,并提供全库汇总与互相关标定工具。

**Architecture:** 三层——纯算法层(`audio/drift_dll.rs`,无 I/O)、测量接线层(`AudioFrame` 携带 host 时间戳,三条采集后端填入,`pipeline/drift_monitor.rs` 在 frame_tap 消费)、出口层(停录写报告 + `bin/drift_stats` 汇总 + `bin/xcorr_align` 标定)。传感器全旁路:任何失败只丢报告,不影响录音主链路。

**Tech Stack:** Rust;cpal 0.15(mic)/screencapturekit 8(系统声)/coreaudio-rs 0.12(VPIO);serde_json 落盘;hound 读 WAV(标定工具)。

## Global Constraints

- 设计文档:`docs/2026-08-12-clock-drift-sensor-design.md`(裁决标准、参数依据都在里面;冲突时以设计文档为准)。
- **一期铁律:只测不动数据**——不得修改任何音频样本,不得改动 frame_tap 现有 1% 容差对账逻辑。
- 全本地:漂移数据只落笔记目录与 stderr 日志,禁止进 telemetry(网络遥测)。
- Windows:全部新代码 `#[cfg]` 隔离或喂 `None` 降级,不得破坏 Windows 构建(先例:`audio/aec_stub.rs`)。
- 测试命令一律在 `src-tauri/` 下执行;**勿跑全量 `cargo fmt`**(仓库规矩),只 `cargo fmt -- <改动文件>`。
- 提交信息中文、不加任何 Co-Authored-By trailer(仓库规矩)。
- DLL 参数常量必须带"调研共识初值,待 E1 标定校准"注释。

---

### Task 1: host 时基换算模块

**Files:**
- Create: `src-tauri/src/audio/host_time.rs`
- Modify: `src-tauri/src/audio/mod.rs`(挂 `pub mod host_time;`)

**Interfaces:**
- Produces: `host_time::now_ns() -> u64`(mach 时基,ns);`host_time::mach_ticks_to_ns(ticks: u64) -> u64`;`host_time::cmtime_to_ns(value: i64, timescale: i32) -> Option<u64>`。后续任务(2/3/4)全部消费这三个函数。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/audio/host_time.rs` 底部(先建文件,只含测试与空骨架):

```rust
//! host 时钟(mach 时基)统一换算:全仓唯一的 ticks→ns / CMTime→ns 入口。
//! 依据:`docs/2026-08-12-clock-drift-sensor-design.md` 第二节。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ns_is_monotonic_nondecreasing() {
        let a = now_ns();
        let b = now_ns();
        assert!(b >= a, "host 时钟必须单调不减: {a} -> {b}");
    }

    #[test]
    fn cmtime_converts_by_timescale() {
        // CMTime{value: 48_000, timescale: 48_000} = 1 秒
        assert_eq!(cmtime_to_ns(48_000, 48_000), Some(1_000_000_000));
        // 非法 timescale → None
        assert_eq!(cmtime_to_ns(1, 0), None);
        assert_eq!(cmtime_to_ns(-1, 48_000), None);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib audio::host_time -- --nocapture`
Expected: 编译错误(`now_ns`/`cmtime_to_ns` 未定义)。

- [ ] **Step 3: 最小实现**

```rust
/// 当前 host 时刻(纳秒)。macOS 用 mach 时基(与 CoreAudio mHostTime、SCK PTS 同源);
/// 其他平台退化为进程内单调钟(仅保证单调,不与任何采集时间戳同源)。
pub fn now_ns() -> u64 {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: mach_absolute_time 无前置条件。
        mach_ticks_to_ns(unsafe { mach_absolute_time() })
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

/// mach ticks → ns。timebase 只查一次(进程级常量)。
#[cfg(target_os = "macos")]
pub fn mach_ticks_to_ns(ticks: u64) -> u64 {
    use std::sync::OnceLock;
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    let (numer, denom) = *TB.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: 传合法指针;失败(非零返回)时退化为 1:1(Apple Silicon 实际即 1:1)。
        let rc = unsafe { mach_timebase_info(&mut info) };
        if rc == 0 && info.denom != 0 {
            (info.numer as u64, info.denom as u64)
        } else {
            (1, 1)
        }
    });
    (ticks as u128 * numer as u128 / denom as u128) as u64
}

#[cfg(not(target_os = "macos"))]
pub fn mach_ticks_to_ns(ticks: u64) -> u64 {
    ticks
}

/// CMTime → ns。value<0 或 timescale<=0(含 invalid 标志的常见形态)→ None。
pub fn cmtime_to_ns(value: i64, timescale: i32) -> Option<u64> {
    if value < 0 || timescale <= 0 {
        return None;
    }
    Some((value as u128 * 1_000_000_000 / timescale as u128) as u64)
}
```

`audio/mod.rs` 中(与既有 `pub mod` 声明并列)加:`pub mod host_time;`

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib audio::host_time`
Expected: 2 passed。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/audio/host_time.rs src-tauri/src/audio/mod.rs
git commit -m "feat(drift): host 时基换算模块——mach ticks/CMTime 统一到 ns"
```

---

### Task 2: DLL 核心(纯算法)

**Files:**
- Create: `src-tauri/src/audio/drift_dll.rs`
- Modify: `src-tauri/src/audio/mod.rs`(挂 `pub mod drift_dll;`)

**Interfaces:**
- Produces(Task 4 消费):

```rust
pub struct DllConfig {
    pub steady_bw_hz: f64,      // 0.05
    pub warmup_bw_hz: f64,      // 0.5
    pub warmup_secs: f64,       // 4.0
    pub reanchor_err_secs: f64, // 0.005
}
impl Default for DllConfig { /* 上述值 */ }

pub struct DriftDll { /* 私有状态 */ }
impl DriftDll {
    pub fn new(nominal_hz: f64, cfg: DllConfig) -> Self;
    /// 喂一个测量点:该源自会话起累计样本数 + 首样本 host 时刻。任意帧距可用(dt 感知)。
    pub fn push(&mut self, total_samples: u64, host_time_ns: u64);
    /// 设备切换/长补零后调用:清相位重锚。keep_freq=true 保留频率状态(掉帧场景);
    /// false 全清(设备真换了,晶振不是同一颗)。
    pub fn reanchor(&mut self, keep_freq: bool);
    pub fn estimate(&self) -> DriftEstimate;
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct DriftEstimate {
    pub rate_ppm: f64,      // 实测速率相对标称的偏差(+ = 设备实际比标称快)
    pub phase_err_us: f64,  // 最近一次相位误差
    pub converged: bool,    // 过了 warmup 且最近 5 秒 |e| 持续 < 1ms
    pub reanchors: u32,     // 含内部自动重锚与外部调用
    pub updates: u64,
}
```

- [ ] **Step 1: 写失败测试**

```rust
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
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = if jitter_ns == 0 { 0i64 } else { (seed % (2 * jitter_ns + 1)) as i64 - jitter_ns as i64 };
            dll.push(total, (ideal_ns as i64 + j).max(0) as u64 + 1_000_000_000); // 任意起点偏移
        }
        dll.estimate()
    }

    #[test]
    fn zero_drift_stays_near_zero() {
        let e = run_synthetic(0.0, 0, 30.0);
        assert!(e.converged, "30s 无抖动必须收敛");
        assert!(e.rate_ppm.abs() < 0.1, "零漂移输入不得虚报: {} ppm", e.rate_ppm);
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
```

(注意符号:观测间隔被拉长 `(1+500ppm)` 倍 ⇒ 同样样本数花更久 ⇒ 设备实际率低 ⇒ `rate_ppm ≈ -500`。)

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib audio::drift_dll`
Expected: 编译错误(类型未定义)。

- [ ] **Step 3: 实现(dt 感知的二阶临界阻尼环)**

```rust
//! Adriaensen 二阶 DLL(LAC 2005/2012)的 dt 感知变体:滤(累计样本数, host 时刻)
//! 测量点,输出实测速率 ppm 与相位误差。纯算法,无 I/O、无系统调用。
//! 参数为调研共识初值,待 E1 标定校准(docs/2026-08-12-clock-drift-sensor-design.md 第一节)。

use std::f64::consts::{PI, SQRT_2};

pub struct DllConfig {
    pub steady_bw_hz: f64,
    pub warmup_bw_hz: f64,
    pub warmup_secs: f64,
    pub reanchor_err_secs: f64,
}

impl Default for DllConfig {
    fn default() -> Self {
        Self { steady_bw_hz: 0.05, warmup_bw_hz: 0.5, warmup_secs: 4.0, reanchor_err_secs: 0.005 }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct DriftEstimate {
    pub rate_ppm: f64,
    pub phase_err_us: f64,
    pub converged: bool,
    pub reanchors: u32,
    pub updates: u64,
}

pub struct DriftDll {
    cfg: DllConfig,
    nominal_hz: f64,
    /// 环路状态:预测下一测量点的 host 时刻(秒,绝对)。
    t_pred: f64,
    /// 频率状态:实测秒 / 标称秒 之比(1.0 = 无漂移;>1 = 设备慢)。
    ratio: f64,
    last_samples: u64,
    started_at: Option<f64>,
    last_e: f64,
    stable_since: Option<f64>,
    reanchors: u32,
    updates: u64,
}

impl DriftDll {
    pub fn new(nominal_hz: f64, cfg: DllConfig) -> Self {
        Self {
            cfg,
            nominal_hz,
            t_pred: 0.0,
            ratio: 1.0,
            last_samples: 0,
            started_at: None,
            last_e: 0.0,
            stable_since: None,
            reanchors: 0,
            updates: 0,
        }
    }

    pub fn push(&mut self, total_samples: u64, host_time_ns: u64) {
        let t = host_time_ns as f64 / 1e9;
        let Some(start) = self.started_at else {
            self.started_at = Some(t);
            self.t_pred = t;
            self.last_samples = total_samples;
            return;
        };
        let ds = total_samples.saturating_sub(self.last_samples);
        if ds == 0 || t < self.t_pred - 10.0 {
            return; // 无新样本或时间戳大幅倒退(非单调,丢弃)
        }
        self.last_samples = total_samples;
        let dt_nom = ds as f64 / self.nominal_hz;

        let predicted = self.t_pred + dt_nom * self.ratio;
        let e = t - predicted;
        self.updates += 1;

        if e.abs() > self.cfg.reanchor_err_secs {
            // 自动重锚:保频率,清相位(掉帧/暂停恢复场景;真换设备走外部 reanchor(false))。
            self.t_pred = t;
            self.reanchors += 1;
            self.last_e = e;
            self.stable_since = None;
            return;
        }

        let bw = if t - start < self.cfg.warmup_secs { self.cfg.warmup_bw_hz } else { self.cfg.steady_bw_hz };
        let w = 2.0 * PI * bw * dt_nom;
        let b = SQRT_2 * w;
        let c = w * w;
        self.t_pred = predicted + b * e;
        self.ratio += c * e / dt_nom;
        self.last_e = e;

        // 收敛判据:warmup 后 |e| 连续 5 秒 < 1ms。
        if t - start > self.cfg.warmup_secs && e.abs() < 0.001 {
            if self.stable_since.is_none() {
                self.stable_since = Some(t);
            }
        } else {
            self.stable_since = None;
        }
    }

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
        let converged = match (self.stable_since, self.started_at) {
            (Some(s), Some(_)) => self.t_pred - s >= 5.0 || self.last_e.abs() < 0.001 && self.t_pred - s >= 0.0 && self.updates > 500,
            _ => false,
        };
        DriftEstimate {
            rate_ppm: (1.0 / self.ratio - 1.0) * 1e6,
            phase_err_us: self.last_e * 1e6,
            converged,
            reanchors: self.reanchors,
            updates: self.updates,
        }
    }
}
```

实现提示:`converged` 的表达式以测试通过为准——语义是"warmup 完成且最近 5 秒相位误差持续 <1ms",如上式繁琐可改存 `last_t` 辅助字段实现,测试是唯一契约。`audio/mod.rs` 加 `pub mod drift_dll;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib audio::drift_dll`
Expected: 5 passed(若 `converges_to_injected_drift_with_hw_jitter` 边缘超差,允许微调 warmup 长度/收敛窗实现,**不得放宽断言阈值**)。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/audio/drift_dll.rs src-tauri/src/audio/mod.rs
git commit -m "feat(drift): Adriaensen 二阶 DLL(dt 感知)——ppm 级速率估计核心"
```

---

### Task 3: AudioFrame 携带 host 时间戳,三条采集后端填入

**Files:**
- Modify: `src-tauri/src/audio/mod.rs:28-32`(AudioFrame 定义)
- Modify: `src-tauri/src/audio/microphone.rs:66-73`(cpal 回调)
- Modify: `src-tauri/src/audio/vpio.rs:100-144`(VPIO 回调)
- Modify: `src-tauri/src/audio/system.rs:180-201`(SCK 回调)
- Modify: `src-tauri/src/audio/loopback.rs:76,90`、`src-tauri/src/audio/mock.rs:23`(补 `None`)
- Modify: 全部因新字段报错的测试构造点(`pipeline/segment_worker.rs`、`pipeline/frame_tap.rs` 等,机械补 `host_time_ns: None`)

**Interfaces:**
- Produces: `AudioFrame.host_time_ns: Option<u64>` —— **帧首样本**的 host 时刻(ns,mach 时基);`None` = 该后端拿不到硬件时间戳(降级)。Task 4 消费。

- [ ] **Step 1: 改结构体**

`audio/mod.rs`:

```rust
pub struct AudioFrame {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
    /// 帧首样本的 host 时刻(ns,mach 时基;见 audio/host_time.rs)。
    /// None = 采集后端拿不到硬件时间戳,漂移传感器按"到达墙钟"降级(行为等同引入前)。
    pub host_time_ns: Option<u64>,
}
```

- [ ] **Step 2: 编译收集报错清单**

Run: `cargo check 2>&1 | grep -c "missing field"`
Expected: 报错数 ≈ 20(生产 6 处 + 测试若干)。

- [ ] **Step 3: 逐点填入**

生产构造点:

1. **cpal(microphone.rs)**——capture 时间戳是 `StreamInstant`(cpal 文档明示 coreaudio 后端源自 `mach_absolute_time`,即 `mHostTime` 换算),但字段私有,只能做差。锚定方案:首个回调时记 `(anchor_instant, anchor_ns=host_time::now_ns())`,此后 `ns = anchor_ns + capture.duration_since(anchor_instant)`。**已知常量偏移**:anchor_ns 是回调进入时刻而 capture 是硬件时刻,差一个缓冲时长量级的常量——斜率(ppm)不受影响,绝对偏移由 E1 互相关标定,注释写明。

```rust
// 回调闭包外:
let mut anchor: Option<(cpal::StreamInstant, u64)> = None;
// 回调内(签名改为使用 info: &cpal::InputCallbackInfo):
move |data: &[f32], info: &cpal::InputCallbackInfo| {
    let cap = info.timestamp().capture;
    let host_time_ns = match anchor {
        None => {
            let now = crate::audio::host_time::now_ns();
            anchor = Some((cap, now));
            Some(now)
        }
        Some((a_inst, a_ns)) => cap
            .duration_since(&a_inst)
            .map(|d| a_ns + d.as_nanos() as u64),
    };
    let _ = sink.send(AudioFrame { samples: data.to_vec(), sample_rate, channels, host_time_ns });
}
```

2. **VPIO(vpio.rs input_cb)**——直接硬件时间戳:

```rust
let host_time_ns = if (*in_time_stamp).mFlags & kAudioTimeStampHostTimeValid != 0 {
    Some(crate::audio::host_time::mach_ticks_to_ns((*in_time_stamp).mHostTime))
} else {
    None
};
```

(`kAudioTimeStampHostTimeValid` 来自 `coreaudio::sys`,值为 `1 << 1`;若名称不在 crate 里就用 `0b10` 并注释。)

3. **SCK(system.rs AudioSink)**——PTS:

```rust
use screencapturekit::cm::sample_buffer::CMSampleBufferExt; // 若 prelude 未含
let pts = sample.output_presentation_timestamp();
let host_time_ns = crate::audio::host_time::cmtime_to_ns(pts.value, pts.timescale);
```

4. **loopback.rs(Windows)/mock.rs**:`host_time_ns: None`。

测试构造点:机械补 `host_time_ns: None`(不引入 helper,保持与仓库"就地字面量"测试风格一致)。

- [ ] **Step 4: 编译 + 全量测试**

Run: `cargo check && cargo test --lib`
Expected: 0 error;既有测试全绿(新字段不改变任何现有行为)。

- [ ] **Step 5: Commit**

```bash
git add -A src-tauri/src
git commit -m "feat(drift): AudioFrame 携带 host 时间戳——cpal 锚定/VPIO mHostTime/SCK PTS 三路填入"
```

---

### Task 4: DriftMonitor(每源监视器 + 报告构建)

**Files:**
- Create: `src-tauri/src/pipeline/drift_monitor.rs`
- Modify: `src-tauri/src/pipeline/mod.rs`(挂 `pub mod drift_monitor;`)

**Interfaces:**
- Consumes: `audio::drift_dll::{DriftDll, DllConfig, DriftEstimate}`、`audio::host_time::now_ns`、`audio::AudioFrame`、`audio::Source`。
- Produces(Task 5/6 消费):

```rust
pub struct DriftMonitor { /* Mutex<Inner> */ }
impl DriftMonitor {
    pub fn new(nominal_hz: u32) -> Self;
    /// frame_tap 对每个【真实】帧调用(补零帧不喂)。带时间戳→hw 路径;无→降级用 now_ns。
    pub fn feed(&self, frame: &AudioFrame);
    /// 设备切换(rate 变)→ full=true;补零段结束/暂停恢复 → full=false。
    pub fn mark_reanchor(&self, why: &'static str, full: bool);
    pub fn snapshot(&self) -> DriftSourceReport;
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DriftSourceReport {
    pub nominal_hz: u32,
    pub quality: String,            // "hw" | "degraded"(见过任一无时间戳帧即降)
    pub rate_ppm: f64,
    pub converged: bool,
    pub converge_secs: Option<f64>,
    pub reanchors: u32,
    pub rate_ppm_series: Vec<(f64, f64)>, // (自首帧秒, ppm),每 10s 一点
    pub events: Vec<DriftEvent>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DriftEvent { pub t_s: f64, pub kind: String, pub why: String }

/// 汇总两源成 drift_report.json 的顶层结构(纯函数,好测)。
pub fn build_report(sources: &[(Source, DriftSourceReport)]) -> serde_json::Value;
```

`build_report` 输出 schema(与设计文档第四节一致):顶层 `{"schema":1,"sources":{...},"inter_track":{...},"anomalies":[...]}`;`inter_track.rel_ppm = mic.rate_ppm - system.rate_ppm`(双源都在时),`est_misalign_ms_per_hour = rel_ppm * 3.6`;`anomalies` 按设计阈值生成:`|rate_ppm|>500`、`quality=="degraded"`、`reanchors>3`。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{AudioFrame, Source};

    fn frame(n: usize, rate: u32, ns: Option<u64>) -> AudioFrame {
        AudioFrame { samples: vec![0.1; n], sample_rate: rate, channels: 1, host_time_ns: ns }
    }

    #[test]
    fn hw_frames_yield_hw_quality_and_ppm() {
        let m = DriftMonitor::new(48_000);
        // 500ppm 慢设备,60 秒(6000 帧 × 10ms)
        for k in 0..6000u64 {
            let ns = (k as f64 * 0.01 * (1.0 + 500.0 / 1e6) * 1e9) as u64;
            m.feed(&frame(480, 48_000, Some(ns)));
        }
        let r = m.snapshot();
        assert_eq!(r.quality, "hw");
        assert!((r.rate_ppm + 500.0).abs() < 5.0, "got {}", r.rate_ppm);
        assert!(r.rate_ppm_series.len() >= 5, "每 10s 一点,60s 应有 ≥5 点");
        assert!(r.converge_secs.is_some());
    }

    #[test]
    fn any_untimed_frame_degrades_quality() {
        let m = DriftMonitor::new(48_000);
        m.feed(&frame(480, 48_000, Some(0)));
        m.feed(&frame(480, 48_000, None));
        assert_eq!(m.snapshot().quality, "degraded");
    }

    #[test]
    fn reanchor_is_recorded_as_event() {
        let m = DriftMonitor::new(48_000);
        m.feed(&frame(480, 48_000, Some(0)));
        m.mark_reanchor("device_switch", true);
        let r = m.snapshot();
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.events[0].why, "device_switch");
    }

    #[test]
    fn report_computes_inter_track_and_anomalies() {
        let mic = DriftSourceReport {
            nominal_hz: 48_000, quality: "hw".into(), rate_ppm: 100.0, converged: true,
            converge_secs: Some(14.0), reanchors: 0, rate_ppm_series: vec![], events: vec![],
        };
        let sys = DriftSourceReport { rate_ppm: -600.0, reanchors: 5, quality: "degraded".into(), ..mic.clone() };
        let v = build_report(&[(Source::Mic, mic), (Source::System, sys)]);
        assert_eq!(v["schema"], 1);
        let rel = v["inter_track"]["rel_ppm"].as_f64().unwrap();
        assert!((rel - 700.0).abs() < 1e-6);
        assert!((v["inter_track"]["est_misalign_ms_per_hour"].as_f64().unwrap() - 2520.0).abs() < 1e-6);
        let anomalies = v["anomalies"].as_array().unwrap();
        // system 触发三条:|ppm|>500、degraded、reanchors>3
        assert_eq!(anomalies.iter().filter(|a| a["source"] == "system").count(), 3);
    }
}
```

(注:`DriftSourceReport` 需 `#[derive(Clone)]` 以支持测试里的 `..mic.clone()`。)

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib pipeline::drift_monitor`
Expected: 编译错误。

- [ ] **Step 3: 实现**

要点(完整逻辑,非片段):

```rust
struct Inner {
    nominal_hz: u32,
    dll: DriftDll,
    total_samples: u64,
    saw_untimed: bool,
    saw_any: bool,
    first_feed_ns: Option<u64>,
    last_series_bucket: u64,          // 已记录到第几个 10s 桶
    series: Vec<(f64, f64)>,
    events: Vec<DriftEvent>,
    converge_secs: Option<f64>,
}

pub struct DriftMonitor { inner: std::sync::Mutex<Inner> }

impl DriftMonitor {
    pub fn feed(&self, frame: &AudioFrame) {
        let mut g = self.inner.lock().unwrap();
        let ns = match frame.host_time_ns {
            Some(ns) => ns,
            None => { g.saw_untimed = true; crate::audio::host_time::now_ns() }
        };
        let first = *g.first_feed_ns.get_or_insert(ns);
        let total = g.total_samples;
        g.dll.push(total, ns);
        g.total_samples += (frame.samples.len() / frame.channels.max(1) as usize) as u64;
        g.saw_any = true;
        let t_s = ns.saturating_sub(first) as f64 / 1e9;
        let est = g.dll.estimate();
        if g.converge_secs.is_none() && est.converged { g.converge_secs = Some(t_s); }
        let bucket = (t_s / 10.0) as u64;
        if bucket > g.last_series_bucket || g.series.is_empty() {
            g.last_series_bucket = bucket;
            g.series.push((t_s, est.rate_ppm));
        }
    }
    // mark_reanchor: dll.reanchor(!full 时 keep_freq=true);push 事件(t_s 按最近 feed 推算,无 feed 记 0.0)。
    // snapshot: 组装 DriftSourceReport;quality = if saw_untimed {"degraded"} else {"hw"}(未见任何帧也算 degraded)。
}
```

`build_report`:纯函数按 Step 1 测试的 schema 组装;`Source::as_str()` 做 key;单源(仅 mic)时 `inter_track` 为 `null`。anomalies 生成规则三条,每条 `{"source", "kind", "value"}`。

注意帧样本数→单声道样本数:`samples.len() / channels`(AudioFrame 到 tap 时已是 to_mono 前的原始交错?——**不是**:tap 在 to_mono 之前,samples 是交错多声道。总样本推进必须除以声道数,与 `dll` 的 nominal_hz(单声道率)对上;测试用 channels=1 规避,生产注释写明)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib pipeline::drift_monitor`
Expected: 4 passed。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/pipeline/drift_monitor.rs src-tauri/src/pipeline/mod.rs
git commit -m "feat(drift): DriftMonitor——每源 DLL 监视 + drift_report 构建(纯函数可测)"
```

---

### Task 5: frame_tap 接线(真实帧喂料 + 事件重锚)

**Files:**
- Modify: `src-tauri/src/pipeline/frame_tap.rs`(TappedCapture、run_frame_tap_with_origin)
- Test: 同文件测试模块

**Interfaces:**
- Consumes: `DriftMonitor::{feed, mark_reanchor}`。
- Produces: `TappedCapture::with_drift(self, Arc<DriftMonitor>) -> Self`(builder,不动既有 `new`/`new_with_timeline_origin` 签名与全部既有调用点)。

- [ ] **Step 1: 写失败测试**(frame_tap.rs 测试模块内,复用既有 `frame()`/通道装配惯例)

```rust
#[test]
fn drift_monitor_fed_with_real_frames_only() {
    use crate::pipeline::drift_monitor::DriftMonitor;
    let monitor = std::sync::Arc::new(DriftMonitor::new(16_000));
    let (cap_tx, cap_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
    let (out_tx, out_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
    let health = std::sync::Arc::new(SourceHealth::default());
    let policy = TapPolicy { fill_after: Duration::from_millis(50), ..wallclock_policy() };
    let m2 = monitor.clone();
    let t = std::thread::spawn(move || {
        run_frame_tap_with_drift(
            Source::Mic, cap_rx, out_tx, health, policy, TapNotify::none(),
            std::sync::Arc::new(OnceLock::new()), Some(m2),
        )
    });
    // 两个真实帧 → 帧荒 200ms(触发补零) → 再一个真实帧
    for k in 0..2u64 {
        cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000, channels: 1,
            host_time_ns: Some(k * 10_000_000) }).unwrap();
    }
    std::thread::sleep(Duration::from_millis(200));
    cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000, channels: 1,
        host_time_ns: Some(300_000_000) }).unwrap();
    drop(cap_tx);
    t.join().unwrap();
    while out_rx.try_recv().is_ok() {}
    let r = monitor.snapshot();
    // 3 个真实帧全喂,补零帧不喂:总样本 = 3×160
    assert_eq!(r.quality, "hw");
    // 补零段结束应记一次重锚事件
    assert!(r.events.iter().any(|e| e.kind == "reanchor" && e.why == "gap_end"),
        "补零恢复必须重锚,events: {:?}", r.events);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib pipeline::frame_tap::tests::drift_monitor_fed_with_real_frames_only`
Expected: 编译错误(`run_frame_tap_with_drift` 不存在)。

- [ ] **Step 3: 实现**

1. `TappedCapture` 加字段 `drift: Option<Arc<DriftMonitor>>`(两个构造函数初始化为 `None`)+ builder:

```rust
pub fn with_drift(mut self, m: Arc<crate::pipeline::drift_monitor::DriftMonitor>) -> Self {
    self.drift = Some(m);
    self
}
```

2. `run_frame_tap_with_origin` 重命名内部真身为 `run_frame_tap_with_drift(..., drift: Option<Arc<DriftMonitor>>)`,原名保留为 `drift: None` 的转发(既有测试/调用零改动)。
3. 主循环接线(三处,均在现有逻辑旁挂,不改既有语句):
   - 收到真实帧处:`if let Some(m) = &drift { m.feed(&frame); }`(在时钟核对改写 sample_rate **之前**喂——DLL 要的是设备原始声明率下的样本计数与真实时间戳,改写后的率是对账逻辑的产物);
   - 补零段结束(既有 gap 恢复分支):`m.mark_reanchor("gap_end", false)`;
   - 时钟核对改写采样率处(rate_fixes 递增点):`m.mark_reanchor("rate_fix", true)`。
4. `TappedCapture::start` 里把 `self.drift.clone()` 传进线程闭包。

- [ ] **Step 4: 跑测试确认通过(含既有全量)**

Run: `cargo test --lib pipeline::frame_tap`
Expected: 新测试 + 既有 frame_tap 测试全绿。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/pipeline/frame_tap.rs
git commit -m "feat(drift): frame_tap 接线——真实帧喂 DriftMonitor,补零恢复/换率触发重锚"
```

---

### Task 6: 会话装配 + 停录写 drift_report.json

**Files:**
- Modify: `src-tauri/src/lib.rs`(AppState/ActiveSession 附近 `health: Vec<(Source, Arc<SourceHealth>)>` 的同款位置加 `drift: Vec<(Source, Arc<DriftMonitor>)>`;装配处 `lib.rs:1241-1400` 一带;`persist_track_sync`(lib.rs:2102-2147)同伴函数写报告)
- Test: `src-tauri/src/pipeline/drift_monitor.rs` 补落盘函数测试

**Interfaces:**
- Consumes: `TappedCapture::with_drift`、`drift_monitor::build_report`。
- Produces: `drift_monitor::persist_report(note_dir: &Path, sources: &[(Source, Arc<DriftMonitor>)]) -> std::io::Result<()>` → 写 `<note_dir>/drift_report.json`;停录路径在 `persist_track_sync` 调用点之后调用它。

- [ ] **Step 1: 写失败测试**(drift_monitor.rs 测试模块)

```rust
#[test]
fn persist_report_writes_json_with_anomalies() {
    let dir = tempfile::tempdir().unwrap();
    let m = std::sync::Arc::new(DriftMonitor::new(48_000));
    m.feed(&frame(480, 48_000, None)); // degraded → 必然产生一条 anomaly
    persist_report(dir.path(), &[(Source::Mic, m)]).unwrap();
    let raw = std::fs::read_to_string(dir.path().join("drift_report.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["schema"], 1);
    assert_eq!(v["sources"]["mic"]["quality"], "degraded");
    assert!(!v["anomalies"].as_array().unwrap().is_empty());
}
```

(`tempfile` 已是仓库 dev-dependency;若不是,`Cargo.toml [dev-dependencies]` 加 `tempfile = "3"`。)

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib pipeline::drift_monitor::tests::persist_report_writes_json_with_anomalies`
Expected: `persist_report` 未定义。

- [ ] **Step 3: 实现并接线**

1. `persist_report`:snapshot 各源 → `build_report` → `serde_json::to_string_pretty` → 写文件(直接覆盖写,无锁——报告是终值,停录单点调用)。
2. lib.rs 装配(对照 `session_health` 的既有写法):

```rust
// lib.rs:1242 一带,session_health 声明旁:
let mut session_drift: Vec<(Source, Arc<pipeline::drift_monitor::DriftMonitor>)> = Vec::new();
// mic 装配处(mic_health 旁):
let mic_drift = Arc::new(pipeline::drift_monitor::DriftMonitor::new(16_000));
// 注:monitor 的 nominal_hz 用【设备声明率】。tap 在重采样之前,mic 声明率在 start 后
// 才可知(cpal 默认配置)——因此 DriftMonitor::new 改用首帧的 sample_rate 惰性初始化:
// new(0) 表示"以首帧声明率为准"(实现:Inner.nominal_hz==0 时首帧 feed 现场设置,
// 同时设 dll = DriftDll::new(该率))。装配处统一 new(0)。
session_drift.push((Source::Mic, mic_drift.clone()));
// TappedCapture 链:
//   TappedCapture::new(...).with_drift(mic_drift)
// system 同款(sys_health 旁)。
```

3. `ActiveSession`(或 AppState 中与 `health` 同居处,lib.rs:96-98)加 `drift: Vec<(Source, Arc<DriftMonitor>)>`,随会话建立赋值、随拆除丢弃。
4. 停录写报告:`persist_track_sync` 的调用点之后(同一持有 note_dir 的作用域):

```rust
if let Err(e) = pipeline::drift_monitor::persist_report(&note_dir, &state_drift) {
    eprintln!("[drift] 报告写入失败: {e}");
}
for a in /* build_report 里的 anomalies,persist_report 返回或再算一次 */ {
    eprintln!("[drift] 异常: {a}");
}
```

(实现自由度:`persist_report` 返回 `anomalies` 数量或 Value 以便打日志;测试是契约。)

5. `DriftMonitor::new(0)` 惰性率初始化按上述注释实现,并在 Task 4 的测试里补一条:

```rust
#[test]
fn lazy_nominal_rate_locks_to_first_frame() {
    let m = DriftMonitor::new(0);
    m.feed(&frame(441, 44_100, Some(0)));
    assert_eq!(m.snapshot().nominal_hz, 44_100);
}
```

- [ ] **Step 4: 编译 + 全量测试 + 真机冒烟**

Run: `cargo test --lib && cargo check`
Expected: 全绿。
真机冒烟(可选,需 app 环境):录 30 秒停录,确认 `<note_dir>/drift_report.json` 存在、`sources.mic.quality == "hw"`、`rate_ppm` 是个位数到几百的合理值。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/pipeline/drift_monitor.rs
git commit -m "feat(drift): 会话装配双源监视器,停录落 drift_report.json(全本地)"
```

---

### Task 7: mic 实际采样率旁证(kAudioDevicePropertyActualSampleRate)

**Files:**
- Create: `src-tauri/src/audio/actual_rate.rs`(macOS only + 非 macOS stub)
- Modify: `src-tauri/src/audio/mod.rs`(挂模块)、`src-tauri/src/pipeline/drift_monitor.rs`(报告加字段)、lib.rs 装配处

**Interfaces:**
- Produces: `actual_rate::default_input_actual_hz() -> Option<f64>`(查默认输入设备的 `kAudioDevicePropertyActualSampleRate`);`DriftSourceReport` 加 `pub actual_rate_ppm: Option<f64>`(系统口径的实测偏差,与 DLL 估计互为旁证);`DriftMonitor::set_actual_rate_hz(hz: f64)`。

- [ ] **Step 1: 写失败测试**(drift_monitor.rs;CoreAudio 调用本身不做单测,真机冒烟验)

```rust
#[test]
fn actual_rate_ppm_is_relative_to_nominal() {
    let m = DriftMonitor::new(48_000);
    m.feed(&frame(480, 48_000, Some(0)));
    m.set_actual_rate_hz(48_000.48); // +10ppm
    let r = m.snapshot();
    assert!((r.actual_rate_ppm.unwrap() - 10.0).abs() < 0.01);
}
```

- [ ] **Step 2: 确认失败**

Run: `cargo test --lib pipeline::drift_monitor::tests::actual_rate_ppm_is_relative_to_nominal`
Expected: 方法/字段未定义。

- [ ] **Step 3: 实现**

`actual_rate.rs`(coreaudio-sys 直调,与 vpio.rs 同风格):

```rust
//! 查询 CoreAudio 对默认输入设备的实测采样率(系统自己的漂移测量,当 DLL 旁证)。
#[cfg(target_os = "macos")]
pub fn default_input_actual_hz() -> Option<f64> {
    use coreaudio::sys::*;
    unsafe {
        let mut dev: AudioDeviceID = 0;
        let mut size = std::mem::size_of::<AudioDeviceID>() as u32;
        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultInputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMaster,
        };
        if AudioObjectGetPropertyData(kAudioObjectSystemObject, &addr, 0, std::ptr::null(),
            &mut size, &mut dev as *mut _ as *mut _) != 0 || dev == 0 {
            return None;
        }
        let mut rate: f64 = 0.0;
        let mut size = std::mem::size_of::<f64>() as u32;
        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyActualSampleRate,
            mScope: kAudioObjectPropertyScopeInput,
            mElement: kAudioObjectPropertyElementMaster,
        };
        if AudioObjectGetPropertyData(dev, &addr, 0, std::ptr::null(), &mut size,
            &mut rate as *mut _ as *mut _) != 0 || rate <= 0.0 {
            return None;
        }
        Some(rate)
    }
}

#[cfg(not(target_os = "macos"))]
pub fn default_input_actual_hz() -> Option<f64> { None }
```

(若 `kAudioDevicePropertyActualSampleRate` 常量不在 coreaudio-sys:其 FourCC 为 `'asrt'` = 0x61737274,本地定义并注释出处。)

`DriftMonitor`:`set_actual_rate_hz` 存 `actual_rate_ppm = (hz / nominal - 1) * 1e6`;lib.rs 装配处在录音会话内起一个 10s 间隔的轻量线程(或复用现有周期任务)调 `default_input_actual_hz()` 喂给 mic monitor,会话结束随通道关闭退出。

- [ ] **Step 4: 确认通过**

Run: `cargo test --lib pipeline::drift_monitor && cargo check`
Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/audio/actual_rate.rs src-tauri/src/audio/mod.rs src-tauri/src/pipeline/drift_monitor.rs src-tauri/src/lib.rs
git commit -m "feat(drift): ActualSampleRate 旁证——系统实测率与 DLL 估计互校"
```

---

### Task 8: 全库汇总工具 bin/drift_stats

**Files:**
- Create: `src-tauri/src/bin/drift_stats.rs`

**Interfaces:**
- Consumes: 各笔记目录的 `drift_report.json`(schema 1)。
- Produces: CLI。用法 `cargo run --bin drift_stats -- <data_root>`;递归找 `drift_report.json`,输出:场次数、hw/degraded 占比、`inter_track.rel_ppm` 的 P50/P95/max、重锚总数、异常清单 TopN。

- [ ] **Step 1: 写核心纯函数与测试**(统计逻辑独立成函数,bin main 只做 IO)

```rust
//! 漂移报告全库汇总:真实分布的出口(设计文档第四节)。
//! 用法: drift_stats <data_root>   递归扫描 drift_report.json

use std::path::Path;

#[derive(Default)]
struct Agg {
    sessions: usize,
    degraded: usize,
    rel_ppm: Vec<f64>,
    reanchors: u64,
    anomalies: Vec<String>,
}

fn ingest(agg: &mut Agg, v: &serde_json::Value, origin: &str) {
    agg.sessions += 1;
    if v["sources"].as_object().map_or(false, |m| m.values().any(|s| s["quality"] == "degraded")) {
        agg.degraded += 1;
    }
    if let Some(p) = v["inter_track"]["rel_ppm"].as_f64() {
        agg.rel_ppm.push(p.abs());
    }
    for s in v["sources"].as_object().into_iter().flat_map(|m| m.values()) {
        agg.reanchors += s["reanchors"].as_u64().unwrap_or(0);
    }
    for a in v["anomalies"].as_array().into_iter().flatten() {
        agg.anomalies.push(format!("{origin}: {a}"));
    }
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() { return None; }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    Some(sorted[idx])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ingest_and_percentiles() {
        let mut agg = Agg::default();
        for ppm in [10.0, -50.0, 120.0] {
            let v = serde_json::json!({
                "schema": 1,
                "sources": {"mic": {"quality": "hw", "reanchors": 1}},
                "inter_track": {"rel_ppm": ppm},
                "anomalies": []
            });
            ingest(&mut agg, &v, "n");
        }
        agg.rel_ppm.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(agg.sessions, 3);
        assert_eq!(agg.reanchors, 3);
        assert_eq!(percentile(&agg.rel_ppm, 0.5), Some(50.0));
        assert_eq!(percentile(&agg.rel_ppm, 1.0), Some(120.0));
    }
}
```

- [ ] **Step 2: 确认失败→补 main→通过**

Run: `cargo test --bin drift_stats`
Expected: 先编译错误,补齐后通过。main:

```rust
fn main() {
    let root = std::env::args().nth(1).expect("用法: drift_stats <data_root>");
    let mut agg = Agg::default();
    let mut stack = vec![std::path::PathBuf::from(&root)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() { stack.push(p); continue; }
            if p.file_name().map_or(false, |n| n == "drift_report.json") {
                if let Ok(v) = std::fs::read_to_string(&p)
                    .map_err(anyhow::Error::from)
                    .and_then(|s| Ok(serde_json::from_str::<serde_json::Value>(&s)?)) {
                    ingest(&mut agg, &v, &p.display().to_string());
                }
            }
        }
    }
    agg.rel_ppm.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("场次: {}  含降级源: {}", agg.sessions, agg.degraded);
    println!("|轨间漂移| ppm  P50={:?} P95={:?} max={:?}",
        percentile(&agg.rel_ppm, 0.5), percentile(&agg.rel_ppm, 0.95), percentile(&agg.rel_ppm, 1.0));
    println!("重锚总数: {}", agg.reanchors);
    for a in agg.anomalies.iter().take(20) { println!("异常 {a}"); }
}
```

- [ ] **Step 3: 手工验证**

Run: `cargo run --bin drift_stats -- /tmp/nonexistent`
Expected: `场次: 0 ...`(不崩)。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/bin/drift_stats.rs
git commit -m "feat(drift): drift_stats 汇总工具——真实分布出口(P50/P95/异常清单)"
```

---

### Task 9: E1 标定工具 bin/xcorr_align(传感器的裁判)

**Files:**
- Create: `src-tauri/src/bin/xcorr_align.rs`
- Create: `scripts/drift-calibration.md`(标定操作步骤)

**Interfaces:**
- Consumes: 双轨 WAV(`mic.wav`/`system.wav`,16k mono s16,仓库录音即此格式)。
- Produces: CLI。用法 `cargo run --bin xcorr_align -- <a.wav> <b.wav>`;每 10s 取 1s 窗做归一化互相关(搜索 ±250ms),输出 `t_s, offset_ms` 表 + 对 offset(t) 的线性拟合斜率(即互相关口径的轨间 ppm),与 drift_report 的 `rel_ppm` 对照。

- [ ] **Step 1: 写核心纯函数与测试**

```rust
//! E1 标定:互相关直接量两条 WAV 的真实错位曲线,校准 DLL 传感器
//! (docs/2026-08-12-clock-drift-sensor-design.md 第五节 E1)。

/// 归一化互相关:在 b 中搜索与 a[t0..t0+win] 最相似的偏移(±search 样本)。
/// 返回 (best_offset_samples, peak_corr)。互相关无峰(能量过低)→ None。
fn xcorr_offset(a: &[f32], b: &[f32], t0: usize, win: usize, search: i64) -> Option<(i64, f32)> {
    let a_seg = a.get(t0..t0 + win)?;
    let ea: f32 = a_seg.iter().map(|x| x * x).sum();
    if ea < 1e-6 { return None; }
    let mut best = (0i64, f32::MIN);
    for off in -search..=search {
        let start = t0 as i64 + off;
        if start < 0 { continue; }
        let Some(b_seg) = b.get(start as usize..start as usize + win) else { continue };
        let eb: f32 = b_seg.iter().map(|x| x * x).sum();
        if eb < 1e-6 { continue; }
        let dot: f32 = a_seg.iter().zip(b_seg).map(|(x, y)| x * y).sum();
        let corr = dot / (ea.sqrt() * eb.sqrt());
        if corr > best.1 { best = (off, corr); }
    }
    (best.1 > 0.5).then_some(best)
}

/// 对 (t_s, offset_ms) 序列做最小二乘线性拟合,返回斜率(ms/s → ×1000 = ppm)。
fn linear_slope_ppm(points: &[(f64, f64)]) -> Option<f64> {
    if points.len() < 2 { return None; }
    let n = points.len() as f64;
    let (sx, sy): (f64, f64) = points.iter().fold((0.0, 0.0), |(a, b), p| (a + p.0, b + p.1));
    let (mx, my) = (sx / n, sy / n);
    let num: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let den: f64 = points.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
    (den > 0.0).then(|| num / den * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_known_offset() {
        // a = 白噪(确定性伪随机);b = a 右移 37 样本
        let mut seed = 1u64;
        let mut noise = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 40) as f32 / 8388608.0 - 1.0 };
        let a: Vec<f32> = (0..32_000).map(|_| noise()).collect();
        let mut b = vec![0.0f32; 37];
        b.extend_from_slice(&a);
        let (off, corr) = xcorr_offset(&a, &b, 8_000, 1_600, 100).unwrap();
        assert_eq!(off, 37);
        assert!(corr > 0.9);
    }

    #[test]
    fn slope_recovers_ppm() {
        // 100ppm ⇒ 每秒错位增长 0.1ms
        let pts: Vec<(f64, f64)> = (0..30).map(|k| (k as f64 * 10.0, k as f64 * 10.0 * 0.1)).collect();
        assert!((linear_slope_ppm(&pts).unwrap() - 100.0).abs() < 1e-6);
    }
}
```

- [ ] **Step 2: 确认失败→补 main→通过**

Run: `cargo test --bin xcorr_align`
main:hound 读两个 WAV(断言 16k mono),循环 `t0 = k·10s`,`win = 1s = 16_000`,`search = 4_000`(±250ms),打印表格与 `linear_slope_ppm` 结果。

```rust
fn main() {
    let mut args = std::env::args().skip(1);
    let (pa, pb) = (args.next().expect("用法: xcorr_align <a.wav> <b.wav>"), args.next().expect("缺第二个 wav"));
    let read = |p: &str| -> (Vec<f32>, u32) {
        let mut r = hound::WavReader::open(p).expect("打不开 wav");
        let spec = r.spec();
        assert_eq!(spec.channels, 1, "只支持 mono");
        let v = r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect();
        (v, spec.sample_rate)
    };
    let (a, ra) = read(&pa);
    let (b, rb) = read(&pb);
    assert_eq!(ra, rb, "两轨采样率必须一致");
    let (win, step, search) = (ra as usize, ra as usize * 10, ra as i64 / 4);
    let mut pts = Vec::new();
    let mut t0 = 0usize;
    println!("t_s\toffset_ms\tcorr");
    while t0 + win < a.len().min(b.len()) {
        if let Some((off, corr)) = xcorr_offset(&a, &b, t0, win, search) {
            let (t_s, off_ms) = (t0 as f64 / ra as f64, off as f64 * 1000.0 / ra as f64);
            println!("{t_s:.0}\t{off_ms:+.2}\t{corr:.2}");
            pts.push((t_s, off_ms));
        }
        t0 += step;
    }
    match linear_slope_ppm(&pts) {
        Some(ppm) => println!("互相关口径轨间漂移: {ppm:+.1} ppm(对照 drift_report.inter_track.rel_ppm)"),
        None => println!("有效窗不足,无法拟合"),
    }
}
```

- [ ] **Step 3: 写标定操作文档** `scripts/drift-calibration.md`

```markdown
# E1 漂移传感器标定(可重复)

1. 准备刺激音:任何含丰富瞬态的音频(如节拍器/click track),用系统播放器循环播放。
2. voice-notes 正常开录 ≥10 分钟(双轨:mic 收房间回放,system 收播放流)。
3. 停录,找到笔记目录(设置页可打开数据目录,notes/<id>/)。
4. 互相关真值:
   cargo run --bin xcorr_align -- <note_dir>/mic.wav <note_dir>/system.wav
5. 传感器读数:<note_dir>/drift_report.json 的 inter_track.rel_ppm。
6. 判定:两者之差 < 5ppm 且 offset 曲线形状一致 → 传感器可当裁判(E2/E3 复用本流程)。
   注意:互相关量的是"含起点偏移的错位曲线",传感器量的是斜率;只对比斜率(ppm)。
```

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/bin/xcorr_align.rs scripts/drift-calibration.md
git commit -m "feat(drift): xcorr_align 标定工具 + E1 操作文档——互相关真值校准传感器"
```

---

### Task 10: 收尾——全量回归 + 文档勘误同步

**Files:**
- Modify: `docs/2026-08-12-clock-drift-sensor-design.md`(如实现与设计有偏差,回写)
- Modify: `docs/asr-pipeline.md` 第 9 节(质量诊断切面,补一句 drift_report 观测点)

- [ ] **Step 1: 全量回归**

Run: `cargo test --lib && cargo test --bins && cargo check --target x86_64-pc-windows-msvc 2>/dev/null || cargo check`
Expected: 全绿(Windows 交叉 check 若无工具链则本机 check,Windows 兼容由 `#[cfg]`/`None` 降级保证,CI 兜底)。

- [ ] **Step 2: 文档同步**

`docs/asr-pipeline.md` 第 9 节表尾追加一行:

```markdown
A -. "时钟漂移(drift_report.json,每源 ppm + 轨间错位估计)" .-> QA
```

设计文档如有实现偏差(常量、字段名),逐条回写并注明"实现期修订"。

- [ ] **Step 3: 真机冒烟清单**(人工,记录进 PR 描述)

1. 正常双轨录音 1 分钟 → drift_report.json 存在,mic/system 均 `quality: "hw"`;
2. rel_ppm 与 xcorr_align 斜率同量级(粗对照,精确校准走 E1 10 分钟流程);
3. 录音中途拔插耳机 → events 出现 reanchor,录音不崩;
4. Windows 构建通过(CI)。

- [ ] **Step 4: Commit + 收尾**

```bash
git add docs/
git commit -m "docs(drift): 诊断切面补 drift 观测点 + 设计文档实现期修订"
```

之后按 superpowers:finishing-a-development-branch 走 PR 流程(PR 描述附真机冒烟清单)。

---

## Self-Review 记录

- **Spec 覆盖**:设计文档一~四节(DLL/时间戳/接线/出口)→ Task 1–7;第五节 E1 工具 → Task 9;`bin/drift_stats` → Task 8;异常打点(勘误后口径:report.anomalies + eprintln)→ Task 4/6;"只测不动数据"铁律 → 各任务未触碰音频样本与既有对账;Windows stub → Task 1/3/7 的 cfg/None 路径。E2/E3 实验本身是二期执行事项,不在本计划(工具已备)。
- **占位符扫描**:无 TBD;Task 6 的"实现自由度"处均指明契约=测试。
- **类型一致性**:`DriftMonitor::new(0)` 惰性率(Task 6 引入)与 Task 4 的 `new(nominal_hz)` 兼容(0 = 哨兵);`run_frame_tap_with_drift` 在 Task 5 定义、Task 5 测试消费;`persist_report` 在 Task 6 定义并被 lib.rs 消费;`DriftSourceReport.actual_rate_ppm` Task 7 增补(Task 4 测试用 `..clone()` 构造,新增字段不破坏)——注意 Task 4 的 `report_computes_inter_track_and_anomalies` 测试在 Task 7 后需补 `actual_rate_ppm: None` 字段初始化,属机械修补。
