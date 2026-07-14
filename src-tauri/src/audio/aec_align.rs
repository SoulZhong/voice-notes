//! 实时预对齐(二期核心):把 system 参考按实测延迟扣压后再喂 AEC3,残余滞后
//! 落回其 ~250ms 舒适区。设计见 specs/2026-07-14-...-design.md §二期;一期标定
//! 实锤蓝牙延迟可漂至 1200ms 搜索上限,故滑窗每 5s 重估。
//!
//! 控制回路稳定性:两条包络都取自进 AEC 层之前的原始流(ref 在环形缓冲之前),
//! 估计值 D_env 与预延迟 P 无关;目标 P = D_env - 100ms,调 P 不影响后续测量,
//! 无反馈振荡。估计计算 ~0.24M 次乘加、亚毫秒级,内联在 render 线程,无独立
//! worker、无 panic 面(纯安全代码)。
//!
//! 低置信度/数据不足 → 不动 P:行为等同现状,增强永不劣化录制。

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
/// 双门限与一期离线清洗同源(echo_clean 真实录音标定):conf 保证峰唯一,
/// peak 保证回声真实存在。20s 窗与标定的 60s 窗分布同域,沿用定值。
const CONFIDENCE_GATE: f32 = 2.0;
const PEAK_GATE: f32 = 0.30;
/// 包络滑窗容量(30s,留窗外余量)与环形缓冲硬上限(防病态预延迟吃内存)。
const ENV_CAP: usize = 3000;
const RING_CAP: usize = (MAX_DELAY_MS as usize) * 2 * 16;

pub struct AlignState {
    inner: Mutex<Inner>,
}

struct Inner {
    predelay: usize, // 样本数
    ring: VecDeque<f32>,
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
    Arc::new(AlignState {
        inner: Mutex::new(Inner {
            predelay: (initial_predelay_ms as usize) * 16,
            ring: VecDeque::new(),
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

    /// mic 路(消回声之前调用):只累积 obs 包络。
    pub fn on_capture(&self, samples: &[f32]) {
        let mut g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.accumulate_obs_env(samples);
    }

    pub fn predelay_ms(&self) -> u32 {
        let g = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (g.predelay / 16) as u32
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
    let Some(DelayEstimate { delay_ms, confidence, peak }) =
        estimate_delay(&ref_win, &obs_win, MAX_DELAY_MS)
    else {
        return;
    };
    if confidence < CONFIDENCE_GATE || peak < PEAK_GATE {
        return; // 无可靠回声证据:永不动时间轴
    }
    let target_ms = delay_ms.saturating_sub(HEADROOM_MS);
    let current_ms = (g.predelay / 16) as u32;
    if (target_ms as i64 - current_ms as i64).unsigned_abs() <= HYSTERESIS_MS as u64 {
        return; // 滞回带内
    }
    let target = (target_ms as usize) * 16;
    if target < g.predelay {
        // 减小预延迟:立即放掉多扣的参考(丢弃语义,AEC3 随后重收敛)。
        let drop = g.predelay - target;
        let n = drop.min(g.ring.len());
        g.ring.drain(..n);
    }
    // 增大预延迟:无需动环,后续 take 自然扣更多(参考流出现等长静默,AEC3 重收敛)。
    g.predelay = target;
    eprintln!(
        "实时预对齐调整: {current_ms}ms -> {target_ms}ms (估计延迟 {delay_ms}ms conf {confidence:.2} peak {peak:.3})"
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
