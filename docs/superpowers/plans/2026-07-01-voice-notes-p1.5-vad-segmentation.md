# P1.5 VAD 语句分段重构 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 把快流从"对整段累积音频重复识别"改为"按语句分段识别"，根治 O(n²) CPU 与无界内存增长；句内实时 partial、句末定稿 final。

**Architecture:** 引入 Silero VAD（sherpa-onnx）做语句边界检测，抽象为 `Segmenter` trait（真实 `SileroSegmenter` + 测试 `MockSegmenter`）。`run_pipeline` 改为：每块音频喂给 segmenter；取出已完成语句 → 识别 → 发 `final`；按采样节流对"当前句"识别 → 发 `partial`。每次识别音频量被限制在一句话内（受 VAD `max_speech_duration` 上限约束），CPU/内存恒定。

**Tech Stack:** 复用 Rust + sherpa-rs 0.6.8（新增 `silero_vad` 模块）+ Tauri + SvelteKit。

## Global Constraints

- 识别器输入仍为 **16000 Hz 单声道 f32**。
- VAD 模型 `silero_vad.onnx` 放 `src-tauri/models/`，**不进 git**（已 gitignore `*.onnx` + `/src-tauri/models/`）。
- 事件契约：`partial { text: String }`（当前句临时文本，替换）；新增 `final { text: String }`（一句定稿，前端追加）；`status { state: String }` 不变。事件名小写：`"partial"`、`"final"`、`"status"`。
- `Segmenter` 与 `Recognizer` 都是 P1 已建立的可替换边界风格——pipeline 不直接依赖 sherpa 具体类型，便于无模型单测。
- crate `[lib] name` = `app_lib`。Whisper `language=""`（自动）、int8 模型、多线程不变。
- **sherpa-rs SileroVad API（已核对 0.6.8 源码，供参考）**：
  - `SileroVadConfig { model, min_silence_duration, min_speech_duration, max_speech_duration, threshold, sample_rate, window_size, provider, num_threads, debug }`；`Default` 的 `max_speech_duration=0.5` 偏小，**必须显式设大**。
  - `SileroVad::new(config, buffer_size_in_seconds: f32) -> eyre::Result<Self>`
  - `accept_waveform(&mut self, samples: Vec<f32>)` 喂音频
  - `is_empty(&mut self) -> bool` / `front(&mut self) -> SpeechSegment { start: i32, samples: Vec<f32> }` / `pop(&mut self)` —— 已完成语句队列
  - `is_speech(&mut self) -> bool` 当前是否在说话
  - `flush(&mut self)` 收尾把尾段推入队列；`clear(&mut self)` 复位
  - `SileroVad` 是 `Send`。若签名与此不符，按源码 `~/.cargo/registry/src/*/sherpa-rs-0.6.8/src/silero_vad.rs` 调整，保持本计划定义的 `Segmenter` 接口不变。

---

## 文件结构（P1.5 结束时）

```
src-tauri/
  models/silero_vad.onnx            # (gitignored) 新增 VAD 模型
  src/
    pipeline/
      mod.rs                        # 改：去掉 buffer，加 segmenter
      segmenter.rs                  # 新：Segmenter trait + Segment + MockSegmenter
      silero.rs                     # 新：SileroSegmenter（包 sherpa SileroVad）
      buffer.rs                     # 删除（被 VAD 取代）
    session.rs                      # 改：run_pipeline 用 Segmenter，发 final+partial
    ipc.rs                          # 改：新增 FinalEvent
    lib.rs                          # 改：构造 SileroSegmenter，wire final/partial
  tests/
    segmenter_it.rs                 # 新：SileroSegmenter 集成测试（需 vad 模型，#[ignore]）
  scripts/fetch_models.sh           # 改：追加下载 silero_vad.onnx
src/routes/+page.svelte             # 改：渲染 final 历史列表 + 当前 partial 行
src/lib/events.ts                   # 改：新增 onFinal
```

---

## Task 1: 下载 Silero VAD 模型

**Files:** Modify `scripts/fetch_models.sh`

**Interfaces:** Produces `src-tauri/models/silero_vad.onnx` on disk (gitignored).

- [ ] **Step 1: 追加下载逻辑**

在 `scripts/fetch_models.sh` 末尾追加：

```bash
# Silero VAD 模型（单文件 onnx）
if [ ! -f silero_vad.onnx ]; then
  curl -L -o silero_vad.onnx \
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx"
  echo "VAD 模型已就绪：$DIR/silero_vad.onnx"
fi
```

- [ ] **Step 2: 执行下载并校验**

Run: `./scripts/fetch_models.sh && ls -lh src-tauri/models/silero_vad.onnx`
Expected: 文件存在（约 1–2MB）。若 URL 404，到 https://github.com/k2-fsa/sherpa-onnx/releases (asr-models) 找 `silero_vad.onnx` 资源。

- [ ] **Step 3: 提交（仅脚本，模型不进 git）**

```bash
git add scripts/fetch_models.sh
git commit -m "chore(models): fetch silero_vad.onnx for VAD segmentation"
```

---

## Task 2: `Segmenter` trait + `Segment` + `MockSegmenter`

**Files:** Create `src-tauri/src/pipeline/segmenter.rs`; Modify `src-tauri/src/pipeline/mod.rs`

**Interfaces:**
- Produces:
  - `pub struct Segment { pub samples: Vec<f32> }`
  - `pub trait Segmenter: Send { fn accept(&mut self, samples: &[f32]); fn take_finished(&mut self) -> Vec<Segment>; fn current_partial(&mut self) -> Option<Vec<f32>>; fn flush(&mut self); }`
  - `pub struct MockSegmenter { /* 私有 */ }` with `MockSegmenter::new(utterance_len: usize) -> Self` implementing `Segmenter`：每累计 `utterance_len` 个样本产出一个完成段；`current_partial` 返回当前未满段。

- [ ] **Step 1: 写失败测试**

`src-tauri/src/pipeline/segmenter.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_emits_segment_per_utterance_len() {
        let mut s = MockSegmenter::new(100);
        s.accept(&vec![0.0; 60]);
        assert!(s.take_finished().is_empty(), "不足一段");
        assert_eq!(s.current_partial().map(|v| v.len()), Some(60));
        s.accept(&vec![0.0; 50]); // 累计 110 >= 100
        let segs = s.take_finished();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].samples.len(), 100);
        // 段产出后，剩余 10 作为当前句
        assert_eq!(s.current_partial().map(|v| v.len()), Some(10));
    }

    #[test]
    fn mock_flush_emits_remainder() {
        let mut s = MockSegmenter::new(100);
        s.accept(&vec![0.0; 30]);
        s.flush();
        let segs = s.take_finished();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].samples.len(), 30);
        assert!(s.current_partial().is_none(), "flush 后无当前句");
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd src-tauri && cargo test segmenter`
Expected: 编译失败（未定义）。

- [ ] **Step 3: 实现**

`src-tauri/src/pipeline/segmenter.rs` 顶部：

```rust
/// 一个已完成的语句音频段（16kHz 单声道 f32）。
#[derive(Debug, Clone)]
pub struct Segment {
    pub samples: Vec<f32>,
}

/// 语句分段器：吃入音频流，切出完整语句，并提供当前未定稿语句用于实时 partial。
/// 真实实现用 Silero VAD；测试用 MockSegmenter。
pub trait Segmenter: Send {
    /// 喂入一块 16kHz 单声道样本。
    fn accept(&mut self, samples: &[f32]);
    /// 取出自上次调用以来已完成的语句（可能为空）。
    fn take_finished(&mut self) -> Vec<Segment>;
    /// 当前正在说、尚未定稿的语句音频；静音/无内容时返回 None。
    fn current_partial(&mut self) -> Option<Vec<f32>>;
    /// 收尾：把尾部残留语句也切成完成段（录制结束时调用）。
    fn flush(&mut self);
}

/// 测试用：每累计 utterance_len 个样本切一段，不依赖模型。
pub struct MockSegmenter {
    utterance_len: usize,
    current: Vec<f32>,
    finished: Vec<Segment>,
}

impl MockSegmenter {
    pub fn new(utterance_len: usize) -> Self {
        Self { utterance_len: utterance_len.max(1), current: Vec::new(), finished: Vec::new() }
    }
}

impl Segmenter for MockSegmenter {
    fn accept(&mut self, samples: &[f32]) {
        self.current.extend_from_slice(samples);
        while self.current.len() >= self.utterance_len {
            let rest = self.current.split_off(self.utterance_len);
            let seg = std::mem::replace(&mut self.current, rest);
            self.finished.push(Segment { samples: seg });
        }
    }
    fn take_finished(&mut self) -> Vec<Segment> {
        std::mem::take(&mut self.finished)
    }
    fn current_partial(&mut self) -> Option<Vec<f32>> {
        if self.current.is_empty() { None } else { Some(self.current.clone()) }
    }
    fn flush(&mut self) {
        if !self.current.is_empty() {
            self.finished.push(Segment { samples: std::mem::take(&mut self.current) });
        }
    }
}
```

`src-tauri/src/pipeline/mod.rs` 改为：

```rust
pub mod segmenter;
pub mod silero;
```

(删除 `pub mod buffer;`——见 Task 4 删除 buffer.rs。)

- [ ] **Step 4: 运行确认通过**

Run: `cd src-tauri && cargo test segmenter`
Expected: 2 测试 PASS。（此刻 `silero` 模块还不存在，会编译失败——本 Task 先把 `pub mod silero;` 注释掉或建空占位 `src-tauri/src/pipeline/silero.rs` 写 `// 见 Task 3`，保证编译。建占位文件更干净。）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/pipeline/
git commit -m "feat(pipeline): add Segmenter trait + MockSegmenter"
```

---

## Task 3: `SileroSegmenter`（包 sherpa Silero VAD）

**Files:** Create `src-tauri/src/pipeline/silero.rs`; Create `src-tauri/tests/segmenter_it.rs`

**Interfaces:**
- Consumes: `Segmenter`, `Segment`（Task 2）
- Produces: `pub struct SileroSegmenter`，`SileroSegmenter::new(model_path: &std::path::Path) -> anyhow::Result<Self>`，实现 `Segmenter`。

- [ ] **Step 1: 写失败的集成测试**

`src-tauri/tests/segmenter_it.rs`:

```rust
// 需要 VAD 模型；默认 ignore：cargo test --test segmenter_it -- --ignored
use std::path::PathBuf;

fn read_wav_16k(path: &str) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).expect("wav");
    let spec = r.spec();
    assert_eq!(spec.sample_rate, 16000);
    match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect(),
    }
}

#[test]
#[ignore]
fn silero_segments_speech_then_silence() {
    use app_lib::pipeline::segmenter::Segmenter;
    use app_lib::pipeline::silero::SileroSegmenter;
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models/silero_vad.onnx");
    let samples = read_wav_16k(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample_16k.wav"));
    let mut seg = SileroSegmenter::new(&model).expect("load vad");
    // 按 ~30ms 块喂入，模拟真实节奏
    for chunk in samples.chunks(512) {
        seg.accept(chunk);
    }
    seg.flush();
    let finished = seg.take_finished();
    assert!(!finished.is_empty(), "应至少切出一个语句段");
    assert!(finished.iter().all(|s| !s.samples.is_empty()));
}
```

需要 `pipeline` 模块对集成测试可见：本 Task 同时把 `lib.rs` 的 `mod pipeline;` 改为 `pub mod pipeline;`（与 `pub mod asr;` 同理，供 `app_lib::pipeline::...`）。

- [ ] **Step 2: 运行确认失败**

Run: `cd src-tauri && cargo test --test segmenter_it -- --ignored`
Expected: 编译失败（`SileroSegmenter` 未定义）。

- [ ] **Step 3: 实现**

`src-tauri/src/pipeline/silero.rs`:

```rust
use super::segmenter::{Segment, Segmenter};
use std::path::Path;

/// 基于 sherpa-onnx Silero VAD 的语句分段器。
/// 内部维护"当前句"缓冲：只在说话时累积，VAD 切出完整段时清空，用于实时 partial。
pub struct SileroSegmenter {
    vad: sherpa_rs::silero_vad::SileroVad,
    current: Vec<f32>,
}

impl SileroSegmenter {
    pub fn new(model_path: &Path) -> anyhow::Result<Self> {
        let config = sherpa_rs::silero_vad::SileroVadConfig {
            model: model_path.to_string_lossy().into_owned(),
            min_silence_duration: 0.6, // 静音 > 0.6s 视为一句结束
            min_speech_duration: 0.25,
            max_speech_duration: 15.0, // 上限：超 15s 强制切，界定每次识别量
            threshold: 0.5,
            sample_rate: 16000,
            window_size: 512,
            num_threads: Some(1),
            ..Default::default()
        };
        // buffer_size_in_seconds：内部环形缓冲容量，给足
        let vad = sherpa_rs::silero_vad::SileroVad::new(config, 30.0)
            .map_err(|e| anyhow::anyhow!("加载 Silero VAD 失败: {e}"))?;
        Ok(Self { vad, current: Vec::new() })
    }
}

impl Segmenter for SileroSegmenter {
    fn accept(&mut self, samples: &[f32]) {
        self.vad.accept_waveform(samples.to_vec());
        if self.vad.is_speech() {
            self.current.extend_from_slice(samples);
        }
    }

    fn take_finished(&mut self) -> Vec<Segment> {
        let mut out = Vec::new();
        while !self.vad.is_empty() {
            let seg = self.vad.front();
            out.push(Segment { samples: seg.samples });
            self.vad.pop();
        }
        if !out.is_empty() {
            // 已完成的语句对应的"当前句"已结束，清空预览缓冲。
            self.current.clear();
        }
        out
    }

    fn current_partial(&mut self) -> Option<Vec<f32>> {
        if self.current.is_empty() { None } else { Some(self.current.clone()) }
    }

    fn flush(&mut self) {
        self.vad.flush();
        self.current.clear();
    }
}
```

> API 核对：若 `accept_waveform` 取 `&[f32]` 或返回值不同、或字段名有别，按 silero_vad.rs 源码调整，保持 `Segmenter` 接口不变。

- [ ] **Step 4: 运行集成测试确认通过**

Run: `cd src-tauri && cargo test --test segmenter_it -- --ignored`
Expected: PASS（至少切出一个非空语句段）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/pipeline/silero.rs src-tauri/tests/segmenter_it.rs src-tauri/src/lib.rs
git commit -m "feat(pipeline): add SileroSegmenter over sherpa-onnx Silero VAD"
```

---

## Task 4: 重写 `run_pipeline`（final + partial），删除 buffer，更新 ipc

**Files:** Modify `src-tauri/src/session.rs`, `src-tauri/src/ipc.rs`; Delete `src-tauri/src/pipeline/buffer.rs`

**Interfaces:**
- Consumes: `Segmenter`, `Segment`, `Recognizer`, `resample_linear`, `to_mono`, `AudioCapture`
- Produces:
  - `ipc::FinalEvent { text: String }`
  - `run_pipeline(capture, recognizer, segmenter, target_rate, partial_interval_samples, on_partial, on_final)` —— 见下方签名。

- [ ] **Step 1: ipc 加 FinalEvent**

`src-tauri/src/ipc.rs` 追加：

```rust
/// 一句定稿文本，事件名 "final"。
#[derive(Debug, Clone, Serialize)]
pub struct FinalEvent {
    pub text: String,
}
```

- [ ] **Step 2: 删除 buffer 模块**

删除文件 `src-tauri/src/pipeline/buffer.rs`（其窗口逻辑已被 VAD 取代）。确认 `pipeline/mod.rs` 不再含 `pub mod buffer;`（Task 2 已移除）。

- [ ] **Step 3: 写新的 run_pipeline + 失败测试**

替换 `src-tauri/src/session.rs` 的 `run_pipeline` 与其测试为：

```rust
use crate::asr::Recognizer;
use crate::audio::{resample::resample_linear, to_mono, AudioCapture};
use crate::pipeline::segmenter::Segmenter;
use crossbeam_channel::bounded;

/// 录制管线核心：capture 取帧 → 归一 16kHz 单声道 → 喂 segmenter。
/// 每出现完成语句 → 识别 → on_final；按采样节流对当前句识别 → on_partial。
#[allow(clippy::too_many_arguments)]
pub fn run_pipeline(
    mut capture: Box<dyn AudioCapture>,
    mut recognizer: Box<dyn Recognizer>,
    mut segmenter: Box<dyn Segmenter>,
    target_rate: u32,
    partial_interval_samples: usize,
    mut on_partial: impl FnMut(String),
    mut on_final: impl FnMut(String),
) -> anyhow::Result<()> {
    let (tx, rx) = bounded::<crate::audio::AudioFrame>(256);
    capture.start(tx)?;

    let result = (|| -> anyhow::Result<()> {
        let mut since_partial: usize = 0;
        for frame in rx.iter() {
            let mono = to_mono(&frame.samples, frame.channels);
            let resampled = resample_linear(&mono, frame.sample_rate, target_rate);
            since_partial += resampled.len();
            segmenter.accept(&resampled);

            // 完成的语句：定稿
            for seg in segmenter.take_finished() {
                let t = recognizer.recognize(&seg.samples)?;
                on_final(t.text);
                since_partial = 0; // 定稿后重置 partial 节流
            }

            // 当前句：按采样节流出 partial
            if since_partial >= partial_interval_samples {
                since_partial = 0;
                if let Some(cur) = segmenter.current_partial() {
                    let t = recognizer.recognize(&cur)?;
                    on_partial(t.text);
                }
            }
        }
        // 收尾：尾段定稿
        segmenter.flush();
        for seg in segmenter.take_finished() {
            let t = recognizer.recognize(&seg.samples)?;
            on_final(t.text);
        }
        Ok(())
    })();

    capture.stop();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::{Recognizer, Transcript};
    use crate::audio::mock::MockCapture;
    use crate::pipeline::segmenter::MockSegmenter;
    use std::sync::{Arc, Mutex};

    /// 假识别器：回传样本数，便于断言管线确实送来了归一化音频。
    struct CountingRecognizer;
    impl Recognizer for CountingRecognizer {
        fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: format!("len={}", samples.len()) })
        }
    }

    #[test]
    fn pipeline_emits_finals_via_segmenter() {
        let capture = Box::new(
            MockCapture::from_wav(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample_16k.wav"))
                .expect("fixture"),
        );
        // 小 utterance_len 确保从 fixture 切出多个 final；partial 间隔给小值确保至少触发一次。
        let segmenter = Box::new(MockSegmenter::new(8000));
        let finals = Arc::new(Mutex::new(Vec::<String>::new()));
        let partials = Arc::new(Mutex::new(Vec::<String>::new()));
        let f2 = finals.clone();
        let p2 = partials.clone();
        run_pipeline(
            capture,
            Box::new(CountingRecognizer),
            segmenter,
            16000,
            4000,
            move |t| p2.lock().unwrap().push(t),
            move |t| f2.lock().unwrap().push(t),
        )
        .expect("run");
        assert!(!finals.lock().unwrap().is_empty(), "应至少有一个 final");
        assert!(finals.lock().unwrap().iter().all(|s| s.starts_with("len=")));
    }
}
```

- [ ] **Step 4: 运行确认 RED→GREEN**

Run: `cd src-tauri && cargo test pipeline_emits_finals`
Expected: 实现后 PASS。

- [ ] **Step 5: 提交**

```bash
git add -A
git commit -m "feat(session): rewrite run_pipeline around VAD segmenter (final + throttled partial); remove buffer"
```

---

## Task 5: lib.rs 接线 + 前端渲染 final/partial

**Files:** Modify `src-tauri/src/lib.rs`, `src/lib/events.ts`, `src/routes/+page.svelte`

**Interfaces:**
- Consumes: `SileroSegmenter`, `run_pipeline`, `ipc::{PartialEvent, FinalEvent, StatusEvent}`

- [ ] **Step 1: lib.rs 构造 segmenter 并接线**

在 `start_recording` 的线程内，模型目录已有 whisper；新增 vad 模型路径 `models/silero_vad.onnx`。把 `run_pipeline` 调用替换为带 segmenter + on_final 版本：

```rust
let vad_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models/silero_vad.onnx");
let segmenter = match pipeline::silero::SileroSegmenter::new(&vad_path) {
    Ok(s) => Box::new(s) as Box<dyn pipeline::segmenter::Segmenter>,
    Err(e) => {
        let _ = app.emit("status", ipc::StatusEvent { state: format!("error: {e}") });
        *running.lock().unwrap() = false;
        return;
    }
};
let app_p = app.clone();
let app_f = app.clone();
if let Err(e) = session::run_pipeline(
    capture, recognizer, segmenter, 16000, 16000, // partial 间隔 ~1s
    move |text| { let _ = app_p.emit("partial", ipc::PartialEvent { text }); },
    move |text| { let _ = app_f.emit("final", ipc::FinalEvent { text }); },
) {
    let _ = app.emit("status", ipc::StatusEvent { state: format!("error: {e}") });
    *running.lock().unwrap() = false;
    return;
}
```

(注意：`pipeline` 模块在 Task 3 已改为 `pub mod pipeline;`。)

- [ ] **Step 2: events.ts 加 onFinal**

`src/lib/events.ts` 追加：

```ts
export type FinalEvent = { text: string };
export function onFinal(cb: (e: FinalEvent) => void) {
  return listen<FinalEvent>("final", (ev) => cb(ev.payload));
}
```

- [ ] **Step 3: +page.svelte 渲染 final 列表 + 当前 partial 行**

在 `src/routes/+page.svelte`：用 `$state` 维护 `finals: string[]` 与 `partial: string`。`onFinal` 时 `finals = [...finals, e.text]` 且 `partial = ""`；`onPartial` 时 `partial = e.text`；`onStatus` 时若变为 `recording` 可清空上轮内容。渲染：

```svelte
<div class="transcript">
  {#each finals as line, i}
    <p class="final">{line}</p>
  {/each}
  {#if partial}
    <p class="partial">{partial}</p>
  {/if}
  {#if finals.length === 0 && !partial}
    <p class="hint">（开始说话…）</p>
  {/if}
</div>
```

`.partial` 用浅色/斜体区分"未定稿"，`.final` 实色。保留开始/停止按钮与 status 显示不变。

- [ ] **Step 4: 构建确认**

Run: `cd src-tauri && cargo build` （后端）；项目根 `npm run build`（前端）
Expected: 均成功，无类型错误。

- [ ] **Step 5: 提交**

```bash
git add -A
git commit -m "feat(ui): render finalized utterances list + live partial; wire SileroSegmenter"
```

---

## Self-Review（计划对照目标）

**1. 覆盖：** O(n²) 根因（对整段累积识别）→ Task 4 改为按语句段识别，每次量受 `max_speech_duration` 上限约束 ✓；无界内存 → segmenter 完成段后清 `current`，buffer 删除 ✓；句内 partial → Task 4 采样节流 + `current_partial` ✓；句末 final → VAD 完成段 ✓；前端历史+实时行 → Task 5 ✓；可无模型单测 → `Segmenter`/`MockSegmenter` ✓。

**2. 占位符：** 无 TBD。VAD 参数（0.6/0.25/15.0/0.5/512）均为显式具体值。

**3. 类型一致：** `Segmenter::{accept,take_finished,current_partial,flush}`、`Segment{samples}`、`run_pipeline` 七参签名、`FinalEvent{text}`/事件名 `"final"` 在 lib.rs 与 events.ts 一致。

**4. 外部依赖：** sherpa-rs SileroVad API 已据 0.6.8 源码核对；偏差按源码微调、保持 trait 不变。
