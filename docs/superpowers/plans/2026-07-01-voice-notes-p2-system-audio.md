# P2 系统声音采集 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 macOS 系统声音采集（会议对方的声音），与麦克风做成两个音频源，一起喂进现有识别管线，转写按来源（我/对方）标记显示。

**Architecture:** 两路 capture（cpal 麦克风 + ScreenCaptureKit 系统声音）各自 VAD 按句分段，完成句进"不丢"的 finals 队列、当前句写"每源覆盖式 partial 槽"，汇入单个 SenseVoice 识别 worker 串行消费并 emit 带 source 的事件。识别彻底移出采集回调；`RecordingHandle` 提供真正的停止；模型就绪后才发 recording 状态。系统声音未授权屏幕录制时降级为仅麦克风。

**Tech Stack:** Rust + Tauri 2 + SvelteKit(Svelte 5) + sherpa-rs 0.6（SenseVoice/Silero VAD）+ screencapturekit 8（macOS）+ crossbeam-channel 0.5 + cpal 0.15。

## Global Constraints

- 全程离线、音频不上传（模型下载除外）；**会议进行中不丢内容**（finals 永不丢弃）。
- 中英混合识别；按句（Silero VAD）分段；SenseVoice `language="auto"`、`use_itn=true`。
- macOS 优先，架构跨平台：所有平台相关代码隔离在 `AudioCapture` 实现；系统声音代码 `#[cfg(target_os = "macos")]` 门控，非 macOS 退化为仅麦克风且仍可编译。
- crate lib 名 `app_lib`；模型不入库（gitignored，`scripts/fetch_models.sh` 下载）。
- 依赖版本：`screencapturekit = { version = "8", features = ["macos_13_0"] }`（仅 macOS target）、`sherpa-rs 0.6`、`crossbeam-channel 0.5`、`cpal 0.15`。
- IPC 来源字符串固定为 `"mic"` / `"system"`；`status.system_audio` ∈ `{"on","denied","unavailable"}`。
- Rust 测试工作目录为 `src-tauri/`（`cargo test` 在此运行）；前端质量门为 `npm run check` + `npm run build`。
- 每次 commit 消息以下面这行结尾：
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

## Task 概览与依赖

1. **Task 1** — screencapturekit 依赖 + 系统声音 spike（去风险，忽略式探针）
2. **Task 2** — `Source` 枚举 + `planar_to_mono` 降混助手（TDD）
3. **Task 3** — `SystemAudioCapture` 实现 `AudioCapture`（SCKit 胶水，依赖 T1、T2）
4. **Task 4** — `FinalJob`/`PartialJob` + `run_segment_worker`（TDD，依赖 T2）
5. **Task 5** — `run_asr_worker`（TDD，依赖 T4 类型）
6. **Task 6** — `start_session` + `RecordingHandle` + `SessionStart`（TDD，依赖 T4、T5）
7. **Task 7** — ipc 字段 + lib.rs 接线（降级/真停止/状态时机），删除 `run_pipeline`（依赖 T3、T6）
8. **Task 8** — 前端：事件类型 + 源徽章 + 两条 partial + 降级横幅（依赖 T7 的 ipc 形状）

每个任务结束时整棵树可编译、既有测试仍绿。T1–T6 以"新增模块 + 保留旧 `run_pipeline`"的方式并存，直到 T7 一次性切换并删除旧路径。

---

## Task 1: screencapturekit 依赖 + 系统声音 spike

**目的：** 打掉最大不确定性——用 `screencapturekit` 牌能否从 Rust 拿到系统声音的 f32 帧（含权限流），并弄清交付的采样率/声道/planar-or-interleaved。这是**探针任务（spike）**，不走 TDD；产物是一个忽略式（`#[ignore]`）手动探针 + 一份 findings 记录。

**Files:**
- Modify: `src-tauri/Cargo.toml`（加 macOS 专属依赖）
- Create: `src-tauri/tests/sckit_probe.rs`
- Create: `.superpowers/sdd/p2-sckit-spike.md`（findings + 决策）

**Interfaces:**
- Consumes: 无
- Produces: 供 Task 3 使用的既定事实——crate API 名（`SCStreamConfiguration`/`SCContentFilter`/`SCStream`/`SCStreamOutputTrait::did_output_sample_buffer`/`CMSampleBuffer::format_description`/`CMSampleBuffer::get_audio_buffer_list`）、系统音频的采样率/声道数/样本格式（f32 planar 还是 interleaved）。

- [ ] **Step 1: 加依赖（仅 macOS target，保持跨平台可编译）**

在 `src-tauri/Cargo.toml` 末尾追加：

```toml
[target.'cfg(target_os = "macos")'.dependencies]
screencapturekit = { version = "8", features = ["macos_13_0"] }
```

- [ ] **Step 2: 写探针（忽略式，手动运行）**

创建 `src-tauri/tests/sckit_probe.rs`：

```rust
//! 系统声音采集 spike：手动运行，验证能否拿到系统音频回调并打印其格式。
//! 运行：cd src-tauri && cargo test --test sckit_probe -- --ignored --nocapture
//! 前置：系统设置授予本终端/应用"屏幕录制"权限；并在别处播放声音。
#![cfg(target_os = "macos")]

use screencapturekit::prelude::*;
use std::sync::mpsc;
use std::time::Duration;

#[test]
#[ignore = "manual: 需屏幕录制授权 + 正在播放的系统声音"]
fn probe_system_audio() {
    let (tx, rx) = mpsc::channel::<String>();

    struct Handler(mpsc::Sender<String>);
    impl SCStreamOutputTrait for Handler {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _t: SCStreamOutputType) {
            // 目标：打印采样率/声道/缓冲个数/首缓冲字节数，判定 f32 与 planar/interleaved。
            let fmt = sample.format_description();
            let list = sample.get_audio_buffer_list();
            let _ = self
                .0
                .send(format!("format_description={fmt:?} | audio_buffer_list={list:?}"));
        }
    }

    let content = SCShareableContent::get().expect("需要屏幕录制授权");
    let display = &content.displays()[0];
    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();
    let config = SCStreamConfiguration::new()
        .with_width(2)
        .with_height(2)
        .with_captures_audio(true)
        .with_sample_rate(48_000)
        .with_channel_count(2);

    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(Handler(tx), SCStreamOutputType::Audio);
    stream.start_capture().expect("start_capture 失败");

    let mut n = 0;
    while let Ok(msg) = rx.recv_timeout(Duration::from_secs(2)) {
        println!("AUDIO#{n}: {msg}");
        n += 1;
        if n >= 20 {
            break;
        }
    }
    stream.stop_capture().ok();
    assert!(n > 0, "未收到系统音频回调——检查屏幕录制授权与是否有声音在播放");
}
```

> 若 crate v8 的具体方法名/构造器与上面不符，运行 `cargo doc -p screencapturekit --open` 校准；**不变的目标**是拿到 f32 样本 + 采样率 + 声道数。API 名的最终事实以本任务实际编译通过的版本为准，并写进 findings 供 Task 3 直接引用。

- [ ] **Step 3: 编译探针（不运行）**

Run: `cd src-tauri && cargo test --test sckit_probe --no-run`
Expected: 编译通过（若 API 名不符则据 `cargo doc` 修正 Step 2 直至通过）。

- [ ] **Step 4: 手动运行探针（人工）**

先在系统设置授予"屏幕录制"，并在浏览器/音乐播放声音，然后：
Run: `cd src-tauri && cargo test --test sckit_probe -- --ignored --nocapture`
Expected: 打印若干 `AUDIO#..` 行；测试 PASS。记录：采样率、声道数、样本是否 f32、AudioBufferList 是 1 个交错缓冲（interleaved）还是 N 个平面缓冲（planar）、`mDataByteSize`/`mNumberChannels`。

- [ ] **Step 5: 写 findings + 决策**

创建 `.superpowers/sdd/p2-sckit-spike.md`，记录：确认的 crate API 名与签名、系统音频格式（采样率/声道/f32/planar-or-interleaved）、从 `CMSampleBuffer` 取字节并解释为 `&[f32]` 的具体方式、未授权时 `SCShareableContent::get()` 的报错形态。**决策门**：能拿到 f32 音频回调 → 继续用牌走 Task 3；若牌无法交付音频 → 停下来上报，改用 Swift 垫片（需另行补一版 Task 3 方案，本计划其余任务不受影响，因为都在 `AudioCapture` trait 后面）。

- [ ] **Step 6: Commit**

```bash
cd /Users/teemo/workspace-soul/voice-notes
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tests/sckit_probe.rs .superpowers/sdd/p2-sckit-spike.md
git commit -m "spike(p2): screencapturekit 依赖 + 系统声音探针与 findings

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `Source` 枚举 + `planar_to_mono` 降混助手

**目的：** 引入来源标记类型，以及把 SCKit 的多声道音频降成单声道的纯函数（可确定性测试）。不触碰任何既有路径。

**Files:**
- Modify: `src-tauri/src/audio/mod.rs`（加 `Source` 枚举与其测试）
- Create: `src-tauri/src/audio/system.rs`（先只放纯函数 `planar_to_mono` + 测试；`SystemAudioCapture` 留到 Task 3）
- Modify: `src-tauri/src/audio/mod.rs`（`#[cfg(target_os = "macos")] pub mod system;`）

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub enum Source { Mic, System }`，`impl Source { pub fn as_str(&self) -> &'static str }` → `"mic"`/`"system"`；`#[derive(Debug, Clone, Copy, PartialEq, Eq)]`
  - `pub fn planar_to_mono(channels: &[Vec<f32>]) -> Vec<f32>`（各声道等长；按样本平均；空→空；单声道→克隆）

- [ ] **Step 1: 写 `Source` 的失败测试**

在 `src-tauri/src/audio/mod.rs` 的 `#[cfg(test)] mod tests` 内追加：

```rust
#[test]
fn source_as_str_maps_to_ipc_strings() {
    assert_eq!(Source::Mic.as_str(), "mic");
    assert_eq!(Source::System.as_str(), "system");
}
```

- [ ] **Step 2: 运行看它失败**

Run: `cd src-tauri && cargo test --lib source_as_str_maps_to_ipc_strings`
Expected: 编译失败（`Source` 未定义）。

- [ ] **Step 3: 实现 `Source`**

在 `src-tauri/src/audio/mod.rs` 顶部（`AudioFrame` 定义之后）加入：

```rust
/// 音频来源标记：接线时确定，随 Job/事件流转。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Mic,
    System,
}

impl Source {
    /// IPC 事件里用的稳定字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Mic => "mic",
            Source::System => "system",
        }
    }
}
```

- [ ] **Step 4: 运行看它通过**

Run: `cd src-tauri && cargo test --lib source_as_str_maps_to_ipc_strings`
Expected: PASS。

- [ ] **Step 5: 写 `planar_to_mono` 的失败测试**

创建 `src-tauri/src/audio/system.rs`：

```rust
//! macOS 系统声音采集（ScreenCaptureKit）。本文件仅 macOS 编译。
//! Task 2 先放纯函数 planar_to_mono；Task 3 加 SystemAudioCapture。

/// 把多个声道平面（planar：每声道一段等长 f32）按样本平均成单声道。
/// 空输入 → 空；单声道 → 克隆；多声道以最短声道长度为准，避免越界。
pub fn planar_to_mono(channels: &[Vec<f32>]) -> Vec<f32> {
    match channels.len() {
        0 => Vec::new(),
        1 => channels[0].clone(),
        n => {
            let len = channels.iter().map(|c| c.len()).min().unwrap_or(0);
            (0..len)
                .map(|i| channels.iter().map(|c| c[i]).sum::<f32>() / n as f32)
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planar_stereo_averages_per_sample() {
        let ch = vec![vec![1.0, 3.0, 5.0], vec![3.0, 5.0, 7.0]];
        assert_eq!(planar_to_mono(&ch), vec![2.0, 4.0, 6.0]);
    }

    #[test]
    fn planar_empty_and_mono() {
        assert_eq!(planar_to_mono(&[]), Vec::<f32>::new());
        assert_eq!(planar_to_mono(&[vec![0.1, 0.2]]), vec![0.1, 0.2]);
    }

    #[test]
    fn planar_uses_shortest_channel_len() {
        let ch = vec![vec![2.0, 4.0], vec![6.0]];
        assert_eq!(planar_to_mono(&ch), vec![4.0]); // (2+6)/2；第二样本因越界被裁掉
    }
}
```

在 `src-tauri/src/audio/mod.rs` 顶部模块声明处追加（与现有 `pub mod resample;` 等并列）：

```rust
#[cfg(target_os = "macos")]
pub mod system;
```

- [ ] **Step 6: 运行看它失败→实现已随文件给出→运行看它通过**

Run: `cd src-tauri && cargo test --lib planar_`
Expected: PASS（实现与测试在同文件，直接过；三个用例全绿）。

- [ ] **Step 7: 全量测试确保无回归**

Run: `cd src-tauri && cargo test --lib`
Expected: 既有单测 + 新增全部 PASS。

- [ ] **Step 8: Commit**

```bash
cd /Users/teemo/workspace-soul/voice-notes
git add src-tauri/src/audio/mod.rs src-tauri/src/audio/system.rs
git commit -m "feat(audio): Source 枚举 + planar_to_mono 降混助手

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `SystemAudioCapture` 实现 `AudioCapture`

**目的：** 用 Task 1 确认的 crate API + Task 2 的 `planar_to_mono`，把系统声音做成一个 `AudioCapture`：后台线程持有 SCKit 流，回调里把音频转成单声道 `AudioFrame` 发给 sink；未授权时返回可分类的错误供上层降级。设备相关、无法单测，靠 Task 2 的纯函数测试 + 手动冒烟。

**Files:**
- Modify: `src-tauri/src/audio/system.rs`（加 `SystemAudioCapture`）

**Interfaces:**
- Consumes: `AudioCapture`/`AudioFrame`（`audio/mod.rs`）、`planar_to_mono`（Task 2）
- Produces:
  - `pub struct SystemAudioCapture`，`impl SystemAudioCapture { pub fn new() -> Self }`
  - `impl AudioCapture for SystemAudioCapture`：`start` 成功后经 sink 持续发 `AudioFrame{ samples:<单声道 f32>, sample_rate:<原生,如 48000>, channels:1 }`；未授权 → `Err`，其 `to_string()` 以 `"unauthorized:"` 前缀开头；其它启动失败 → `Err` 以 `"unavailable:"` 前缀开头。`stop` 停止并释放流。

- [ ] **Step 1: 实现 `SystemAudioCapture`（后台线程持流 + 停止通道，镜像 microphone.rs）**

在 `src-tauri/src/audio/system.rs` 顶部补充导入并追加实现（`planar_to_mono` 保留）：

```rust
use super::{AudioCapture, AudioFrame};
use crossbeam_channel::Sender;
use screencapturekit::prelude::*;

/// ScreenCaptureKit 系统声音采集。
///
/// SCKit 的流对象可能是 `!Send`，故与 microphone.rs 一致：在后台线程持有流，
/// 通过停止通道（drop 即断开）通知退出。回调里把音频降成单声道后发给 sink。
pub struct SystemAudioCapture {
    stop_tx: Option<crossbeam_channel::Sender<()>>,
}

impl SystemAudioCapture {
    pub fn new() -> Self {
        Self { stop_tx: None }
    }
}

impl AudioCapture for SystemAudioCapture {
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
        // 权限/内容枚举失败 → 归类为 unauthorized（最常见原因是未授予屏幕录制）。
        let content = SCShareableContent::get()
            .map_err(|e| anyhow::anyhow!("unauthorized: 无法枚举可共享内容（未授权屏幕录制？）: {e}"))?;
        let display = content
            .displays()
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unavailable: 未找到可采集的显示器"))?;
        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();
        let config = SCStreamConfiguration::new()
            .with_width(2)
            .with_height(2)
            .with_captures_audio(true)
            .with_sample_rate(48_000)
            .with_channel_count(2);

        // 回调把 CMSampleBuffer → 单声道 AudioFrame。
        // 注：format_description/get_audio_buffer_list 的确切取样方式以 Task 1 findings 为准；
        // 下面按"planar f32、采样率取自格式"处理，若 findings 判定为 interleaved 则改用 super::to_mono。
        struct Sink {
            tx: Sender<AudioFrame>,
        }
        impl SCStreamOutputTrait for Sink {
            fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _t: SCStreamOutputType) {
                let sample_rate = audio_sample_rate(&sample).unwrap_or(48_000);
                let channels = extract_planar_f32(&sample);
                let mono = super::system::planar_to_mono(&channels);
                if !mono.is_empty() {
                    let _ = self.tx.send(AudioFrame {
                        samples: mono,
                        sample_rate,
                        channels: 1,
                    });
                }
            }
        }

        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(0);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);

        std::thread::spawn(move || {
            let mut stream = SCStream::new(&filter, &config);
            stream.add_output_handler(Sink { tx: sink }, SCStreamOutputType::Audio);
            if let Err(e) = stream.start_capture() {
                let _ = ready_tx.send(Err(format!("unavailable: 无法启动系统声音流: {e}")));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            // 阻塞直到 stop_tx 被 drop（stop() 调用）。
            stop_rx.recv().ok();
            stream.stop_capture().ok();
            // stream 在此 drop。
        });

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(anyhow::anyhow!(e)),
            Err(_) => return Err(anyhow::anyhow!("unavailable: 系统声音线程意外退出")),
        }
        self.stop_tx = Some(stop_tx);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop_tx = None;
    }
}

/// 从 CMSampleBuffer 的格式描述取采样率（Hz）。具体 getter 名以 Task 1 findings 为准。
fn audio_sample_rate(sample: &CMSampleBuffer) -> Option<u32> {
    // 依据 findings 从 sample.format_description() 读取采样率；无法读取时返回 None。
    let _ = sample;
    None
}

/// 从 CMSampleBuffer 取每声道 f32 平面数据。具体 AudioBufferList 遍历以 Task 1 findings 为准。
fn extract_planar_f32(sample: &CMSampleBuffer) -> Vec<Vec<f32>> {
    // 依据 findings 遍历 get_audio_buffer_list() 的各 AudioBuffer，
    // 把 mData/mDataByteSize 解释为 &[f32]，每个 buffer 作为一个声道平面返回。
    let _ = sample;
    Vec::new()
}
```

> 两个 `fn audio_sample_rate` / `fn extract_planar_f32` 是本任务的**唯一实体**：把 Task 1 findings 里确认的 `CMSampleBuffer` 取样代码填进去（读采样率、遍历 AudioBufferList 得到各声道 f32）。若 findings 判定音频是 **interleaved**（单个交错缓冲、`mNumberChannels>1`），则改为取出交错 `&[f32]` 后调用现成的 `super::to_mono(&interleaved, channels)`，并把回调改为发 `channels:1` 的单声道帧。无论哪种，回调对外都只发单声道帧，把格式差异关死在本文件内。

- [ ] **Step 2: 编译**

Run: `cd src-tauri && cargo build`
Expected: 通过（macOS）。若 crate API 名不符，据 Task 1 findings / `cargo doc` 修正。

- [ ] **Step 3: 复跑纯函数测试确保未破坏**

Run: `cd src-tauri && cargo test --lib planar_`
Expected: PASS。

- [ ] **Step 4: 手动冒烟（人工，装置相关）**

临时在 `main` 或一个忽略式测试里 `SystemAudioCapture::new().start(tx)`，播放声音，确认 sink 收到非空单声道帧、`sample_rate` 合理（48000）、`channels==1`。或推迟到 Task 7 整体接线后一起冒烟；此处至少确认"未授权时 `start` 返回 `unauthorized:` 前缀错误、授权后不报错"。记录到 `.superpowers/sdd/p2-sckit-spike.md` 末尾。

- [ ] **Step 5: Commit**

```bash
cd /Users/teemo/workspace-soul/voice-notes
git add src-tauri/src/audio/system.rs .superpowers/sdd/p2-sckit-spike.md
git commit -m "feat(audio): SystemAudioCapture（ScreenCaptureKit 系统声音，降级用可分类错误）

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `FinalJob`/`PartialJob` + `run_segment_worker`

**目的：** 把现有 `run_pipeline` 的"归一/重采样/VAD 分段"抽成单源 worker，去掉内联识别：完成句进 finals 队列、当前句写覆盖式 partial 槽，全部带 `Source` 标记。新增模块，不动旧 `run_pipeline`。

**Files:**
- Modify: `src-tauri/src/session.rs`（加 `FinalJob`/`PartialJob` 类型）
- Create: `src-tauri/src/pipeline/segment_worker.rs`
- Modify: `src-tauri/src/pipeline/mod.rs`（`pub mod segment_worker;`）

**Interfaces:**
- Consumes: `to_mono`/`resample_linear`/`AudioFrame`/`Source`（audio）、`Segmenter`（pipeline）
- Produces:
  - `pub struct FinalJob { pub source: Source, pub samples: Vec<f32> }`（`#[derive(Debug, Clone)]`）
  - `pub struct PartialJob { pub source: Source, pub samples: Vec<f32> }`（`#[derive(Debug, Clone)]`）
  - `pub fn run_segment_worker(source: Source, frame_rx: Receiver<AudioFrame>, target_rate: u32, partial_interval_samples: usize, finals_tx: Sender<FinalJob>, partial_slot: Arc<Mutex<Option<PartialJob>>>, segmenter: Box<dyn Segmenter>)`：阻塞直至 `frame_rx` 关闭；完成句 `finals_tx.send`；当前句节流覆盖 `partial_slot`；结束前 `flush` 尾段。

- [ ] **Step 1: 加 Job 类型**

在 `src-tauri/src/session.rs` 顶部（`use` 之后）加入：

```rust
use crate::audio::Source;

/// 完成句识别任务：进 finals 队列，永不丢弃（保证不丢内容）。
#[derive(Debug, Clone)]
pub struct FinalJob {
    pub source: Source,
    pub samples: Vec<f32>,
}

/// 当前句预览任务：写入每源覆盖式槽，忙时被更新版本覆盖（best-effort）。
#[derive(Debug, Clone)]
pub struct PartialJob {
    pub source: Source,
    pub samples: Vec<f32>,
}
```

- [ ] **Step 2: 写 segment worker 的失败测试**

创建 `src-tauri/src/pipeline/segment_worker.rs`：

```rust
use crate::audio::{resample::resample_linear, to_mono, AudioFrame, Source};
use crate::pipeline::segmenter::Segmenter;
use crate::session::{FinalJob, PartialJob};
use crossbeam_channel::{Receiver, Sender};
use std::sync::{Arc, Mutex};

/// 单源分段 worker：frame_rx 取原生帧 → 归一 16kHz 单声道 → VAD 分段。
/// 完成句 → finals_tx.send(FinalJob)；当前句按采样节流 → 覆盖 partial_slot。
/// frame_rx 关闭（采集停止/结束）后 flush 尾段并返回。
pub fn run_segment_worker(
    source: Source,
    frame_rx: Receiver<AudioFrame>,
    target_rate: u32,
    partial_interval_samples: usize,
    finals_tx: Sender<FinalJob>,
    partial_slot: Arc<Mutex<Option<PartialJob>>>,
    mut segmenter: Box<dyn Segmenter>,
) {
    let mut since_partial: usize = 0;
    for frame in frame_rx.iter() {
        let mono = to_mono(&frame.samples, frame.channels);
        let resampled = resample_linear(&mono, frame.sample_rate, target_rate);
        since_partial += resampled.len();
        segmenter.accept(&resampled);

        for seg in segmenter.take_finished() {
            *partial_slot.lock().unwrap() = None; // 定稿：清过时预览
            let _ = finals_tx.send(FinalJob { source, samples: seg.samples });
            since_partial = 0;
        }

        if since_partial >= partial_interval_samples {
            since_partial = 0;
            if let Some(cur) = segmenter.current_partial() {
                *partial_slot.lock().unwrap() = Some(PartialJob { source, samples: cur });
            }
        }
    }

    // 采集结束：尾段定稿
    segmenter.flush();
    for seg in segmenter.take_finished() {
        *partial_slot.lock().unwrap() = None;
        let _ = finals_tx.send(FinalJob { source, samples: seg.samples });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::mock::MockCapture;
    use crate::audio::AudioCapture;
    use crate::pipeline::segmenter::MockSegmenter;

    #[test]
    fn segment_worker_tags_finals_with_source() {
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(256);
        let (final_tx, final_rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(None));
        let slot2 = slot.clone();

        // 先起 worker（消费者），再让 MockCapture 同步灌帧，避免灌满 256 阻塞。
        let worker = std::thread::spawn(move || {
            run_segment_worker(
                Source::System,
                frx,
                16000,
                4000,
                final_tx,
                slot2,
                Box::new(MockSegmenter::new(8000)),
            );
        });

        let mut cap = MockCapture::from_wav(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample_16k.wav"
        ))
        .expect("fixture");
        cap.start(ftx).expect("start"); // 灌完帧后 ftx 被 drop → frx 关闭
        worker.join().expect("join");

        let finals: Vec<FinalJob> = final_rx.try_iter().collect();
        assert!(!finals.is_empty(), "应至少产出一个 final");
        assert!(finals.iter().all(|f| f.source == Source::System), "全部带 System 标记");
        assert!(finals.iter().all(|f| !f.samples.is_empty()), "final 样本非空");
    }
}
```

在 `src-tauri/src/pipeline/mod.rs` 追加模块声明：

```rust
pub mod segment_worker;
```

- [ ] **Step 3: 运行看它通过（实现随文件给出）**

Run: `cd src-tauri && cargo test --lib segment_worker_tags_finals_with_source`
Expected: PASS。

> partial 槽的行为在 Task 6 的端到端测试里用 `on_partial` 回调断言（此处槽会在 flush 时被清空，单测不易稳定观测），故本任务只断言 finals + 来源标记。

- [ ] **Step 4: 全量测试**

Run: `cd src-tauri && cargo test --lib`
Expected: 全绿（旧 `run_pipeline` 及其测试仍在、仍过）。

- [ ] **Step 5: Commit**

```bash
cd /Users/teemo/workspace-soul/voice-notes
git add src-tauri/src/session.rs src-tauri/src/pipeline/segment_worker.rs src-tauri/src/pipeline/mod.rs
git commit -m "feat(pipeline): FinalJob/PartialJob + 单源 run_segment_worker（识别外置）

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `run_asr_worker`

**目的：** 单识别 worker：串行消费 finals（不丢、优先）+ 空闲时取每源最新 partial（best-effort、覆盖合并）。识别失败的完成句 emit `"[识别失败]"` 占位、worker 不退出。

**Files:**
- Modify: `src-tauri/src/session.rs`（加 `run_asr_worker` + 测试）

**Interfaces:**
- Consumes: `Recognizer`（asr）、`Source`（audio）、`FinalJob`/`PartialJob`（Task 4）
- Produces:
  - `pub fn run_asr_worker(recognizer: Box<dyn Recognizer>, finals_rx: Receiver<FinalJob>, partial_slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)>, on_final: impl FnMut(Source, String), on_partial: impl FnMut(Source, String))`：`finals_rx` 关闭且排干后返回。

- [ ] **Step 1: 写失败测试（finals 全数带源、错误占位、partial best-effort）**

在 `src-tauri/src/session.rs` 的 `#[cfg(test)] mod tests` 内追加（文件已有 `CountingRecognizer`，复用；若在不同 mod，见下自带定义）：

```rust
#[cfg(test)]
mod asr_worker_tests {
    use super::*;
    use crate::asr::{Recognizer, Transcript};
    use crate::audio::Source;
    use std::sync::{Arc, Mutex};

    struct CountingRecognizer;
    impl Recognizer for CountingRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: format!("len={}", s.len()) })
        }
    }

    struct FlakyRecognizer { n: usize }
    impl Recognizer for FlakyRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            self.n += 1;
            if self.n == 1 {
                anyhow::bail!("boom");
            }
            Ok(Transcript { text: format!("len={}", s.len()) })
        }
    }

    #[test]
    fn emits_all_finals_tagged_in_order() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 10] }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.0; 20] }).unwrap();
        drop(tx);

        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        run_asr_worker(
            Box::new(CountingRecognizer),
            rx,
            vec![],
            move |s, t| f2.lock().unwrap().push((s, t)),
            |_, _| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![(Source::Mic, "len=10".into()), (Source::System, "len=20".into())]
        );
    }

    #[test]
    fn failed_final_becomes_placeholder_and_worker_survives() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 3] }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 4] }).unwrap();
        drop(tx);

        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        run_asr_worker(
            Box::new(FlakyRecognizer { n: 0 }),
            rx,
            vec![],
            move |s, t| f2.lock().unwrap().push((s, t)),
            |_, _| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![(Source::Mic, "[识别失败]".into()), (Source::Mic, "len=4".into())]
        );
    }

    #[test]
    fn services_latest_partial_when_idle() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(Some(PartialJob { source: Source::System, samples: vec![0.0; 7] })));
        let partials = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let p2 = partials.clone();
        let slot_for_worker = slot.clone();

        let worker = std::thread::spawn(move || {
            run_asr_worker(
                Box::new(CountingRecognizer),
                rx,
                vec![(Source::System, slot_for_worker)],
                |_, _| {},
                move |s, t| p2.lock().unwrap().push((s, t)),
            );
        });

        // 轮询等待 worker 在空闲分支服务了 partial 槽（有界，避免固定 sleep 假设）。
        let mut serviced = false;
        for _ in 0..200 {
            if !partials.lock().unwrap().is_empty() {
                serviced = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(tx); // 结束 worker
        worker.join().unwrap();

        assert!(serviced, "空闲时应服务 partial 槽");
        assert_eq!(*partials.lock().unwrap(), vec![(Source::System, "len=7".into())]);
        assert!(slot.lock().unwrap().is_none(), "partial 取出后槽应清空");
    }
}
```

- [ ] **Step 2: 运行看它失败**

Run: `cd src-tauri && cargo test --lib asr_worker_tests`
Expected: 编译失败（`run_asr_worker` 未定义）。

- [ ] **Step 3: 实现 `run_asr_worker`**

在 `src-tauri/src/session.rs`（`FinalJob`/`PartialJob` 之后、`run_pipeline` 之前）加入：

```rust
use crate::asr::Recognizer;
use crossbeam_channel::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 单识别 worker：串行消费 finals（不丢、优先），空闲时取每源最新 partial（best-effort）。
/// finals_rx 关闭且排干后返回。识别失败的完成句 emit "[识别失败]" 占位，worker 不退出。
pub fn run_asr_worker(
    mut recognizer: Box<dyn Recognizer>,
    finals_rx: Receiver<FinalJob>,
    partial_slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)>,
    mut on_final: impl FnMut(Source, String),
    mut on_partial: impl FnMut(Source, String),
) {
    loop {
        match finals_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(job) => {
                let text = match recognizer.recognize(&job.samples) {
                    Ok(t) => t.text,
                    Err(_) => "[识别失败]".to_string(),
                };
                on_final(job.source, text);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // 空闲：服务每源最新 partial（取出即清空，只识别最新一版）。
                for (src, slot) in &partial_slots {
                    let job = slot.lock().unwrap().take();
                    if let Some(job) = job {
                        if let Ok(t) = recognizer.recognize(&job.samples) {
                            on_partial(*src, t.text);
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}
```

> `recv_timeout` 会先返回通道里已缓冲的 finals（`Ok`），仅当通道空且所有发送端已 drop 时才 `Disconnected` → 因此 finals 一条不丢。

- [ ] **Step 4: 运行看它通过**

Run: `cd src-tauri && cargo test --lib asr_worker_tests`
Expected: 三个用例全 PASS。

- [ ] **Step 5: 全量测试**

Run: `cd src-tauri && cargo test --lib`
Expected: 全绿。

- [ ] **Step 6: Commit**

```bash
cd /Users/teemo/workspace-soul/voice-notes
git add src-tauri/src/session.rs
git commit -m "feat(session): run_asr_worker（finals 不丢优先 + partial 覆盖合并 + 失败占位）

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: `start_session` + `RecordingHandle` + `SessionStart`

**目的：** 编排层：给定若干 `(Source, capture, segmenter)` + 一个 recognizer，起分段 worker（每源一条）+ 单 ASR worker，接好 finals 通道与每源 partial 槽；返回可真正停止（优雅排干 + join）的句柄，并报告哪些源成功启动、哪些失败（供降级）。

**Files:**
- Modify: `src-tauri/src/session.rs`（加 `SessionStart`/`RecordingHandle`/`start_session` + 测试）

**Interfaces:**
- Consumes: `AudioCapture`/`AudioFrame`/`Source`、`Segmenter`、`Recognizer`、Task 4/5 的类型与函数
- Produces:
  - `pub struct RecordingHandle`（`Send`；`pub fn stop(self)`：停各 capture → join 分段 worker（flush 尾段）→ join ASR worker）
  - `pub struct SessionStart { pub handle: RecordingHandle, pub active: Vec<Source>, pub failed: Vec<(Source, String)> }`
  - `pub fn start_session(sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)>, recognizer: Box<dyn Recognizer>, target_rate: u32, partial_interval_samples: usize, on_final: impl FnMut(Source, String) + Send + 'static, on_partial: impl FnMut(Source, String) + Send + 'static) -> anyhow::Result<SessionStart>`：无任何源能启动 → `Err`。

- [ ] **Step 1: 写失败测试（两源合并带标记 + 真停止）**

在 `src-tauri/src/session.rs` 的测试区追加：

```rust
#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::asr::{Recognizer, Transcript};
    use crate::audio::mock::MockCapture;
    use crate::audio::{AudioCapture, AudioFrame, Source};
    use crate::pipeline::segmenter::MockSegmenter;
    use crossbeam_channel::Sender;
    use std::sync::{Arc, Mutex};

    struct CountingRecognizer;
    impl Recognizer for CountingRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: format!("len={}", s.len()) })
        }
    }

    /// 发完 fixture 帧后保持通道开启，直到 stop() 被调用——用于测真停止与运行中的会话。
    struct IdlingCapture {
        frames: Vec<AudioFrame>,
        stop_tx: Option<Sender<()>>,
    }
    impl IdlingCapture {
        fn from_fixture() -> Self {
            let mut cap = MockCapture::from_wav(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/sample_16k.wav"
            ))
            .expect("fixture");
            // 借 MockCapture 的分帧：把它的帧抽出来（通过一次性 start 到本地通道）。
            let (tx, rx) = crossbeam_channel::unbounded::<AudioFrame>();
            cap.start(tx).unwrap();
            Self { frames: rx.try_iter().collect(), stop_tx: None }
        }
    }
    impl AudioCapture for IdlingCapture {
        fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
            let frames = std::mem::take(&mut self.frames);
            let (stx, srx) = crossbeam_channel::bounded::<()>(0);
            self.stop_tx = Some(stx);
            std::thread::spawn(move || {
                for f in frames {
                    let _ = sink.send(f);
                }
                srx.recv().ok(); // 阻塞直到 stop() drop 掉 stx
                // sink 在此 drop → 分段 worker 的 frame_rx 关闭 → flush 退出
            });
            Ok(())
        }
        fn stop(&mut self) {
            self.stop_tx = None;
        }
    }

    #[test]
    fn merges_two_sources_and_stops_cleanly() {
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();

        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = vec![
            (Source::Mic, Box::new(IdlingCapture::from_fixture()), Box::new(MockSegmenter::new(8000))),
            (Source::System, Box::new(IdlingCapture::from_fixture()), Box::new(MockSegmenter::new(8000))),
        ];

        let start = start_session(
            sources,
            Box::new(CountingRecognizer),
            16000,
            4000,
            move |s, t| f2.lock().unwrap().push((s, t)),
            |_, _| {},
        )
        .expect("start_session");

        assert_eq!(start.active.len(), 2, "两源都应启动");
        assert!(start.failed.is_empty());

        // 等待两源都产出至少一个 final（有界轮询）。
        let mut ok = false;
        for _ in 0..300 {
            let g = finals.lock().unwrap();
            let has_mic = g.iter().any(|(s, _)| *s == Source::Mic);
            let has_sys = g.iter().any(|(s, _)| *s == Source::System);
            if has_mic && has_sys {
                ok = true;
                break;
            }
            drop(g);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        start.handle.stop(); // 真停止：停 capture → join workers → join asr
        assert!(ok, "两源都应产出带标记的 final");
    }

    #[test]
    fn all_sources_fail_returns_err() {
        struct FailingCapture;
        impl AudioCapture for FailingCapture {
            fn start(&mut self, _sink: Sender<AudioFrame>) -> anyhow::Result<()> {
                anyhow::bail!("unauthorized: nope")
            }
            fn stop(&mut self) {}
        }
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> =
            vec![(Source::System, Box::new(FailingCapture), Box::new(MockSegmenter::new(8000)))];
        let r = start_session(sources, Box::new(CountingRecognizer), 16000, 4000, |_, _| {}, |_, _| {});
        assert!(r.is_err(), "无源可启动应返回 Err");
    }
}
```

- [ ] **Step 2: 运行看它失败**

Run: `cd src-tauri && cargo test --lib session_tests`
Expected: 编译失败（`start_session`/`RecordingHandle`/`SessionStart` 未定义）。

- [ ] **Step 3: 实现 `start_session` + `RecordingHandle` + `SessionStart`**

在 `src-tauri/src/session.rs` 加入（用到 `AudioCapture`/`AudioFrame`/`Segmenter`，按需补 `use`）：

```rust
use crate::audio::{AudioCapture, AudioFrame};
use crate::pipeline::segment_worker::run_segment_worker;
use crate::pipeline::segmenter::Segmenter;

/// 一次录制会话的句柄：持两路 capture + 各 worker 的 join 句柄。
pub struct RecordingHandle {
    captures: Vec<Box<dyn AudioCapture>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    asr: Option<std::thread::JoinHandle<()>>,
}

impl RecordingHandle {
    /// 优雅停止：停各 capture（关帧通道）→ 分段 worker flush 尾段后退出并 join
    /// →（其 finals 发送端随之 drop）ASR worker 排干剩余 finals 后退出并 join。
    pub fn stop(mut self) {
        for c in self.captures.iter_mut() {
            c.stop();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        if let Some(a) = self.asr.take() {
            let _ = a.join();
        }
    }
}

/// start_session 的结果：句柄 + 成功启动的源 + 失败的源（含错误串，供降级归类）。
pub struct SessionStart {
    pub handle: RecordingHandle,
    pub active: Vec<Source>,
    pub failed: Vec<(Source, String)>,
}

/// 起会话：每源一条分段 worker + 单 ASR worker，接好 finals 通道与每源 partial 槽。
/// 某源 capture 启动失败 → 跳过该源并记入 failed（用于降级）；无任何源启动 → Err。
#[allow(clippy::too_many_arguments)]
pub fn start_session(
    sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)>,
    recognizer: Box<dyn Recognizer>,
    target_rate: u32,
    partial_interval_samples: usize,
    on_final: impl FnMut(Source, String) + Send + 'static,
    on_partial: impl FnMut(Source, String) + Send + 'static,
) -> anyhow::Result<SessionStart> {
    let (finals_tx, finals_rx) = crossbeam_channel::unbounded::<FinalJob>();
    let mut slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)> = Vec::new();
    let mut captures: Vec<Box<dyn AudioCapture>> = Vec::new();
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut active: Vec<Source> = Vec::new();
    let mut failed: Vec<(Source, String)> = Vec::new();

    for (source, mut capture, segmenter) in sources {
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(256);
        let slot = Arc::new(Mutex::new(None));
        let slot_for_worker = slot.clone();
        let final_tx = finals_tx.clone();
        // 先起 worker（消费者），再启动 capture：兼容同步灌帧的 MockCapture，
        // 且若 capture 启动失败，ftx 在 start 内被 drop → frx 关闭 → worker 立即退出。
        let w = std::thread::spawn(move || {
            run_segment_worker(
                source,
                frx,
                target_rate,
                partial_interval_samples,
                final_tx,
                slot_for_worker,
                segmenter,
            );
        });
        match capture.start(ftx) {
            Ok(()) => {
                active.push(source);
                slots.push((source, slot));
                captures.push(capture);
                workers.push(w);
            }
            Err(e) => {
                failed.push((source, e.to_string()));
                let _ = w.join(); // frx 已关闭，worker 已在退出
            }
        }
    }

    drop(finals_tx); // 仅剩各 worker 持有发送端 → 它们结束后 ASR 才断开

    if active.is_empty() {
        return Err(anyhow::anyhow!("没有可用音频源可启动: {failed:?}"));
    }

    let asr = std::thread::spawn(move || {
        run_asr_worker(recognizer, finals_rx, slots, on_final, on_partial);
    });

    Ok(SessionStart {
        handle: RecordingHandle { captures, workers, asr: Some(asr) },
        active,
        failed,
    })
}
```

- [ ] **Step 4: 运行看它通过**

Run: `cd src-tauri && cargo test --lib session_tests`
Expected: 两个用例 PASS。

- [ ] **Step 5: 全量测试**

Run: `cd src-tauri && cargo test --lib`
Expected: 全绿（旧 `run_pipeline` 及其测试仍在）。

- [ ] **Step 6: Commit**

```bash
cd /Users/teemo/workspace-soul/voice-notes
git add src-tauri/src/session.rs
git commit -m "feat(session): start_session + RecordingHandle（双源汇入、真停止、降级报告）

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: ipc 字段 + lib.rs 接线（降级 / 真停止 / 状态时机），删除 `run_pipeline`

**目的：** 一次性切换到新架构：IPC 事件加 `source` 与 `system_audio`；lib.rs 构建 recognizer + 两路 capture + 两个 VAD，调用 `start_session`；就绪后才发 recording；持 `RecordingHandle` 于 `AppState`；`stop_recording` 真停止；删除旧 `run_pipeline` 及其测试。

**Files:**
- Modify: `src-tauri/src/ipc.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/session.rs`（删除 `run_pipeline` 及其 `#[cfg(test)]` 老测试）

**Interfaces:**
- Consumes: `start_session`/`SessionStart`/`RecordingHandle`（Task 6）、`SystemAudioCapture`（Task 3）、`Microphone`、`SileroSegmenter`、`SenseVoiceRecognizer`、`Source`
- Produces: 无（终端接线）

- [ ] **Step 1: 扩展 IPC 事件结构**

把 `src-tauri/src/ipc.rs` 改为：

```rust
use serde::Serialize;

/// 快流临时文本，事件名 "partial"。
#[derive(Debug, Clone, Serialize)]
pub struct PartialEvent {
    pub source: String, // "mic" | "system"
    pub text: String,
}

/// 录制状态，事件名 "status"。
#[derive(Debug, Clone, Serialize)]
pub struct StatusEvent {
    pub state: String, // "recording" | "stopped" | "error: .."
    /// 系统声音可用性："on" | "denied" | "unavailable"；非录制态可为空串。
    pub system_audio: String,
}

/// 一句定稿文本，事件名 "final"。
#[derive(Debug, Clone, Serialize)]
pub struct FinalEvent {
    pub source: String, // "mic" | "system"
    pub text: String,
}
```

- [ ] **Step 2: 删除旧 `run_pipeline` 及其测试**

在 `src-tauri/src/session.rs` 删除 `pub fn run_pipeline(...) { ... }` 整段，以及其对应的旧 `#[cfg(test)] mod tests { fn pipeline_emits_finals_via_segmenter ... }`（Task 4/5/6 的新测试保留）。

- [ ] **Step 3: 重写 lib.rs（构建两路源、降级、就绪后发 recording、真停止）**

把 `src-tauri/src/lib.rs` 改为：

```rust
mod audio;
pub mod pipeline;
pub mod asr;
mod ipc;
mod session;

use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, State};

use audio::{AudioCapture, Source};
use pipeline::segmenter::Segmenter;
use session::RecordingHandle;

#[derive(Default)]
struct AppState {
    running: Arc<Mutex<bool>>,
    handle: Arc<Mutex<Option<RecordingHandle>>>,
}

fn models_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models")
}

fn new_silero(vad_path: &std::path::Path) -> anyhow::Result<Box<dyn Segmenter>> {
    Ok(Box::new(pipeline::silero::SileroSegmenter::new(vad_path)?) as Box<dyn Segmenter>)
}

/// 从 failed 列表把 System 的失败归类为 "denied"（未授权）/ "unavailable"（其它）。
fn classify_system(active: &[Source], failed: &[(Source, String)]) -> String {
    if active.contains(&Source::System) {
        return "on".into();
    }
    match failed.iter().find(|(s, _)| *s == Source::System) {
        Some((_, msg)) if msg.contains("unauthorized") => "denied".into(),
        Some(_) => "unavailable".into(),
        None => "unavailable".into(),
    }
}

#[tauri::command]
fn start_recording(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    {
        let mut r = state.running.lock().unwrap();
        if *r {
            return Err("已在录制".into());
        }
        *r = true;
    }
    let running = state.running.clone();
    let handle_slot = state.handle.clone();

    std::thread::spawn(move || {
        let fail = |app: &AppHandle, running: &Arc<Mutex<bool>>, msg: String| {
            let _ = app.emit("status", ipc::StatusEvent { state: msg, system_audio: String::new() });
            *running.lock().unwrap() = false;
        };

        // 1) 先建 recognizer（加载模型，耗时）——就绪后才发 recording，消除闪烁。
        let sv_dir = models_dir().join("sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17");
        let recognizer = match asr::sense_voice::SenseVoiceRecognizer::new(&sv_dir) {
            Ok(r) => Box::new(r) as Box<dyn asr::Recognizer>,
            Err(e) => return fail(&app, &running, format!("error: {e}")),
        };

        // 2) 构建两路源（各自 VAD）。麦克风必备；系统声音失败则由 start_session 降级。
        let vad_path = models_dir().join("silero_vad.onnx");
        let mic_seg = match new_silero(&vad_path) {
            Ok(s) => s,
            Err(e) => return fail(&app, &running, format!("error: {e}")),
        };
        let mut sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = vec![(
            Source::Mic,
            Box::new(audio::microphone::Microphone::new()),
            mic_seg,
        )];

        #[cfg(target_os = "macos")]
        {
            match new_silero(&vad_path) {
                Ok(sys_seg) => sources.push((
                    Source::System,
                    Box::new(audio::system::SystemAudioCapture::new()),
                    sys_seg,
                )),
                Err(e) => {
                    let _ = app.emit(
                        "status",
                        ipc::StatusEvent { state: format!("error: {e}"), system_audio: String::new() },
                    );
                }
            }
        }

        // 3) 起会话。emit 回调带 source 字符串。
        let app_f = app.clone();
        let app_p = app.clone();
        let start = session::start_session(
            sources,
            recognizer,
            16000,
            16000,
            move |src, text| {
                let _ = app_f.emit(
                    "final",
                    ipc::FinalEvent { source: src.as_str().into(), text },
                );
            },
            move |src, text| {
                let _ = app_p.emit(
                    "partial",
                    ipc::PartialEvent { source: src.as_str().into(), text },
                );
            },
        );

        match start {
            Ok(start) => {
                let system_audio = classify_system(&start.active, &start.failed);
                *handle_slot.lock().unwrap() = Some(start.handle);
                let _ = app.emit(
                    "status",
                    ipc::StatusEvent { state: "recording".into(), system_audio },
                );
            }
            Err(e) => return fail(&app, &running, format!("error: {e}")),
        }
    });

    Ok(())
}

#[tauri::command]
fn stop_recording(app: AppHandle, state: State<AppState>) {
    // 真停止：取出句柄并优雅停止（停 capture → flush 尾段 → 排干 finals → join）。
    let handle = state.handle.lock().unwrap().take();
    if let Some(h) = handle {
        h.stop();
    }
    *state.running.lock().unwrap() = false;
    let _ = app.emit(
        "status",
        ipc::StatusEvent { state: "stopped".into(), system_audio: String::new() },
    );
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

- [ ] **Step 4: 编译整个 crate**

Run: `cd src-tauri && cargo build`
Expected: 通过。若 `audio`/`Source` 可见性报错，确保 `audio/mod.rs` 中 `Source` 为 `pub`、`lib.rs` 中 `use audio::Source;`。

- [ ] **Step 5: 全量测试（旧 run_pipeline 测试已删，新测试全绿）**

Run: `cd src-tauri && cargo test --lib`
Expected: 全绿；无对 `run_pipeline` 的悬空引用。

- [ ] **Step 6: Commit**

```bash
cd /Users/teemo/workspace-soul/voice-notes
git add src-tauri/src/ipc.rs src-tauri/src/lib.rs src-tauri/src/session.rs
git commit -m "feat(app): 双源接线 + 真停止 + 就绪后发 recording + 系统声音降级；删除旧 run_pipeline

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: 前端——事件类型 + 源徽章 + 两条 partial + 降级横幅

**目的：** 前端消费带 `source` 的事件：单一时间流每条 final 带 我/对方 徽章 + 淡色；两条 live partial（每源一条）；`system_audio != on` 时显示可操作降级横幅（打开系统设置）。无 JS 测试框架，质量门为 `npm run check` + `npm run build` + 手动冒烟。

**Files:**
- Modify: `src/lib/events.ts`
- Modify: `src/routes/+page.svelte`

**Interfaces:**
- Consumes: Task 7 的 IPC 形状（partial/final 带 `source`；status 带 `system_audio`）
- Produces: 无

- [ ] **Step 1: 更新事件类型**

把 `src/lib/events.ts` 改为：

```ts
import { listen } from "@tauri-apps/api/event";

export type Source = "mic" | "system";
export type SystemAudio = "on" | "denied" | "unavailable" | "";

export type PartialEvent = { source: Source; text: string };
export type FinalEvent = { source: Source; text: string };
export type StatusEvent = { state: string; system_audio: SystemAudio };

export function onPartial(cb: (e: PartialEvent) => void) {
  return listen<PartialEvent>("partial", (ev) => cb(ev.payload));
}

export function onStatus(cb: (e: StatusEvent) => void) {
  return listen<StatusEvent>("status", (ev) => cb(ev.payload));
}

export function onFinal(cb: (e: FinalEvent) => void) {
  return listen<FinalEvent>("final", (ev) => cb(ev.payload));
}
```

- [ ] **Step 2: 重写录制视图**

把 `src/routes/+page.svelte` 的 `<script>` 与 `<main>` 改为下述（`<style>` 在 Step 3 增补徽章/横幅样式；保留既有基础样式）：

```svelte
<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { onMount } from "svelte";
  import { onPartial, onStatus, onFinal, type Source, type SystemAudio } from "$lib/events";

  type Line = { source: Source; text: string };

  let status = $state("idle");
  let systemAudio = $state<SystemAudio>("");
  let finals = $state<Line[]>([]);
  let partialMic = $state("");
  let partialSystem = $state("");

  const label = (s: Source) => (s === "mic" ? "我" : "对方");

  onMount(() => {
    const u1 = onPartial((e) => {
      if (e.source === "mic") partialMic = e.text;
      else partialSystem = e.text;
    });
    const u2 = onStatus((e) => {
      status = e.state;
      systemAudio = e.system_audio;
      if (e.state === "recording") {
        finals = [];
        partialMic = "";
        partialSystem = "";
      } else if (e.state === "stopped" || e.state.startsWith("error:")) {
        partialMic = "";
        partialSystem = "";
      }
    });
    const u3 = onFinal((e) => {
      if (e.text.trim()) finals = [...finals, { source: e.source, text: e.text }];
      if (e.source === "mic") partialMic = "";
      else partialSystem = "";
    });
    return () => {
      u1.then((f) => f());
      u2.then((f) => f());
      u3.then((f) => f());
    };
  });

  async function start() {
    try {
      await invoke("start_recording");
    } catch (err) {
      status = `error: ${err}`;
    }
  }
  async function stop() {
    await invoke("stop_recording");
  }
  function isError(s: string) {
    return s.startsWith("error:");
  }
  async function openScreenRecordingSettings() {
    await openUrl(
      "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
    );
  }
</script>

<main class="container">
  <h1>实时转写</h1>
  <div class="row">
    <button onclick={start} disabled={status === "recording"}>开始录音</button>
    <button onclick={stop} disabled={status !== "recording"}>停止</button>
    <span class="status" class:error={isError(status)}>状态：{status}</span>
  </div>

  {#if status === "recording" && systemAudio !== "on" && systemAudio !== ""}
    <div class="banner">
      系统声音不可用（未授权屏幕录制）。仅麦克风在录。
      <button class="link" onclick={openScreenRecordingSettings}>打开系统设置</button>
      <span class="hint">授权后重新开录生效。</span>
    </div>
  {/if}

  <div class="transcript">
    {#each finals as line}
      <p class="final">
        <span class="badge" class:mic={line.source === "mic"} class:system={line.source === "system"}>
          {label(line.source)}
        </span>
        {line.text}
      </p>
    {/each}
    {#if partialMic}
      <p class="partial"><span class="badge mic">我</span>{partialMic}</p>
    {/if}
    {#if partialSystem}
      <p class="partial"><span class="badge system">对方</span>{partialSystem}</p>
    {/if}
    {#if finals.length === 0 && !partialMic && !partialSystem}
      <p class="hint">（开始说话…）</p>
    {/if}
  </div>
</main>
```

- [ ] **Step 3: 增补样式（徽章 + 横幅）**

在 `src/routes/+page.svelte` 的 `<style>` 内追加：

```css
.badge {
  display: inline-block;
  min-width: 2.2em;
  text-align: center;
  font-size: 0.75em;
  font-weight: 600;
  border-radius: 6px;
  padding: 0.05em 0.4em;
  margin-right: 0.4em;
  color: #fff;
}
.badge.mic { background: #396cd8; }
.badge.system { background: #2e9e5b; }

.banner {
  background: #fff4e5;
  border: 1px solid #f0c98a;
  color: #8a5a00;
  border-radius: 8px;
  padding: 0.6rem 0.8rem;
  margin: 0.5rem 0 1rem;
  font-size: 0.95rem;
}
.banner .link {
  background: none;
  border: none;
  color: #396cd8;
  text-decoration: underline;
  cursor: pointer;
  padding: 0 0.2em;
  box-shadow: none;
  font-size: inherit;
}
.banner .hint { color: #a07a3a; }

@media (prefers-color-scheme: dark) {
  .banner { background: #3a2e18; border-color: #6b5426; color: #e8c88a; }
  .banner .hint { color: #c9a866; }
}
```

- [ ] **Step 4: 类型检查**

Run: `npm run check`
Expected: 0 errors（`openUrl` 来自 `@tauri-apps/plugin-opener`，已在依赖内）。

- [ ] **Step 5: 构建前端**

Run: `npm run build`
Expected: 构建成功。

- [ ] **Step 6: 手动冒烟（人工，端到端）**

Run: `npm run tauri dev`
播放会议软件/浏览器声音并说话，确认：`我` 蓝徽章行来自麦克风、`对方` 绿徽章行来自系统声音；两条 partial 分别刷新；停止能真正停下（不再需要重启）。若首次运行系统弹屏幕录制授权，授权后重新开录应见 `对方` 行；未授权时应见橙色降级横幅且麦克风照常出字。记录结果到 `.superpowers/sdd/progress.md`。

- [ ] **Step 7: Commit**

```bash
cd /Users/teemo/workspace-soul/voice-notes
git add src/lib/events.ts src/routes/+page.svelte
git commit -m "feat(ui): 源徽章(我/对方) + 两条 partial + 系统声音降级横幅

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## 收尾（全部任务完成后）

- [ ] 在 `.superpowers/sdd/progress.md` 追加 P2 小节：记录各任务 commit、审查结论、spike findings 指针、手动冒烟结果。
- [ ] 按 `superpowers:finishing-a-development-branch` 决定合并/PR：P2 分支 `p2-system-audio` → master。
- [ ] 人工全链路冒烟（中英混合、mic+system 双源、真停止、降级路径）通过后再合并。

## Self-Review 记录（作者自检）

- **Spec 覆盖**：范围（双源+3 修复、diarization 出）→ T1–T8 全覆盖；数据流（finals 队列/partial 槽/单 ASR worker）→ T4/T5/T6；SCKit（牌+spike+降级）→ T1/T3/T7；IPC（source/system_audio）→ T7；UI（徽章/两 partial/横幅）→ T8；三修复（ASR 外置/真停止/状态时机）→ T4+T5+T6+T7；账本"非 ignored VAD 分段测试 + 提交 fixture"→ 由 T4/T6 用既有 `tests/fixtures/sample_16k.wav` 的非 ignored 测试覆盖。
- **占位符扫描**：无 TBD/TODO 式空洞步骤。T1/T3 中 `CMSampleBuffer` 取样的两个函数体是**受控留白**——明确指向 T1 findings 提供的确切 crate 代码，且给了 interleaved 的备选路径；这是 spike 型任务的固有性质，非计划失败。
- **类型一致性**：`Source`/`FinalJob`/`PartialJob`/`run_segment_worker`/`run_asr_worker`/`start_session`/`SessionStart`/`RecordingHandle` 的签名在 T2/T4/T5/T6/T7 间一致；`source` 字符串统一 `as_str()`→`"mic"/"system"`；`system_audio` 统一 `"on"/"denied"/"unavailable"`。
