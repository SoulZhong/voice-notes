//! 实时预对齐(签名版):按实测**签名**到达延迟,把参考或 capture 扣压后再喂
//! AEC3,残余滞后落回其 ~250ms 舒适区。设计见 specs/2026-07-14-...-design.md
//! §二期;2026-08-07 根因调查扩展为双向。
//!
//! 两个方向,两种病:
//! - **正延迟**(回声滞后参考,蓝牙外放 300~1200ms):扣压 render,原有机制。
//! - **负延迟**(参考迟到——SCK 交付滞后超过内置扬声器 ~30ms 的声学差,参考在
//!   回声之后才到达):因果性破坏,AEC3 消除恒为零且其估计器钉在 0(负值钳位),
//!   正向搜索域也看不见它。修复:扣压 **capture** 侧,把 mic 样本对 AEC 的交付
//!   推迟到参考就位之后。只动墙钟交付时刻,样本域(内容/顺序/计数/文件位置/
//!   VAD 时间戳)完全不变;流结束由 flush_capture 排空,不丢尾音。
//!
//! 目标:有效延迟恒 ≈ +HEADROOM(100ms)。签名目标 T = D_env − 100:T ≥ 0 扣
//! render T;T < 0 扣 capture −T(上限 CAPTURE_MAX_MS)。映射连续,无 0 点跳变
//! (估计噪声不会引发两侧翻覆振荡);滞回带按签名值判定。
//!
//! 控制回路稳定性:两条包络都取自进环形缓冲**之前**的原始流,估计值 D_env 与
//! 两侧预延迟均无关;调整不影响后续测量,无反馈振荡。估计内联在 render 线程,
//! 亚毫秒级,无独立 worker、无 panic 面(纯安全代码)。
//!
//! 低置信度/数据不足 → 不动任何一侧:行为等同现状,增强永不劣化录制。

use crate::audio::delay_estimate::{estimate_delay, DelayEstimate};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// 包络帧 10ms @16k。
const ENV_FRAME: usize = 160;
/// 滑窗与节奏:每 5s 用最近 20s 重估;搜索上限与一期一致。
const ESTIMATE_EVERY_FRAMES: usize = 500; // 5s
const WINDOW_FRAMES: usize = 2000; // 20s
const MAX_DELAY_MS: u32 = 1200;
/// 目标残余延迟:预对齐后留给 AEC3 内置估计器的量(舒适区中段)。
const HEADROOM_MS: u32 = 100;
/// 滞回带:与现值差超过才调整(调整触发 AEC3 重收敛,不能抖)。
const HYSTERESIS_MS: u32 = 80;
/// 双门限直接引用一期离线清洗的导出常量(echo_clean 真实录音标定,依据见
/// 该处注释)——单一定义点,将来重标定只改一处,实时/离线永不漂移。
/// 20s 实时窗与标定的 60s 窗分布同域假设未实测(冒烟见 peak 0.301 贴 0.30
/// 门限通过,余量薄);失败方向安全:漏检只退化为不调整=现状。
use crate::audio::echo_clean::{CONFIDENCE_GATE, PEAK_GATE};
/// 包络滑窗容量(30s,留窗外余量)与环形缓冲硬上限(防病态预延迟吃内存)。
const ENV_CAP: usize = 3000;
const RING_CAP: usize = (MAX_DELAY_MS as usize) * 2 * 16;
/// capture 侧扣压上限:SCK 交付滞后实测量级为几十到几百 ms,600ms 足够覆盖;
/// 超出此值的估计视为病态,宁可不修(留给离线清洗)也不给 mic 路加更大延迟。
const CAPTURE_MAX_MS: u32 = 600;
const CAP_RING_CAP: usize = (CAPTURE_MAX_MS as usize) * 2 * 16;

pub struct AlignState {
    inner: Mutex<Inner>,
}

struct Inner {
    predelay: usize, // render 侧扣压(样本数)
    ring: VecDeque<f32>,
    /// capture 侧扣压(样本数):参考迟到(负到达延迟)时启用,与 predelay 互斥
    /// (签名目标只落在一侧,另一侧恒 0)。
    capture_predelay: usize,
    cap_ring: VecDeque<f32>,
    ref_env: VecDeque<f32>,
    ref_carry: Vec<f32>,
    obs_env: VecDeque<f32>,
    obs_carry: Vec<f32>,
    frames_since_estimate: usize,
}

impl Inner {
    fn accumulate_ref_env(&mut self, samples: &[f32]) -> usize {
        accumulate_env(&mut self.ref_carry, &mut self.ref_env, samples)
    }

    fn accumulate_obs_env(&mut self, samples: &[f32]) -> usize {
        accumulate_env(&mut self.obs_carry, &mut self.obs_env, samples)
    }
}

pub fn new(initial_predelay_ms: u32) -> Arc<AlignState> {
    // 钳到搜索上限:预延迟超过 RING_CAP 会让 take 恒为 0、render 输出永久饿死
    // (静默禁用回声消除)。正常路径(初值 450/估计上限 1100)远够不着,钳制只防
    // 越界入参这一潜在脚枪(终审跟进)。
    let clamped = initial_predelay_ms.min(MAX_DELAY_MS);
    Arc::new(AlignState {
        inner: Mutex::new(Inner {
            predelay: (clamped as usize) * 16,
            ring: VecDeque::new(),
            capture_predelay: 0,
            cap_ring: VecDeque::new(),
            ref_env: VecDeque::new(),
            ref_carry: Vec::new(),
            obs_env: VecDeque::new(),
            obs_carry: Vec::new(),
            frames_since_estimate: 0,
        }),
    })
}

/// carry+定长分帧的增量 RMS 包络:与 delay_estimate::envelope 同公式,
/// 但适配流式零散块(尾部不足一帧滞留 carry,不像批处理并入末帧)。
fn accumulate_env(carry: &mut Vec<f32>, env: &mut VecDeque<f32>, samples: &[f32]) -> usize {
    carry.extend_from_slice(samples);
    let mut new_frames = 0;
    while carry.len() >= ENV_FRAME {
        let frame: Vec<f32> = carry.drain(..ENV_FRAME).collect();
        let rms = (frame.iter().map(|x| x * x).sum::<f32>() / ENV_FRAME as f32).sqrt();
        env.push_back(rms);
        if env.len() > ENV_CAP {
            env.pop_front();
        }
        new_frames += 1;
    }
    new_frames
}

impl AlignState {
    /// system 路:累积 ref 包络 → 样本入环 → 吐出超过预延迟的部分。
    /// 每满 5s(按 ref 包络帧计)内联重估一次。
    pub fn on_render(&self, samples: &[f32]) -> Vec<f32> {
        let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let new_frames = g.accumulate_ref_env(samples);
        g.frames_since_estimate += new_frames;

        g.ring.extend(samples.iter().copied());
        // 硬上限:病态大预延迟下也不无界吃内存(截断意味着丢最老参考,可接受)。
        while g.ring.len() > RING_CAP {
            g.ring.pop_front();
        }
        let take = g.ring.len().saturating_sub(g.predelay);
        let out: Vec<f32> = g.ring.drain(..take).collect();

        if g.frames_since_estimate >= ESTIMATE_EVERY_FRAMES {
            g.frames_since_estimate = 0;
            maybe_adjust(&mut g);
        }
        out
    }

    /// mic 路(消回声之前调用):累积 obs 包络(取自扣压之前的原始到达流,保持
    /// 估计与预延迟解耦)→ 样本入环 → 吐出超过 capture 扣压量的部分。
    /// 扣压为 0(常态)时等价于透传,零拷贝开销仅一次 extend+drain。
    pub fn on_capture(&self, samples: &[f32]) -> Vec<f32> {
        let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.accumulate_obs_env(samples);
        g.cap_ring.extend(samples.iter().copied());
        while g.cap_ring.len() > CAP_RING_CAP {
            // 硬上限兜底:正常路径 capture_predelay ≤ CAPTURE_MAX_MS,够不着;
            // 若真触发,丢最老样本(≤10ms 级)好过无界吃内存。
            g.cap_ring.pop_front();
        }
        let take = g.cap_ring.len().saturating_sub(g.capture_predelay);
        g.cap_ring.drain(..take).collect()
    }

    /// 流结束:排空 capture 环里被扣压的尾部样本。**必须调用**——否则扣压量
    /// (最多 CAPTURE_MAX_MS)的真实尾音会丢失;render 环无需对应操作(参考流
    /// 丢尾无害)。
    pub fn flush_capture(&self) -> Vec<f32> {
        let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.cap_ring.drain(..).collect()
    }

    pub fn predelay_ms(&self) -> u32 {
        let g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (g.predelay / 16) as u32
    }

    pub fn capture_predelay_ms(&self) -> u32 {
        let g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (g.capture_predelay / 16) as u32
    }

    #[cfg(test)]
    pub(crate) fn set_capture_predelay_for_test(&self, ms: u32) {
        let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.capture_predelay = (ms as usize) * 16;
    }
}

/// 重估并按门限+滞回决定是否调整预延迟。锁内调用(计算亚毫秒级)。
fn maybe_adjust(g: &mut Inner) {
    let n = WINDOW_FRAMES.min(g.ref_env.len()).min(g.obs_env.len());
    if n < WINDOW_FRAMES {
        return; // 不足 20s 数据,不动
    }
    let ref_win: Vec<f32> = g.ref_env.iter().rev().take(n).rev().copied().collect();
    let obs_win: Vec<f32> = g.obs_env.iter().rev().take(n).rev().copied().collect();
    // 双向签名测量:正向 = obs 落后 ref(正常因果,蓝牙外放),反向 = ref 落后
    // obs(参考迟到——SCK 交付滞后,2026-08-07 实锤的一贯失效根因)。
    let fwd = estimate_delay(&ref_win, &obs_win, MAX_DELAY_MS);
    let rev = estimate_delay(&obs_win, &ref_win, MAX_DELAY_MS);
    // [诊断日志,每 5s 一行] 真机验证修复效果的主要读数,验证期保留。
    {
        let f = fwd
            .as_ref()
            .map(|d| format!("+{}ms conf {:.1} peak {:.3}", d.delay_ms, d.confidence, d.peak))
            .unwrap_or_else(|| "无".into());
        let r = rev
            .as_ref()
            .map(|d| format!("-{}ms conf {:.1} peak {:.3}", d.delay_ms, d.confidence, d.peak))
            .unwrap_or_else(|| "无".into());
        eprintln!(
            "[对齐诊断] 正向 {f} | 反向 {r} | 当前扣压 render {}ms capture {}ms",
            g.predelay / 16,
            g.capture_predelay / 16
        );
    }
    // 取签名延迟 D:两个方向各自过门限后,置信度高者胜;平票取正向(render 侧
    // 扣压不带 mic 路延迟代价,更保守)。零点附近包络自相关会让两向同时给小值,
    // 连续映射(下方 T = D − HEADROOM)保证两侧目标差 ≤ 2×HEADROOM,滞回可吸收。
    let gated = |d: &Option<DelayEstimate>| -> Option<DelayEstimate> {
        d.clone().filter(|d| d.confidence >= CONFIDENCE_GATE && d.peak >= PEAK_GATE)
    };
    let signed_d_ms: i64 = match (gated(&fwd), gated(&rev)) {
        (Some(f), Some(r)) => {
            if r.confidence > f.confidence {
                -(r.delay_ms as i64)
            } else {
                f.delay_ms as i64
            }
        }
        (Some(f), None) => f.delay_ms as i64,
        (None, Some(r)) => -(r.delay_ms as i64),
        (None, None) => return, // 无可靠回声证据:永不动时间轴
    };
    // 连续签名目标:T = D − HEADROOM。T ≥ 0 扣 render T;T < 0 扣 capture −T。
    // 有效延迟恒被拉到 ≈ +HEADROOM(AEC3 舒适区),映射在 D 全域连续,无跳变。
    let t_ms = signed_d_ms - HEADROOM_MS as i64;
    let target_render_ms = t_ms.max(0).min(MAX_DELAY_MS as i64) as u32;
    let target_capture_ms = (-t_ms).max(0).min(CAPTURE_MAX_MS as i64) as u32;
    let current_signed_ms = (g.predelay / 16) as i64 - (g.capture_predelay / 16) as i64;
    let target_signed_ms = target_render_ms as i64 - target_capture_ms as i64;
    if (target_signed_ms - current_signed_ms).unsigned_abs() <= HYSTERESIS_MS as u64 {
        return; // 滞回带内
    }
    let old_render = (g.predelay / 16) as u32;
    let old_capture = (g.capture_predelay / 16) as u32;
    // render 侧:减小时立即放掉多扣的参考(丢弃语义,AEC3 随后重收敛);
    // 增大无需动环,后续 take 自然扣更多。
    let target = (target_render_ms as usize) * 16;
    if target < g.predelay {
        let drop = g.predelay - target;
        let n = drop.min(g.ring.len());
        g.ring.drain(..n);
    }
    g.predelay = target;
    // capture 侧:减小时**不丢样本**——那是真实 mic 音频,多扣的部分随后续
    // take 自然吐出(一次性多交付,下游按序消费,样本域不变);增大同理无需动环。
    g.capture_predelay = (target_capture_ms as usize) * 16;
    eprintln!(
        "实时预对齐调整: render {old_render}->{target_render_ms}ms capture {old_capture}->{target_capture_ms}ms \
         (签名延迟 {signed_d_ms}ms)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::delay_estimate::tests::block_modulated_noise;

    /// 初始预延迟生效:前 P 毫秒的参考被扣住,输出总量 = 输入 - P。
    #[test]
    fn initial_predelay_withholds_reference() {
        let a = new(450);
        let mut out = 0usize;
        for _ in 0..100 {
            out += a.on_render(&vec![0.1f32; 160]).len(); // 100×10ms = 1s
        }
        assert_eq!(out, 16_000 - 450 * 16, "1s 输入应吐出 1s-450ms");
        assert_eq!(a.predelay_ms(), 450);
    }

    /// 600ms 真回声:喂 30s 后预延迟应收敛到 600-100=500ms 附近(±60ms)。
    #[test]
    fn converges_predelay_on_600ms_echo() {
        let a = new(0);
        let mut seed = 5u64;
        let system = block_modulated_noise(16_000 * 30, &mut seed);
        let delay = 9600;
        let mut mic = vec![0.0f32; system.len()];
        for i in delay..mic.len() {
            mic[i] = system[i - delay] * 0.5;
        }
        for (s, m) in system.chunks(160).zip(mic.chunks(160)) {
            let _ = a.on_render(s);
            a.on_capture(m);
        }
        let p = a.predelay_ms() as i64;
        assert!((p - 500).unsigned_abs() <= 60, "预延迟应≈500ms,实际 {p}ms");
    }

    /// 滞回:估计与现值差 <80ms 不动(500ms 初值,真实 540ms → 目标 440,差 60 → 保持)。
    #[test]
    fn hysteresis_ignores_small_drift() {
        let a = new(500);
        let mut seed = 9u64;
        let system = block_modulated_noise(16_000 * 30, &mut seed);
        let delay = 8640; // 540ms → 目标 P=440,与现值 500 差 60ms < 80ms
        let mut mic = vec![0.0f32; system.len()];
        for i in delay..mic.len() {
            mic[i] = system[i - delay] * 0.5;
        }
        for (s, m) in system.chunks(160).zip(mic.chunks(160)) {
            let _ = a.on_render(s);
            a.on_capture(m);
        }
        assert_eq!(a.predelay_ms(), 500, "差距在滞回带内不得调整");
    }

    /// 无关信号(无回声):门限拒绝,预延迟保持初值。
    #[test]
    fn unrelated_streams_never_adjust() {
        let a = new(450);
        let mut s1 = 7u64;
        let mut s2 = 4242u64;
        let system = block_modulated_noise(16_000 * 30, &mut s1);
        let mic = block_modulated_noise(16_000 * 30, &mut s2);
        for (s, m) in system.chunks(160).zip(mic.chunks(160)) {
            let _ = a.on_render(s);
            a.on_capture(m);
        }
        assert_eq!(a.predelay_ms(), 450, "无回声证据不得动时间轴");
    }

    /// 参考迟到(负到达延迟,2026-08-07 根因):mic 回声比参考内容早 150ms 到达。
    /// 正向搜索域 [0,1200] 看不见负延迟 → 修复前永不调整;修复后应在 capture 侧
    /// 扣压 100+150=250ms,把有效延迟拉回 +100ms 舒适区。
    #[test]
    fn converges_capture_predelay_when_reference_late() {
        let a = new(0);
        let mut seed = 5u64;
        let content = block_modulated_noise(16_000 * 30, &mut seed);
        let late = 2400usize; // 参考交付滞后 150ms
        for i in 0..(content.len() / 160) {
            let lo = i * 160;
            let hi = lo + 160;
            // mic:回声瞬时到达(声学延迟并入 late 的相对量,简化为 0)
            let echo: Vec<f32> = content[lo..hi].iter().map(|x| x * 0.5).collect();
            let _ = a.on_capture(&echo);
            // 参考:内容整体滞后 late 个样本
            let mut refch = vec![0.0f32; 160];
            for j in lo..hi {
                if j >= late {
                    refch[j - lo] = content[j - late];
                }
            }
            let _ = a.on_render(&refch);
        }
        assert_eq!(a.predelay_ms(), 0, "render 侧不该扣压");
        let p = a.capture_predelay_ms() as i64;
        assert!((p - 250).unsigned_abs() <= 60, "capture 侧应≈250ms,实际 {p}ms");
    }

    /// capture 侧样本守恒:扣压只推迟交付,不丢样本——累计输出+flush = 累计输入。
    #[test]
    fn capture_withholding_conserves_samples_via_flush() {
        let a = new(0);
        // 手动置一个 capture 扣压量(绕过估计器,直接测环机制)
        a.set_capture_predelay_for_test(200);
        let mut fed = 0usize;
        let mut out = 0usize;
        for i in 0..500 {
            let n = if i % 3 == 0 { 96 } else { 160 };
            fed += n;
            out += a.on_capture(&vec![0.05f32; n]).len();
        }
        assert_eq!(out, fed.saturating_sub(200 * 16), "扣压期输出=输入-扣压量");
        out += a.flush_capture().len();
        assert_eq!(out, fed, "flush 后样本全数交付");
    }

    /// 样本守恒(总量):任意时刻累计输出 = 累计输入 - 当前扣压量(预延迟窗内样本)。
    #[test]
    fn render_output_conserves_minus_held() {
        let a = new(200);
        let mut fed = 0usize;
        let mut out = 0usize;
        for i in 0..1000 {
            let n = if i % 3 == 0 { 96 } else { 160 }; // 零散块
            fed += n;
            out += a.on_render(&vec![0.05f32; n]).len();
        }
        assert_eq!(out, fed.saturating_sub(200 * 16), "输出=输入-扣压量");
    }
}
