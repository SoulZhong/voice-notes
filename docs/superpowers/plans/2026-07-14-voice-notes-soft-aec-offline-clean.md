# 软件回声消除一期：离线清洗 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 停录转码前，对「保持外放音量」场次的 mic 轨做延迟对齐 + AEC3/NS 离线重跑，让 m4a 里存的就是无回声、低噪音版本。

**Architecture:** 三个新纯模块（延迟估计器 / 离线清洗引擎 / 清洗用 APM 构造器）+ 两个既有文件的最小侵入（录制时打 soft-AEC 标记、转码 worker encode 前插清洗工序）。所有失败降级为跳过清洗、原样转码。

**Tech Stack:** Rust；webrtc-audio-processing 2.1.0（既有依赖，AEC3+NS）；16kHz 单声道 s16le WAV（既有落盘格式）。

**规格:** `docs/superpowers/specs/2026-07-14-voice-notes-soft-aec-tuning-design.md`（一期部分；二期实时预对齐另立计划）

## Global Constraints

- 零用户配置：全部自动检测（存在性 + soft-AEC 标记 + 置信度门限），不加设置项。
- 清洗是增值层：任一步失败只 eprintln + 跳过，绝不丢录音、不阻塞停录/转码。
- 音频格式恒为 16kHz 单声道 s16le（`store::audio::AUDIO_SAMPLE_RATE`），WAV 头 44 字节（`HEADER_LEN`）。
- 不动 VPIO / ducking / 实时 AEC 链路（二期范围）。
- 提交信息中文、动机导向，**不加任何 Co-Authored-By / Generated-with 尾注**。
- 每任务结束 `cargo test`（在 `src-tauri/` 下）保持全绿再提交。

## 文件结构

| 文件 | 职责 |
|---|---|
| Create `src-tauri/src/audio/delay_estimate.rs` | 纯函数延迟估计：包络 + 归一化互相关 + 分窗 |
| Create `src-tauri/src/audio/echo_clean.rs` | 离线清洗引擎：WAV 进 WAV 出，内部调估计器与 APM |
| Modify `src-tauri/src/audio/aec.rs` | 新增 `new_clean_pair`（AEC3+NS High，无 AGC） |
| Modify `src-tauri/src/audio/mod.rs` | 挂两个新模块 |
| Modify `src-tauri/src/store/audio.rs` | TrackMeta 增 `soft_aec`/`clean` 字段 + 两个 setter |
| Modify `src-tauri/src/lib.rs` | 录制启用软件 AEC 时给 mic 轨打标记 |
| Modify `src-tauri/src/store/transcode.rs` | encode 前的清洗工序 + 残留 tmp 清扫 |

---

### Task 1: 延迟估计器 `audio/delay_estimate.rs`

**Files:**
- Create: `src-tauri/src/audio/delay_estimate.rs`
- Modify: `src-tauri/src/audio/mod.rs`（加 `pub mod delay_estimate;`）

**Interfaces:**
- Produces:
  - `pub struct DelayEstimate { pub delay_ms: u32, pub confidence: f32, pub peak: f32 }` — confidence=主峰/次峰比(排除±300ms邻域,峰唯一性)；peak=主峰 NCC 绝对值(回声强度)。判别真回声需两者联合:无关信号的比值噪声大(实测可到 4.5),但绝对峰值低
  - `pub fn envelope(samples: &[f32]) -> Vec<f32>` — 10ms(160样本)一帧的 RMS 包络
  - `pub fn estimate_delay(ref_env: &[f32], obs_env: &[f32], max_delay_ms: u32) -> Option<DelayEstimate>` — 输入是**包络**（不是原始样本），None=重叠不足
  - `pub fn estimate_windows(ref_env: &[f32], obs_env: &[f32], win_ms: u32, max_delay_ms: u32) -> Vec<Option<DelayEstimate>>` — 按 obs 时间轴分窗逐个估计

- [ ] **Step 1: 写失败测试**

新文件底部：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 分块调幅噪声:每 300ms 一个 LCG 随机增益,包络非周期,互相关峰唯一。
    /// (正弦调幅是周期的,相关峰会按周期重复,测不出真延迟。)
    pub(crate) fn block_modulated_noise(len: usize, seed: &mut u64) -> Vec<f32> {
        let block = 4800; // 300ms @16k
        let mut gain = 0.5f32;
        (0..len)
            .map(|i| {
                if i % block == 0 {
                    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    gain = 0.1 + 0.9 * ((*seed >> 33) as f32 / (1u64 << 31) as f32).abs();
                }
                *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (((*seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5) * gain
            })
            .collect()
    }

    #[test]
    fn envelope_is_10ms_rms_frames() {
        let s = vec![0.5f32; 320]; // 20ms 常值
        let env = envelope(&s);
        assert_eq!(env.len(), 2);
        assert!((env[0] - 0.5).abs() < 1e-4 && (env[1] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn estimates_600ms_delay_within_20ms() {
        let mut seed = 3u64;
        let reference = block_modulated_noise(16_000 * 60, &mut seed); // 60s
        let delay = 9600; // 600ms
        let mut observed = vec![0.0f32; reference.len()];
        for i in delay..reference.len() {
            observed[i] = reference[i - delay] * 0.4;
        }
        let est = estimate_delay(&envelope(&reference), &envelope(&observed), 1200).unwrap();
        assert!((est.delay_ms as i64 - 600).unsigned_abs() <= 20, "估计 {}ms", est.delay_ms);
        assert!(est.confidence >= 2.0, "真回声峰唯一性应显著: {}", est.confidence);
        assert!(est.peak >= 0.5, "真回声主峰 NCC 应高: {}", est.peak);
    }

    #[test]
    fn unrelated_signals_yield_low_peak() {
        let mut s1 = 7u64;
        let mut s2 = 1234u64;
        let a = block_modulated_noise(16_000 * 60, &mut s1);
        let b = block_modulated_noise(16_000 * 60, &mut s2);
        let est = estimate_delay(&envelope(&a), &envelope(&b), 1200);
        // 无关信号的比值(confidence)噪声大不可断言;判别力在绝对峰值。
        if let Some(e) = est {
            assert!(e.peak < 0.25, "无关信号主峰 NCC 应低: {}", e.peak);
        }
    }

    #[test]
    fn windows_detect_drifted_delays() {
        let mut seed = 11u64;
        let reference = block_modulated_noise(16_000 * 120, &mut seed); // 120s
        let mut observed = vec![0.0f32; reference.len()];
        let half = reference.len() / 2;
        for i in 9600..half {
            observed[i] = reference[i - 9600] * 0.4; // 前半 600ms
        }
        for i in (half + 10_560)..reference.len() {
            observed[i] = reference[i - 10_560] * 0.4; // 后半 660ms
        }
        let wins = estimate_windows(&envelope(&reference), &envelope(&observed), 30_000, 1200);
        assert_eq!(wins.len(), 4); // 120s / 30s
        let d0 = wins[0].as_ref().unwrap().delay_ms as i64;
        let d3 = wins[3].as_ref().unwrap().delay_ms as i64;
        assert!((d0 - 600).unsigned_abs() <= 20 && (d3 - 660).unsigned_abs() <= 20,
            "分窗应跟上漂移: {d0} / {d3}");
    }

    #[test]
    fn too_short_input_returns_none() {
        assert!(estimate_delay(&[0.1; 10], &[0.1; 10], 1200).is_none());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test delay_estimate -- --nocapture`
Expected: 编译错误（模块/函数不存在）

- [ ] **Step 3: 最小实现**

```rust
//! 延迟估计(两期共用核心):10ms 能量包络 + 归一化互相关。
//! 纯函数无状态;置信度 = 主峰/次峰比(排除主峰 ±300ms 邻域)。
//! 门限不在本模块定——离线清洗(echo_clean)与二期实时侧各自持有并标定。

/// 10ms @16k 一帧。
const ENV_FRAME: usize = 160;
/// 包络帧毫秒数。
const ENV_FRAME_MS: u32 = 10;
/// 最少需要的重叠包络帧数(3s):再短互相关峰不可信。
const MIN_OVERLAP_FRAMES: usize = 300;
/// 次峰排除主峰邻域的半宽(帧,±300ms)。语音/音乐包络自相关宽度约数百 ms:
/// 邻域太窄会把主峰肩膀当"次峰",真回声的置信度被压到 1 附近(实施中踩过)。
const PEAK_EXCLUSION_FRAMES: i64 = 30;

#[derive(Debug, Clone, Copy)]
pub struct DelayEstimate {
    pub delay_ms: u32,
    /// 主峰/次峰比(次峰排除主峰±300ms邻域):峰的唯一性。无关信号下噪声大
    /// (少量有效样本的最大值比,实测可到 4+),不可单独作真回声判据。
    pub confidence: f32,
    /// 主峰 NCC 绝对值:回声强度。真回声(参考的衰减拷贝)接近 1,无关信号
    /// 通常 <0.2——与 confidence 联合门限才可靠。
    pub peak: f32,
}

/// 10ms RMS 能量包络。尾部不足一帧的样本并入最后一帧。
pub fn envelope(samples: &[f32]) -> Vec<f32> {
    samples
        .chunks(ENV_FRAME)
        .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
        .collect()
}

/// 在 0..=max_delay_ms 搜索 obs 相对 ref 的延迟。输入为包络(envelope 的输出)。
/// 相关按去均值归一化(NCC);置信度=主峰/次峰(次峰排除主峰±300ms邻域)。
pub fn estimate_delay(ref_env: &[f32], obs_env: &[f32], max_delay_ms: u32) -> Option<DelayEstimate> {
    let max_lag = (max_delay_ms / ENV_FRAME_MS) as usize;
    let n = ref_env.len().min(obs_env.len());
    if n <= max_lag || n - max_lag < MIN_OVERLAP_FRAMES {
        return None;
    }
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
    let rm = mean(&ref_env[..n]);
    let om = mean(&obs_env[..n]);
    let mut best = (0usize, f32::MIN);
    let mut scores = Vec::with_capacity(max_lag + 1);
    for lag in 0..=max_lag {
        // obs[t] 对齐 ref[t-lag],重叠区间 t ∈ [lag, n)
        let mut dot = 0.0f64;
        let mut nr = 0.0f64;
        let mut no = 0.0f64;
        for t in lag..n {
            let r = (ref_env[t - lag] - rm) as f64;
            let o = (obs_env[t] - om) as f64;
            dot += r * o;
            nr += r * r;
            no += o * o;
        }
        let score = if nr > 0.0 && no > 0.0 { (dot / (nr.sqrt() * no.sqrt())) as f32 } else { 0.0 };
        scores.push(score);
        if score > best.1 {
            best = (lag, score);
        }
    }
    if best.1 <= 0.0 {
        return None;
    }
    // 次峰:排除主峰 ±PEAK_EXCLUSION_FRAMES 邻域后的最大值。
    let second = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| (*i as i64 - best.0 as i64).abs() > PEAK_EXCLUSION_FRAMES)
        .map(|(_, s)| *s)
        .fold(f32::MIN, f32::max);
    let confidence = if second > 1e-6 { best.1 / second } else { best.1 / 1e-6 };
    Some(DelayEstimate { delay_ms: best.0 as u32 * ENV_FRAME_MS, confidence, peak: best.1 })
}

/// 按 obs 时间轴分窗(win_ms)逐窗估计。ref 与 obs 取同一 [start..end) 窗口,
/// lag 语义即全局延迟;窗首 lag 帧因参考越窗不参与相关(60s 窗 vs ≤1.2s 延迟,
/// 损失可忽略)。不得给 ref 段前伸提前量——那会让真实延迟对应的 lag 变负,
/// 掉出 0..=max_lag 搜索域(实施中踩过:窗估计撞 1200ms 边界)。
pub fn estimate_windows(
    ref_env: &[f32],
    obs_env: &[f32],
    win_ms: u32,
    max_delay_ms: u32,
) -> Vec<Option<DelayEstimate>> {
    let win = (win_ms / ENV_FRAME_MS) as usize;
    let n = obs_env.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < n {
        let end = (start + win).min(n);
        let end_r = end.min(ref_env.len());
        if start >= end_r {
            out.push(None);
        } else {
            out.push(estimate_delay(&ref_env[start..end_r], &obs_env[start..end], max_delay_ms));
        }
        start = end;
    }
    out
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test delay_estimate -- --nocapture`
Expected: 5 个测试 PASS（`estimates_600ms` 与 `windows_detect_drifted` 是关键）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/audio/delay_estimate.rs src-tauri/src/audio/mod.rs
git commit -m "延迟估计器:10ms包络+归一化互相关,分窗抗漂移,两期共用核心"
```

---

### Task 2: 清洗用 APM 构造器 `aec.rs::new_clean_pair`

**Files:**
- Modify: `src-tauri/src/audio/aec.rs`

**Interfaces:**
- Consumes: 既有 `AecRender` / `AecCapture` / `FRAME`
- Produces: `pub fn new_clean_pair(sample_rate: u32) -> anyhow::Result<(AecRender, AecCapture)>` — AEC3 + NS(High)，**无 AGC**（离线清洗不动电平，录制时已增益过）

- [ ] **Step 1: 写失败测试**

在 `aec.rs` 的 `mod tests` 里追加：

```rust
    /// 清洗对(AEC3+NS,无AGC):回声照样消,且不做增益(输出功率不该高于输入)。
    #[test]
    fn clean_pair_cancels_echo_without_gain() {
        let (mut r, mut c) = new_clean_pair(16_000).unwrap();
        let mut seed = 42u64;
        let far = noise(16_000 * 4, &mut seed);
        let delay = 960;
        let mut near = vec![0.0f32; far.len()];
        for i in delay..far.len() {
            near[i] = far[i - delay] * 0.5;
        }
        let tail_from = far.len() - 16_000 / 2;
        let mut out_tail = Vec::new();
        for (i, (f, n)) in far.chunks(FRAME).zip(near.chunks(FRAME)).enumerate() {
            r.push(f);
            let cleaned = c.process(n);
            if i * FRAME >= tail_from {
                out_tail.extend_from_slice(&cleaned);
            }
        }
        let echo_power = power(&near[tail_from..]);
        let out_power = power(&out_tail);
        assert!(out_power < echo_power / 4.0, "回声至少衰减 6dB: {echo_power:.6} -> {out_power:.6}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test clean_pair -- --nocapture`
Expected: 编译错误 `new_clean_pair` 不存在

- [ ] **Step 3: 实现**

在 `new_pair` 之后追加：

```rust
/// 离线清洗用的一对句柄:AEC3 + 降噪(NS High),不开 AGC。
/// 与实时录制的 new_pair 区别:清洗输入是录制时已经 AGC 过的波形,再增益会把
/// 底噪二次抬升;降噪在这里开(实时链路二期再评估),清掉普通麦克风路径的底噪。
pub fn new_clean_pair(sample_rate: u32) -> anyhow::Result<(AecRender, AecCapture)> {
    let ap = Processor::new(sample_rate).map_err(|e| anyhow::anyhow!("清洗 APM 初始化失败: {e}"))?;
    ap.set_config(Config {
        echo_canceller: Some(config::EchoCanceller::default()),
        noise_suppression: Some(config::NoiseSuppression {
            level: config::NoiseSuppressionLevel::High,
            ..Default::default()
        }),
        ..Default::default()
    });
    let ap = Arc::new(ap);
    Ok((
        AecRender { ap: ap.clone(), buf: Vec::new() },
        AecCapture { ap, buf: Vec::new() },
    ))
}
```

若 `config::NoiseSuppression` 字段与上面不符（以 crate 实际定义为准，`~/.cargo/registry/src/*/webrtc-audio-processing-2.1.0/src/config.rs` 可查），按实际字段名修正——语义锁定：NS 开、级别 High、AEC3 默认、无 gain_controller。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test clean_pair -- --nocapture`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/audio/aec.rs
git commit -m "清洗用 APM 构造器:AEC3+NS(High)无AGC,离线重跑不二次抬底噪"
```

---

### Task 3: 离线清洗引擎 `audio/echo_clean.rs`

**Files:**
- Create: `src-tauri/src/audio/echo_clean.rs`
- Modify: `src-tauri/src/audio/mod.rs`（加 `pub mod echo_clean;`）

**Interfaces:**
- Consumes: `delay_estimate::{envelope, estimate_windows, DelayEstimate}`；`aec::new_clean_pair`；`store::audio` 的 WAV 头常量（HEADER_LEN=44，16k 单声道 s16le）
- Produces:
  - `pub struct CleanReport { pub delay_ms: u32, pub confidence: f32, pub segments: u32 }`
  - `pub fn clean_wav(mic_wav: &Path, system_wav: &Path, mic_offset_ms: u64, system_offset_ms: u64, out_tmp: &Path) -> anyhow::Result<Option<CleanReport>>` — `Ok(None)`=置信度不足跳过（不写 out_tmp）；`Ok(Some)`=out_tmp 写好合法 WAV
  - `pub const CONFIDENCE_GATE: f32 = 2.0;` 与 `pub const PEAK_GATE: f32 = 0.25;` — **临时值**（双门限:比值+绝对峰值），Task 6 用真实录音标定后更新

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::delay_estimate::tests::block_modulated_noise;
    use std::io::Write;

    fn write_wav(path: &std::path::Path, samples: &[f32]) {
        let pcm: Vec<u8> = samples
            .iter()
            .flat_map(|s| crate::store::audio::f32_to_s16(*s).to_le_bytes())
            .collect();
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&crate::store::audio::wav_header(pcm.len() as u32)).unwrap();
        f.write_all(&pcm).unwrap();
    }

    fn read_wav_f32(path: &std::path::Path) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap();
        bytes[44..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect()
    }

    fn power(s: &[f32]) -> f32 {
        s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32
    }

    /// 端到端:mic = 前 30s 纯回声(system 延迟 600ms×0.5) + 后 30s 纯本地人声。
    /// 清洗后:回声区能量降 ≥15dB,本地人声区能量保持在 ±6dB 内。
    #[test]
    fn cleans_600ms_bluetooth_echo_and_keeps_local_voice() {
        let dir = tempfile::tempdir().unwrap();
        let mut s1 = 21u64;
        let mut s2 = 99u64;
        let system = block_modulated_noise(16_000 * 60, &mut s1);
        let local = block_modulated_noise(16_000 * 30, &mut s2);
        let delay = 9600;
        let half = 16_000 * 30;
        let mut mic = vec![0.0f32; 16_000 * 60];
        for i in delay..half {
            mic[i] = system[i - delay] * 0.5; // 前半:纯回声
        }
        for i in half..mic.len() {
            mic[i] = local[i - half] * 0.3; // 后半:纯本地声(system 后半也在响,但不进 mic)
        }
        let mic_wav = dir.path().join("mic.wav");
        let sys_wav = dir.path().join("system.wav");
        let out = dir.path().join("mic.clean.tmp");
        write_wav(&mic_wav, &mic);
        write_wav(&sys_wav, &system);

        let report = clean_wav(&mic_wav, &sys_wav, 0, 0, &out).unwrap().expect("应过置信度门限");
        assert!((report.delay_ms as i64 - 600).unsigned_abs() <= 20, "报告延迟 {}ms", report.delay_ms);

        let cleaned = read_wav_f32(&out);
        assert_eq!(cleaned.len(), mic.len(), "样本数守恒");
        // 评估回声区避开头 10s(收敛+暖机边界),取 10s..30s。
        let echo_in = power(&mic[16_000 * 10..half]);
        let echo_out = power(&cleaned[16_000 * 10..half]);
        assert!(echo_out < echo_in / 31.6, "回声应降 ≥15dB: {echo_in:.6} -> {echo_out:.6}");
        // 本地声区(取 35s..55s 避段界):NS 会削一些底噪,允许 ±6dB。
        let loc_in = power(&mic[16_000 * 35..16_000 * 55]);
        let loc_out = power(&cleaned[16_000 * 35..16_000 * 55]);
        assert!(loc_out > loc_in / 4.0 && loc_out < loc_in * 4.0,
            "本地声应保持 ±6dB: {loc_in:.6} -> {loc_out:.6}");
    }

    /// mic 与 system 无关(没回声):置信度不足,返回 None,不写输出文件。
    #[test]
    fn unrelated_tracks_skip_cleaning() {
        let dir = tempfile::tempdir().unwrap();
        let mut s1 = 5u64;
        let mut s2 = 777u64;
        write_wav(&dir.path().join("mic.wav"), &block_modulated_noise(16_000 * 60, &mut s1));
        write_wav(&dir.path().join("system.wav"), &block_modulated_noise(16_000 * 60, &mut s2));
        let out = dir.path().join("mic.clean.tmp");
        let r = clean_wav(&dir.path().join("mic.wav"), &dir.path().join("system.wav"), 0, 0, &out).unwrap();
        assert!(r.is_none(), "无关轨道应跳过");
        assert!(!out.exists(), "跳过时不得写输出");
    }

    /// 轨道太短(<3s):直接跳过,不 panic。
    #[test]
    fn tiny_tracks_skip_cleaning() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = 1u64;
        write_wav(&dir.path().join("mic.wav"), &block_modulated_noise(16_000, &mut s));
        write_wav(&dir.path().join("system.wav"), &block_modulated_noise(16_000, &mut s));
        let out = dir.path().join("mic.clean.tmp");
        let r = clean_wav(&dir.path().join("mic.wav"), &dir.path().join("system.wav"), 0, 0, &out).unwrap();
        assert!(r.is_none());
    }
}
```

前置：`delay_estimate::tests::block_modulated_noise` 需改成 `pub(crate)`（Task 1 已按此写）；`store::audio::wav_header` 当前是 `pub(crate)`，`f32_to_s16` 是 pub——都在 crate 内可用。`tempfile` 已在 dev-dependencies（transcode 测试在用；若没有，`cargo add tempfile --dev`）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test echo_clean -- --nocapture`
Expected: 编译错误（模块不存在）

- [ ] **Step 3: 实现**

```rust
//! 离线回声清洗:停录后、转码前,把 system 参考按实测延迟对齐,用 AEC3+NS
//! 重跑 mic 轨。设计见 specs/2026-07-14-voice-notes-soft-aec-tuning-design.md。
//!
//! 全自动零配置:分窗延迟估计,置信度不过门限就不动任何字节(内置扬声器等
//! AEC3 实时已收敛的场景天然被拒)。任何失败调用方降级为原样转码。

use crate::audio::aec;
use crate::audio::delay_estimate::{self, DelayEstimate};
use std::io::Write;
use std::path::Path;

/// 双门限(Task 6 用真实录音标定后更新,标定依据写在这条注释里):
/// confidence(主峰/次峰比)保证峰唯一,peak(主峰 NCC 绝对值)保证回声真实存在——
/// 无关信号的比值噪声大(实测 4+),单靠比值会误清洗。
pub const CONFIDENCE_GATE: f32 = 2.0;
pub const PEAK_GATE: f32 = 0.25;

/// 分窗宽度与延迟搜索上限。
const WIN_MS: u32 = 60_000;
const MAX_DELAY_MS: u32 = 1200;
/// 相邻窗延迟差不超过此值视为同段(AEC3 自身窗口能吸收的残差)。
const MERGE_MS: u32 = 40;
/// 暖机长度:段首多喂 10s 对齐好的双流,输出丢弃,消掉 AEC3 收敛期。
const WARMUP_SAMPLES: usize = 16_000 * 10;

#[derive(Debug, Clone, Copy)]
pub struct CleanReport {
    pub delay_ms: u32,
    pub confidence: f32,
    pub segments: u32,
}

/// 读 WAV(跳 44 头)为 f32 样本。
fn read_wav_f32(path: &Path) -> anyhow::Result<Vec<f32>> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 44 {
        anyhow::bail!("WAV 过短: {path:?}");
    }
    Ok(bytes[44..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect())
}

/// 清洗主入口。Ok(None)=置信度不足或轨道过短,未写 out_tmp;
/// Ok(Some)=out_tmp 已写好完整合法 WAV,调用方负责 rename。
pub fn clean_wav(
    mic_wav: &Path,
    system_wav: &Path,
    mic_offset_ms: u64,
    system_offset_ms: u64,
    out_tmp: &Path,
) -> anyhow::Result<Option<CleanReport>> {
    let mic = read_wav_f32(mic_wav)?;
    let system = read_wav_f32(system_wav)?;

    // 轨道起点对齐到共同时间轴:把晚出现的轨道前面补零,之后统一用 mic 下标。
    // (通常两轨 offset 都是 0;续录/单源迟到场景才有差。)
    let to_samples = |ms: u64| (ms as usize) * 16;
    let sys_shift = to_samples(mic_offset_ms) as i64 - to_samples(system_offset_ms) as i64;
    // system_aligned[t] = system[t + sys_shift](越界取 0):t 为 mic 时间轴下标。
    let sys_at = |t: i64| -> f32 {
        let idx = t + sys_shift;
        if idx < 0 || idx as usize >= system.len() { 0.0 } else { system[idx as usize] }
    };
    let system_aligned: Vec<f32> = (0..mic.len() as i64).map(sys_at).collect();

    // 分窗延迟估计。
    let ref_env = delay_estimate::envelope(&system_aligned);
    let obs_env = delay_estimate::envelope(&mic);
    let wins = delay_estimate::estimate_windows(&ref_env, &obs_env, WIN_MS, MAX_DELAY_MS);
    let confident: Vec<(usize, DelayEstimate)> = wins
        .iter()
        .enumerate()
        .filter_map(|(i, w)| {
            w.as_ref()
                .filter(|e| e.confidence >= CONFIDENCE_GATE && e.peak >= PEAK_GATE)
                .map(|e| (i, *e))
        })
        .collect();
    if confident.is_empty() {
        return Ok(None);
    }

    // 相邻置信窗延迟差 ≤MERGE_MS 归并为段;每段延迟取窗中位数。
    // 无置信度的窗并入前一段(没检测到回声的窗,照常处理无害)。
    let mut segments: Vec<(usize, u32)> = Vec::new(); // (起始窗序号, delay_ms)
    for (i, e) in &confident {
        match segments.last() {
            Some((_, d)) if (e.delay_ms as i64 - *d as i64).unsigned_abs() <= MERGE_MS as u64 => {}
            _ => segments.push((*i, e.delay_ms)),
        }
    }
    // 首段起点回拉到 0(段前的低置信窗同样用首段延迟处理)。
    segments[0].0 = 0;

    let win_samples = (WIN_MS / 1000) as usize * 16_000;
    let mut cleaned: Vec<f32> = Vec::with_capacity(mic.len());
    let seg_count = segments.len() as u32;
    for (si, (start_win, delay_ms)) in segments.iter().enumerate() {
        let seg_start = start_win * win_samples;
        let seg_end = segments.get(si + 1).map(|(w, _)| w * win_samples).unwrap_or(mic.len());
        let delay = (*delay_ms as usize) * 16;
        // 段内参考:ref_seg[t] = system_aligned[t - delay](越界补零)。
        let ref_of = |t: usize| -> f32 {
            if t < delay { 0.0 } else { system_aligned.get(t - delay).copied().unwrap_or(0.0) }
        };
        // 每段新建 APM:延迟跳变后旧滤波器状态有害无益。
        let (mut render, mut capture) = aec::new_clean_pair(16_000)
            .map_err(|e| anyhow::anyhow!("清洗 APM 构建失败: {e}"))?;
        // 暖机:段首 10s(或不足则全段)喂一遍,输出丢弃。
        let warm_end = (seg_start + WARMUP_SAMPLES).min(seg_end);
        for t0 in (seg_start..warm_end).step_by(160) {
            let t1 = (t0 + 160).min(warm_end);
            let rframe: Vec<f32> = (t0..t1).map(ref_of).collect();
            render.push(&rframe);
            let _ = capture.process(&mic[t0..t1]);
        }
        // 正式:整段重喂,取输出。
        for t0 in (seg_start..seg_end).step_by(160) {
            let t1 = (t0 + 160).min(seg_end);
            let rframe: Vec<f32> = (t0..t1).map(ref_of).collect();
            render.push(&rframe);
            cleaned.extend_from_slice(&capture.process(&mic[t0..t1]));
        }
        // capture 内部滞留的 <10ms 余量:原样补足,样本数守恒。
        while cleaned.len() < seg_end {
            cleaned.push(mic[cleaned.len()]);
        }
    }

    // 写 out_tmp:合法 WAV + fsync。
    let pcm: Vec<u8> = cleaned
        .iter()
        .flat_map(|s| crate::store::audio::f32_to_s16(*s).to_le_bytes())
        .collect();
    let mut f = std::fs::File::create(out_tmp)?;
    f.write_all(&crate::store::audio::wav_header(pcm.len() as u32))?;
    f.write_all(&pcm)?;
    f.sync_all()?;

    let best = confident.iter().map(|(_, e)| *e).fold(confident[0].1, |a, b| {
        if b.confidence > a.confidence { b } else { a }
    });
    Ok(Some(CleanReport { delay_ms: best.delay_ms, confidence: best.confidence, segments: seg_count }))
}
```

实现备注（引擎内在约束，不是可选项）：
- **内存**：全量读入 f32，1 小时双轨约 460MB 峰值。可接受（桌面应用、低频时刻）；若实测吃紧，后续再改流式——本期不做。
- `store::audio::wav_header` 需从 `pub(crate)` 保持不变（同 crate 可用）；`HEADER_LEN` 同理。
- 段界落在 60s 窗边界即可（spec 提到静音择点是优化项；AEC3 换段重建 + 暖机已把爆音风险压到段首 10s 的暖机区内，且段界处延迟本来就在漂移，择点收益小——如实现后试听发现段界可闻爆点，再加静音择点）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test echo_clean -- --nocapture`
Expected: 3 个测试 PASS。`cleans_600ms` 跑 60s 音频 ×2 遍 APM，预计数秒。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/audio/echo_clean.rs src-tauri/src/audio/mod.rs src-tauri/src/audio/delay_estimate.rs
git commit -m "离线清洗引擎:分窗延迟对齐+AEC3/NS重跑,置信度门限拒绝无回声场次"
```

---

### Task 4: soft-AEC 标记与清洗报告落 meta

**Files:**
- Modify: `src-tauri/src/store/audio.rs`（TrackMeta 两个新字段 + 两个 setter）
- Modify: `src-tauri/src/lib.rs`（录制启用软件 AEC 时打标记）

**Interfaces:**
- Produces:
  - `TrackMeta.soft_aec: Option<bool>`、`TrackMeta.clean: Option<CleanInfo>`
  - `pub struct CleanInfo { pub delay_ms: u32, pub confidence: f32, pub segments: u32 }`（serde 序列化，存 audio.json）
  - `pub fn set_track_soft_aec(note_dir: &Path, source: &str) -> anyhow::Result<()>`
  - `pub fn set_track_clean_info(note_dir: &Path, source: &str, info: CleanInfo) -> anyhow::Result<()>`
- Consumes: 既有 `meta_guard()` / `load_audio_meta` / meta 保存路径（仿照 `set_track_compressed` 的实现形状——先 `meta_guard()`，load→改→save）

- [ ] **Step 1: 写失败测试**

在 `store/audio.rs` 测试模块追加：

```rust
    #[test]
    fn soft_aec_flag_and_clean_info_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        set_track_soft_aec(dir.path(), "mic").unwrap();
        set_track_soft_aec(dir.path(), "mic").unwrap(); // 幂等
        let meta = load_audio_meta(dir.path());
        assert_eq!(meta.tracks["mic"].soft_aec, Some(true));
        assert!(meta.tracks["mic"].clean.is_none());

        set_track_clean_info(dir.path(), "mic",
            CleanInfo { delay_ms: 600, confidence: 3.2, segments: 1 }).unwrap();
        let meta = load_audio_meta(dir.path());
        let c = meta.tracks["mic"].clean.as_ref().unwrap();
        assert_eq!((c.delay_ms, c.segments), (600, 1));
    }

    /// 旧 audio.json(无新字段)必须照常反序列化——新字段全 default。
    #[test]
    fn old_audio_json_without_new_fields_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("audio.json"),
            r#"{"schema_version":1,"tracks":{"mic":{"offset_ms":0}}}"#).unwrap();
        let meta = load_audio_meta(dir.path());
        assert_eq!(meta.tracks["mic"].soft_aec, None);
        assert!(meta.tracks["mic"].clean.is_none());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test store::audio -- --nocapture`
Expected: 编译错误（字段/函数不存在）

- [ ] **Step 3: 实现**

`TrackMeta` 追加字段（形状与既有 codec/duration_ms 完全一致）：

```rust
    /// 本轨录制时走了软件 AEC 路径(「保持外放音量」):转码前的离线回声清洗
    /// 只对这类轨道启动。录制启用时写 true,从不清除(续录混合场景由清洗端的
    /// 置信度门限兜底)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_aec: Option<bool>,
    /// 离线清洗结果(排障用):估计延迟/置信度/分段数。None=未清洗过。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean: Option<CleanInfo>,
```

`CleanInfo` 与两个 setter（紧挨 `set_track_compressed`，同一模板）：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanInfo {
    pub delay_ms: u32,
    pub confidence: f32,
    pub segments: u32,
}

/// 录制装配软件 AEC 成功后调用:给 <source> 轨打 soft_aec 标记。幂等。
/// 持 META_LOCK,与 set_track_compressed 同模板。
pub fn set_track_soft_aec(note_dir: &Path, source: &str) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    meta.tracks.entry(source.to_string()).or_default().soft_aec = Some(true);
    save_audio_meta(note_dir, &meta)
}

/// 离线清洗完成后调用:记录清洗报告。持 META_LOCK。
pub fn set_track_clean_info(note_dir: &Path, source: &str, info: CleanInfo) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    meta.tracks.entry(source.to_string()).or_default().clean = Some(info);
    save_audio_meta(note_dir, &meta)
}
```

注意 `load_audio_meta` 不可失败（缺失/损坏回落默认空表，返回 `AudioMeta` 而非 Result）——上面代码没有 `?`。

`lib.rs` 打标记——位置：`spawn_session` 内 `let note_dir = writer.dir().to_path_buf();`（约 768 行）所在的元信息读取块**之后**；条件变量在 AEC 装配处（约 714 行）捕获：

```rust
// AEC 装配处(aec_roles push 之后)已有作用域内加:
let soft_aec_on = aec_roles.iter().any(|(_, r)| matches!(r, audio::aec::AecRole::Capture(_)));
```

```rust
// note_dir 可用后:
if soft_aec_on {
    if let Err(e) = store::audio::set_track_soft_aec(&note_dir, "mic") {
        eprintln!("软件AEC标记写入失败(不影响录制,本场将跳过离线清洗): {e}");
    }
}
```

- [ ] **Step 4: 跑测试确认通过 + 全量回归**

Run: `cd src-tauri && cargo test`
Expected: 新增 2 个 PASS，既有全绿

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/store/audio.rs src-tauri/src/lib.rs
git commit -m "soft-AEC 轨道标记+清洗报告入 audio.json:离线清洗只认软件AEC场次"
```

---

### Task 5: 转码 worker 集成清洗工序

**Files:**
- Modify: `src-tauri/src/store/transcode.rs`

**Interfaces:**
- Consumes: `echo_clean::{clean_wav, CleanReport, CONFIDENCE_GATE}`；`store::audio::{load_audio_meta, set_track_clean_info, CleanInfo, repair_wav_header}`
- Produces: `transcode_note_dir` 行为扩展——encode 循环前先清洗 mic；无新公开接口

- [ ] **Step 1: 写失败测试**

在 `transcode.rs` 测试模块追加（fixture 手法仿照本文件既有测试——`wav_header` + PCM 直写）：

```rust
    /// 清洗触发矩阵。合成信号同 echo_clean 测试:system 60s,mic 前半纯回声。
    fn clean_fixture(dir: &std::path::Path, with_system: bool, with_flag: bool, echo: bool) {
        use crate::audio::delay_estimate::tests::block_modulated_noise;
        let mut s1 = 21u64;
        let system = block_modulated_noise(16_000 * 60, &mut s1);
        let mut mic = vec![0.0f32; 16_000 * 60];
        if echo {
            for i in 9600..mic.len() {
                mic[i] = system[i - 9600] * 0.5;
            }
        } else {
            let mut s2 = 888u64;
            mic = block_modulated_noise(16_000 * 60, &mut s2); // 与 system 无关
        }
        let write = |name: &str, samples: &[f32]| {
            let pcm: Vec<u8> = samples.iter()
                .flat_map(|s| crate::store::audio::f32_to_s16(*s).to_le_bytes()).collect();
            let mut buf = Vec::new();
            buf.extend_from_slice(&wav_header(pcm.len() as u32));
            buf.extend_from_slice(&pcm);
            std::fs::write(dir.join(name), buf).unwrap();
        };
        write("mic.wav", &mic);
        if with_system {
            write("system.wav", &system);
        }
        if with_flag {
            crate::store::audio::set_track_soft_aec(dir, "mic").unwrap();
        }
    }

    #[test]
    fn clean_runs_only_with_flag_system_and_confidence() {
        // 情形1:齐备+有回声 → mic.wav 被替换,meta 记报告
        let d1 = tempfile::tempdir().unwrap();
        clean_fixture(d1.path(), true, true, true);
        let before = std::fs::read(d1.path().join("mic.wav")).unwrap();
        clean_mic_before_encode(d1.path());
        let after = std::fs::read(d1.path().join("mic.wav")).unwrap();
        assert_ne!(before, after, "有回声应被清洗");
        let meta = crate::store::audio::load_audio_meta(d1.path());
        assert!(meta.tracks["mic"].clean.is_some(), "清洗报告应落 meta");
        assert!(!d1.path().join("mic.wav.clean.tmp").exists(), "tmp 应被 rename 走");

        // 情形2:无 soft_aec 标记 → 字节不动
        let d2 = tempfile::tempdir().unwrap();
        clean_fixture(d2.path(), true, false, true);
        let before = std::fs::read(d2.path().join("mic.wav")).unwrap();
        clean_mic_before_encode(d2.path());
        assert_eq!(before, std::fs::read(d2.path().join("mic.wav")).unwrap());

        // 情形3:无 system 轨 → 字节不动
        let d3 = tempfile::tempdir().unwrap();
        clean_fixture(d3.path(), false, true, true);
        let before = std::fs::read(d3.path().join("mic.wav")).unwrap();
        clean_mic_before_encode(d3.path());
        assert_eq!(before, std::fs::read(d3.path().join("mic.wav")).unwrap());

        // 情形4:齐备但无回声(置信度不过门限) → 字节不动,meta 无报告
        let d4 = tempfile::tempdir().unwrap();
        clean_fixture(d4.path(), true, true, false);
        let before = std::fs::read(d4.path().join("mic.wav")).unwrap();
        clean_mic_before_encode(d4.path());
        assert_eq!(before, std::fs::read(d4.path().join("mic.wav")).unwrap());
        let meta = crate::store::audio::load_audio_meta(d4.path());
        assert!(meta.tracks["mic"].clean.is_none());
    }

    /// 崩溃残留的 .clean.tmp 在下次转码时被清扫。
    #[test]
    fn stale_clean_tmp_swept_on_next_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mic.wav.clean.tmp"), b"garbage").unwrap();
        transcode_note_dir(dir.path());
        assert!(!dir.path().join("mic.wav.clean.tmp").exists());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test transcode -- --nocapture`
Expected: 编译错误 `clean_mic_before_encode` 不存在

- [ ] **Step 3: 实现**

`transcode_note_dir` 开头（`.m4a.tmp` 清扫旁）追加清扫与调用：

```rust
pub fn transcode_note_dir(note_dir: &Path) {
    // 清残留:上次清洗写到一半的 tmp(与 .m4a.tmp 同理)。
    let _ = std::fs::remove_file(note_dir.join("mic.wav.clean.tmp"));
    for source in sources_with_suffix(note_dir, ".m4a.tmp") {
        let _ = std::fs::remove_file(note_dir.join(format!("{source}.m4a.tmp")));
    }
    // 离线回声清洗(增值层,内部消化一切失败):必须在 encode 循环前——
    // 清洗改写 mic.wav,encode 后 WAV 已删。
    clean_mic_before_encode(note_dir);
    for source in sources_with_suffix(note_dir, ".wav") {
        // ……既有循环不动……
```

新函数（放 `transcode_one` 旁）：

```rust
/// encode 前的离线回声清洗:仅当 mic.wav+system.wav 都在、mic 轨带 soft_aec
/// 标记、且延迟估计过置信度门限。任何失败/跳过只 eprintln,转码照旧。
fn clean_mic_before_encode(note_dir: &Path) {
    let mic = note_dir.join("mic.wav");
    let system = note_dir.join("system.wav");
    if !mic.exists() || !system.exists() {
        return;
    }
    // load_audio_meta 不可失败:损坏/缺失回落空表 → 无 soft_aec 标记 → 下一行返回。
    let meta = crate::store::audio::load_audio_meta(note_dir);
    if meta.tracks.get("mic").and_then(|t| t.soft_aec) != Some(true) {
        return; // VPIO 场次/旧笔记:无标记不清洗
    }
    let mic_off = meta.tracks.get("mic").map(|t| t.offset_ms).unwrap_or(0);
    let sys_off = meta.tracks.get("system").map(|t| t.offset_ms).unwrap_or(0);
    // 头部按实际长度修正,防止陈旧头让清洗读到截断数据(同 transcode_one 首步)。
    for p in [&mic, &system] {
        if let Err(e) = repair_wav_header(p) {
            eprintln!("清洗跳过(修 WAV 头失败 {}): {e}", p.display());
            return;
        }
    }
    let out_tmp = note_dir.join("mic.wav.clean.tmp");
    match crate::audio::echo_clean::clean_wav(&mic, &system, mic_off, sys_off, &out_tmp) {
        Ok(Some(report)) => {
            if let Err(e) = std::fs::rename(&out_tmp, &mic) {
                let _ = std::fs::remove_file(&out_tmp);
                eprintln!("清洗产物替换失败,保留原 mic.wav: {e}");
                return;
            }
            let info = crate::store::audio::CleanInfo {
                delay_ms: report.delay_ms,
                confidence: report.confidence,
                segments: report.segments,
            };
            if let Err(e) = crate::store::audio::set_track_clean_info(note_dir, "mic", info) {
                eprintln!("清洗报告写 meta 失败(音频已清洗): {e}");
            }
            eprintln!(
                "离线回声清洗完成: 延迟 {}ms 置信度 {:.2} 分段 {}",
                report.delay_ms, report.confidence, report.segments
            );
        }
        Ok(None) => {
            eprintln!("离线回声清洗跳过: 未检出显著回声(门限 conf≥{} peak≥{})",
                crate::audio::echo_clean::CONFIDENCE_GATE,
                crate::audio::echo_clean::PEAK_GATE);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&out_tmp);
            eprintln!("离线回声清洗失败,原样转码: {e}");
        }
    }
}
```

- [ ] **Step 4: 跑测试确认通过 + 全量回归**

Run: `cd src-tauri && cargo test`
Expected: 新增 2 个 PASS，既有全绿（尤其 transcode 既有测试——清洗对无标记 fixture 是 no-op，不该扰动）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/store/transcode.rs
git commit -m "转码前离线回声清洗:标记+置信度双闸,失败一律降级原样转码"
```

---

### Task 6: 真实录音标定置信度门限

**Files:**
- Modify: `src-tauri/src/audio/echo_clean.rs`（`CONFIDENCE_GATE` 定值 + 注释写标定依据；`#[ignore]` 标定测试）

**Interfaces:**
- Consumes: 用户数据目录 `~/Documents/voice-notes/notes/<id>/{mic,system}.m4a`（既有录音,只读）
- Produces: 标定后的 `CONFIDENCE_GATE` 终值

- [ ] **Step 1: 写标定工具（`#[ignore]` 测试，不进 CI）**

```rust
#[cfg(test)]
mod calibrate {
    use super::*;

    /// 手动标定:VN_NOTE_DIR 指向一个笔记目录(含 mic/system 的 m4a 或 wav),
    /// 打印分窗延迟与置信度、若过门限则输出清洗文件供试听。
    /// 用法(蓝牙场次与内置扬声器场次各跑一遍):
    ///   VN_NOTE_DIR=~/Documents/voice-notes/notes/20260708-XXXXXX \
    ///   cargo test calibrate_note -- --ignored --nocapture
    #[test]
    #[ignore]
    fn calibrate_note() {
        let dir = std::path::PathBuf::from(
            std::env::var("VN_NOTE_DIR").expect("需设 VN_NOTE_DIR"));
        let tmp = tempfile::tempdir().unwrap();
        // m4a → 16k 单声道 WAV(afconvert,与转码模块同参数)
        let prep = |src: &str| -> std::path::PathBuf {
            let wav = dir.join(format!("{src}.wav"));
            if wav.exists() {
                return wav;
            }
            let out = tmp.path().join(format!("{src}.wav"));
            let st = std::process::Command::new("/usr/bin/afconvert")
                .args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"])
                .arg(dir.join(format!("{src}.m4a")))
                .arg(&out)
                .status()
                .unwrap();
            assert!(st.success(), "afconvert 解码失败: {src}");
            out
        };
        let mic = prep("mic");
        let sys = prep("system");
        let mic_s = read_wav_f32(&mic).unwrap();
        let sys_s = read_wav_f32(&sys).unwrap();
        let wins = crate::audio::delay_estimate::estimate_windows(
            &crate::audio::delay_estimate::envelope(&sys_s),
            &crate::audio::delay_estimate::envelope(&mic_s),
            60_000, 1200);
        for (i, w) in wins.iter().enumerate() {
            match w {
                Some(e) => eprintln!("窗{i}: 延迟 {}ms conf {:.2} peak {:.3}", e.delay_ms, e.confidence, e.peak),
                None => eprintln!("窗{i}: 无估计(过短)"),
            }
        }
        let out = tmp.path().join("mic.cleaned.wav");
        match clean_wav(&mic, &sys, 0, 0, &out).unwrap() {
            Some(r) => eprintln!("清洗完成: {r:?}\n试听文件: {}", out.display()),
            None => eprintln!("置信度不足,跳过清洗"),
        }
        // 试听文件在 tmp 目录,测试结束会删;要保留就 cp 出来再退出。
        std::mem::forget(tmp);
    }
}
```

- [ ] **Step 2: 蓝牙场次标定**

Run: 2026-07-08 面试那场（蓝牙外放实锤,lag≈600ms;在 `~/Documents/voice-notes/notes/` 下按日期找 20260708 开头的笔记目录）:
`VN_NOTE_DIR=<该目录> cargo test calibrate_note -- --ignored --nocapture`
Expected: 多数窗置信度显著（预期 >2），延迟 ≈600ms；清洗输出可试听确认回声消失、本人声音完好

- [ ] **Step 3: 内置扬声器场次标定（反例）**

Run: 近期内置扬声器录音（如 20260714-095607 周二上午的会议）同法跑
Expected: 各窗置信度低（AEC3 实时已消掉,残余弱）,`clean_wav` 返回 None——若反而过了门限,说明门限偏低,上调并复跑两例

- [ ] **Step 4: 定值并写依据**

把两轮实测的 confidence/peak 分布写进 `CONFIDENCE_GATE`/`PEAK_GATE` 注释（例:「蓝牙场次窗中位 conf X.X peak X.XX,内置场次窗最大 conf Y.Y peak Y.YY,取几何中点」），更新两常量。

- [ ] **Step 5: 全量回归 + 提交**

Run: `cd src-tauri && cargo test`
Expected: 全绿（合成测试的置信度断言若受新门限影响,按实测调整断言阈值——断言的是「显著/不显著」这个语义,不是魔法数）

```bash
git add src-tauri/src/audio/echo_clean.rs
git commit -m "置信度门限真实录音标定:蓝牙场次通过/内置场次拒绝,依据入注释"
```

---

### Task 7: 端到端验证与收尾

**Files:**
- 无新改动（验证任务）；如发现问题按 systematic-debugging 回溯

- [ ] **Step 1: 全量回归**

Run: `cd src-tauri && cargo test`
Expected: 全绿

- [ ] **Step 2: 真机端到端冒烟**

构造蓝牙外放场景实录一段（蓝牙音箱放音乐/视频 + 对麦克风说话 1~2 分钟）→ 停录 → 观察 stderr.log:
- 期望顺序: `软件回声消除已启用` → 停录后 `离线回声清洗完成: 延迟 XXXms ...` → 转码日志
- 回放该笔记 mic 轨:外放内容应不可闻/明显衰减,本人声音清晰
- `notes/<id>/audio.json` 里 mic 轨有 `soft_aec:true` 与 `clean:{...}`

- [ ] **Step 3: 反例冒烟**

内置扬声器同法录一段 → 停录 → 期望 `离线回声清洗跳过: 未检出显著回声`,mic.wav 原样转码

- [ ] **Step 4: 收尾提交（如有文档/注释零星修正）**

```bash
git add -u
git commit -m "离线回声清洗一期收尾:真机冒烟记录与注释修正"
```

---

## 实施偏差记录（正文代码块为设计时点,以本节为准）

- **T1** 分窗估计去掉参考前伸(负 lag 出域)、次峰排除邻域 ±3 帧→±300ms、`DelayEstimate` 增 `peak` 字段并改双门限——正文已同步修真(commits 4e331c5/95e613b)。`delay_estimate` 的 `mod tests` 改 `pub(crate) mod tests`(跨模块引用 helper 需模块本身可见)。
- **T2** 补无增益判别测试 `clean_pair_applies_no_gain_on_quiet_near_end`(与 AGC 抬升测试同形输入、断言区间不相交,审查跟进)。
- **T3 e2e 测试**回声合成从单 tap 改 4-tap 衰减路径:本 AEC3 构建对理想单 tap 收敛 ~6s 后线性滤波冻结(ratio≈1.0,跨 Full/Mobile/开关 NS 均复现);多 tap 更接近真实声学路径。
- **T3 e2e 测试**本地声保持容差 ±6dB→±9dB:NS(High) 对无谐波结构的合成噪声过度抑制(纯 NS 探针实测 ~3.7x),系合成信号局限非引擎缺陷;真实语音行为由 T6 真实录音验收兜底。
- **T3 引擎**审查实锤守恒破坏并修复(commit 69edf7f):暖机段非 160 倍数时残帧泄漏进正式 pass——补零冲洗+`cleaned.truncate(seg_end)` 硬保证;新增 `short_tail_segment_conserves_sample_count`(8s+90 样本布局)。段延迟取该段首个置信窗的值(非中位数,正文注释已过时)。

- **T5** 删除正文代码中的清洗前 `repair_wav_header` 预修头(commit 7e1d2d5,审查跟进):引擎按字节读(44 头后全量,不看头内长度字段),预修头反而让「跳过清洗则 mic.wav 字节不动」在陈旧头场景失守;encode 前修头由 `transcode_one` 统一负责。补 `stale_header_untouched_when_cleaning_skipped` 锁约束。已知接受的局限:①矩阵情形3(无 system 轨)的字节断言无法区分早退与下游自然失败,等价降级;②rename 成功后、写 meta 前崩溃 → 清洗报告丢失(音频安全,soft_aec 标记保留,下次重入由门限自然拒绝),排障信息缺口按增值层哲学接受。

## 二期预告（另立计划,不在本计划内）

一期落地并验证延迟估计器可靠后,把 `delay_estimate` 以滑窗方式接入实时链路:预延迟环形缓冲(蓝牙探测给初值)+ 每 5s 重估 + `stats().delay_ms` 观测。届时用一期标定的门限与真实分布数据定二期的调整滞回参数(80ms/置信度阈)。
