# P3 存储与笔记 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 定稿段边录边落盘(JSONL 追加)、崩溃可恢复;笔记列表/详情/改名/删除/导出,应用达到"可日常使用"。

**Architecture:** 管线先补时间戳(`Segment.start` → `FinalJob.start_ms/end_ms` → `FinalEvent`);新增 `store` 模块(`NoteWriter` 录制期追加写 + `NoteStore` 静态读写);`lib.rs` 会话集成(final 先落盘再 emit,停止时 finalize);前端拆三页(`/` 列表、`/record` 录制、`/notes/[id]` 详情)。

**Tech Stack:** 复用 Rust + Tauri 2 + SvelteKit(Svelte 5 runes)+ crossbeam;新增 chrono(本地时间)、tempfile(dev)。

**Spec:** `docs/superpowers/specs/2026-07-03-voice-notes-p3-storage-notes-design.md`

## Global Constraints

- 音频/识别管线不变:16kHz 单声道 f32;SenseVoice 识别;Silero VAD 分段(`max_speech_duration: 15.0`)。
- crate `[lib] name = "app_lib"`;package name `voice-notes`;Rust 测试命令一律 `cargo test --manifest-path src-tauri/Cargo.toml`。
- 事件名小写:`"partial"`、`"final"`、`"status"`,新增 `"storage"`。
- 存储布局(spec §2):`<app_data_dir>/notes/<id>/` 下 `meta.json`(原子写:临时文件+rename)+ `segments.jsonl`(追加写+flush)+ 导出的 `transcript.md/.txt`。`app_data_dir` 对应 identifier `com.teemo.voice-notes`。
- `segments.jsonl` 每行:`{ "seq", "source": "mic"|"system", "text", "start_ms", "end_ms", "speaker": null }`;`speaker` P3 恒为 `null`(P4 预留)。
- meta `state`:`"recording" | "complete"`;`schema_version: 1`。
- **只落定稿段,partial 不落盘;落盘失败绝不中断录制**(段进内存待写队列,后续重试)。
- 时间戳为相对会议开始的毫秒,按各源 16kHz 样本钟换算;展示格式 `hh:mm:ss`。
- 前端验证 = `npm run check` 通过(项目无前端测试基建,不新建)。
- 每个 Task 结束提交一次;commit message 末尾带 `Co-Authored-By` 行(见各 Task)。

---

## 文件结构(P3 结束时)

```
src-tauri/
  Cargo.toml                        # 改:+chrono;+[dev-dependencies] tempfile
  capabilities/default.json         # 改:+opener:allow-reveal-item-in-dir
  src/
    pipeline/
      segmenter.rs                  # 改:Segment.start;MockSegmenter 追踪流内偏移
      silero.rs                     # 改:透传 sherpa SpeechSegment.start
      segment_worker.rs             # 改:FinalJob 带 start_ms/end_ms
    session.rs                      # 改:FinalJob 字段;on_final 回调带时间戳
    ipc.rs                          # 改:FinalEvent+ms;StatusEvent+note_id;新 StorageEvent
    store/
      mod.rs                        # 新:类型(NoteMeta/SegmentRecord/Note/NoteSummary)+原子 meta 写
      writer.rs                     # 新:NoteWriter(create/append_final/finalize/待写队列)
      notes.rs                      # 新:NoteStore(list/load/rename/delete)
      export.rs                     # 新:导出 md/txt(NoteStore::export)
    lib.rs                          # 改:ActiveSession(handle+writer);5 个新 command;storage 事件
src/
  lib/
    events.ts                       # 改:FinalEvent+ms;StatusEvent+note_id;onStorage
    notes.ts                        # 新:invoke 封装(listNotes/getNote/renameNote/deleteNote/exportNote)
  routes/
    +page.svelte                    # 改:笔记列表页(原录制视图迁走)
    record/+page.svelte             # 新:录制视图(自原 +page.svelte 迁移 + 落盘横幅 + 停止跳转)
    notes/[id]/+page.svelte         # 新:详情页(只读 + 改名 + 导出)
```

---

### Task 1: Segment 带流内样本偏移

**Files:**
- Modify: `src-tauri/src/pipeline/segmenter.rs`
- Modify: `src-tauri/src/pipeline/silero.rs`

**Interfaces:**
- Produces: `Segment { samples: Vec<f32>, start: usize }` — `start` 为该段首样本相对该源流开始的偏移(16kHz 单声道计)。Task 2 依赖它换算毫秒。
- `Segmenter` trait 方法签名不变。

- [ ] **Step 1: 改写 MockSegmenter 单测,断言 start**

在 `src-tauri/src/pipeline/segmenter.rs` 的 `mod tests` 中,替换两个既有测试为:

```rust
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
    assert_eq!(segs[0].start, 0, "首段起点为 0");
    // 段产出后，剩余 10 作为当前句
    assert_eq!(s.current_partial().map(|v| v.len()), Some(10));
    // 再来 190 → 累计 200 → 第二段 [100, 200)
    s.accept(&vec![0.0; 190]);
    let segs = s.take_finished();
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0].start, 100, "第二段起点 = 前一段末尾");
    assert_eq!(segs[1].start, 200);
}

#[test]
fn mock_flush_emits_remainder_with_start() {
    let mut s = MockSegmenter::new(100);
    s.accept(&vec![0.0; 130]); // 一段 [0,100) + 残留 30
    let _ = s.take_finished();
    s.flush();
    let segs = s.take_finished();
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].samples.len(), 30);
    assert_eq!(segs[0].start, 100, "尾段起点接在已切段之后");
    assert!(s.current_partial().is_none(), "flush 后无当前句");
}
```

- [ ] **Step 2: 运行测试确认编译失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml segmenter`
Expected: FAIL — `Segment` 无 `start` 字段(编译错误)。

- [ ] **Step 3: 实现 Segment.start 与 MockSegmenter 偏移追踪**

`src-tauri/src/pipeline/segmenter.rs` 顶部结构改为:

```rust
/// 一个已完成的语句音频段（16kHz 单声道 f32）。
#[derive(Debug, Clone)]
pub struct Segment {
    pub samples: Vec<f32>,
    /// 段首样本相对该源音频流开始的偏移（16kHz 单声道样本数）。
    pub start: usize,
}
```

`MockSegmenter` 增加 `current_start: usize`(当前句首样本的流内偏移):

```rust
pub struct MockSegmenter {
    utterance_len: usize,
    current: Vec<f32>,
    current_start: usize,
    finished: Vec<Segment>,
}

impl MockSegmenter {
    pub fn new(utterance_len: usize) -> Self {
        Self {
            utterance_len: utterance_len.max(1),
            current: Vec::new(),
            current_start: 0,
            finished: Vec::new(),
        }
    }
}

impl Segmenter for MockSegmenter {
    fn accept(&mut self, samples: &[f32]) {
        self.current.extend_from_slice(samples);
        while self.current.len() >= self.utterance_len {
            let rest = self.current.split_off(self.utterance_len);
            let seg = std::mem::replace(&mut self.current, rest);
            self.finished.push(Segment { samples: seg, start: self.current_start });
            self.current_start += self.utterance_len;
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
            let len = self.current.len();
            self.finished.push(Segment {
                samples: std::mem::take(&mut self.current),
                start: self.current_start,
            });
            self.current_start += len;
        }
    }
}
```

`src-tauri/src/pipeline/silero.rs` 的 `take_finished` 改为透传 sherpa 的段起点(`SpeechSegment.start: i32`,相对 accept 过的流):

```rust
    fn take_finished(&mut self) -> Vec<Segment> {
        let mut out = Vec::new();
        while !self.vad.is_empty() {
            let seg = self.vad.front();
            out.push(Segment { samples: seg.samples, start: seg.start.max(0) as usize });
            self.vad.pop();
        }
        if !out.is_empty() {
            // 已完成的语句对应的"当前句"已结束，清空预览缓冲。
            self.current.clear();
        }
        out
    }
```

注意:`segment_worker.rs` 与 `session.rs` 此时**不会编译失败**(它们只用 `seg.samples`),Task 1 不动它们。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml segmenter`
Expected: PASS(2 个测试)。再跑全量确认无回归:`cargo test --manifest-path src-tauri/Cargo.toml`
Expected: PASS(`#[ignore]` 的模型集成测试除外)。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/pipeline/segmenter.rs src-tauri/src/pipeline/silero.rs
git commit -m "P3 Task 1: Segment 带流内样本偏移

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: 时间戳贯通 FinalJob → ASR worker → FinalEvent → 前端类型

**Files:**
- Modify: `src-tauri/src/session.rs`
- Modify: `src-tauri/src/pipeline/segment_worker.rs`
- Modify: `src-tauri/src/ipc.rs`
- Modify: `src-tauri/src/lib.rs`(仅 on_final 闭包与 emit)
- Modify: `src/lib/events.ts`

**Interfaces:**
- Consumes: `Segment { samples, start }`(Task 1)。
- Produces:
  - `FinalJob { source: Source, samples: Vec<f32>, start_ms: u64, end_ms: u64 }`
  - `run_asr_worker(..., on_final: impl FnMut(Source, String, u64, u64), ...)`;`start_session` 的 `on_final` 同签名(`(Source, String, u64, u64)`)。Task 6 的落盘闭包依赖此签名。
  - `FinalEvent { source: String, text: String, start_ms: u64, end_ms: u64 }`(事件 `"final"`)
  - TS `FinalEvent = { source: Source; text: string; start_ms: number; end_ms: number }`

- [ ] **Step 1: 改 segment_worker 单测断言毫秒**

`src-tauri/src/pipeline/segment_worker.rs` 测试 `segment_worker_tags_finals_with_source` 末尾追加断言(fixture 16kHz、MockSegmenter(8000) → 每段 500ms):

```rust
        assert!(finals.iter().all(|f| f.source == Source::System), "全部带 System 标记");
        assert!(finals.iter().all(|f| !f.samples.is_empty()), "final 样本非空");
        // 时间戳：MockSegmenter(8000) @16k → 每段 500ms,依次递增
        assert_eq!(finals[0].start_ms, 0);
        assert_eq!(finals[0].end_ms, 500);
        if finals.len() > 1 {
            assert_eq!(finals[1].start_ms, 500);
            assert_eq!(finals[1].end_ms, 1000);
        }
```

- [ ] **Step 2: 运行确认编译失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml segment_worker`
Expected: FAIL — `FinalJob` 无 `start_ms` 字段。

- [ ] **Step 3: 实现贯通**

`src-tauri/src/session.rs`:

```rust
/// 完成句识别任务：进 finals 队列，永不丢弃（保证不丢内容）。
#[derive(Debug, Clone)]
pub struct FinalJob {
    pub source: Source,
    pub samples: Vec<f32>,
    /// 相对该源流开始的毫秒（16kHz 样本钟换算）。
    pub start_ms: u64,
    pub end_ms: u64,
}
```

`run_asr_worker` 签名与 final 分支:

```rust
pub fn run_asr_worker(
    mut recognizer: Box<dyn Recognizer>,
    finals_rx: Receiver<FinalJob>,
    partial_slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)>,
    mut on_final: impl FnMut(Source, String, u64, u64),
    mut on_partial: impl FnMut(Source, String),
) {
```

final 分支结尾 `on_final(job.source, text);` 改为:

```rust
                on_final(job.source, text, job.start_ms, job.end_ms);
```

`start_session` 参数 `on_final: impl FnMut(Source, String) + Send + 'static` 改为:

```rust
    on_final: impl FnMut(Source, String, u64, u64) + Send + 'static,
```

`src-tauri/src/pipeline/segment_worker.rs`:在函数体开头加换算辅助,两处 `FinalJob` 构造(循环内与 flush 后)都改用它:

```rust
    let ms = |samples: usize| samples as u64 * 1000 / target_rate as u64;
```

```rust
        for seg in segmenter.take_finished() {
            *partial_slot.lock().unwrap() = None; // 定稿：清过时预览
            let (start_ms, end_ms) = (ms(seg.start), ms(seg.start + seg.samples.len()));
            if finals_tx
                .send(FinalJob { source, samples: seg.samples, start_ms, end_ms })
                .is_err()
            {
                eprintln!("segment_worker: finals 通道已关闭，一段完成句被丢弃 ({source:?})");
            }
            since_partial = 0;
        }
```

flush 段同样:

```rust
    segmenter.flush();
    for seg in segmenter.take_finished() {
        *partial_slot.lock().unwrap() = None;
        let (start_ms, end_ms) = (ms(seg.start), ms(seg.start + seg.samples.len()));
        if finals_tx
            .send(FinalJob { source, samples: seg.samples, start_ms, end_ms })
            .is_err()
        {
            eprintln!("segment_worker: finals 通道已关闭，一段完成句被丢弃 ({source:?})");
        }
    }
```

`src-tauri/src/ipc.rs`:

```rust
/// 一句定稿文本，事件名 "final"。
#[derive(Debug, Clone, Serialize)]
pub struct FinalEvent {
    pub source: String, // "mic" | "system"
    pub text: String,
    /// 相对会议开始的毫秒。
    pub start_ms: u64,
    pub end_ms: u64,
}
```

`src-tauri/src/lib.rs` 的 on_final 闭包:

```rust
            move |src, text, start_ms, end_ms| {
                let _ = app_f.emit(
                    "final",
                    ipc::FinalEvent { source: src.as_str().into(), text, start_ms, end_ms },
                );
            },
```

`src/lib/events.ts`:

```ts
export type FinalEvent = { source: Source; text: string; start_ms: number; end_ms: number };
```

- [ ] **Step 4: 修复 session.rs 既有测试的回调签名**

session.rs 中所有 `run_asr_worker`/`start_session` 测试调用点,final 回调从 `move |s, t| ...` 改为 `move |s, t, _, _| ...`(4 个 asr_worker 测试 + 2 个 session 测试;`|_, _| {}` 形式的 final 回调改为 `|_, _, _, _| {}`)。断言内容不变。

另在 `emits_all_finals_tagged_in_order` 中把发送的两个 job 改为带时间戳并断言透传:

```rust
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 10], start_ms: 0, end_ms: 625 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.0; 20], start_ms: 625, end_ms: 1875 }).unwrap();
```

收集元组扩为 `(Source, String, u64, u64)`,断言:

```rust
        assert_eq!(
            *finals.lock().unwrap(),
            vec![
                (Source::Mic, "len=10".into(), 0, 625),
                (Source::System, "len=20".into(), 625, 1875)
            ]
        );
```

其余测试构造 `FinalJob` 处补 `start_ms: 0, end_ms: 0`。

- [ ] **Step 5: 运行全量测试与前端检查**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: PASS。
Run: `npm run check`
Expected: 0 errors。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/session.rs src-tauri/src/pipeline/segment_worker.rs src-tauri/src/ipc.rs src-tauri/src/lib.rs src/lib/events.ts
git commit -m "P3 Task 2: final 事件贯通 start_ms/end_ms 时间戳

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: store 类型 + NoteWriter(追加落盘、原子 meta、待写队列)

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Create: `src-tauri/src/store/mod.rs`
- Create: `src-tauri/src/store/writer.rs`
- Modify: `src-tauri/src/lib.rs`(仅加 `mod store;`)

**Interfaces:**
- Produces(后续 Task 依赖的精确签名):
  - `store::NoteMeta { schema_version: u32, id: String, title: String, started_at: String, ended_at: Option<String>, state: String }`(Serialize/Deserialize/Clone/Debug/PartialEq)
  - `store::SegmentRecord { seq: u64, source: String, text: String, start_ms: u64, end_ms: u64, speaker: Option<String> }`(同上派生)
  - `store::Note { meta: NoteMeta, segments: Vec<SegmentRecord>, skipped_lines: u32 }`(Serialize/Clone/Debug)
  - `store::NoteSummary { id: String, title: String, started_at: String, duration_secs: Option<u64>, state: String }`(Serialize/Clone/Debug)
  - `store::write_meta_atomic(note_dir: &Path, meta: &NoteMeta) -> anyhow::Result<()>`(crate 内可见)
  - `store::writer::NoteWriter`:
    - `create(notes_dir: &Path, now: chrono::DateTime<chrono::Local>) -> anyhow::Result<NoteWriter>`
    - `note_id(&self) -> &str` / `has_content(&self) -> bool` / `dir(&self) -> &Path`
    - `append_final(&mut self, source: &str, text: &str, start_ms: u64, end_ms: u64) -> anyhow::Result<()>`
    - `finalize(&mut self, now: chrono::DateTime<chrono::Local>) -> anyhow::Result<()>`

- [ ] **Step 1: 加依赖**

`src-tauri/Cargo.toml` 的 `[dependencies]` 追加一行,并新增 `[dev-dependencies]` 节(放在 `[target.'cfg(target_os = "macos")'.dependencies]` 之前):

```toml
chrono = "0.4"
```

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: 写 NoteWriter 失败测试**

创建 `src-tauri/src/store/mod.rs`:

```rust
pub mod writer;

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;

/// 一场会议的元数据，存 meta.json（原子写）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteMeta {
    pub schema_version: u32,
    pub id: String,
    pub title: String,
    /// RFC3339 本地时区；meta 损坏兜底时可为空串。
    pub started_at: String,
    pub ended_at: Option<String>,
    /// "recording" | "complete"
    pub state: String,
}

/// 一条定稿段，存 segments.jsonl（每段一行）。speaker 为 P4 说话人区分预留。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentRecord {
    pub seq: u64,
    pub source: String, // "mic" | "system"
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: Option<String>,
}

/// 一场会议的完整内容（详情页 / 导出用）。
#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub meta: NoteMeta,
    pub segments: Vec<SegmentRecord>,
    /// load 时因损坏被跳过的行数（>0 时前端可提示）。
    pub skipped_lines: u32,
}

/// 列表项。state 除 meta 的两态外，command 层会把当前活动会话改写为 "active"。
#[derive(Debug, Clone, Serialize)]
pub struct NoteSummary {
    pub id: String,
    pub title: String,
    pub started_at: String,
    pub duration_secs: Option<u64>,
    pub state: String,
}

/// meta.json 原子写：先写 meta.json.tmp 再 rename，任何时刻磁盘上的 meta.json 都完整。
pub(crate) fn write_meta_atomic(note_dir: &Path, meta: &NoteMeta) -> anyhow::Result<()> {
    let tmp = note_dir.join("meta.json.tmp");
    let json = serde_json::to_string_pretty(meta)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, note_dir.join("meta.json"))?;
    Ok(())
}
```

创建 `src-tauri/src/store/writer.rs`,先只写测试骨架能编译的最小声明——按 TDD 直接写完整测试(实现留空会编译失败,故本任务测试与实现在同文件,先写测试再实现):

`writer.rs` 末尾测试模块:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NoteMeta;

    fn now() -> chrono::DateTime<chrono::Local> {
        chrono::Local::now()
    }

    fn read_meta(dir: &std::path::Path) -> NoteMeta {
        serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap()
    }

    fn read_lines(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("segments.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn create_writes_recording_meta_and_unique_id() {
        let tmp = tempfile::tempdir().unwrap();
        let w1 = NoteWriter::create(tmp.path(), now()).unwrap();
        let meta = read_meta(w1.dir());
        assert_eq!(meta.state, "recording");
        assert_eq!(meta.schema_version, crate::store::SCHEMA_VERSION);
        assert_eq!(meta.id, w1.note_id());
        assert!(meta.ended_at.is_none());
        assert!(!meta.started_at.is_empty());
        assert!(meta.title.ends_with("会议"));
        // 同秒再建：id 加后缀不冲突
        let w2 = NoteWriter::create(tmp.path(), now()).unwrap();
        assert_ne!(w1.note_id(), w2.note_id());
    }

    #[test]
    fn append_and_finalize_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        assert!(!w.has_content());
        w.append_final("mic", "第一句", 0, 1500).unwrap();
        w.append_final("system", "second", 1500, 3000).unwrap();
        assert!(w.has_content());

        let lines = read_lines(w.dir());
        assert_eq!(lines.len(), 2);
        let r0: crate::store::SegmentRecord = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(r0.seq, 0);
        assert_eq!(r0.source, "mic");
        assert_eq!(r0.text, "第一句");
        assert_eq!((r0.start_ms, r0.end_ms), (0, 1500));
        assert_eq!(r0.speaker, None);
        let r1: crate::store::SegmentRecord = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(r1.seq, 1);

        w.finalize(now()).unwrap();
        let meta = read_meta(w.dir());
        assert_eq!(meta.state, "complete");
        assert!(meta.ended_at.is_some());
    }

    #[test]
    fn write_failure_queues_and_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let dir = w.dir().to_path_buf();

        // 模拟句柄失效 + 目录消失：追加必须失败但段保留在待写队列
        w.file = None;
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(w.append_final("mic", "丢不得", 0, 1000).is_err());

        // 目录恢复后，下一次追加把队列里的段一并补写
        std::fs::create_dir_all(&dir).unwrap();
        w.append_final("mic", "第二句", 1000, 2000).unwrap();
        let lines = read_lines(&dir);
        assert_eq!(lines.len(), 2, "失败段重试补写，一段不丢");
        let r0: crate::store::SegmentRecord = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(r0.text, "丢不得");
        assert_eq!(r0.seq, 0);

        // finalize 重建 meta（此前随目录被删）
        w.finalize(now()).unwrap();
        assert_eq!(read_meta(&dir).state, "complete");
    }
}
```

`src-tauri/src/lib.rs` 第 5 行 `mod session;` 之后加:

```rust
mod store;
```

- [ ] **Step 3: 运行确认编译失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml store::`
Expected: FAIL — `NoteWriter` 未定义。

- [ ] **Step 4: 实现 NoteWriter**

`src-tauri/src/store/writer.rs` 测试模块之前:

```rust
use super::{write_meta_atomic, NoteMeta, SegmentRecord, SCHEMA_VERSION};
use chrono::{DateTime, Local};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 录制期落盘器：meta 原子写 + segments.jsonl 追加写。
/// 写失败时段进内存待写队列（不设上界：内存丢内容比 OOM 更早违背原则，
/// 几小时会议的文本量级仅 MB），后续 append/finalize 先重试队列。
pub struct NoteWriter {
    dir: PathBuf,
    meta: NoteMeta,
    /// segments.jsonl 追加句柄；写失败置 None，重试时按需重开。
    pub(super) file: Option<File>,
    next_seq: u64,
    pending: VecDeque<String>,
}

impl NoteWriter {
    /// 在 notes_dir 下建会议文件夹（id = 本地时间 YYYYmmdd-HHMMSS，同秒冲突加 -2/-3 后缀），
    /// 写入 state=recording 的 meta，打开 segments.jsonl。
    pub fn create(notes_dir: &Path, now: DateTime<Local>) -> anyhow::Result<Self> {
        std::fs::create_dir_all(notes_dir)?;
        let base = now.format("%Y%m%d-%H%M%S").to_string();
        let mut id = base.clone();
        let mut n = 1;
        let dir = loop {
            let d = notes_dir.join(&id);
            if !d.exists() {
                break d;
            }
            n += 1;
            id = format!("{base}-{n}");
        };
        std::fs::create_dir(&dir)?;
        let meta = NoteMeta {
            schema_version: SCHEMA_VERSION,
            id: id.clone(),
            title: now.format("%Y-%m-%d %H:%M 会议").to_string(),
            started_at: now.to_rfc3339(),
            ended_at: None,
            state: "recording".into(),
        };
        write_meta_atomic(&dir, &meta)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("segments.jsonl"))?;
        Ok(Self { dir, meta, file: Some(file), next_seq: 0, pending: VecDeque::new() })
    }

    pub fn note_id(&self) -> &str {
        &self.meta.id
    }

    /// 是否已产生过任何定稿段（含仍在待写队列中的）。
    pub fn has_content(&self) -> bool {
        self.next_seq > 0
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 追加一条定稿段。失败时段留在待写队列并返回 Err（调用方发 storage 降级事件），
    /// 后续调用先重试队列，保证顺序与 seq 单调。
    pub fn append_final(
        &mut self,
        source: &str,
        text: &str,
        start_ms: u64,
        end_ms: u64,
    ) -> anyhow::Result<()> {
        let rec = SegmentRecord {
            seq: self.next_seq,
            source: source.into(),
            text: text.into(),
            start_ms,
            end_ms,
            speaker: None,
        };
        self.next_seq += 1;
        let line = serde_json::to_string(&rec)?;
        self.pending.push_back(line);
        self.flush_pending()
    }

    /// 收尾：补写待写队列 → meta 置 complete。队列仍写不出时也更新 meta，
    /// 并把队列错误上抛（内容此时只可能因磁盘持续故障而丢，调用方告警）。
    pub fn finalize(&mut self, now: DateTime<Local>) -> anyhow::Result<()> {
        let flush_result = self.flush_pending();
        self.meta.ended_at = Some(now.to_rfc3339());
        self.meta.state = "complete".into();
        write_meta_atomic(&self.dir, &self.meta)?;
        flush_result
    }

    fn flush_pending(&mut self) -> anyhow::Result<()> {
        while let Some(line) = self.pending.front() {
            if self.file.is_none() {
                self.file = Some(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(self.dir.join("segments.jsonl"))
                        .map_err(|e| anyhow::anyhow!("重开 segments.jsonl 失败: {e}"))?,
                );
            }
            let file = self.file.as_mut().unwrap();
            let res = file
                .write_all(line.as_bytes())
                .and_then(|_| file.write_all(b"\n"))
                .and_then(|_| file.flush());
            if let Err(e) = res {
                // 句柄可能已坏（如卷被卸载），丢弃句柄，下次重开重试。
                // 半行写入的风险由读取端容忍（load 跳过损坏行）。
                self.file = None;
                anyhow::bail!("写 segments.jsonl 失败: {e}");
            }
            self.pending.pop_front();
        }
        Ok(())
    }
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml store::`
Expected: PASS(3 个测试)。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/store/ src-tauri/src/lib.rs
git commit -m "P3 Task 3: store 类型 + NoteWriter 追加落盘（原子 meta、待写队列）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: NoteStore(list / load / rename / delete,损坏容忍)

**Files:**
- Create: `src-tauri/src/store/notes.rs`
- Modify: `src-tauri/src/store/mod.rs`(挂模块 + re-export)

**Interfaces:**
- Consumes: Task 3 的类型与 `NoteWriter`(测试里用它造数据)。
- Produces:
  - `store::NoteStore::new(notes_dir: PathBuf) -> NoteStore`
  - `list(&self) -> Vec<NoteSummary>`(started_at 倒序;meta 损坏项兜底;`recording` 态时长取 jsonl 最后可解析行的 `end_ms/1000`)
  - `load(&self, id: &str) -> anyhow::Result<Note>`(跳过损坏行,计入 `skipped_lines`)
  - `rename(&self, id: &str, title: &str) -> anyhow::Result<()>`
  - `delete(&self, id: &str) -> anyhow::Result<()>`
  - `pub(super) note_dir(&self, id: &str) -> anyhow::Result<PathBuf>`(id 合法性校验,Task 5 导出复用)

- [ ] **Step 1: 写测试**

`src-tauri/src/store/notes.rs` 末尾:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::writer::NoteWriter;

    fn now() -> chrono::DateTime<chrono::Local> {
        chrono::Local::now()
    }

    /// 造一场完成的会议，返回 id。
    fn make_note(notes_dir: &std::path::Path, texts: &[&str], finalize: bool) -> String {
        let mut w = NoteWriter::create(notes_dir, now()).unwrap();
        for (i, t) in texts.iter().enumerate() {
            let s = i as u64 * 1000;
            w.append_final(if i % 2 == 0 { "mic" } else { "system" }, t, s, s + 900).unwrap();
        }
        if finalize {
            w.finalize(now()).unwrap();
        }
        w.note_id().to_string()
    }

    #[test]
    fn list_sorts_desc_and_loads_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let id1 = make_note(tmp.path(), &["你好", "hello"], true);
        let id2 = make_note(tmp.path(), &["第二场"], true);
        let store = NoteStore::new(tmp.path().to_path_buf());

        let list = store.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, id2, "倒序：新会议在前");
        assert_eq!(list[1].id, id1);
        assert_eq!(list[0].state, "complete");
        assert!(list[0].duration_secs.is_some());

        let note = store.load(&id1).unwrap();
        assert_eq!(note.segments.len(), 2);
        assert_eq!(note.segments[0].text, "你好");
        assert_eq!(note.segments[1].source, "system");
        assert_eq!(note.skipped_lines, 0);
    }

    #[test]
    fn interrupted_note_lists_with_duration_from_last_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_note(tmp.path(), &["一", "二", "三"], false); // 不 finalize = 崩溃
        let store = NoteStore::new(tmp.path().to_path_buf());
        let list = store.list();
        assert_eq!(list[0].state, "recording", "落盘态保持诚实");
        // 第 3 段 end_ms = 2000+900 → 2 秒
        assert_eq!(list[0].duration_secs, Some(2));
        let note = store.load(&id).unwrap();
        assert_eq!(note.segments.len(), 3, "崩溃前内容完好");
    }

    #[test]
    fn load_skips_truncated_tail_line() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_note(tmp.path(), &["完整句"], false);
        // 模拟崩溃写了半行
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path().join(&id).join("segments.jsonl"))
            .unwrap();
        f.write_all(b"{\"seq\":1,\"source\":\"mic\",\"te").unwrap();
        drop(f);

        let note = NoteStore::new(tmp.path().to_path_buf()).load(&id).unwrap();
        assert_eq!(note.segments.len(), 1);
        assert_eq!(note.skipped_lines, 1);
    }

    #[test]
    fn corrupt_meta_falls_back_to_folder_name() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_note(tmp.path(), &["x"], true);
        std::fs::write(tmp.path().join(&id).join("meta.json"), "not json").unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert!(list[0].title.contains("元数据损坏"));
        // 内容仍可读
        let note = store.load(&id).unwrap();
        assert_eq!(note.segments.len(), 1);
    }

    #[test]
    fn rename_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_note(tmp.path(), &["x"], true);
        let store = NoteStore::new(tmp.path().to_path_buf());
        store.rename(&id, "周会").unwrap();
        assert_eq!(store.load(&id).unwrap().meta.title, "周会");
        assert_eq!(store.list()[0].title, "周会");
        store.delete(&id).unwrap();
        assert!(store.list().is_empty());
        assert!(store.load(&id).is_err());
    }

    #[test]
    fn rejects_path_traversal_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        for bad in ["../x", "a/b", "a\\b", "..", ""] {
            assert!(store.delete(bad).is_err(), "应拒绝非法 id: {bad}");
            assert!(store.load(bad).is_err());
            assert!(store.rename(bad, "t").is_err());
        }
    }

    #[test]
    fn empty_or_missing_notes_dir_lists_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().join("不存在"));
        assert!(store.list().is_empty());
    }
}
```

`src-tauri/src/store/mod.rs` 顶部改为:

```rust
pub mod writer;
mod notes;
pub use notes::NoteStore;
```

- [ ] **Step 2: 运行确认编译失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml store::notes`
Expected: FAIL — `NoteStore` 未定义。

- [ ] **Step 3: 实现 NoteStore**

`src-tauri/src/store/notes.rs` 测试模块之前:

```rust
use super::{write_meta_atomic, Note, NoteMeta, NoteSummary, SegmentRecord, SCHEMA_VERSION};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// 笔记静态读写：目录扫描出列表，逐行解析 jsonl，损坏容忍。
pub struct NoteStore {
    notes_dir: PathBuf,
}

impl NoteStore {
    pub fn new(notes_dir: PathBuf) -> Self {
        Self { notes_dir }
    }

    /// id 合法性校验（防路径穿越）+ 存在性检查。
    pub(super) fn note_dir(&self, id: &str) -> anyhow::Result<PathBuf> {
        if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
            anyhow::bail!("非法笔记 id: {id:?}");
        }
        let dir = self.notes_dir.join(id);
        if !dir.is_dir() {
            anyhow::bail!("笔记不存在: {id}");
        }
        Ok(dir)
    }

    /// 扫描 notes 目录，按 started_at 倒序（RFC3339 同时区字典序即时间序；
    /// meta 损坏项 started_at 为空串，自然沉底）。
    pub fn list(&self) -> Vec<NoteSummary> {
        let Ok(entries) = fs::read_dir(&self.notes_dir) else {
            return Vec::new();
        };
        let mut out: Vec<NoteSummary> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| summarize(&e.path()))
            .collect();
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
        out
    }

    pub fn load(&self, id: &str) -> anyhow::Result<Note> {
        let dir = self.note_dir(id)?;
        let meta = read_meta(&dir).unwrap_or_else(|| fallback_meta(&dir));
        let mut segments = Vec::new();
        let mut skipped_lines = 0u32;
        if let Ok(f) = fs::File::open(dir.join("segments.jsonl")) {
            for line in std::io::BufReader::new(f).lines() {
                let Ok(line) = line else {
                    skipped_lines += 1;
                    continue;
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<SegmentRecord>(&line) {
                    Ok(r) => segments.push(r),
                    Err(_) => skipped_lines += 1,
                }
            }
        }
        Ok(Note { meta, segments, skipped_lines })
    }

    pub fn rename(&self, id: &str, title: &str) -> anyhow::Result<()> {
        let dir = self.note_dir(id)?;
        let mut meta = read_meta(&dir).unwrap_or_else(|| fallback_meta(&dir));
        meta.title = title.to_string();
        write_meta_atomic(&dir, &meta)
    }

    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        let dir = self.note_dir(id)?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}

fn read_meta(dir: &Path) -> Option<NoteMeta> {
    let s = fs::read_to_string(dir.join("meta.json")).ok()?;
    serde_json::from_str(&s).ok()
}

/// meta 损坏/缺失兜底：以文件夹名当 id，标题标注损坏，按 complete 展示。
fn fallback_meta(dir: &Path) -> NoteMeta {
    let id = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    NoteMeta {
        schema_version: SCHEMA_VERSION,
        id: id.clone(),
        title: format!("{id}（元数据损坏）"),
        started_at: String::new(),
        ended_at: None,
        state: "complete".into(),
    }
}

fn summarize(dir: &Path) -> NoteSummary {
    let meta = read_meta(dir).unwrap_or_else(|| fallback_meta(dir));
    let duration_secs = if meta.state == "complete" {
        duration_from_meta(&meta)
    } else {
        // 中断会议：时长 = 最后一条可解析段的 end_ms
        last_end_ms(&dir.join("segments.jsonl")).map(|ms| ms / 1000)
    };
    NoteSummary {
        id: meta.id,
        title: meta.title,
        started_at: meta.started_at,
        duration_secs,
        state: meta.state,
    }
}

fn duration_from_meta(meta: &NoteMeta) -> Option<u64> {
    let start = chrono::DateTime::parse_from_rfc3339(&meta.started_at).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(meta.ended_at.as_deref()?).ok()?;
    Some((end - start).num_seconds().max(0) as u64)
}

fn last_end_ms(jsonl: &Path) -> Option<u64> {
    let f = fs::File::open(jsonl).ok()?;
    let mut last = None;
    for line in std::io::BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
        if let Ok(r) = serde_json::from_str::<SegmentRecord>(&line) {
            last = Some(r.end_ms);
        }
    }
    last
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml store::`
Expected: PASS(Task 3 的 3 个 + 本任务 7 个)。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/store/
git commit -m "P3 Task 4: NoteStore 列表/加载/改名/删除，崩溃与损坏容忍

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: 导出 Markdown / 纯文本

**Files:**
- Create: `src-tauri/src/store/export.rs`
- Modify: `src-tauri/src/store/mod.rs`(挂模块)

**Interfaces:**
- Consumes: `NoteStore::load`、`pub(super) note_dir`(Task 4)。
- Produces:
  - `NoteStore::export(&self, id: &str, format: &str) -> anyhow::Result<PathBuf>`(`format` 取 `"md"|"txt"`,写入 `<note_dir>/transcript.md|.txt`,返回绝对路径;Task 6 的 `export_note` command 依赖)
  - `store::export::format_ts(ms: u64) -> String`(`hh:mm:ss`)

- [ ] **Step 1: 写测试**

`src-tauri/src/store/export.rs` 末尾:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::writer::NoteWriter;
    use crate::store::NoteStore;

    #[test]
    fn format_ts_is_hhmmss() {
        assert_eq!(format_ts(0), "00:00:00");
        assert_eq!(format_ts(83_000), "00:01:23");
        assert_eq!(format_ts(4_083_000), "01:08:03");
    }

    #[test]
    fn human_duration_formats() {
        assert_eq!(human_duration(4080), "1 小时 8 分");
        assert_eq!(human_duration(723), "12 分 3 秒");
        assert_eq!(human_duration(45), "45 秒");
    }

    #[test]
    fn export_md_and_txt() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "今天开会讨论项目进度。", 83_000, 86_000).unwrap();
        w.append_final("system", "好的，先看上周的问题。", 91_000, 94_000).unwrap();
        w.finalize(chrono::Local::now()).unwrap();

        let store = NoteStore::new(tmp.path().to_path_buf());
        let md_path = store.export(&id, "md").unwrap();
        assert_eq!(md_path.file_name().unwrap(), "transcript.md");
        let md = std::fs::read_to_string(&md_path).unwrap();
        let title = store.load(&id).unwrap().meta.title;
        assert!(md.starts_with(&format!("# {title}\n")), "首行为标题: {md}");
        assert!(md.contains("**[我] 00:01:23** 今天开会讨论项目进度。"), "{md}");
        assert!(md.contains("**[对方] 00:01:31** 好的，先看上周的问题。"), "{md}");

        let txt_path = store.export(&id, "txt").unwrap();
        let txt = std::fs::read_to_string(&txt_path).unwrap();
        assert!(txt.contains("[我] 00:01:23 今天开会讨论项目进度。"), "{txt}");
        assert!(!txt.contains("**"), "纯文本无 markdown 记号");

        assert!(store.export(&id, "pdf").is_err(), "未知格式报错");
    }

    #[test]
    fn export_uses_speaker_name_when_present() {
        let note = crate::store::Note {
            meta: crate::store::NoteMeta {
                schema_version: 1,
                id: "x".into(),
                title: "t".into(),
                started_at: String::new(),
                ended_at: None,
                state: "complete".into(),
            },
            segments: vec![crate::store::SegmentRecord {
                seq: 0,
                source: "mic".into(),
                text: "hi".into(),
                start_ms: 0,
                end_ms: 1000,
                speaker: Some("张三".into()),
            }],
            skipped_lines: 0,
        };
        assert!(render_markdown(&note).contains("**[张三] 00:00:00** hi"));
    }
}
```

- [ ] **Step 2: 运行确认编译失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml store::export`
Expected: FAIL — 模块/函数未定义(需先在 mod.rs 挂上 `mod export;`,见 Step 3)。

- [ ] **Step 3: 实现导出**

`src-tauri/src/store/mod.rs` 模块声明区改为:

```rust
pub mod writer;
mod export;
mod notes;
pub use notes::NoteStore;
```

`src-tauri/src/store/export.rs` 实现:

```rust
use super::{Note, NoteStore, SegmentRecord};
use std::path::PathBuf;

impl NoteStore {
    /// 导出到会议文件夹内的 transcript.md / transcript.txt，返回文件路径。
    pub fn export(&self, id: &str, format: &str) -> anyhow::Result<PathBuf> {
        let note = self.load(id)?;
        let dir = self.note_dir(id)?;
        let (name, content) = match format {
            "md" => ("transcript.md", render_markdown(&note)),
            "txt" => ("transcript.txt", render_text(&note)),
            _ => anyhow::bail!("未知导出格式: {format}"),
        };
        let path = dir.join(name);
        std::fs::write(&path, content)?;
        Ok(path)
    }
}

/// 毫秒 → hh:mm:ss。
pub fn format_ts(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// 秒 → 人读时长："1 小时 8 分" / "12 分 3 秒" / "45 秒"。
pub(super) fn human_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h} 小时 {m} 分")
    } else if m > 0 {
        format!("{m} 分 {s} 秒")
    } else {
        format!("{s} 秒")
    }
}

/// 段落标签：有说话人名用名字，否则按来源 我/对方。
fn label(seg: &SegmentRecord) -> &str {
    match &seg.speaker {
        Some(name) => name,
        None if seg.source == "mic" => "我",
        None => "对方",
    }
}

/// 头部第二行："2026-07-03 15:04 – 16:12（1 小时 8 分）"；中断会议结束时间标「中断」。
fn header_line(note: &Note) -> Option<String> {
    let start = chrono::DateTime::parse_from_rfc3339(&note.meta.started_at).ok()?;
    let start_str = start.format("%Y-%m-%d %H:%M").to_string();
    match note
        .meta
        .ended_at
        .as_deref()
        .and_then(|e| chrono::DateTime::parse_from_rfc3339(e).ok())
    {
        Some(end) => {
            let dur = human_duration((end - start).num_seconds().max(0) as u64);
            Some(format!("{start_str} – {}（{dur}）", end.format("%H:%M")))
        }
        None => Some(format!("{start_str} – 中断")),
    }
}

pub(super) fn render_markdown(note: &Note) -> String {
    let mut out = format!("# {}\n\n", note.meta.title);
    if let Some(h) = header_line(note) {
        out.push_str(&h);
        out.push_str("\n\n");
    }
    for seg in &note.segments {
        out.push_str(&format!(
            "**[{}] {}** {}\n\n",
            label(seg),
            format_ts(seg.start_ms),
            seg.text
        ));
    }
    out
}

pub(super) fn render_text(note: &Note) -> String {
    let mut out = format!("{}\n", note.meta.title);
    if let Some(h) = header_line(note) {
        out.push_str(&h);
        out.push('\n');
    }
    out.push('\n');
    for seg in &note.segments {
        out.push_str(&format!(
            "[{}] {} {}\n",
            label(seg),
            format_ts(seg.start_ms),
            seg.text
        ));
    }
    out
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml store::`
Expected: PASS(累计 14 个 store 测试)。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/store/
git commit -m "P3 Task 5: 导出 Markdown / 纯文本

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: 会话集成 + IPC commands + storage 事件

**Files:**
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/ipc.rs`
- Modify: `src-tauri/src/store/writer.rs`(仅追加集成测试)
- Modify: `src-tauri/capabilities/default.json`

**Interfaces:**
- Consumes: `NoteWriter`(Task 3)、`NoteStore`(Task 4/5)、`on_final(Source, String, u64, u64)`(Task 2)。
- Produces(前端 Task 7-9 依赖):
  - Tauri commands:`list_notes() -> Vec<NoteSummary>`(活动会话项 state 改写为 `"active"`)、`get_note(id) -> Note`、`rename_note(id, title)`、`delete_note(id)`(录制中的笔记拒删)、`export_note(id, format) -> String`(导出文件绝对路径)。
  - `StatusEvent` 增加 `note_id: String`(`recording`/`stopped` 时携带,其余为空串)。
  - 新事件 `"storage"`:`StorageEvent { state: String }`,`"degraded"`(落盘失败)/`"ok"`(恢复)。
  - capabilities 增加 `opener:allow-reveal-item-in-dir`(详情页「在 Finder 中显示」用)。

- [ ] **Step 1: ipc.rs 加字段与事件**

`src-tauri/src/ipc.rs` 的 `StatusEvent` 改为:

```rust
/// 录制状态，事件名 "status"。
#[derive(Debug, Clone, Serialize)]
pub struct StatusEvent {
    pub state: String, // "recording" | "stopped" | "error: .."
    /// 系统声音可用性："on" | "denied" | "unavailable"；非录制态可为空串。
    pub system_audio: String,
    /// 本次会话的笔记 id；recording / stopped 时携带，其余为空串。
    pub note_id: String,
}
```

末尾新增:

```rust
/// 落盘健康度，事件名 "storage"。"degraded" = 追加写失败（段暂存内存）；"ok" = 已恢复。
#[derive(Debug, Clone, Serialize)]
pub struct StorageEvent {
    pub state: String,
}
```

- [ ] **Step 2: lib.rs 会话集成**

`src-tauri/src/lib.rs` 全量改动如下。

顶部 use 增加:

```rust
use tauri::Manager;
```

`AppState` 与新结构(替换原 `handle` 字段;锁序注释中 `handle_slot` 相应改名 `session_slot`,协议不变):

```rust
/// 一次活动录制：会话句柄 + 落盘器 + 笔记 id。
struct ActiveSession {
    handle: RecordingHandle,
    writer: Arc<Mutex<store::writer::NoteWriter>>,
    note_id: String,
}

#[derive(Default)]
struct AppState {
    running: Arc<Mutex<bool>>,
    generation: Arc<Mutex<u64>>,
    session: Arc<Mutex<Option<ActiveSession>>>,
}
```

新增两个辅助函数(放在 `models_dir()` 附近):

```rust
/// notes 根目录（不存在则创建）。
fn notes_dir(app: &AppHandle) -> anyhow::Result<PathBuf> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| anyhow::anyhow!("app_data_dir 不可用: {e}"))?
        .join("notes");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 会话未正常存续时的笔记收尾：有内容则 finalize 保全，无内容则删掉空文件夹。
fn abort_or_finalize(writer: &Arc<Mutex<store::writer::NoteWriter>>) {
    let mut w = writer.lock().unwrap();
    if w.has_content() {
        if let Err(e) = w.finalize(chrono::Local::now()) {
            eprintln!("abort_or_finalize: finalize 失败: {e}");
        }
    } else {
        let dir = w.dir().to_path_buf();
        drop(w);
        let _ = std::fs::remove_dir_all(dir);
    }
}
```

`start_recording` 加载线程改动(在两路源构建成功后、`start_session` 之前,插入笔记创建;`fail` 闭包不变,但所有 `StatusEvent` 构造补 `note_id` 字段):

```rust
        // 2.5) 创建笔记落盘器（此后任何失败路径都要 abort_or_finalize 清理）。
        let writer = match notes_dir(&app)
            .and_then(|d| store::writer::NoteWriter::create(&d, chrono::Local::now()))
        {
            Ok(w) => Arc::new(Mutex::new(w)),
            Err(e) => return fail(&app, &running, &generation, my_gen, format!("error: 创建笔记失败: {e}")),
        };
        let note_id = writer.lock().unwrap().note_id().to_string();
```

on_final 闭包(替换原 emit-only 版本;**先落盘再 emit**,落盘健康度去抖上报):

```rust
        let app_f = app.clone();
        let app_p = app.clone();
        let writer_f = writer.clone();
        let mut degraded = false;
        let start = session::start_session(
            sources,
            recognizer,
            16000,
            16000,
            move |src, text, start_ms, end_ms| {
                // 不丢内容优先：先落盘（失败进待写队列），再通知 UI。
                match writer_f.lock().unwrap().append_final(src.as_str(), &text, start_ms, end_ms) {
                    Ok(()) => {
                        if degraded {
                            degraded = false;
                            let _ = app_f.emit("storage", ipc::StorageEvent { state: "ok".into() });
                        }
                    }
                    Err(e) => {
                        eprintln!("append_final 失败（段暂存内存待重试）: {e}");
                        if !degraded {
                            degraded = true;
                            let _ = app_f.emit("storage", ipc::StorageEvent { state: "degraded".into() });
                        }
                    }
                }
                let _ = app_f.emit(
                    "final",
                    ipc::FinalEvent { source: src.as_str().into(), text, start_ms, end_ms },
                );
            },
            move |src, text| {
                let _ = app_p.emit(
                    "partial",
                    ipc::PartialEvent { source: src.as_str().into(), text },
                );
            },
        );
```

成功/失败分支(原逻辑保持,三处补笔记处理;`handle_slot` 变量名改 `session_slot`,存入 `ActiveSession`):

```rust
        match start {
            Ok(start) => {
                if !start.active.contains(&Source::Mic) {
                    start.handle.stop(); // 先排干可能已产生的 system finals
                    abort_or_finalize(&writer);
                    let mic_err = start.failed.iter()
                        .find(|(s, _)| *s == Source::Mic)
                        .map(|(_, msg)| format!("error: 麦克风未能启动: {msg}"))
                        .unwrap_or_else(|| "error: 麦克风未能启动".into());
                    return fail(&app, &running, &generation, my_gen, mic_err);
                }
                let running_guard = running.lock().unwrap();
                let gen_guard = generation.lock().unwrap();
                if !*running_guard || *gen_guard != my_gen {
                    drop(gen_guard);
                    drop(running_guard);
                    start.handle.stop();
                    abort_or_finalize(&writer); // 被 stop/新 start 抢先：内容保全为 complete
                    return;
                }
                drop(gen_guard);
                let system_audio = classify_system(&start.active, &start.failed);
                *session_slot.lock().unwrap() = Some(ActiveSession {
                    handle: start.handle,
                    writer: writer.clone(),
                    note_id: note_id.clone(),
                });
                drop(running_guard);
                let _ = app.emit(
                    "status",
                    ipc::StatusEvent { state: "recording".into(), system_audio, note_id: note_id.clone() },
                );
            }
            Err(e) => {
                abort_or_finalize(&writer);
                return fail(&app, &running, &generation, my_gen, format!("error: {e}"));
            }
        }
```

注意:`fail` 闭包里的 `StatusEvent` 构造补 `note_id: String::new()`。

`stop_recording`(取会话 → 停 → finalize → 带 note_id 的 stopped):

```rust
#[tauri::command]
fn stop_recording(app: AppHandle, state: State<AppState>) {
    // （原锁序注释保持，handle 改叫 session）
    { *state.running.lock().unwrap() = false; }
    { *state.generation.lock().unwrap() += 1; }
    let sess = state.session.lock().unwrap().take();
    let mut note_id = String::new();
    if let Some(s) = sess {
        s.handle.stop(); // 排干 finals：所有 append 在此完成
        note_id = s.note_id;
        if let Err(e) = s.writer.lock().unwrap().finalize(chrono::Local::now()) {
            eprintln!("stop_recording: finalize 失败: {e}");
            let _ = app.emit("storage", ipc::StorageEvent { state: "degraded".into() });
        }
    }
    let _ = app.emit(
        "status",
        ipc::StatusEvent { state: "stopped".into(), system_audio: String::new(), note_id },
    );
}
```

5 个新 command:

```rust
#[tauri::command]
fn list_notes(app: AppHandle, state: State<AppState>) -> Result<Vec<store::NoteSummary>, String> {
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    let mut list = store::NoteStore::new(dir).list();
    // 正在录制的笔记在磁盘上也是 recording 态；用活动会话区分「录制中」与「已中断」。
    if let Some(active_id) = state.session.lock().unwrap().as_ref().map(|s| s.note_id.clone()) {
        for n in &mut list {
            if n.id == active_id {
                n.state = "active".into();
            }
        }
    }
    Ok(list)
}

#[tauri::command]
fn get_note(app: AppHandle, id: String) -> Result<store::Note, String> {
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir).load(&id).map_err(|e| e.to_string())
}

#[tauri::command]
fn rename_note(app: AppHandle, id: String, title: String) -> Result<(), String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("标题不能为空".into());
    }
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir).rename(&id, title).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_note(app: AppHandle, state: State<AppState>, id: String) -> Result<(), String> {
    if state.session.lock().unwrap().as_ref().map(|s| s.note_id == id).unwrap_or(false) {
        return Err("录制中的笔记不能删除".into());
    }
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir).delete(&id).map_err(|e| e.to_string())
}

#[tauri::command]
fn export_note(app: AppHandle, id: String, format: String) -> Result<String, String> {
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir)
        .export(&id, &format)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| e.to_string())
}
```

注册:

```rust
        .invoke_handler(tauri::generate_handler![
            start_recording,
            stop_recording,
            list_notes,
            get_note,
            rename_note,
            delete_note,
            export_note
        ])
```

- [ ] **Step 3: capabilities 加 reveal 权限**

`src-tauri/capabilities/default.json` 的 `permissions` 改为:

```json
  "permissions": [
    "core:default",
    "opener:default",
    "opener:allow-reveal-item-in-dir"
  ]
```

- [ ] **Step 4: 会话×落盘集成测试**

`src-tauri/src/store/writer.rs` 测试模块内追加(MockCapture 灌 fixture → 全管线 → 断言 jsonl 与 final 事件一一对应):

```rust
    #[test]
    fn full_session_persists_every_final() {
        use crate::audio::mock::MockCapture;
        use crate::audio::{AudioCapture, Source};
        use crate::pipeline::segmenter::{MockSegmenter, Segmenter};
        use crate::store::NoteStore;
        use std::sync::{Arc, Mutex};

        struct CountingRecognizer;
        impl crate::asr::Recognizer for CountingRecognizer {
            fn recognize(&mut self, s: &[f32]) -> anyhow::Result<crate::asr::Transcript> {
                Ok(crate::asr::Transcript { text: format!("len={}", s.len()) })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(Mutex::new(NoteWriter::create(tmp.path(), now()).unwrap()));
        let id = writer.lock().unwrap().note_id().to_string();
        let emitted = Arc::new(Mutex::new(0usize));

        let cap = MockCapture::from_wav(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample_16k.wav"
        ))
        .expect("fixture");
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> =
            vec![(Source::Mic, Box::new(cap), Box::new(MockSegmenter::new(2000)))];

        let (w2, e2) = (writer.clone(), emitted.clone());
        let start = crate::session::start_session(
            sources,
            Box::new(CountingRecognizer),
            16000,
            4000,
            move |src, text, start_ms, end_ms| {
                w2.lock().unwrap().append_final(src.as_str(), &text, start_ms, end_ms).unwrap();
                *e2.lock().unwrap() += 1;
            },
            |_, _| {},
        )
        .expect("start_session");
        start.handle.stop(); // MockCapture 已灌完帧；stop 排干全部 finals
        writer.lock().unwrap().finalize(now()).unwrap();

        let n = *emitted.lock().unwrap();
        assert!(n > 0, "fixture 应产出至少一个 final");
        let note = NoteStore::new(tmp.path().to_path_buf()).load(&id).unwrap();
        assert_eq!(note.segments.len(), n, "jsonl 行数 = final 事件数，一段不丢");
        assert!(note.segments.windows(2).all(|w| w[1].seq == w[0].seq + 1), "seq 单调");
        assert!(note.segments.windows(2).all(|w| w[1].start_ms >= w[0].start_ms), "时间戳单调");
        assert_eq!(note.meta.state, "complete");
        assert_eq!(note.skipped_lines, 0);
    }
```

- [ ] **Step 5: 全量验证**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全部 PASS。
Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 编译通过,无 warning 增量。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/ipc.rs src-tauri/src/store/writer.rs src-tauri/capabilities/default.json
git commit -m "P3 Task 6: 会话集成边录边落盘 + 笔记 IPC commands + storage 事件

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: 前端 API 层 + 录制视图迁移到 /record

**Files:**
- Modify: `src/lib/events.ts`
- Create: `src/lib/notes.ts`
- Create: `src/routes/record/+page.svelte`(自 `src/routes/+page.svelte` 迁移)
- Modify: `src/routes/+page.svelte`(临时重定向,Task 8 替换为列表)

**Interfaces:**
- Consumes: Task 6 的 commands 与事件。
- Produces(Task 8/9 依赖):
  - `src/lib/notes.ts` 的类型与函数(下方完整代码)。
  - `/record` 路由:录制视图,停止后 `goto('/notes/' + note_id)`。

- [ ] **Step 1: events.ts 同步事件契约**

`src/lib/events.ts` 全文替换为:

```ts
import { listen } from "@tauri-apps/api/event";

export type Source = "mic" | "system";
export type SystemAudio = "on" | "denied" | "unavailable" | "";

export type PartialEvent = { source: Source; text: string };
export type FinalEvent = { source: Source; text: string; start_ms: number; end_ms: number };
export type StatusEvent = { state: string; system_audio: SystemAudio; note_id: string };
export type StorageEvent = { state: "ok" | "degraded" };

export function onPartial(cb: (e: PartialEvent) => void) {
  return listen<PartialEvent>("partial", (ev) => cb(ev.payload));
}

export function onStatus(cb: (e: StatusEvent) => void) {
  return listen<StatusEvent>("status", (ev) => cb(ev.payload));
}

export function onFinal(cb: (e: FinalEvent) => void) {
  return listen<FinalEvent>("final", (ev) => cb(ev.payload));
}

export function onStorage(cb: (e: StorageEvent) => void) {
  return listen<StorageEvent>("storage", (ev) => cb(ev.payload));
}
```

- [ ] **Step 2: 新建 notes.ts**

`src/lib/notes.ts`:

```ts
import { invoke } from "@tauri-apps/api/core";
import type { Source } from "./events";

export type NoteState = "active" | "recording" | "complete";

export type NoteSummary = {
  id: string;
  title: string;
  started_at: string;
  duration_secs: number | null;
  state: NoteState;
};

export type NoteMeta = {
  schema_version: number;
  id: string;
  title: string;
  started_at: string;
  ended_at: string | null;
  state: string;
};

export type SegmentRecord = {
  seq: number;
  source: Source;
  text: string;
  start_ms: number;
  end_ms: number;
  speaker: string | null;
};

export type Note = { meta: NoteMeta; segments: SegmentRecord[]; skipped_lines: number };

export const listNotes = () => invoke<NoteSummary[]>("list_notes");
export const getNote = (id: string) => invoke<Note>("get_note", { id });
export const renameNote = (id: string, title: string) =>
  invoke<void>("rename_note", { id, title });
export const deleteNote = (id: string) => invoke<void>("delete_note", { id });
/** 返回导出文件绝对路径 */
export const exportNote = (id: string, format: "md" | "txt") =>
  invoke<string>("export_note", { id, format });

/** 00:01:23 */
export function formatTs(ms: number): string {
  const s = Math.floor(ms / 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(Math.floor(s / 3600))}:${pad(Math.floor((s % 3600) / 60))}:${pad(s % 60)}`;
}

/** 1 小时 8 分 / 12 分 3 秒 / 45 秒 */
export function formatDuration(secs: number | null): string {
  if (secs == null) return "—";
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (h > 0) return `${h} 小时 ${m} 分`;
  if (m > 0) return `${m} 分 ${s} 秒`;
  return `${s} 秒`;
}

/** RFC3339 → "2026-07-03 15:04"；空串（元数据损坏）→ "—" */
export function formatDate(rfc3339: string): string {
  if (!rfc3339) return "—";
  const d = new Date(rfc3339);
  if (isNaN(d.getTime())) return "—";
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
```

- [ ] **Step 3: 迁移录制视图到 /record 并接入落盘横幅与跳转**

创建 `src/routes/record/+page.svelte`:内容 = 现 `src/routes/+page.svelte` 全文,做以下修改(其余原样保留,含全部样式):

script 部分:

```svelte
<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { onPartial, onStatus, onFinal, onStorage, type Source, type SystemAudio } from "$lib/events";

  type Line = { source: Source; text: string };

  let status = $state("idle");
  let systemAudio = $state<SystemAudio>("");
  let finals = $state<Line[]>([]);
  let partialMic = $state("");
  let partialSystem = $state("");
  let storageDegraded = $state(false);

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
        storageDegraded = false;
      } else if (e.state === "stopped" || e.state.startsWith("error:")) {
        partialMic = "";
        partialSystem = "";
        if (e.state === "stopped" && e.note_id) {
          goto(`/notes/${e.note_id}`);
        }
      }
    });
    const u3 = onFinal((e) => {
      if (e.text.trim()) finals = [...finals, { source: e.source, text: e.text }];
      if (e.source === "mic") partialMic = "";
      else partialSystem = "";
    });
    const u4 = onStorage((e) => {
      storageDegraded = e.state === "degraded";
    });
    return () => {
      u1.then((f) => f());
      u2.then((f) => f());
      u3.then((f) => f());
      u4.then((f) => f());
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
```

模板部分:标题行上方加返回链接,系统声音横幅之后加落盘横幅:

```svelte
<main class="container">
  <p><a href="/">← 笔记列表</a></p>
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

  {#if storageDegraded}
    <div class="banner">落盘异常：内容暂存内存并自动重试，请检查磁盘空间。录制不受影响。</div>
  {/if}
  ...（转写区与样式与原文件相同）
```

`src/routes/+page.svelte` 全文替换为临时重定向(Task 8 会替换成列表页):

```svelte
<script lang="ts">
  import { goto } from "$app/navigation";
  import { onMount } from "svelte";
  onMount(() => goto("/record", { replaceState: true }));
</script>
```

- [ ] **Step 4: 检查**

Run: `npm run check`
Expected: 0 errors。
Run: `npm run build`
Expected: 构建成功(SPA fallback 已配置,动态路由可用)。

- [ ] **Step 5: Commit**

```bash
git add src/lib/events.ts src/lib/notes.ts src/routes/record/ src/routes/+page.svelte
git commit -m "P3 Task 7: 前端事件/笔记 API 层，录制视图迁移 /record，停止跳详情

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: 笔记列表主页

**Files:**
- Modify: `src/routes/+page.svelte`(替换临时重定向为列表页)

**Interfaces:**
- Consumes: `src/lib/notes.ts`(Task 7)。
- Produces: `/` 列表页——标题过滤、日期倒序、改名/删除(两步确认)/「已中断」「录制中」徽章、「开始录制」入口。

- [ ] **Step 1: 实现列表页**

`src/routes/+page.svelte` 全文替换:

```svelte
<script lang="ts">
  import { onMount } from "svelte";
  import {
    listNotes,
    renameNote,
    deleteNote,
    formatDate,
    formatDuration,
    type NoteSummary,
  } from "$lib/notes";

  let notes = $state<NoteSummary[]>([]);
  let query = $state("");
  let error = $state("");
  let editingId = $state<string | null>(null);
  let editingTitle = $state("");
  let confirmingDeleteId = $state<string | null>(null);

  const filtered = $derived(
    query.trim() ? notes.filter((n) => n.title.toLowerCase().includes(query.trim().toLowerCase())) : notes,
  );

  async function refresh() {
    try {
      notes = await listNotes();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }

  onMount(refresh);

  function beginRename(n: NoteSummary) {
    editingId = n.id;
    editingTitle = n.title;
  }

  async function commitRename() {
    if (!editingId) return;
    const id = editingId;
    editingId = null;
    try {
      await renameNote(id, editingTitle);
    } catch (e) {
      error = `改名失败: ${e}`;
    }
    await refresh();
  }

  async function confirmDelete(id: string) {
    confirmingDeleteId = null;
    try {
      await deleteNote(id);
    } catch (e) {
      error = `删除失败: ${e}`;
    }
    await refresh();
  }

  const stateBadge = (s: NoteSummary["state"]) =>
    s === "active" ? "录制中" : s === "recording" ? "已中断" : "";
</script>

<main class="container">
  <div class="row header">
    <h1>会议笔记</h1>
    <a class="primary" href="/record">开始录制</a>
  </div>

  <input class="search" type="search" placeholder="按标题过滤…" bind:value={query} />

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if filtered.length === 0}
    <p class="hint">{notes.length === 0 ? "还没有笔记，点「开始录制」来第一场。" : "没有匹配的笔记。"}</p>
  {/if}

  <ul class="list">
    {#each filtered as n (n.id)}
      <li class="item">
        <div class="main">
          {#if editingId === n.id}
            <!-- svelte-ignore a11y_autofocus -->
            <input
              class="rename"
              autofocus
              bind:value={editingTitle}
              onkeydown={(e) => {
                if (e.key === "Enter") commitRename();
                if (e.key === "Escape") editingId = null;
              }}
              onblur={commitRename}
            />
          {:else}
            <a class="title" href={n.state === "active" ? "/record" : `/notes/${n.id}`}>
              {n.title}
              {#if stateBadge(n.state)}
                <span class="state" class:interrupted={n.state === "recording"} class:active={n.state === "active"}>
                  {stateBadge(n.state)}
                </span>
              {/if}
            </a>
          {/if}
          <span class="meta">{formatDate(n.started_at)} · {formatDuration(n.duration_secs)}</span>
        </div>
        <div class="actions">
          <button class="link" onclick={() => beginRename(n)}>改名</button>
          {#if confirmingDeleteId === n.id}
            <button class="link danger" onclick={() => confirmDelete(n.id)}>确认删除</button>
            <button class="link" onclick={() => (confirmingDeleteId = null)}>取消</button>
          {:else}
            <button class="link" onclick={() => (confirmingDeleteId = n.id)}>删除</button>
          {/if}
        </div>
      </li>
    {/each}
  </ul>
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 42rem;
  }
  .row.header {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  h1 {
    margin: 0 0 0.5rem;
  }
  a.primary {
    background: #396cd8;
    color: #fff;
    border-radius: 8px;
    padding: 0.5em 1.2em;
    text-decoration: none;
    font-weight: 500;
  }
  .search {
    width: 100%;
    box-sizing: border-box;
    margin: 0.75rem 0 1rem;
    padding: 0.5em 0.8em;
    border-radius: 8px;
    border: 1px solid #ccc;
    font-size: 1em;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .item {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.5rem;
    padding: 0.7rem 0.4rem;
    border-bottom: 1px solid #e5e5e7;
  }
  .main {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    min-width: 0;
  }
  .title {
    color: inherit;
    text-decoration: none;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .title:hover {
    color: #396cd8;
  }
  .rename {
    font-size: 1em;
    padding: 0.2em 0.4em;
    border-radius: 6px;
    border: 1px solid #396cd8;
  }
  .meta {
    color: #888;
    font-size: 0.85em;
  }
  .state {
    font-size: 0.7em;
    font-weight: 600;
    border-radius: 6px;
    padding: 0.1em 0.45em;
    margin-left: 0.4em;
    vertical-align: middle;
    color: #fff;
  }
  .state.interrupted {
    background: #d88a39;
  }
  .state.active {
    background: #c0392b;
  }
  .actions {
    display: flex;
    gap: 0.3rem;
    flex-shrink: 0;
  }
  .link {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    padding: 0.2em 0.3em;
    font-size: 0.9em;
    box-shadow: none;
  }
  .link.danger {
    color: #c0392b;
    font-weight: 600;
  }
  .banner {
    background: #fff4e5;
    border: 1px solid #f0c98a;
    color: #8a5a00;
    border-radius: 8px;
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .hint {
    color: #aaa;
  }
  @media (prefers-color-scheme: dark) {
    .item {
      border-color: #3a3a3a;
    }
    .search {
      background: #2a2a2a;
      border-color: #444;
      color: #f0f0f0;
    }
    .rename {
      background: #2a2a2a;
      color: #f0f0f0;
    }
    .banner {
      background: #3a2e18;
      border-color: #6b5426;
      color: #e8c88a;
    }
    .hint {
      color: #555;
    }
  }
</style>
```

- [ ] **Step 2: 检查**

Run: `npm run check`
Expected: 0 errors。

- [ ] **Step 3: Commit**

```bash
git add src/routes/+page.svelte
git commit -m "P3 Task 8: 笔记列表主页（过滤/改名/删除/中断与录制中徽章）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: 笔记详情页(只读 + 改名 + 导出)

**Files:**
- Create: `src/routes/notes/[id]/+page.svelte`

**Interfaces:**
- Consumes: `getNote/renameNote/exportNote/formatTs/formatDate/formatDuration`(Task 7);`revealItemInDir`(@tauri-apps/plugin-opener,权限已在 Task 6 开通)。
- Produces: `/notes/[id]` 详情页。

- [ ] **Step 1: 实现详情页**

`src/routes/notes/[id]/+page.svelte`:

```svelte
<script lang="ts">
  import { onMount } from "svelte";
  import { page } from "$app/stores";
  import { revealItemInDir } from "@tauri-apps/plugin-opener";
  import {
    getNote,
    renameNote,
    exportNote,
    formatTs,
    formatDate,
    formatDuration,
    type Note,
  } from "$lib/notes";
  import type { Source } from "$lib/events";

  let note = $state<Note | null>(null);
  let error = $state("");
  let editing = $state(false);
  let editingTitle = $state("");
  let exportMsg = $state("");

  const id = $derived($page.params.id);

  const label = (source: Source, speaker: string | null) =>
    speaker ?? (source === "mic" ? "我" : "对方");

  function durationSecs(n: Note): number | null {
    if (n.meta.ended_at && n.meta.started_at) {
      const d = (new Date(n.meta.ended_at).getTime() - new Date(n.meta.started_at).getTime()) / 1000;
      return isNaN(d) ? null : Math.max(0, Math.floor(d));
    }
    const last = n.segments.at(-1);
    return last ? Math.floor(last.end_ms / 1000) : null;
  }

  async function refresh() {
    try {
      note = await getNote(id);
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }

  onMount(refresh);

  function beginRename() {
    if (!note) return;
    editing = true;
    editingTitle = note.meta.title;
  }

  async function commitRename() {
    if (!editing || !note) return;
    editing = false;
    try {
      await renameNote(id, editingTitle);
      await refresh();
    } catch (e) {
      error = `改名失败: ${e}`;
    }
  }

  async function doExport(format: "md" | "txt") {
    exportMsg = "";
    try {
      const path = await exportNote(id, format);
      exportMsg = `已导出：${path}`;
      await revealItemInDir(path);
    } catch (e) {
      error = `导出失败: ${e}`;
    }
  }
</script>

<main class="container">
  <p><a href="/">← 笔记列表</a></p>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if note}
    {#if editing}
      <!-- svelte-ignore a11y_autofocus -->
      <input
        class="rename"
        autofocus
        bind:value={editingTitle}
        onkeydown={(e) => {
          if (e.key === "Enter") commitRename();
          if (e.key === "Escape") editing = false;
        }}
        onblur={commitRename}
      />
    {:else}
      <h1 class="title" title="点击改名" onclick={beginRename}>{note.meta.title}</h1>
    {/if}

    <p class="meta">
      {formatDate(note.meta.started_at)} · {formatDuration(durationSecs(note))}
      {#if note.meta.state === "recording"}
        <span class="state interrupted">已中断</span>
      {/if}
    </p>

    {#if note.meta.state === "recording"}
      <div class="banner">这场会议曾意外中断，以下是中断前保存的全部内容。</div>
    {/if}
    {#if note.skipped_lines > 0}
      <div class="banner">有 {note.skipped_lines} 行记录损坏被跳过。</div>
    {/if}

    <div class="row">
      <button onclick={() => doExport("md")}>导出 Markdown</button>
      <button onclick={() => doExport("txt")}>导出纯文本</button>
      {#if exportMsg}<span class="hint">{exportMsg}</span>{/if}
    </div>

    <div class="transcript">
      {#each note.segments as seg (seg.seq)}
        <p class="final">
          <span class="badge" class:mic={seg.source === "mic"} class:system={seg.source === "system"}>
            {label(seg.source, seg.speaker)}
          </span>
          <span class="ts">{formatTs(seg.start_ms)}</span>
          {seg.text}
        </p>
      {/each}
      {#if note.segments.length === 0}
        <p class="hint">（这场会议没有转写内容）</p>
      {/if}
    </div>
  {/if}
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 42rem;
  }
  .title {
    cursor: text;
    margin: 0 0 0.25rem;
  }
  .rename {
    font-size: 1.6em;
    font-weight: 700;
    width: 100%;
    box-sizing: border-box;
    padding: 0.1em 0.3em;
    border-radius: 8px;
    border: 1px solid #396cd8;
  }
  .meta {
    color: #888;
    margin: 0 0 1rem;
  }
  .row {
    display: flex;
    gap: 0.75rem;
    align-items: center;
    margin: 0 0 1rem;
  }
  button {
    border-radius: 8px;
    border: 1px solid transparent;
    padding: 0.5em 1.2em;
    font-size: 0.95em;
    font-weight: 500;
    cursor: pointer;
    background-color: #ffffff;
    box-shadow: 0 2px 2px rgba(0, 0, 0, 0.2);
  }
  button:hover {
    border-color: #396cd8;
  }
  .transcript {
    background: #f5f5f7;
    border-radius: 8px;
    padding: 1rem;
    font-size: 1.05rem;
    line-height: 1.6;
  }
  .transcript p {
    margin: 0 0 0.35rem;
  }
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
  .badge.mic {
    background: #396cd8;
  }
  .badge.system {
    background: #2e9e5b;
  }
  .ts {
    color: #999;
    font-size: 0.8em;
    margin-right: 0.4em;
    font-variant-numeric: tabular-nums;
  }
  .state.interrupted {
    background: #d88a39;
    color: #fff;
    font-size: 0.7em;
    font-weight: 600;
    border-radius: 6px;
    padding: 0.1em 0.45em;
    margin-left: 0.4em;
  }
  .banner {
    background: #fff4e5;
    border: 1px solid #f0c98a;
    color: #8a5a00;
    border-radius: 8px;
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .hint {
    color: #aaa;
  }
  @media (prefers-color-scheme: dark) {
    .transcript {
      background: #2a2a2a;
    }
    .rename {
      background: #2a2a2a;
      color: #f0f0f0;
    }
    button {
      color: #ffffff;
      background-color: #0f0f0f98;
    }
    .banner {
      background: #3a2e18;
      border-color: #6b5426;
      color: #e8c88a;
    }
    .hint {
      color: #555;
    }
  }
</style>
```

- [ ] **Step 2: 检查与构建**

Run: `npm run check`
Expected: 0 errors(`onclick` 于 h1 会有 a11y warning 属可接受;若 check 报 error 级,给 h1 加 `role="button"` 与 `onkeydown` 处理 Enter)。
Run: `npm run build`
Expected: 构建成功。

- [ ] **Step 3: Commit**

```bash
git add src/routes/notes/
git commit -m "P3 Task 9: 笔记详情页（只读时间轴 + 改名 + 导出并在 Finder 显示）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 10: 端到端人工冒烟(需真人操作)

**Files:** 无代码改动;结果记入 `.superpowers/sdd/progress.md`。

- [ ] **Step 1: 跑全量自动验证**

```bash
cargo test --manifest-path src-tauri/Cargo.toml
npm run check && npm run build
```

Expected: 全部通过。

- [ ] **Step 2: 人工冒烟清单(逐项确认)**

```bash
npm run tauri dev
```

1. 主页为空列表 → 「开始录制」→ 说几句中英混合 → 停止 → **自动跳到详情页**,内容与时间戳完整。
2. 详情页改名生效;导出 Markdown → Finder 中出现 `transcript.md`,格式为 `**[我] hh:mm:ss** 文本`;导出纯文本同理。
3. 返回列表:笔记按日期倒序,标题过滤有效;改名、删除(两步确认)生效。
4. **崩溃恢复**:开始录制并说话出现若干定稿段 → `kill -9` 应用进程 → 重启 → 列表出现该笔记带「已中断」徽章 → 打开详情,崩溃前定稿段完好,可导出。
5. 录制中切回列表(地址栏或返回链接):该笔记显示「录制中」徽章,点击回到 /record;录制中删除该笔记被拒绝。
6. 检查磁盘:`~/Library/Application Support/com.teemo.voice-notes/notes/<id>/` 下 `meta.json` + `segments.jsonl` 内容符合 spec §2。

- [ ] **Step 3: 记录进度并收尾**

把冒烟结果追加到 `.superpowers/sdd/progress.md`(P3 小节),未通过项开修复任务;全部通过则 P3 完成,汇报最终评审。

---

## Self-Review 记录

- **Spec 覆盖**:落盘(T3/T6)、崩溃恢复(T3/T4/T6 + T10.4)、列表/过滤/改名/删除(T4/T6/T8)、详情只读(T9)、导出 md/txt + Finder 显示(T5/T6/T9)、Final 时间戳前置(T1/T2)、storage 降级横幅(T6/T7)、「录制中/已中断」区分(T6/T8)、notes 目录创建失败阻止开录(T6 notes_dir 在 create 前失败即 fail)——spec §1-§7 全覆盖。
- **占位符**:无 TBD/TODO;所有代码步骤含完整代码。
- **类型一致性**:`append_final(&mut self, source: &str, text: &str, start_ms: u64, end_ms: u64)` 在 T3 定义、T6 两处调用一致;`on_final(Source, String, u64, u64)` T2 定义、T6 闭包一致;TS `NoteSummary.state` 三态与 T6 command 改写一致;`formatTs/formatDate/formatDuration` T7 定义、T8/T9 使用一致。
- **既有测试联动**:T2 Step 4 明确列出 session.rs 6 处回调签名修复,避免半途编译失败。
