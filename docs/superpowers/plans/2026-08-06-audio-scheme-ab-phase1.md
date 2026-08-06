# 录音双方案可切换 · 第一期(录制期时间轴混音)实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 录制期把 mic/system 两路 post-AEC 样本按时间轴混成一条 `mixed.wav` 落盘,并把每源墙钟-样本对账写进 audio.json,为方案 A/B 对比提供产物与客观判据。

**Architecture:** 纯逻辑混音核心(`audio/timeline_mix.rs`)按**时间轴位置**索引累加、按水位线定稿,不依赖到达顺序;`pipeline/recording_sink.rs` 定义 `RecordingSink` 与双实现,`MixedSink` 包住现有双轨写入再挂混音器;混音器输出经既有 `AudioTrackWriter` 写成第三个轨源 `mixed`,存储层零改动(`known_sources` 已泛化)。

**Tech Stack:** Rust(cargo test)/ 既有 `AudioTrackWriter`、`SourceHealth`、`crossbeam_channel`。

## Global Constraints

- 设计依据:`docs/superpowers/specs/2026-08-06-audio-scheme-ab-design.md`,冲突以 spec 为准。
- 工作目录:仓库根目录;cargo 在 `src-tauri/`。分支 `feature/audio-scheme-ab`。
- **硬约束:混音是旁路,录制主链路绝不因它失败。** 混音器任何异常 → 停写 `mixed.wav`,双轨照常落盘。
- 混音器挂在 `frame_tap` 与 AEC **之后**(消费 `audio_sinks` 收到的样本),不得前移。
- 样本格式恒 16k 单声道 f32;落盘经既有 `AudioTrackWriter`(内部 clamp 转 s16le)。
- 代码注释风格:中文、讲约束与「为什么」,不讲流水账(与仓库一致)。
- 既有测试不许破坏:`cargo test` 全绿、`npm run check` 0 错误。
- 每个 Task 结尾提交,消息用 `feat(mix):` / `test(mix):` 前缀,落款
  `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`。

---

## File Structure

| 文件 | 职责 |
|---|---|
| `src-tauri/src/audio/timeline_mix.rs`(新建) | 纯逻辑混音核心:时间轴索引累加 + 水位线定稿。无 IO、无线程、无 Tauri 依赖,全部可单测。 |
| `src-tauri/src/audio/mod.rs`(改) | 注册 `pub mod timeline_mix;` |
| `src-tauri/src/pipeline/recording_sink.rs`(新建) | `RecordingSink` trait + `DualTrackSink` / `MixedSink` 两实现。持有写盘通道与混音线程。 |
| `src-tauri/src/pipeline/mod.rs`(改) | 注册 `pub mod recording_sink;` |
| `src-tauri/src/lib.rs`(改,~1197-1216) | 用 `RecordingSink` 替换裸 `audio_sinks` 构造;按设置选实现 |
| `src-tauri/src/settings.rs`(改) | 新增 `mix_track: bool` 字段(`#[serde(default)]`) |
| `src-tauri/src/store/audio.rs`(改) | `TrackMeta` 新增 `sync: Option<SyncInfo>`;新增 `set_track_sync()` |

---

### Task 1: 时间轴混音核心(纯逻辑)

**Files:**
- Create: `src-tauri/src/audio/timeline_mix.rs`
- Modify: `src-tauri/src/audio/mod.rs`(在 `pub mod resample;` 一行后加 `pub mod timeline_mix;`)
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `pub const MIC: usize = 0;` / `pub const SYSTEM: usize = 1;`
  - `pub struct TimelineMixer`
  - `pub fn TimelineMixer::new(margin_samples: u64) -> Self`
  - `pub fn TimelineMixer::accept(&mut self, src: usize, samples: &[f32]) -> Vec<f32>`
    — 接受某源一块样本,返回本次新定稿的连续样本(可能为空)
  - `pub fn TimelineMixer::finish(&mut self) -> Vec<f32>` — 收尾,定稿窗内全部剩余
  - `pub const DEFAULT_MARGIN_SAMPLES: u64 = 6400;`(400ms @16k,spec §架构 的待校准初值)
  - Task 2 消费。

- [ ] **Step 1: 写失败测试**

新建 `src-tauri/src/audio/timeline_mix.rs`,先只写测试模块(实现留空,让编译失败):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// f32 逐元素容差比较。不用 assert_eq! 精确比:和式(如 1.0+0.1)是否恰好等于
    /// 字面量 1.1f32 取决于舍入,断言不该赌这个。
    fn assert_close(got: &[f32], want: &[f32]) {
        assert_eq!(got.len(), want.len(), "长度不符: got {got:?} want {want:?}");
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-6, "got {got:?} want {want:?}");
        }
    }

    /// 两源等速到达:定稿部分应是逐样本和。
    #[test]
    fn equal_rate_sources_sum_pointwise() {
        let mut m = TimelineMixer::new(0); // margin=0 便于逐样本断言
        assert!(m.accept(MIC, &[1.0, 1.0, 1.0]).is_empty(), "只有一源时水位线为 0,不该定稿");
        assert_close(&m.accept(SYSTEM, &[0.5, 0.5, 0.5]), &[1.5, 1.5, 1.5]);
    }

    /// 一源滞后:滞后期间不定稿;补上后按位置对齐,不与更晚的对面窗错配。
    #[test]
    fn lagging_source_aligns_by_position_not_arrival() {
        let mut m = TimelineMixer::new(0);
        // mic 先跑 4 个样本,system 一个都没来
        assert!(m.accept(MIC, &[1.0, 2.0, 3.0, 4.0]).is_empty());
        // system 追上前 2 个 → 只定稿前 2 个位置
        assert_close(&m.accept(SYSTEM, &[0.1, 0.2]), &[1.1, 2.2]);
        // system 再追 2 个 → 定稿第 3、4 个位置(而非和 mic 后来的样本错配)
        assert_close(&m.accept(SYSTEM, &[0.3, 0.4]), &[3.3, 4.4]);
    }

    /// 缺口:某源在时间轴上有空洞(frame_tap 已补零帧,此处等价于喂 0.0),
    /// 另一源内容原样保留,位置不漂移。
    #[test]
    fn silent_fill_does_not_shift_positions() {
        let mut m = TimelineMixer::new(0);
        m.accept(MIC, &[0.0, 0.0, 9.0]);
        assert_close(&m.accept(SYSTEM, &[1.0, 1.0, 1.0]), &[1.0, 1.0, 10.0]);
    }

    /// 不等长块 + 交替到达:定稿序列与位置严格对应。
    #[test]
    fn uneven_chunks_keep_positional_correspondence() {
        let mut m = TimelineMixer::new(0);
        let mut all = Vec::new();
        all.extend(m.accept(MIC, &[0.1])); // 水位线仍为 0,空
        all.extend(m.accept(SYSTEM, &[0.01, 0.02, 0.03])); // 水位线到 1 → 定稿位置 0
        all.extend(m.accept(MIC, &[0.2, 0.3])); // 水位线到 3 → 定稿位置 1、2
        all.extend(m.finish());
        // 位置 0 = 0.1+0.01,位置 1 = 0.2+0.02,位置 2 = 0.3+0.03
        assert_eq!(all.len(), 3);
        for (got, want) in all.iter().zip([0.11_f32, 0.22, 0.33]) {
            assert!((got - want).abs() < 1e-6, "got {all:?}");
        }
    }

    /// 水位线余量:margin 之内的位置不定稿,留给尚未到达的样本。
    #[test]
    fn margin_holds_back_recent_positions() {
        let mut m = TimelineMixer::new(2);
        m.accept(MIC, &[1.0, 1.0, 1.0, 1.0]);
        // 两源 min 位置 = 4,减 margin 2 → 只定稿位置 0、1
        assert_close(&m.accept(SYSTEM, &[1.0, 1.0, 1.0, 1.0]), &[2.0, 2.0]);
        // finish 把剩下的全部吐出
        assert_close(&m.finish(), &[2.0, 2.0]);
    }

    /// 核心**不**钳制:溢出交给落盘侧的 f32_to_s16(既有,已 clamp)。
    /// 混音器是纯加法,钳制是存储层关注点;两处都钳会让核心的单测无法用直观数值断言,
    /// 也掩盖"两路相加真的触顶了"这一诊断信号。
    #[test]
    fn sum_is_not_clamped_here() {
        let mut m = TimelineMixer::new(0);
        m.accept(MIC, &[0.9, -0.9]);
        let out = m.accept(SYSTEM, &[0.9, -0.9]);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 1.8).abs() < 1e-6, "got {out:?}");
        assert!((out[1] + 1.8).abs() < 1e-6, "got {out:?}");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test timeline_mix 2>&1 | tail -20`
Expected: 编译失败,`cannot find type TimelineMixer in this scope`(或类似 unresolved 错误)

- [ ] **Step 3: 写最小实现**

在 `timeline_mix.rs` 的 `#[cfg(test)]` 之前插入:

```rust
//! 时间轴混音核心:按**时间轴位置**索引累加,按水位线定稿。
//!
//! 为什么不按到达顺序:两路采集线程独立,块大小与到达时刻都不可控。按到达顺序配对
//! (meetily 的 `can_mix()` 用 `||` + 零填充)会在某路滞后时拿静音顶替一窗,等真数据
//! 到达已与更晚的对面窗错配,且错位不可恢复。这里每块样本的位置是**算出来的**——
//! `pos = 该源已接受样本数`(调用方保证喂进来的是 post-frame_tap 流,断流已补零帧,
//! 故样本数即时间轴位置),因此位置从不靠推断。
//!
//! 定稿判据:水位线 = `min(各源位置) − margin`。低于水位的位置两源都不可能再来数据,
//! 可安全定稿;margin 吸收两路到达时刻的抖动。

/// 源下标。只有两源,用定长数组避免哈希开销(混音在录制热路径的旁路上)。
pub const MIC: usize = 0;
pub const SYSTEM: usize = 1;
const NSRC: usize = 2;

/// 水位线安全余量默认值:400ms @16k。与 player_gate 的 system 回看窗同量级,
/// 覆盖实测 165~245ms 声学回路延迟与设备抖动。待实测校准的初值,不是定论。
pub const DEFAULT_MARGIN_SAMPLES: u64 = 6400;

pub struct TimelineMixer {
    /// 每源已接受样本数 = 该源在时间轴上的当前写入位置。
    pos: [u64; NSRC],
    /// 累加窗起点(时间轴样本号)。窗内 win[i] 对应位置 win_start + i。
    win_start: u64,
    win: Vec<f32>,
    margin: u64,
}

impl TimelineMixer {
    pub fn new(margin_samples: u64) -> Self {
        Self { pos: [0; NSRC], win_start: 0, win: Vec::new(), margin: margin_samples }
    }

    /// 接受某源一块样本,返回本次新定稿的连续样本(从旧 win_start 起)。
    pub fn accept(&mut self, src: usize, samples: &[f32]) -> Vec<f32> {
        let start = self.pos[src];
        // 按位置累加进窗(窗不足则补 0.0 扩容——那些位置只是还没有任何源写过)。
        let end = start + samples.len() as u64;
        let need = (end - self.win_start) as usize;
        if self.win.len() < need {
            self.win.resize(need, 0.0);
        }
        let base = (start - self.win_start) as usize;
        for (i, s) in samples.iter().enumerate() {
            self.win[base + i] += *s;
        }
        self.pos[src] = end;
        self.drain_below_watermark()
    }

    /// 收尾:两源都不再来数据,窗内剩余全部定稿。
    pub fn finish(&mut self) -> Vec<f32> {
        let out: Vec<f32> = self.win.drain(..).collect();
        self.win_start += out.len() as u64;
        out
    }

    fn drain_below_watermark(&mut self) -> Vec<f32> {
        let low = self.pos.iter().copied().min().unwrap_or(0);
        let watermark = low.saturating_sub(self.margin);
        if watermark <= self.win_start {
            return Vec::new();
        }
        let n = (watermark - self.win_start) as usize;
        let n = n.min(self.win.len());
        let out: Vec<f32> = self.win.drain(..n).collect();
        self.win_start += out.len() as u64;
        out
    }
}
```

在 `src-tauri/src/audio/mod.rs` 的 `pub mod resample;` 一行之后加:

```rust
pub mod timeline_mix;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test timeline_mix 2>&1 | tail -15`
Expected: `test result: ok. 6 passed`

- [ ] **Step 5: 跑全量确认无回归**

Run: `cd src-tauri && cargo test 2>&1 | tail -5`
Expected: 全部 ok,失败数 0

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/audio/timeline_mix.rs src-tauri/src/audio/mod.rs
git commit -m "$(cat <<'EOF'
feat(mix): 时间轴混音核心——按位置索引累加,不按到达顺序配对

两路采集线程独立,块大小与到达时刻不可控。按到达顺序配对(meetily 的 can_mix
用 ||+零填充)会在某路滞后时拿静音顶替一窗,真数据到达已与更晚的对面窗错配且
不可恢复。这里位置是算出来的(pos = 该源已接受样本数,调用方保证喂 post-frame_tap
流,断流已补零帧),位置从不靠推断。

定稿判据:水位线 = min(各源位置) − margin,低于水位的位置两源都不会再写。
margin 初值 400ms(与 player_gate 回看窗同量级),待实测校准。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: `RecordingSink` 接口与双实现

**Files:**
- Create: `src-tauri/src/pipeline/recording_sink.rs`
- Modify: `src-tauri/src/pipeline/mod.rs`(加 `pub mod recording_sink;`)
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: Task 1 的 `TimelineMixer` / `MIC` / `SYSTEM` / `DEFAULT_MARGIN_SAMPLES`
- Produces:
  - `pub struct Wiring { pub sinks: Vec<(Source, Box<dyn FnMut(&[f32]) + Send>)>, pub joins: Vec<std::thread::JoinHandle<()>> }`
  - `pub trait RecordingSink: Send { fn into_wiring(self: Box<Self>) -> Wiring; }`
  - `pub struct DualTrackSink` + `pub fn DualTrackSink::new(note_dir: &Path, base_ms: u64, sources: &[Source]) -> Self`
  - `pub struct MixedSink` + `pub fn MixedSink::new(inner: DualTrackSink) -> Self`
  - `pub fn build_sinks(note_dir: &Path, base_ms: u64, sources: &[Source], mix: bool) -> Wiring`
    — 按 `mix` 选实现并 `into_wiring()`。Task 3 消费。

> **接口形状的取舍**:trait 做成**装配工厂**(`into_wiring`)而不是逐块转发的
> `accept(source, samples)`。后者需要两源共享一个 sink 对象 → `Arc<Mutex<>>` →
> 在绝不许阻塞的采集回调路径上加锁。工厂形态下每源仍是独立闭包 + 独立通道,
> 方案差异体现在"装配出什么",零锁。

- [ ] **Step 1: 写失败测试**

新建 `src-tauri/src/pipeline/recording_sink.rs`,先只写测试:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Source;

    /// 喂完样本后拆掉 sink 让通道关闭,再 join 全部写盘线程。
    fn drain(w: Wiring) {
        drop(w.sinks);
        for j in w.joins {
            j.join().unwrap();
        }
    }

    /// mix=false:只落两条源轨,不产生 mixed.wav。
    #[test]
    fn without_mix_only_source_tracks_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], false);
        for (_, s) in w.sinks.iter_mut() {
            s(&[0.5; 160]);
        }
        drain(w);
        assert!(dir.path().join("mic.wav").exists());
        assert!(dir.path().join("system.wav").exists());
        assert!(!dir.path().join("mixed.wav").exists(), "mix=false 不该产出成品轨");
    }

    /// mix=true:三条轨都在,且 mixed 字节数与源轨一致(水位线不丢内容,finish 收尾)。
    #[test]
    fn with_mix_produces_mixed_track_of_equal_length() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
        // 两源各喂 100 块 × 160 样本 = 16000 样本 = 1 秒
        for _ in 0..100 {
            for (_, s) in w.sinks.iter_mut() {
                s(&[0.25; 160]);
            }
        }
        drain(w);
        let len = |n: &str| std::fs::metadata(dir.path().join(n)).unwrap().len();
        assert!(dir.path().join("mixed.wav").exists());
        // 44 字节头 + 16000 样本 × 2 字节
        assert_eq!(len("mic.wav"), 44 + 32000);
        assert_eq!(len("mixed.wav"), 44 + 32000, "finish 应把窗内剩余全部定稿");
    }

    /// 单源会话:无从混音,不产出 mixed.wav(降级为只有方案 A 可选)。
    #[test]
    fn single_source_session_produces_no_mixed_track() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::System], true);
        for (_, s) in w.sinks.iter_mut() {
            s(&[0.5; 160]);
        }
        drain(w);
        assert!(dir.path().join("system.wav").exists());
        assert!(!dir.path().join("mixed.wav").exists());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test recording_sink 2>&1 | tail -20`
Expected: 编译失败,`cannot find function build_sinks in this scope`

- [ ] **Step 3: 写最小实现**

在 `recording_sink.rs` 的 `#[cfg(test)]` 之前插入:

```rust
//! 录制期产物装配:按方案决定落哪些轨。
//!
//! 现状(方案 A)每源一条通道 + 一个写盘线程 + 一个 AudioTrackWriter。方案 B 在此
//! 之上多挂一条混音通道:两源的 sink 各自把样本**再复制一份**发给混音线程,由
//! TimelineMixer 按位置合成后写第三条轨 `mixed.wav`。
//!
//! 硬约束:混音是旁路。通道满/线程死/写盘失败都只影响 mixed.wav,两条源轨与转写
//! 热路径不受任何影响(与 keep_audio 的既有哲学一致——音频落盘是增值旁路)。
//!
//! 单源会话(record_system_only)无从混音,直接不建混音线程:该笔记只有方案 A 可选。

use crate::audio::timeline_mix::{TimelineMixer, DEFAULT_MARGIN_SAMPLES, MIC, SYSTEM};
use crate::audio::Source;
use crate::store::audio::AudioTrackWriter;
use std::path::Path;

/// 装配产物:每源一个 sink 闭包 + 全部写盘线程句柄。形状与 lib.rs 既有构造一致。
pub struct Wiring {
    pub sinks: Vec<(Source, Box<dyn FnMut(&[f32]) + Send>)>,
    pub joins: Vec<std::thread::JoinHandle<()>>,
}

/// 录制方案。做成**装配工厂**而非逐块转发的 accept:后者要两源共享一个 sink 对象
/// → Arc<Mutex<>> → 在绝不许阻塞的采集回调路径上加锁。工厂形态下每源仍是独立
/// 闭包 + 独立通道,方案差异只体现在"装配出什么",零锁。
pub trait RecordingSink: Send {
    fn into_wiring(self: Box<Self>) -> Wiring;
}

/// 方案 A:每源一条通道 + 一个写盘线程 + 一个 AudioTrackWriter。即现状。
pub struct DualTrackSink {
    note_dir: std::path::PathBuf,
    base_ms: u64,
    sources: Vec<Source>,
}

impl DualTrackSink {
    pub fn new(note_dir: &Path, base_ms: u64, sources: &[Source]) -> Self {
        Self { note_dir: note_dir.to_path_buf(), base_ms, sources: sources.to_vec() }
    }
}

impl RecordingSink for DualTrackSink {
    fn into_wiring(self: Box<Self>) -> Wiring {
        let mut sinks: Vec<(Source, Box<dyn FnMut(&[f32]) + Send>)> = Vec::new();
        let mut joins = Vec::new();
        for source in &self.sources {
            let (tx, rx) = crossbeam_channel::unbounded::<Vec<f32>>();
            let mut w = AudioTrackWriter::new(&self.note_dir, source.as_str(), self.base_ms);
            joins.push(std::thread::spawn(move || {
                for chunk in rx.iter() {
                    w.append(&chunk);
                }
                // sink 被 drop → 通道关闭 → w Drop 补头刷盘收尾。
            }));
            sinks.push((
                *source,
                Box::new(move |s: &[f32]| {
                    let _ = tx.send(s.to_vec());
                }) as Box<dyn FnMut(&[f32]) + Send>,
            ));
        }
        Wiring { sinks, joins }
    }
}

/// 方案 B:在方案 A 之上多挂一条混音轨。两源 sink 各把样本**再复制一份**发给混音
/// 线程,TimelineMixer 按位置合成后写 `mixed.wav`。
pub struct MixedSink {
    inner: DualTrackSink,
}

impl MixedSink {
    pub fn new(inner: DualTrackSink) -> Self {
        Self { inner }
    }
}

impl RecordingSink for MixedSink {
    fn into_wiring(self: Box<Self>) -> Wiring {
        let note_dir = self.inner.note_dir.clone();
        let base_ms = self.inner.base_ms;
        // 单源会话(record_system_only)无从混音:直接退化为方案 A,该笔记只有 A 可选。
        if self.inner.sources.len() < 2 {
            return Box::new(self.inner).into_wiring();
        }
        let mut w = Box::new(self.inner).into_wiring();

        let (tx, rx) = crossbeam_channel::unbounded::<(usize, Vec<f32>)>();
        w.joins.push(std::thread::spawn(move || {
            let mut mixer = TimelineMixer::new(DEFAULT_MARGIN_SAMPLES);
            let mut writer = AudioTrackWriter::new(&note_dir, "mixed", base_ms);
            for (src, chunk) in rx.iter() {
                let out = mixer.accept(src, &chunk);
                if !out.is_empty() {
                    writer.append(&out);
                }
            }
            // 两源 sink 都被 drop → 通道关闭 → 定稿窗内剩余,writer Drop 补头刷盘。
            let tail = mixer.finish();
            if !tail.is_empty() {
                writer.append(&tail);
            }
        }));

        for (source, sink) in w.sinks.iter_mut() {
            let idx = match source {
                Source::Mic => MIC,
                Source::System => SYSTEM,
            };
            let tx = tx.clone();
            let mut inner_sink = std::mem::replace(sink, Box::new(|_: &[f32]| {}));
            *sink = Box::new(move |s: &[f32]| {
                inner_sink(s);
                // 发送失败(混音线程已死)静默忽略:旁路绝不许影响源轨。
                let _ = tx.send((idx, s.to_vec()));
            });
        }
        drop(tx); // 原始 tx 必须丢弃,否则通道永不关闭、混音线程 join 永久阻塞
        w
    }
}

/// 按方案装配。mix=false 即退化为现状。
pub fn build_sinks(note_dir: &Path, base_ms: u64, sources: &[Source], mix: bool) -> Wiring {
    let dual = DualTrackSink::new(note_dir, base_ms, sources);
    if mix {
        Box::new(MixedSink::new(dual)).into_wiring()
    } else {
        Box::new(dual).into_wiring()
    }
}
```

在 `src-tauri/src/pipeline/mod.rs` 加一行:

```rust
pub mod recording_sink;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test recording_sink 2>&1 | tail -15`
Expected: `test result: ok. 3 passed`

若 `tempfile` 未在 dev-dependencies,先加:
Run: `cd src-tauri && cargo add --dev tempfile`

- [ ] **Step 5: 跑全量确认无回归**

Run: `cd src-tauri && cargo test 2>&1 | tail -5`
Expected: 全部 ok

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/pipeline/recording_sink.rs src-tauri/src/pipeline/mod.rs src-tauri/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(mix): build_sinks 统一装配录制期产物,方案 B 多挂一条混音轨

两源 sink 各把样本再复制一份发给混音线程,TimelineMixer 按位置合成后写 mixed.wav。
混音严格是旁路:通道满/线程死/写盘失败只影响 mixed.wav,源轨与转写热路径不受影响
(与 keep_audio 既有哲学一致)。单源会话(record_system_only)不建混音线程,该笔记
只有方案 A 可选。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: 接线进录制流程 + 设置开关

**Files:**
- Modify: `src-tauri/src/settings.rs`(`Settings` 结构体末尾加字段)
- Modify: `src-tauri/src/lib.rs:1197-1216`(替换裸 sink 构造为 `build_sinks`)
- Test: `src-tauri/src/settings.rs` 既有 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: Task 2 的 `pipeline::recording_sink::build_sinks`
- Produces: `Settings::mix_track: bool`(默认 `false`)

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/settings.rs` 的 tests 模块加:

```rust
#[test]
fn mix_track_defaults_false_and_old_files_parse() {
    // 旧配置文件无该字段,必须仍可解析(仓库既有约定:新字段 serde(default))
    let s: Settings = serde_json::from_str("{}").expect("旧文件应可解析");
    assert!(!s.mix_track, "默认关闭:方案 B 是实验特性,不改变现有用户行为");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test mix_track_defaults 2>&1 | tail -15`
Expected: 编译失败,`no field mix_track on type Settings`

- [ ] **Step 3: 写最小实现**

在 `src-tauri/src/settings.rs` 的 `Settings` 结构体里加字段(放在最后一个字段之后):

```rust
    /// 方案 B:录制期把两轨混成 mixed.wav。默认关——实验特性,开启后每分钟多约
    /// 1.9MB 磁盘(转码 m4a 后大幅缩小)。仅影响新录制,已有笔记不受影响。
    #[serde(default)]
    pub mix_track: bool,
```

若 `Settings` 未派生 `Default`,在其 `impl Default` 里补 `mix_track: false,`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test mix_track_defaults 2>&1 | tail -10`
Expected: `test result: ok. 1 passed`

- [ ] **Step 5: 替换 lib.rs 的 sink 构造**

把 `src-tauri/src/lib.rs` 中 `let mut audio_sinks: Vec<...> = Vec::new();` 到该
`if keep_audio { ... }` 块结束的整段(约 1197-1216 行)替换为:

```rust
        // 录制期产物装配移交 pipeline::recording_sink:mix 开启时多落一条 mixed.wav
        // (方案 B)。keep_audio=false 时两者皆空 Vec,行为与此前完全一致。
        let (audio_sinks, audio_joins) = if keep_audio {
            let srcs: Vec<Source> = sources.iter().map(|(s, _, _)| *s).collect();
            let w = pipeline::recording_sink::build_sinks(&note_dir, base_ms, &srcs, mix_track);
            (w.sinks, w.joins)
        } else {
            (Vec::new(), Vec::new())
        };
```

`mix_track` 与 `keep_audio` 同一次 settings load 读出。把 `src-tauri/src/lib.rs:765-766`
的解构改为:

```rust
        let (record_system_only, keep_audio, language_filter, keep_output_volume, mix_track) = (
            cfg.record_system_only,
            cfg.keep_audio,
            cfg.language_filter,
            cfg.keep_output_volume,
            cfg.mix_track,
        );
```

并把 758 行那条「一次性读设置」的注释里补上 `mix_track`。

- [ ] **Step 6: 编译并跑全量**

Run: `cd src-tauri && cargo test 2>&1 | tail -5`
Expected: 全部 ok,失败 0

Run: `npm run check`
Expected: 0 errors

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/settings.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(mix): 录制流程接入 build_sinks,新增 mix_track 设置(默认关)

lib.rs 里的裸 sink 构造移交 pipeline::recording_sink,keep_audio=false 时两者
皆空 Vec、行为与此前完全一致。mix_track 默认关:方案 B 是实验特性,不改变现有
用户行为;开启后每分钟多约 1.9MB 磁盘,仅影响新录制。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: 墙钟-样本对账落 audio.json

**Files:**
- Modify: `src-tauri/src/store/audio.rs`(`TrackMeta` 加字段 + 新增 `SyncInfo` 与 `set_track_sync`)
- Modify: `src-tauri/src/lib.rs`(停录收尾处调用)
- Test: `src-tauri/src/store/audio.rs` 既有 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `pipeline::frame_tap::HealthSnapshot`(既有,字段 `samples`/`silence_ms`/`gaps`/`rate_fixes`)
- Produces:
  - `pub struct SyncInfo { pub wall_ms: u64, pub samples: u64, pub drift_ms: i64, pub silence_ms: u64, pub gaps: u32, pub rate_fixes: u32 }`
  - `pub fn set_track_sync(note_dir: &Path, source: &str, info: SyncInfo) -> anyhow::Result<()>`
  - `TrackMeta::sync: Option<SyncInfo>`

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/store/audio.rs` 的 tests 模块加:

```rust
#[test]
fn sync_info_roundtrips_and_old_json_stays_valid() {
    let dir = tempfile::tempdir().unwrap();
    set_track_sync(
        dir.path(),
        "mic",
        SyncInfo { wall_ms: 60_000, samples: 959_000, drift_ms: -62, silence_ms: 0, gaps: 0, rate_fixes: 1 },
    )
    .unwrap();
    let meta = load_audio_meta(dir.path());
    let s = meta.tracks.get("mic").unwrap().sync.as_ref().expect("应已写入");
    assert_eq!(s.wall_ms, 60_000);
    assert_eq!(s.drift_ms, -62, "负值 = 该轨样本数不足墙钟,时钟跑慢");
    assert_eq!(s.rate_fixes, 1);
}

#[test]
fn sync_absent_serializes_to_old_shape() {
    // 未写 sync 的轨道,JSON 不该出现该键(新旧版本双向兼容,与 codec/waveform 同策略)
    let t = TrackMeta { offset_ms: 5, ..Default::default() };
    let j = serde_json::to_string(&t).unwrap();
    assert!(!j.contains("sync"), "无 sync 时不应序列化该键: {j}");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test sync_info 2>&1 | tail -15`
Expected: 编译失败,`cannot find struct SyncInfo`

- [ ] **Step 3: 写最小实现**

在 `src-tauri/src/store/audio.rs` 的 `CleanInfo` 定义之后加:

```rust
/// 墙钟-样本对账:该轨在本场录制里实际接受的样本数 vs 墙钟应有的样本数。
///
/// 为什么要落盘:回放侧的三种离线错位量法在 0.2~0.9s 区间互不吻合(见
/// player_align.rs 头注),分歧本身已达阈值量级,导致"连残余有多大都测不准"。
/// 录制期我们掌握真值——采集线程的样本计数与墙钟可直接对账,不需要估。有了这条
/// 基准才谈得上判定方案 A/B 孰优,以及反向标定那三种离线量法。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncInfo {
    /// 本场录制墙钟时长(ms)。
    pub wall_ms: u64,
    /// 该轨实际接受的样本数(16k 口径)。
    pub samples: u64,
    /// 漂移 = samples/16 − wall_ms。负值 = 该轨时钟跑慢(样本数不足墙钟)。
    pub drift_ms: i64,
    /// frame_tap 累计补的静音时长(ms)。
    pub silence_ms: u64,
    /// 帧荒次数。
    pub gaps: u32,
    /// 时钟核对改写采样率的次数(>0 说明该源声明的采样率与实测不符)。
    pub rate_fixes: u32,
}
```

在 `TrackMeta` 的 `clean` 字段之后加:

```rust
    /// 墙钟-样本对账(见 SyncInfo)。None = 该轨录制期未记录(旧笔记/中断)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncInfo>,
```

在 `set_track_clean_info` 函数之后加(照抄其锁与只改单字段的写法):

```rust
/// 写入某轨的墙钟-样本对账。全程持 audio.json 写锁;只改 sync 字段,保留其它。
pub fn set_track_sync(note_dir: &Path, source: &str, info: SyncInfo) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    meta.schema_version = 1;
    meta.tracks.entry(source.to_string()).or_default().sync = Some(info);
    save_audio_meta(note_dir, &meta)
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test sync_info 2>&1 | tail -10`
Expected: `test result: ok. 2 passed`

- [ ] **Step 5: `ActiveSession` 补 note_dir 字段**

停录路径只有 `note_id`,拿不到笔记目录。在 `src-tauri/src/lib.rs` 的 `ActiveSession`
结构体里,`health` 字段之后加:

```rust
    /// 笔记目录快照:停录时写墙钟-样本对账要用(该路径在 writer 移交前已确定,
    /// 见 start 处 `writer.dir()`)。
    note_dir: std::path::PathBuf,
```

在构造 `ActiveSession` 的地方(`lib.rs:1491` 附近,`audio_joins,` 一行旁)加:

```rust
                    note_dir: note_dir.clone(),
```

- [ ] **Step 6: 停录时写入对账**

在 `src-tauri/src/lib.rs:1645` 的 join 循环之后、`Some(s.note_id)` 之前插入:

```rust
    // 墙钟-样本对账:趁 health 计数仍在(会话拆除即丢弃),把真值落进 audio.json。
    // wall_ms 取本场会话净时长(扣暂停),base_ms 传 0——对账描述"这一场",不含历史累计。
    let wall_ms = active_elapsed_ms(
        s.started.elapsed(),
        s.paused_accum,
        s.paused_at.map(|p| p.elapsed()),
        0,
    );
    for (source, health) in &s.health {
        let h = health.snapshot(*source);
        let info = store::audio::SyncInfo {
            wall_ms,
            samples: h.samples,
            drift_ms: (h.samples / 16) as i64 - wall_ms as i64,
            silence_ms: h.silence_ms,
            gaps: h.gaps,
            rate_fixes: h.rate_fixes,
        };
        // 失败只记日志:排障数据缺失不是正确性问题,不该挡停录。
        if let Err(e) = store::audio::set_track_sync(&s.note_dir, source.as_str(), info) {
            eprintln!("对账写入失败({}): {e}", source.as_str());
        }
    }
```

> 注意:此处必须在 `for j in s.audio_joins` 之后——`s.audio_joins` 是按值移动的,
> 移动后 `s` 的其余字段仍可访问(部分移动),但顺序反了会让 WAV 头尚未收尾。

- [ ] **Step 7: 编译并跑全量**

Run: `cd src-tauri && cargo test 2>&1 | tail -5`
Expected: 全部 ok

- [ ] **Step 8: 提交**

```bash
git add src-tauri/src/store/audio.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(mix): 墙钟-样本对账落 audio.json——错位从此是读出来的,不是估出来的

回放侧三种离线量法在 0.2~0.9s 区间互不吻合(player_align.rs 头注),分歧已达
阈值量级,"连残余有多大都测不准"才是回放问题收不了尾的真因。录制期掌握真值:
采集线程的样本计数与墙钟直接对账。有了这条基准才谈得上判定方案 A/B 孰优,以及
反向标定那三种离线量法。

写入失败只记日志,不挡停录(排障数据缺失不是正确性问题)。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## 第一期验收

- [ ] `cargo test` 全绿;`npm run check` 0 错误
- [ ] 设置页 `mix_track` 关:录制产物与此前逐字节一致(`mic.wav` / `system.wav`,无 `mixed.wav`)
- [ ] 设置页 `mix_track` 开:三条轨齐全,`mixed.wav` 字节数与源轨一致
- [ ] 单源会话(`record_system_only`)开 `mix_track`:不产出 `mixed.wav`,不报错
- [ ] **真机冒烟**:录一场 ≥5 分钟的真实会议,停录后 `audio.json` 里两轨均有 `sync` 记录
- [ ] ~~**判据检查**:新录笔记两轨 `drift_ms` 绝对值 **< 20ms**(spec §度量 的验收线)~~
      **该判据已被 spec 修订取代,勾这一条时以两轨之差为准**:`|drift_ms(mic) −
      drift_ms(system)| < 20ms`。`drift_ms` 各自的绝对值含一段已知且不修的系统性偏置
      (启动窗等),不能直接当达标线;两轨相减可抵消大部分,且回放对齐关心的本就是两轨的
      相对关系。现行口径见 spec §度量「验收判据」与 `store/audio.rs` 的 `SyncInfo` 文档。

> 若 `drift_ms` 超过 20ms,先不要调混音器——那说明问题在 `frame_tap` 的率纠正或 AEC 的
> 10ms 整帧对齐,应回到 spec §已知限制 第 4 条重新评估该判据是否过严。

## 本期不做(留待二三期)

- `PlaybackSource` 接口与详情页切换按钮 → 第二期
- 离线补生成(从已有双轨生成 `mixed.wav`) → 第二期
- `TranscribeInput` 与文件重转写链路 → 第三期
- 成品轨模式的声纹降级口径落地 → 第三期(本期 `mixed.wav` 只落盘,无人消费)
