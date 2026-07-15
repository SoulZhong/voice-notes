# 软件回声消除二期：实时预对齐 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 录制过程中滑窗实测外放延迟并动态预对齐 system 参考流,让 AEC3 在蓝牙 300~1200ms 延迟下实时收敛——转写与录音落盘的 mic 流即已消回声。

**Architecture:** 全部落在 `audio/` 层:新模块 `aec_align.rs` 的 `AlignState`(参考环形缓冲 + 双流包络滑窗 + 每 5s 重估 + 滞回调整)内嵌进 AEC 句柄的 push/process 路径;`aec.rs` 新增 `new_aligned_pair`(AEC3+AGC2+NS 温和档+对齐);`lib.rs` 一处改调用(蓝牙探测给预对齐初值)。`segment_worker` 零改动。

**Tech Stack:** Rust;复用一期 `delay_estimate`(包络+NCC+双指标)与其真实录音标定值;webrtc-audio-processing 2.1.0。

**规格:** `docs/superpowers/specs/2026-07-14-voice-notes-soft-aec-tuning-design.md` §二期(含一期修订的漂移假设)
**分支:** `soft-aec-p2`,基于 `soft-aec-tuning`(PR #41 之上叠栈;#41 合入后 rebase/改基)

## Global Constraints

- 零用户配置:蓝牙探测给初值,滑窗实测驱动调整,不加设置项。
- 增强是增值层:低置信度/估计不可用 → 预对齐维持现值,行为等同现状;绝不影响录制。
- 音频恒 16kHz 单声道;AEC 帧 10ms(160 样本);包络帧 10ms。
- 调整滞回:置信度双门限(与一期标定同源: conf≥2.0 且 peak≥0.30)且与当前预延迟差 >80ms 才动;目标残余延迟 100ms(AEC3 舒适区)。
- 观测面:每次预对齐调整 eprintln;`get_stats().delay_ms` 每 10s eprintln。
- 一期语义不回归:`new_pair`/`new_clean_pair` 签名与行为不变(既有测试零修改);离线清洗链路不动。
- 提交信息中文、动机导向,不加任何 Co-Authored-By / Generated-with 尾注。
- 每任务结束 `cd src-tauri && cargo test` 全绿再提交(基线 441 lib + 1 集成,8 ignored)。

## 文件结构

| 文件 | 职责 |
|---|---|
| Create `src-tauri/src/audio/aec_align.rs` | AlignState:参考预延迟环形缓冲+双流包络滑窗+周期重估+滞回调整(纯逻辑,可单测) |
| Modify `src-tauri/src/audio/aec.rs` | `new_aligned_pair`(生产实时对):AEC3+AGC2+NS+挂 AlignState;capture 侧 10s 节流 stats 日志 |
| Modify `src-tauri/src/audio/mod.rs` | 挂 `pub mod aec_align;` |
| Modify `src-tauri/src/lib.rs` | 装配点改调 `new_aligned_pair`,蓝牙探测给初值(450ms/0),启动 eprintln 更新 |

## 控制回路要点(实现者必读)

包络采样点与 AEC 喂入点同位:ref 包络取自 `on_render` 入参(**过环形缓冲之前**的原始 system 流),obs 包络取自 `on_capture` 入参(消回声之前的原始 mic 流)。因此估计出的 `D_env` 是与预延迟 P **无关**的原始滞后——AEC 实际看到的残余滞后 = `D_env - P`,目标残余 100ms ⇒ `P = D_env - 100ms`。调整 P 不改变后续测得的 `D_env`,控制回路无反馈振荡,天然稳定。暂停期两侧 worker 都在闸前丢帧、都不进 AEC 层,两条包络同步冻结,对齐关系不破坏。

## 对 spec §二期的三处预定偏差(设计时点即确定,非实施漂移)

1. **无独立估计 worker**:spec 设想「后台 worker + catch_unwind panic 隔离」;实测估计计算 ~0.24M 次乘加、亚毫秒级,内联在 render 线程 5s 一触发,纯安全代码无 panic 面——省掉线程生命周期与隔离整层。
2. **调整条件用双门限**(conf≥2.0 且 peak≥0.30):spec 写「置信度过门限」;一期标定实锤比值单用不可靠(无关信号爆表),沿用一期双门限定值。
3. **NS 档位 Moderate**:spec 只说「实时链路同时开 NS」;实时流同时供 ASR/声纹,取温和档,离线清洗(不影响转写输入)才用 High。

---

### Task 1: 对齐核心 `audio/aec_align.rs`

**Files:**
- Create: `src-tauri/src/audio/aec_align.rs`
- Modify: `src-tauri/src/audio/mod.rs`(加 `pub mod aec_align;`)

**Interfaces:**
- Consumes: `delay_estimate::{envelope 不用,estimate_delay, DelayEstimate}`(直接对包络切片调 `estimate_delay`)
- Produces:
  - `pub struct AlignState`(内部 `Mutex<Inner>`,两个 worker 线程各持 `Arc` 调用)
  - `pub fn new(initial_predelay_ms: u32) -> Arc<AlignState>`
  - `impl AlignState`:
    - `pub fn on_render(&self, samples: &[f32]) -> Vec<f32>` — 累积 ref 包络→样本入环→吐出超过预延迟的部分(即真正喂给 AEC render 的样本);每满 5s 内联重估
    - `pub fn on_capture(&self, samples: &[f32])` — 累积 obs 包络
    - `pub fn predelay_ms(&self) -> u32` — 当前预延迟(测试/日志用)

- [ ] **Step 1: 写失败测试**

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test aec_align -- --nocapture`
Expected: 编译错误(模块不存在)

- [ ] **Step 3: 实现**

```rust
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
        let new_frames = accumulate_env(&mut g.ref_carry, &mut g.ref_env, samples);
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
        accumulate_env(&mut g.obs_carry, &mut g.obs_env, samples);
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test aec_align -- --nocapture`
Expected: 5 个测试 PASS(收敛测试喂 30s×2 流,纯计算,秒级)

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/audio/aec_align.rs src-tauri/src/audio/mod.rs
git commit -m "实时预对齐核心:参考环形扣压+滑窗重估+滞回,控制回路与预延迟解耦无振荡"
```

---

### Task 2: 生产实时对 `aec.rs::new_aligned_pair` + stats 观测

**Files:**
- Modify: `src-tauri/src/audio/aec.rs`

**Interfaces:**
- Consumes: `aec_align::{new as align_new, AlignState}`;既有 `AecRender/AecCapture/FRAME`;crate `Processor::get_stats()`(`stats.delay_ms: Option<u32>`)
- Produces:
  - `pub fn new_aligned_pair(sample_rate: u32, initial_predelay_ms: u32) -> anyhow::Result<(AecRender, AecCapture, Arc<aec_align::AlignState>)>` — AEC3 + AGC2(与 new_pair 同参) + NS(**Moderate**,实时流同时供 ASR/声纹,取温和档;离线清洗才用 High) + 双句柄挂同一 AlignState
  - `AecRender`/`AecCapture` 增私有字段 `align: Option<Arc<AlignState>>`(None=旧行为,`new_pair`/`new_clean_pair` 传 None,签名与行为零变化)
  - capture 侧每 10s(1000 帧)eprintln 一次 `get_stats().delay_ms`

- [ ] **Step 1: 写失败测试**

在 `aec.rs` `mod tests` 追加(复用 `noise`/`power`;`block_modulated_noise` 从 delay_estimate tests 引入):

```rust
    /// 二期端到端判别测试:600ms 回声(蓝牙量级,远超 AEC3 内置 ~250ms 估计范围),
    /// aligned pair 从预延迟 0 起步,应在滑窗重估+预对齐后把回声消下去。
    /// 这正是 new_pair 做不到的场景(一期背景:蓝牙外放软件消回声完全失效)。
    #[test]
    fn aligned_pair_cancels_600ms_echo_after_adjustment() {
        use crate::audio::delay_estimate::tests::block_modulated_noise;
        let (mut r, mut c, align) = new_aligned_pair(16_000, 0).unwrap();
        let mut seed = 77u64;
        let far = block_modulated_noise(16_000 * 45, &mut seed); // 45s
        let delay = 9600; // 600ms
        let echo_gain = 0.5f32;
        let mut near = vec![0.0f32; far.len()];
        for i in delay..far.len() {
            near[i] = far[i - delay] * echo_gain;
        }
        let tail_from = far.len() - 16_000 * 5; // 只评估最后 5s(调整+重收敛之后)
        let mut out_tail = Vec::new();
        for (i, (f, n)) in far.chunks(FRAME).zip(near.chunks(FRAME)).enumerate() {
            r.push(f); // align 为 Some 时 push 内部先过 AlignState 再分帧喂入
            let cleaned = c.process(n);
            if i * FRAME >= tail_from {
                out_tail.extend_from_slice(&cleaned);
            }
        }
        // 预对齐应已发生(600-100=500ms 附近)。
        let p = align.predelay_ms() as i64;
        assert!((p - 500).unsigned_abs() <= 60, "预延迟应≈500ms,实际 {p}ms");
        let echo_power = power(&near[tail_from..]);
        let out_power = power(&out_tail);
        assert!(
            out_power < echo_power / 4.0,
            "预对齐后 600ms 回声应至少衰减 6dB: {echo_power:.6} -> {out_power:.6}"
        );
    }
```

接口注记:对齐内嵌在既有 `push`/`process` 内(align 为 Some 才生效),**不新增公开方法**;`new_pair`/`new_clean_pair` 路径(align=None)的行为必须逐字节不变。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test aligned_pair -- --nocapture`
Expected: 编译错误(`new_aligned_pair` 不存在)

- [ ] **Step 3: 实现**

`new_aligned_pair`(紧挨 `new_pair`):

```rust
/// 二期生产实时对:AEC3 + AGC2(同 new_pair 参数) + NS(Moderate,实时流同时供
/// ASR/声纹,取温和档;离线清洗的 High 档见 new_clean_pair) + 实时预对齐。
/// initial_predelay_ms 由调用方按输出设备给(蓝牙 ≈450,其他 0);之后由
/// AlignState 滑窗实测接管。
pub fn new_aligned_pair(
    sample_rate: u32,
    initial_predelay_ms: u32,
) -> anyhow::Result<(AecRender, AecCapture, Arc<crate::audio::aec_align::AlignState>)> {
    let ap = Processor::new(sample_rate).map_err(|e| anyhow::anyhow!("AEC 初始化失败: {e}"))?;
    ap.set_config(Config {
        echo_canceller: Some(config::EchoCanceller::default()),
        noise_suppression: Some(config::NoiseSuppression {
            level: config::NoiseSuppressionLevel::Moderate,
            ..Default::default()
        }),
        gain_controller: Some(config::GainController::GainController2(config::GainController2 {
            input_volume_controller_enabled: false,
            adaptive_digital: Some(config::AdaptiveDigital {
                headroom_db: 3.0,
                max_gain_db: 60.0,
                initial_gain_db: 22.0,
                max_gain_change_db_per_second: 12.0,
                max_output_noise_level_dbfs: -44.0,
            }),
            fixed_digital: config::FixedDigital::default(),
        })),
        ..Default::default()
    });
    let ap = Arc::new(ap);
    let align = crate::audio::aec_align::new(initial_predelay_ms);
    Ok((
        AecRender { ap: ap.clone(), buf: Vec::new(), align: Some(align.clone()) },
        AecCapture { ap, buf: Vec::new(), align: Some(align.clone()), frames_since_stats: 0 },
        align,
    ))
}
```

句柄改造(既有构造点 `new_pair`/`new_clean_pair` 补 `align: None` 与 `frames_since_stats: 0`):

```rust
pub struct AecRender {
    ap: Arc<Processor>,
    buf: Vec<f32>,
    align: Option<Arc<crate::audio::aec_align::AlignState>>,
}
// push 开头:
//   let samples: Vec<f32>;
//   let samples: &[f32] = if let Some(a) = &self.align {
//       samples = a.on_render(input); &samples
//   } else { input };
//   ……原 10ms 分帧逻辑不变……

pub struct AecCapture {
    ap: Arc<Processor>,
    buf: Vec<f32>,
    align: Option<Arc<crate::audio::aec_align::AlignState>>,
    frames_since_stats: usize,
}
// process 开头:
//   if let Some(a) = &self.align { a.on_capture(samples); }
// process 每处理一个 10ms 帧后 frames_since_stats += 1;
//   达到 1000(10s) 归零并:
//   if self.align.is_some() {
//       if let Some(d) = self.ap.get_stats().delay_ms {
//           eprintln!("AEC3 内部延迟估计: {d}ms (预对齐已扣压部分不计入)");
//       }
//   }
```

若 crate 的 stats 字段/方法名与上面不符(以 `~/.cargo/registry/src/*/webrtc-audio-processing-2.1.0/src/stats.rs` 为准,已知 `pub delay_ms: Option<u32>`、`Processor::get_stats(&self) -> Stats`),按实际对齐,语义锁定:每 10s 打一行内部延迟估计。

- [ ] **Step 4: 跑测试确认通过 + 全量回归**

Run: `cd src-tauri && cargo test`
Expected: 新测试 PASS(45s×2 流过 APM,数秒);既有 aec/echo_clean/transcode 测试全绿零修改

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/audio/aec.rs
git commit -m "实时对齐对:new_aligned_pair 挂预对齐+NS温和档,600ms回声判别测试实证收敛"
```

---

### Task 3: 装配点接线 `lib.rs`

**Files:**
- Modify: `src-tauri/src/lib.rs`(约 714-733 行的 AEC 装配块)

**Interfaces:**
- Consumes: `audio::aec::new_aligned_pair`;既有 `audio::default_output_is_bluetooth()`
- Produces: 行为变化——「保持外放音量」+ 双源时生产链路走 aligned 对;`soft_aec_on` 判定与 aec_roles 形状不变

- [ ] **Step 1: 改造装配块**

现状(约 721 行起):

```rust
            match audio::aec::new_pair(16000) {
                Ok((render, capture)) => {
                    eprintln!("软件回声消除已启用(WebRTC AEC3 + AGC2 自适应增益): system 路为参考,mic 路消回声+自动增益");
```

改为:

```rust
            // 二期:实时预对齐——蓝牙外放延迟(实测可漂至 1200ms)远超 AEC3 内置
            // 估计范围,由 AlignState 滑窗实测扣压参考;初值按当前输出设备给,
            // 之后实测接管。探测失败按非蓝牙(0ms),等同现状。
            let initial_predelay_ms = if audio::default_output_is_bluetooth() { 450 } else { 0 };
            match audio::aec::new_aligned_pair(16000, initial_predelay_ms) {
                Ok((render, capture, _align)) => {
                    eprintln!(
                        "软件回声消除已启用(WebRTC AEC3 + AGC2 + NS + 实时预对齐 初值{initial_predelay_ms}ms): system 路为参考,mic 路消回声"
                    );
```

其余分支(Err 降级 eprintln、aec_roles push、soft_aec_on 判定)逐字保留。

- [ ] **Step 2: 全量回归**

Run: `cd src-tauri && cargo test`
Expected: 全绿(装配点无单测,靠编译+既有 412 项回归+Task 4 冒烟)

- [ ] **Step 3: 提交**

```bash
git add src-tauri/src/lib.rs
git commit -m "录制装配切换实时对齐对:蓝牙探测给预对齐初值,启动日志如实化"
```

---

### Task 4: 真机冒烟与收尾

**Files:** 无代码改动(验证任务;发现问题回 systematic-debugging)

- [ ] **Step 1: 全量回归**

Run: `cd src-tauri && cargo test`
Expected: 全绿

- [ ] **Step 2: 沙箱冒烟(内置扬声器,负例+观测面)**

复用一期冒烟法(沙箱 HOME 经 /tmp 短符号链接防 UDS SUN_LEN;`voice-notes record start/stop/status` CLI 控制;TTS 外放 ≥70s):
- 期望启动日志:`软件回声消除已启用(... 实时预对齐 初值0ms)`
- 期望全程**无**「实时预对齐调整」行(内置延迟 ~50ms,目标 P=0,滞回带内)
- 期望每 10s 出现 `AEC3 内部延迟估计: XXms`
- 停录后离线清洗照旧跳过(实时已消干净),转写正常

- [ ] **Step 3: 蓝牙正例(条件允许时)**

蓝牙耳机/音箱连接可用时同法跑:期望「实时预对齐调整: 0ms -> XXXms」在开录 ~25s 内出现,之后回声去重触发次数显著低于历史蓝牙场次。不可用则如实记录(合成 600ms 判别测试已覆盖该路径)。

- [ ] **Step 4: 收尾提交**

```bash
git add -u
git commit -m "二期收尾:真机冒烟记录与注释零星修正"
```
