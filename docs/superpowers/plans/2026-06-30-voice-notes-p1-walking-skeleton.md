# P1 行走骨架 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 做出一个能运行的 Tauri + Rust 桌面应用：点"开始录音" → 采集麦克风 → 本地 Whisper 识别 → 屏幕上近实时滚动出字；点"停止"结束。

**Architecture:** Rust 核心把 `麦克风采集 → 重采样到 16kHz 单声道 → 累积窗口缓冲 → sherpa-onnx Whisper 识别 → Tauri 事件` 串成一条单向管线。音频采集隐藏在 `AudioCapture` trait 后，识别隐藏在 `Recognizer` trait 后，便于后续计划替换/扩展。前端是最小的 Svelte 页面，监听 `partial`/`status` 事件渲染。

**Tech Stack:** Rust + Tauri 2、Svelte + TypeScript（前端，全项目统一）、`cpal`（麦克风）、`hound`（测试读 WAV）、`sherpa-rs`（sherpa-onnx 的 Rust 绑定，Whisper 识别）、`anyhow`/`thiserror`（错误）、`crossbeam-channel`（线程间音频帧传递）。

## Global Constraints

- **平台**：macOS 优先（Apple Silicon）。本计划只实现 macOS 麦克风采集；系统声音、Windows/Linux 属后续计划。
- **采样格式**：识别器输入统一为 **16000 Hz、单声道、`f32`（取值 [-1.0, 1.0]）**。所有上游数据在进入 `Recognizer` 前必须转成该格式。
- **离线**：除模型文件下载外不联网。
- **前端框架**：Svelte + TypeScript，全项目统一（解决 spec 第 11 节的开放问题）。
- **识别模型**：开发/测试期使用小模型（Whisper **base**，sherpa-onnx ONNX 导出版）以便快速、确定性测试；large-v3 在 P5 模型管理计划接入。模型文件放 `src-tauri/models/`，**不进 git**。
- **测试音频**：固定样本 WAV 放 `src-tauri/tests/fixtures/`，16kHz 单声道，进 git（小文件）。
- **依赖 sherpa-rs API**：`sherpa-rs` 的具体类型/方法名可能随版本变化。涉及它的步骤里，若编译报"找不到类型/方法"，先 `cargo doc -p sherpa-rs --open` 或查 https://docs.rs/sherpa-rs 核对当前 API，再据此微调；本计划中我们自己的 `Recognizer` trait 接口保持不变。

---

## 文件结构（P1 结束时）

```
voice-notes/
  package.json                      # 前端 + Tauri 脚本
  src/                              # Svelte 前端
    main.ts                         # 挂载 App
    App.svelte                      # 录制 UI：开始/停止 + 实时转写区
    lib/events.ts                   # 监听 Tauri 事件的封装
  src-tauri/
    Cargo.toml
    tauri.conf.json
    build.rs
    models/                         # (gitignored) Whisper base ONNX 模型
    tests/
      fixtures/sample_16k.wav       # 测试音频（含可预期关键词）
      recognizer_it.rs              # Recognizer 集成测试（需模型，env 门控）
    src/
      main.rs                       # 二进制入口，调用 lib::run()
      lib.rs                        # Tauri builder、命令注册、模块声明
      audio/
        mod.rs                      # AudioFrame、AudioCapture trait、to_mono
        resample.rs                 # resample_linear
        microphone.rs               # cpal 麦克风实现 (macOS)
        mock.rs                     # MockCapture（从 WAV 回放，供测试）
      pipeline/
        mod.rs
        buffer.rs                   # AccumulatingBuffer（快流窗口）
      asr/
        mod.rs                      # Recognizer trait、Transcript
        whisper.rs                  # sherpa-rs Whisper 实现
      session.rs                    # SessionState 状态机 + 录制编排
      ipc.rs                        # PartialEvent / StatusEvent 负载类型
  docs/superpowers/...
```

每个文件职责单一；`AudioCapture` 与 `Recognizer` 是两个清晰的可替换边界。

---

## Task 1: 项目脚手架（Tauri 2 + Svelte + Rust 模块骨架）

**Files:**
- Create: 整个 Tauri 2 + Svelte 项目（`package.json`、`src/`、`src-tauri/`）
- Create: `src-tauri/src/lib.rs`、`src-tauri/src/main.rs`、空模块文件

**Interfaces:**
- Consumes: 无
- Produces: 一个可 `npm run tauri dev` 启动的空壳应用；模块树就位供后续 Task 填充。

- [ ] **Step 1: 用官方模板初始化项目**

在项目根 `/Users/teemo/workspace-soul/voice-notes` 执行（目录已有 `docs/` 和 `.git/`，模板需写入当前目录）：

```bash
npm create tauri-app@latest . -- --template svelte-ts --manager npm --yes
```

如果工具因目录非空拒绝，改用临时目录生成再拷入：

```bash
npm create tauri-app@latest vn-tmp -- --template svelte-ts --manager npm --yes
cp -R vn-tmp/. . && rm -rf vn-tmp
```

- [ ] **Step 2: 安装依赖并确认能启动**

```bash
npm install
npm run tauri dev
```

Expected: 弹出一个原生窗口显示模板默认页面。确认后 `Ctrl-C` 退出。

- [ ] **Step 3: 加入 Rust 依赖**

编辑 `src-tauri/Cargo.toml`，在 `[dependencies]` 增加：

```toml
anyhow = "1"
thiserror = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
cpal = "0.15"
crossbeam-channel = "0.5"
hound = "3.5"
sherpa-rs = "0.6"
```

(`sherpa-rs` 版本以 crates.io 最新为准；若 0.6 不存在取最近版本。)

- [ ] **Step 4: 建立模块树**

把 `src-tauri/src/lib.rs` 改成声明模块（保留模板已有的 `run()` 与 `greet` 命令暂不删）：

```rust
mod audio;
mod pipeline;
mod asr;
mod ipc;
mod session;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

创建以下占位文件，先放最小内容让其能编译：

`src-tauri/src/audio/mod.rs`:
```rust
pub mod resample;
```
`src-tauri/src/audio/resample.rs`:
```rust
// 见 Task 3
```
`src-tauri/src/pipeline/mod.rs`:
```rust
pub mod buffer;
```
`src-tauri/src/pipeline/buffer.rs`:
```rust
// 见 Task 4
```
`src-tauri/src/asr/mod.rs`:
```rust
// 见 Task 5
```
`src-tauri/src/ipc.rs`:
```rust
// 见 Task 7
```
`src-tauri/src/session.rs`:
```rust
// 见 Task 7
```

- [ ] **Step 5: 确认编译通过**

Run: `cd src-tauri && cargo build`
Expected: 编译成功（可能有 unused 警告，允许）。

- [ ] **Step 6: 提交**

```bash
git add -A
git commit -m "chore: scaffold Tauri 2 + Svelte app and Rust module tree"
```

---

## Task 2: 音频数据类型、`AudioCapture` trait 与单声道转换

**Files:**
- Modify: `src-tauri/src/audio/mod.rs`

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub struct AudioFrame { pub samples: Vec<f32>, pub sample_rate: u32, pub channels: u16 }`
  - `pub trait AudioCapture: Send { fn start(&mut self, sink: crossbeam_channel::Sender<AudioFrame>) -> anyhow::Result<()>; fn stop(&mut self); }`
  - `pub fn to_mono(samples: &[f32], channels: u16) -> Vec<f32>`

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/audio/mod.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_mono_averages_stereo_pairs() {
        // 交错立体声: L0,R0, L1,R1
        let stereo = vec![0.0, 1.0, 0.5, -0.5];
        let mono = to_mono(&stereo, 2);
        assert_eq!(mono, vec![0.5, 0.0]);
    }

    #[test]
    fn to_mono_passthrough_for_mono() {
        let m = vec![0.1, 0.2, 0.3];
        assert_eq!(to_mono(&m, 1), m);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test to_mono`
Expected: 编译失败（`to_mono`、`AudioFrame` 未定义）。

- [ ] **Step 3: 实现类型与转换**

把 `src-tauri/src/audio/mod.rs` 顶部改为：

```rust
pub mod resample;
pub mod mock;
pub mod microphone;

use crossbeam_channel::Sender;

/// 一帧原始音频，来自采集设备的原生格式。
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// 音频采集源的统一接口。后续计划新增系统声音 / 其他平台时实现本 trait。
pub trait AudioCapture: Send {
    /// 开始采集；每采到一块就通过 sink 发出一帧。非阻塞。
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()>;
    /// 停止采集并释放设备。
    fn stop(&mut self);
}

/// 交错多声道 -> 单声道（按帧平均各声道）。
pub fn to_mono(samples: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let ch = channels as usize;
    samples
        .chunks(ch)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}
```

(`mock`/`microphone` 模块文件已在 Task 1 占位为空，会编译；其内容分别在 Task 6、以及下方 Step 提供。先把它们改成空白合法文件：`microphone.rs` 写 `// 见 Task 6`，`mock.rs` 见下一个 Task。若此时为空模块报错，临时各放一行注释即可。)

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test to_mono`
Expected: 2 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add -A
git commit -m "feat(audio): add AudioFrame, AudioCapture trait, to_mono"
```

---

## Task 3: 线性重采样到目标采样率

**Files:**
- Modify: `src-tauri/src/audio/resample.rs`

**Interfaces:**
- Consumes: 无
- Produces: `pub fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32>`（输入须为单声道）

- [ ] **Step 1: 写失败测试**

`src-tauri/src/audio/resample.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_rate_is_passthrough() {
        let x = vec![0.0, 0.5, 1.0];
        assert_eq!(resample_linear(&x, 16000, 16000), x);
    }

    #[test]
    fn downsample_48k_to_16k_thirds_the_length() {
        let input: Vec<f32> = (0..4800).map(|i| i as f32).collect(); // 0.1s @48k
        let out = resample_linear(&input, 48000, 16000);
        // 16k 下 0.1s ≈ 1600 个样本，容许 ±2
        assert!((out.len() as i64 - 1600).abs() <= 2, "len = {}", out.len());
    }

    #[test]
    fn linear_ramp_stays_monotonic() {
        let input: Vec<f32> = (0..300).map(|i| i as f32).collect();
        let out = resample_linear(&input, 44100, 16000);
        for w in out.windows(2) {
            assert!(w[1] >= w[0], "应单调不减");
        }
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test resample`
Expected: 编译失败（`resample_linear` 未定义）。

- [ ] **Step 3: 实现**

在 `src-tauri/src/audio/resample.rs` 顶部加：

```rust
/// 单声道线性插值重采样。骨架阶段够用；高质量重采样（rubato）留待后续优化。
pub fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let out_len = (input.len() as f64 * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let s0 = input.get(idx).copied().unwrap_or(0.0);
        let s1 = input.get(idx + 1).copied().unwrap_or(s0);
        out.push(s0 + (s1 - s0) * frac);
    }
    out
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test resample`
Expected: 3 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add -A
git commit -m "feat(audio): add linear resampler to 16kHz"
```

---

## Task 4: 快流累积窗口缓冲

把进来的 16kHz 单声道样本累积，达到窗口长度就吐出一块给识别器（快流"每 ~1–2 秒识别一次"的机制）。

**Files:**
- Modify: `src-tauri/src/pipeline/buffer.rs`

**Interfaces:**
- Consumes: 16kHz 单声道 `&[f32]`
- Produces:
  - `pub struct AccumulatingBuffer { /* 私有 */ }`
  - `AccumulatingBuffer::new(sample_rate: u32, window_secs: f32) -> Self`
  - `fn push(&mut self, samples: &[f32]) -> Option<Vec<f32>>`（累积够一个窗口则返回该窗口快照，否则 None）
  - `fn drain(&mut self) -> Vec<f32>`（停止时取出剩余不足一窗的样本）

- [ ] **Step 1: 写失败测试**

`src-tauri/src/pipeline/buffer.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_when_window_reached() {
        // 16kHz, 1 秒窗口 => 16000 样本
        let mut buf = AccumulatingBuffer::new(16000, 1.0);
        assert!(buf.push(&vec![0.0; 8000]).is_none(), "不足一窗不应吐出");
        let out = buf.push(&vec![0.0; 8000]).expect("应吐出一窗");
        assert_eq!(out.len(), 16000);
    }

    #[test]
    fn window_grows_across_pushes() {
        // 第二窗应包含累积的全部历史（快流：每次识别"当前累积段"）
        let mut buf = AccumulatingBuffer::new(16000, 1.0);
        buf.push(&vec![1.0; 16000]); // 第 1 窗
        let out = buf.push(&vec![2.0; 16000]).expect("第 2 窗");
        assert_eq!(out.len(), 32000, "第二窗应是累积长度");
    }

    #[test]
    fn drain_returns_remainder() {
        let mut buf = AccumulatingBuffer::new(16000, 1.0);
        buf.push(&vec![0.0; 5000]);
        assert_eq!(buf.drain().len(), 5000);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test buffer`
Expected: 编译失败（`AccumulatingBuffer` 未定义）。

- [ ] **Step 3: 实现**

在 `src-tauri/src/pipeline/buffer.rs` 顶部加：

```rust
/// 累积 16kHz 单声道样本。每累计满 window_secs，就返回"从录制开始到当前"的累积快照，
/// 用于快流：对当前累积段重复识别、实时刷新临时文本。
pub struct AccumulatingBuffer {
    samples: Vec<f32>,
    window_len: usize,
    next_emit_at: usize,
}

impl AccumulatingBuffer {
    pub fn new(sample_rate: u32, window_secs: f32) -> Self {
        let window_len = (sample_rate as f32 * window_secs).round() as usize;
        Self { samples: Vec::new(), window_len: window_len.max(1), next_emit_at: window_len.max(1) }
    }

    pub fn push(&mut self, samples: &[f32]) -> Option<Vec<f32>> {
        self.samples.extend_from_slice(samples);
        if self.samples.len() >= self.next_emit_at {
            self.next_emit_at += self.window_len;
            Some(self.samples.clone())
        } else {
            None
        }
    }

    pub fn drain(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.samples)
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test buffer`
Expected: 3 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add -A
git commit -m "feat(pipeline): add AccumulatingBuffer for fast-stream windowing"
```

---

## Task 5: `Recognizer` trait 与 sherpa-onnx Whisper 实现

**Files:**
- Modify: `src-tauri/src/asr/mod.rs`
- Create: `src-tauri/src/asr/whisper.rs`
- Create: `src-tauri/tests/recognizer_it.rs`
- Create: `src-tauri/tests/fixtures/sample_16k.wav`（见 Step 1）
- Create: `scripts/fetch_models.sh`

**Interfaces:**
- Consumes: 16kHz 单声道 `&[f32]`
- Produces:
  - `pub struct Transcript { pub text: String }`
  - `pub trait Recognizer: Send { fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript>; }`
  - `pub struct WhisperRecognizer { /* 私有 */ }`，`WhisperRecognizer::new(model_dir: &std::path::Path) -> anyhow::Result<Self>`，实现 `Recognizer`

- [ ] **Step 1: 准备测试资产（模型 + 样本音频）**

创建 `scripts/fetch_models.sh`（下载 sherpa-onnx Whisper base 模型到 `src-tauri/models/`）:

```bash
#!/usr/bin/env bash
set -euo pipefail
DIR="$(cd "$(dirname "$0")/../src-tauri/models" && pwd)"
cd "$DIR"
# sherpa-onnx 官方导出的 whisper-base（含 encoder/decoder onnx + tokens.txt）
URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-base.tar.bz2"
curl -L -o whisper-base.tar.bz2 "$URL"
tar xjf whisper-base.tar.bz2
echo "模型已就绪：$DIR/sherpa-onnx-whisper-base"
```

执行：
```bash
mkdir -p src-tauri/models
chmod +x scripts/fetch_models.sh
./scripts/fetch_models.sh
```

录制/获取一段约 3–4 秒、清晰朗读 **"hello world 测试一二三"** 的 16kHz 单声道 WAV，存为 `src-tauri/tests/fixtures/sample_16k.wav`（可用 `say` + 转码：`say -o /tmp/s.aiff "hello world 测试一二三"; ffmpeg -i /tmp/s.aiff -ar 16000 -ac 1 src-tauri/tests/fixtures/sample_16k.wav`）。

- [ ] **Step 2: 写失败的集成测试**

`src-tauri/tests/recognizer_it.rs`:

```rust
// 需要本地模型；默认 ignore，运行：VN_MODELS=1 cargo test --test recognizer_it -- --ignored
use std::path::PathBuf;

fn read_wav_mono_16k(path: &str) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("打开 WAV");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 16000, "fixture 必须是 16kHz");
    assert_eq!(spec.channels, 1, "fixture 必须是单声道");
    match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect(),
    }
}

#[test]
#[ignore]
fn whisper_transcribes_fixture() {
    use app_lib::asr::{whisper::WhisperRecognizer, Recognizer};
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/sherpa-onnx-whisper-base");
    let samples = read_wav_mono_16k(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample_16k.wav"
    ));
    let mut rec = WhisperRecognizer::new(&model_dir).expect("加载模型");
    let t = rec.recognize(&samples).expect("识别");
    let lower = t.text.to_lowercase();
    assert!(lower.contains("hello") || lower.contains("world"),
        "识别结果应含预期关键词，实际: {}", t.text);
}
```

> 注：集成测试通过 `app_lib::` 访问库。确认 `src-tauri/Cargo.toml` 中 `[lib] name` 的值（模板通常是 `app_lib`）；若不同，替换上面的 crate 名。

- [ ] **Step 3: 运行测试确认失败**

Run: `cd src-tauri && cargo test --test recognizer_it -- --ignored`
Expected: 编译失败（`asr::whisper` 未定义）。

- [ ] **Step 4: 定义 trait**

`src-tauri/src/asr/mod.rs`:

```rust
pub mod whisper;

/// 一次识别的结果文本。
#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
}

/// 语音识别接口。输入须为 16kHz 单声道 f32。
/// 后续计划可新增其它实现（如 whisper-rs）而不动调用方。
pub trait Recognizer: Send {
    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript>;
}
```

- [ ] **Step 5: 实现 sherpa-rs Whisper**

`src-tauri/src/asr/whisper.rs`:

```rust
use super::{Recognizer, Transcript};
use std::path::Path;

/// 基于 sherpa-onnx 的离线 Whisper 识别器。
pub struct WhisperRecognizer {
    inner: sherpa_rs::whisper::WhisperRecognizer,
}

impl WhisperRecognizer {
    /// model_dir 应包含 sherpa-onnx 导出的 *-encoder.onnx / *-decoder.onnx / tokens.txt。
    pub fn new(model_dir: &Path) -> anyhow::Result<Self> {
        let encoder = find_one(model_dir, "encoder")?;
        let decoder = find_one(model_dir, "decoder")?;
        let tokens = model_dir.join("tokens.txt");

        let config = sherpa_rs::whisper::WhisperConfig {
            encoder: encoder.to_string_lossy().into_owned(),
            decoder: decoder.to_string_lossy().into_owned(),
            tokens: tokens.to_string_lossy().into_owned(),
            language: "auto".into(), // 中英混合：自动语种
            ..Default::default()
        };
        let inner = sherpa_rs::whisper::WhisperRecognizer::new(config)
            .map_err(|e| anyhow::anyhow!("加载 Whisper 失败: {e:?}"))?;
        Ok(Self { inner })
    }
}

impl Recognizer for WhisperRecognizer {
    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
        // sherpa-rs 识别接口接受 16kHz f32；返回结构含 .text
        let result = self.inner.transcribe(16000, samples.to_vec());
        Ok(Transcript { text: result.text })
    }
}

/// 在目录中找到文件名包含关键字的 .onnx 文件（兼容不同命名）。
fn find_one(dir: &Path, keyword: &str) -> anyhow::Result<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.ends_with(".onnx") && name.contains(keyword) {
            return Ok(p);
        }
    }
    anyhow::bail!("在 {:?} 找不到包含 '{}' 的 .onnx", dir, keyword)
}
```

> **API 核对**：若 `WhisperConfig` 字段名、`WhisperRecognizer::new` 或 `transcribe` 签名与当前 `sherpa-rs` 不符，按 Global Constraints 的指引查 docs.rs 调整——保持本文件对外暴露的 `WhisperRecognizer::new(&Path)` 与 `Recognizer` 实现不变。

- [ ] **Step 6: 运行集成测试确认通过**

Run: `cd src-tauri && VN_MODELS=1 cargo test --test recognizer_it -- --ignored`
Expected: `whisper_transcribes_fixture` PASS（识别文本含 hello/world）。

- [ ] **Step 7: 把 models/ 加入 gitignore，提交**

确认根 `.gitignore` 含 `*.onnx` 与 `/src-tauri/models/`（追加缺失项）。

```bash
git add -A
git commit -m "feat(asr): add Recognizer trait + sherpa-onnx Whisper impl with integration test"
```

---

## Task 6: macOS 麦克风采集（cpal 实现 `AudioCapture`）

**Files:**
- Modify: `src-tauri/src/audio/microphone.rs`

**Interfaces:**
- Consumes: `AudioFrame`、`AudioCapture`（Task 2）
- Produces: `pub struct Microphone { /* 私有 */ }`，`Microphone::new() -> Self`，实现 `AudioCapture`

实时麦克风无法做确定性单元测试，本 Task 以"能编译 + 手动冒烟"验收；管线串联的自动化用 Task 7 的 MockCapture 覆盖。

- [ ] **Step 1: 实现**

`src-tauri/src/audio/microphone.rs`:

```rust
use super::{AudioCapture, AudioFrame};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::Sender;

pub struct Microphone {
    stream: Option<cpal::Stream>,
}

impl Microphone {
    pub fn new() -> Self {
        Self { stream: None }
    }
}

impl AudioCapture for Microphone {
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow::anyhow!("找不到默认麦克风"))?;
        let config = device.default_input_config()?;
        let sample_rate = config.sample_rate().0;
        let channels = config.channels();

        let err_fn = |e| eprintln!("麦克风流错误: {e}");
        let stream = device.build_input_stream(
            &config.into(),
            move |data: &[f32], _| {
                let _ = sink.send(AudioFrame {
                    samples: data.to_vec(),
                    sample_rate,
                    channels,
                });
            },
            err_fn,
            None,
        )?;
        stream.play()?;
        self.stream = Some(stream);
        Ok(())
    }

    fn stop(&mut self) {
        self.stream = None; // drop 即停止
    }
}
```

> 注：上面假设设备样本格式为 f32（绝大多数 macOS 设备如此）。若 `default_input_config().sample_format()` 非 f32，需匹配 `I16`/`U16` 分支转换——骨架阶段先按 f32，遇到再补。

- [ ] **Step 2: 配置麦克风权限**

在 `src-tauri/tauri.conf.json` 的 macOS 段加入麦克风用途说明（Info.plist）：

在 `bundle` 下增加：
```json
"macOS": {
  "infoPlist": {
    "NSMicrophoneUsageDescription": "用于实时转写你的语音。"
  }
}
```

- [ ] **Step 3: 确认编译**

Run: `cd src-tauri && cargo build`
Expected: 编译成功。

- [ ] **Step 4: 提交**

```bash
git add -A
git commit -m "feat(audio): add macOS microphone capture via cpal"
```

(手动冒烟在 Task 7 串起管线后一起做。)

---

## Task 7: 会话编排 + Tauri 命令 + IPC 事件 + MockCapture

把管线串起来：`AudioCapture → 收帧线程（to_mono + 重采样 + 累积）→ Recognizer → 发 partial 事件`。提供 `start_recording`/`stop_recording` 命令。用 `MockCapture` 做管线自动化测试。

**Files:**
- Modify: `src-tauri/src/ipc.rs`
- Modify: `src-tauri/src/session.rs`
- Create: `src-tauri/src/audio/mock.rs`
- Modify: `src-tauri/src/lib.rs`（注册命令、管理状态）

**Interfaces:**
- Consumes: `AudioCapture`、`AudioFrame`、`to_mono`、`resample_linear`、`AccumulatingBuffer`、`Recognizer`、`Transcript`
- Produces:
  - `ipc::PartialEvent { text: String }`、`ipc::StatusEvent { state: String }`
  - `pub fn run_pipeline(capture, recognizer, target_rate, window_secs, on_partial)`（纯函数式核心，便于测试）
  - `MockCapture::from_wav(path) -> Self`
  - Tauri 命令 `start_recording`、`stop_recording`

- [ ] **Step 1: 定义 IPC 负载**

`src-tauri/src/ipc.rs`:

```rust
use serde::Serialize;

/// 快流临时文本，事件名 "partial"。
#[derive(Debug, Clone, Serialize)]
pub struct PartialEvent {
    pub text: String,
}

/// 录制状态，事件名 "status"。
#[derive(Debug, Clone, Serialize)]
pub struct StatusEvent {
    pub state: String, // "recording" | "stopped" | "error"
}
```

- [ ] **Step 2: 写 MockCapture（从 WAV 回放）**

`src-tauri/src/audio/mock.rs`:

```rust
use super::{AudioCapture, AudioFrame};
use crossbeam_channel::Sender;

/// 测试用采集源：把一个 WAV 一次性按帧发出后结束。
pub struct MockCapture {
    frames: Vec<AudioFrame>,
}

impl MockCapture {
    pub fn from_wav(path: &str) -> anyhow::Result<Self> {
        let mut reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
            hound::SampleFormat::Int => {
                reader.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect()
            }
        };
        // 切成 ~100ms 的帧，模拟真实采集节奏
        let frame_len = (spec.sample_rate as usize / 10) * spec.channels as usize;
        let frames = samples
            .chunks(frame_len.max(1))
            .map(|c| AudioFrame {
                samples: c.to_vec(),
                sample_rate: spec.sample_rate,
                channels: spec.channels,
            })
            .collect();
        Ok(Self { frames })
    }
}

impl AudioCapture for MockCapture {
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
        for f in self.frames.drain(..) {
            let _ = sink.send(f);
        }
        Ok(()) // 发完即返回，sink 被 drop 后接收端结束
    }
    fn stop(&mut self) {}
}
```

- [ ] **Step 3: 写管线核心 + 失败测试**

`src-tauri/src/session.rs`:

```rust
use crate::asr::Recognizer;
use crate::audio::{resample::resample_linear, to_mono, AudioCapture};
use crate::pipeline::buffer::AccumulatingBuffer;
use crossbeam_channel::bounded;

/// 录制管线核心：从 capture 取帧，归一到 target_rate 单声道，累积成窗，
/// 每窗调用 recognizer，并通过 on_partial 回调发出临时文本。
/// 同步运行直到 capture 的发送端关闭。
pub fn run_pipeline(
    mut capture: Box<dyn AudioCapture>,
    mut recognizer: Box<dyn Recognizer>,
    target_rate: u32,
    window_secs: f32,
    mut on_partial: impl FnMut(String),
) -> anyhow::Result<()> {
    let (tx, rx) = bounded::<crate::audio::AudioFrame>(256);
    capture.start(tx)?;

    let mut buf = AccumulatingBuffer::new(target_rate, window_secs);
    for frame in rx.iter() {
        let mono = to_mono(&frame.samples, frame.channels);
        let resampled = resample_linear(&mono, frame.sample_rate, target_rate);
        if let Some(window) = buf.push(&resampled) {
            let t = recognizer.recognize(&window)?;
            on_partial(t.text);
        }
    }
    // 收尾：剩余不足一窗的也识别一次
    let rest = buf.drain();
    if !rest.is_empty() {
        let t = recognizer.recognize(&rest)?;
        on_partial(t.text);
    }
    capture.stop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::{Recognizer, Transcript};
    use crate::audio::mock::MockCapture;
    use std::sync::{Arc, Mutex};

    /// 假识别器：返回收到的样本数，便于断言管线确实送来了归一化音频。
    struct CountingRecognizer;
    impl Recognizer for CountingRecognizer {
        fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: format!("len={}", samples.len()) })
        }
    }

    #[test]
    fn pipeline_emits_partials_from_wav() {
        let capture = Box::new(
            MockCapture::from_wav(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/sample_16k.wav"
            ))
            .expect("读取 fixture"),
        );
        let collected = Arc::new(Mutex::new(Vec::<String>::new()));
        let c2 = collected.clone();
        run_pipeline(capture, Box::new(CountingRecognizer), 16000, 1.0, move |t| {
            c2.lock().unwrap().push(t)
        })
        .expect("管线运行");
        let got = collected.lock().unwrap();
        assert!(!got.is_empty(), "应至少发出一次 partial");
        assert!(got.last().unwrap().starts_with("len="));
    }
}
```

- [ ] **Step 4: 运行测试确认失败 → 通过**

Run: `cd src-tauri && cargo test pipeline_emits_partials`
Expected: 先因引用关系编译失败则补全 `mod`/`use`；修正后该测试 PASS（fixture 由 Task 5 提供）。

- [ ] **Step 5: 接 Tauri 命令与真实事件**

`src-tauri/src/lib.rs` 改为：

```rust
mod audio;
mod pipeline;
mod asr;
mod ipc;
mod session;

use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Default)]
struct AppState {
    running: Arc<Mutex<bool>>,
}

#[tauri::command]
fn start_recording(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    {
        let mut r = state.running.lock().unwrap();
        if *r { return Err("已在录制".into()); }
        *r = true;
    }
    let running = state.running.clone();
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/sherpa-onnx-whisper-base");

    std::thread::spawn(move || {
        let _ = app.emit("status", ipc::StatusEvent { state: "recording".into() });
        let recognizer = match asr::whisper::WhisperRecognizer::new(&model_dir) {
            Ok(r) => Box::new(r) as Box<dyn asr::Recognizer>,
            Err(e) => {
                let _ = app.emit("status", ipc::StatusEvent { state: format!("error: {e}") });
                *running.lock().unwrap() = false;
                return;
            }
        };
        let capture = Box::new(audio::microphone::Microphone::new()) as Box<dyn audio::AudioCapture>;
        let app2 = app.clone();
        let _ = session::run_pipeline(capture, recognizer, 16000, 1.5, move |text| {
            let _ = app2.emit("partial", ipc::PartialEvent { text });
        });
        *running.lock().unwrap() = false;
        let _ = app.emit("status", ipc::StatusEvent { state: "stopped".into() });
    });
    Ok(())
}

#[tauri::command]
fn stop_recording(state: State<AppState>) {
    // 骨架：置 false 并依赖关闭设备停止；完整停止逻辑在后续计划完善。
    *state.running.lock().unwrap() = false;
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![start_recording, stop_recording])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

> 注：`stop_recording` 在 P1 是占位（真正的可中断停止依赖给 `Microphone` 加停止信号，留待 P2/P3 完善）。P1 验收以"开始→出字→关窗"为准。

- [ ] **Step 6: 确认编译**

Run: `cd src-tauri && cargo build`
Expected: 编译成功。

- [ ] **Step 7: 提交**

```bash
git add -A
git commit -m "feat(session): wire capture->resample->buffer->recognizer pipeline with Tauri commands"
```

---

## Task 8: 最小录制界面（Svelte）

**Files:**
- Create: `src/lib/events.ts`
- Modify: `src/App.svelte`

**Interfaces:**
- Consumes: Tauri 命令 `start_recording`/`stop_recording`；事件 `partial`/`status`
- Produces: 可操作的 UI

- [ ] **Step 1: 事件封装**

`src/lib/events.ts`:

```ts
import { listen } from "@tauri-apps/api/event";

export type PartialEvent = { text: string };
export type StatusEvent = { state: string };

export function onPartial(cb: (e: PartialEvent) => void) {
  return listen<PartialEvent>("partial", (ev) => cb(ev.payload));
}
export function onStatus(cb: (e: StatusEvent) => void) {
  return listen<StatusEvent>("status", (ev) => cb(ev.payload));
}
```

- [ ] **Step 2: 录制界面**

`src/App.svelte`:

```svelte
<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";
  import { onPartial, onStatus } from "./lib/events";

  let status = "idle";
  let transcript = "";

  onMount(() => {
    const u1 = onPartial((e) => { transcript = e.text; });
    const u2 = onStatus((e) => { status = e.state; });
    return () => { u1.then((f) => f()); u2.then((f) => f()); };
  });

  async function start() { await invoke("start_recording"); }
  async function stop() { await invoke("stop_recording"); }
</script>

<main class="container">
  <h1>实时转写（骨架）</h1>
  <div class="row">
    <button on:click={start} disabled={status === "recording"}>开始录音</button>
    <button on:click={stop} disabled={status !== "recording"}>停止</button>
    <span class="status">状态：{status}</span>
  </div>
  <pre class="transcript">{transcript || "（开始说话…）"}</pre>
</main>

<style>
  .container { padding: 1.5rem; font-family: -apple-system, system-ui, sans-serif; }
  .row { display: flex; gap: 0.75rem; align-items: center; margin: 1rem 0; }
  .status { color: #666; }
  .transcript { white-space: pre-wrap; min-height: 8rem; background: #f5f5f7;
    border-radius: 8px; padding: 1rem; font-size: 1.1rem; line-height: 1.6; }
</style>
```

- [ ] **Step 3: 手动端到端冒烟**

```bash
npm run tauri dev
```

操作：首次运行授予麦克风权限 → 点"开始录音" → 对着麦克风说"hello world 你好" → 观察转写区在 1–2 秒后出现文字并随说话刷新 → 关闭窗口。

Expected: 状态变为 `recording`；说话后转写区出现近实时文本；控制台无致命错误。

> 若首次因权限被拒，到「系统设置 → 隐私与安全性 → 麦克风」授权后重试。

- [ ] **Step 4: 提交**

```bash
git add -A
git commit -m "feat(ui): minimal recording view with live transcript"
```

---

## Self-Review（计划对照 spec / P1 范围）

**1. 覆盖检查（P1 范围内）：** Tauri+Rust 脚手架(T1) ✓；`AudioCapture` 隔离边界(T2) ✓；16kHz 单声道归一(T2 `to_mono`/T3 重采样) ✓；快流窗口(T4) ✓；本地 Whisper 中英混合识别(T5，`language:"auto"`) ✓；macOS 麦克风(T6) ✓；管线编排 + `partial`/`status` 事件契约(T7，与 spec 第 4 节事件名一致) ✓；最小录制 UI(T8) ✓。P1 范围**不含**系统声音/diarization/存储/模型下载 UI（分属 P2–P5），符合既定拆分。

**2. 占位符扫描：** 无 TBD/TODO 式需求；`stop_recording` 与 `microphone.rs` 的非 f32 分支已显式标注为"后续计划完善"，属有意的范围边界而非空缺。

**3. 类型一致性：** `AudioFrame`/`AudioCapture`(T2) 在 T6/T7/mock 中使用一致；`Recognizer::recognize(&[f32]) -> Transcript`(T5) 在 T7 调用一致；`AccumulatingBuffer::new/push/drain`(T4) 在 T7 使用一致；`PartialEvent.text`/`StatusEvent.state`(T7) 与前端 `events.ts`(T8) 字段一致。

**4. 外部依赖风险：** `sherpa-rs` 与 Tauri 2 API 名称可能随版本变化，已在 Global Constraints 与 T5 给出"对外接口不变、按 docs 微调实现"的处理方式。
