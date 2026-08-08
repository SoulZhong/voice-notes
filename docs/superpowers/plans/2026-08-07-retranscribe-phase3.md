# 文件重转写(三期)实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 从零建「读盘上音轨 → 离线切段 → 喂 Recognizer → 说话人归属 → 原子覆盖 segments」链路,双轨/成品轨双入口,详情页与 MCP 可发起,修复约 40 场被回声污染的历史笔记文字。

**Architecture:** 新模块 `src-tauri/src/retranscribe/`,纯逻辑(切段映射/回声去重/归属继承/提交门)与 IO(解码/落盘)分层,全部复用既有积木:`track_pcm` 解码、`SileroSegmenter` 切段、`Recognizer` 识别、`SpeakerRegistry::with_seeds` 只读声纹、`NoteLock` 互斥、tmp+rename 原子提交。lib.rs 起后台线程串行跑单任务,进度直发 `retranscribe` 事件;MCP 经 UDS 桥异步启动 + 轮询。

**Tech Stack:** Rust(cargo test,在 `src-tauri/` 下跑)/ Svelte 5 + TypeScript(`npm run check`、`npm test`)/ 既有 sherpa-onnx VAD、hound(测试写 WAV)。

## Global Constraints

- 设计依据:`docs/superpowers/specs/2026-08-07-retranscribe-phase3-design.md`(母 spec `2026-08-06-audio-scheme-ab-design.md` §TranscribeInput),冲突以三期 spec 为准。
- 分支 `feature/retranscribe-phase3`;工作目录仓库根;cargo 命令在 `src-tauri/` 下执行。
- **重转写全程只读声纹库**:不 `set_enroller`、不写种子、不参与库级自动归并(场内簇合并允许)。
- **破坏性覆盖三重保险**:一次性备份 `segments.orig.jsonl`(已存在不覆盖)+ 提交门(0 段或 `[识别失败]` 占位 >50% 整体放弃)+ tmp+rename 原子切换。
- 全程持 `NoteLock`(与录制/编辑/refine 同一把锁);全局同时只跑一个重转写任务。
- 段落 source 用字符串 `"mic"|"system"|"mixed"`,**不改** `audio::Source` 枚举。
- 样本恒 16k 单声道 f32;时间轴口径 `文件内毫秒 + offset_ms == 段时间轴毫秒`(1ms = 16 样本)。
- 阈值初值(spec 定,单测锁死):回声去重 重叠≥0.5 且 文本相似≥0.75;继承 重叠≥0.3;mixed 完整性容限 500ms;提交门 占位段 >50%。
- 代码注释风格:中文、讲约束与「为什么」,不讲流水账。前端文案一律进 i18n dict(zh+en 双写,有 noHardcodedCjk 测试哨兵)。
- 既有测试不许破坏:`cargo test` 全绿、`npm test` 全绿、`npm run check` 0 错误。
- 每个 Task 结尾提交,消息 `feat(retrans):` / `test(retrans):` / `docs(retrans):` 前缀,落款
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。

---

## File Structure

| 文件 | 职责 |
|---|---|
| `src-tauri/src/retranscribe/mod.rs`(新) | 编排:`run()` 串起 输入→识别→去重→归属→提交门→提交;`Summary` 摘要 |
| `src-tauri/src/retranscribe/input.rs`(新) | `PendingSegment` / `TranscribeInput` trait / `DualTrackInput` / `MixedInput` / `collect_track_segments`(纯)/ `mixed_untrusted` 完整性校验(纯) |
| `src-tauri/src/retranscribe/dedup.rs`(新) | 离线回声去重(纯函数) |
| `src-tauri/src/retranscribe/attribute.rs`(新) | `assign_clusters`(声纹归属)+ `finalize_speakers`(继承兜底 + speakers 表重建,纯) |
| `src-tauri/src/retranscribe/commit.rs`(新) | 备份 / 原子写 segments+speakers / 清抑制表 / 删 align.json / aing 标 stale |
| `src-tauri/src/session.rs`(改) | `normalize_text`/`text_similarity`/`overlap_fraction`/`is_foreign_final`/`split_final`/`SubFinal` 改 `pub(crate)` |
| `src-tauri/src/diar/registry.rs`(改) | 新增 `disable_seed_z()`(mixed 降级口径) |
| `src-tauri/src/store/refined.rs`(改) | `RefinedDoc` 新增 `stale: bool` |
| `src-tauri/src/refine/mod.rs`(改) | `run_local` 的 `RefinedDoc` 字面量补 `stale: false` |
| `src-tauri/src/ipc.rs`(改) | 新增 `RetranscribeEvent` |
| `src-tauri/src/lib.rs`(改) | `AppState.retranscribing` 槽、`do_retranscribe`/`spawn_retranscribe`、三个 command、refine/续录守卫补丁、注册 handler |
| `src-tauri/src/mcp/uds.rs`(改) | `retranscribe` / `retranscribe_status` 两个 op |
| `src-tauri/src/mcp/server.rs`(改) | 两个 MCP tool + catalog 条目 |
| `src/lib/events.ts`(改) | `RetranscribeEvent` + `onRetranscribe` |
| `src/lib/notes.ts`(改) | `retranscribeNote`/`retranscribeStatus`/`mixedInputStatus`;`RefinedDoc` 补 `stale` |
| `src/routes/notes/[id]/+page.svelte`(改) | 重转写按钮 + 来源确认区 + 进度 + stale 横幅 |
| `src/lib/i18n/dict/notes.ts`(改) | `notes.retrans.*` 文案(zh+en) |

---

### Task 1: `input.rs` — 离线切段与 mixed 完整性校验

**Files:**
- Create: `src-tauri/src/retranscribe/input.rs`
- Create: `src-tauri/src/retranscribe/mod.rs`(本 Task 只放 `pub mod input;`)
- Modify: `src-tauri/src/lib.rs`(模块声明区,`mod refine;` 一行附近加 `mod retranscribe;`)
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `crate::pipeline::segmenter::{Segment, Segmenter, MockSegmenter}`、`crate::store::transcode::track_pcm`、`crate::store::audio::{load_audio_meta, AudioMeta}`
- Produces(后续 Task 依赖):
  - `pub struct PendingSegment { pub source: String, pub samples: Vec<f32>, pub start_ms: u64, pub end_ms: u64 }`
  - `pub trait TranscribeInput { fn segments(&mut self) -> anyhow::Result<Vec<PendingSegment>>; }`
  - `pub struct DualTrackInput` / `pub struct MixedInput`,构造 `new(note_dir: PathBuf, make_segmenter: Box<dyn Fn() -> anyhow::Result<Box<dyn Segmenter>>>)`
  - `pub fn collect_track_segments(pcm: &[f32], offset_ms: u64, source: &str, seg: &mut dyn Segmenter) -> Vec<PendingSegment>`
  - `pub fn mixed_untrusted(meta: &AudioMeta) -> Option<String>`(None = 可信;Some(原因) = 拒绝)
  - `pub const MIXED_TOLERANCE_MS: u64 = 500;`

- [ ] **Step 1: 写失败测试**

新建 `src-tauri/src/retranscribe/input.rs`,末尾:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::segmenter::MockSegmenter;
    use crate::store::audio::{AudioMeta, SyncInfo, TrackMeta};

    /// 时间轴映射:段 start_ms = offset_ms + 样本位置/16,end_ms 按段长顺延。
    #[test]
    fn collect_maps_sample_positions_to_timeline_with_offset() {
        // MockSegmenter 每攒 1600 样本(100ms)出一段
        let mut seg = MockSegmenter::new(1600);
        let pcm = vec![0.1f32; 4000]; // 2 整段 + 800 尾样本(flush 后成第 3 段)
        let out = collect_track_segments(&pcm, 250, "mic", &mut seg);
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].start_ms, out[0].end_ms), (250, 350));
        assert_eq!((out[1].start_ms, out[1].end_ms), (350, 450));
        assert_eq!((out[2].start_ms, out[2].end_ms), (450, 500));
        assert!(out.iter().all(|s| s.source == "mic"));
        let total: usize = out.iter().map(|s| s.samples.len()).sum();
        assert_eq!(total, 4000, "切段不得丢样本");
    }

    /// 空 PCM:零段,不 panic。
    #[test]
    fn collect_empty_pcm_yields_nothing() {
        let mut seg = MockSegmenter::new(1600);
        assert!(collect_track_segments(&[], 0, "system", &mut seg).is_empty());
    }

    fn meta_with(mixed_dur: Option<u64>, mic_track: u64, sys_track: u64, sys_off: u64) -> AudioMeta {
        let sync = |track_ms: u64| SyncInfo {
            wall_ms: track_ms, samples: 1, track_ms, drift_ms: 0, silence_ms: 0, gaps: 0, rate_fixes: 0,
        };
        let mut m = AudioMeta::default();
        m.tracks.insert("mic".into(), TrackMeta { sync: Some(sync(mic_track)), ..Default::default() });
        m.tracks.insert("system".into(), TrackMeta {
            offset_ms: sys_off, sync: Some(sync(sys_track)), ..Default::default()
        });
        m.tracks.insert("mixed".into(), TrackMeta { duration_ms: mixed_dur, ..Default::default() });
        m
    }

    /// 可信:mixed 时长 ≈ max(offset+track_ms),容限内放行。
    #[test]
    fn mixed_trusted_when_duration_matches_source_syncs() {
        let m = meta_with(Some(60_400), 60_000, 59_000, 1_200); // max(60000, 60200)=60200,差 200ms
        assert_eq!(mixed_untrusted(&m), None);
    }

    /// 不可信:偏差超 500ms 容限 → 给出原因。
    #[test]
    fn mixed_untrusted_when_duration_diverges() {
        let m = meta_with(Some(50_000), 60_000, 59_000, 0);
        assert!(mixed_untrusted(&m).is_some());
    }

    /// 无 mixed 轨 / 无 duration / 源轨无 sync(旧笔记)→ 全部不可信(不可校验即不可信)。
    #[test]
    fn mixed_untrusted_when_unverifiable() {
        assert!(mixed_untrusted(&AudioMeta::default()).is_some(), "无 mixed 轨");
        let m = meta_with(None, 60_000, 60_000, 0);
        assert!(mixed_untrusted(&m).is_some(), "mixed 无实测时长");
        let mut m2 = meta_with(Some(60_000), 60_000, 60_000, 0);
        m2.tracks.get_mut("mic").unwrap().sync = None;
        assert!(mixed_untrusted(&m2).is_some(), "源轨无 sync 记录");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test retranscribe::input`(在 `src-tauri/`)
Expected: 编译失败(`collect_track_segments` 等未定义)

- [ ] **Step 3: 写实现**

`input.rs` 主体:

```rust
//! 文件重转写的输入侧:把盘上音轨解码-切段成待识别段。
//! 时间轴不变式:`文件内毫秒 + offset_ms == 段时间轴毫秒`(与 refine::slice_range 同源)。

use crate::pipeline::segmenter::Segmenter;
use crate::store::audio::AudioMeta;
use std::path::PathBuf;

/// 一条待识别段:16k 单声道样本 + 时间轴位置 + 归属源。
pub struct PendingSegment {
    pub source: String,
    pub samples: Vec<f32>,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// spec §TranscribeInput:产出待识别段。两实现共用 collect_track_segments,
/// 差别只在读哪些轨、source 标什么。
pub trait TranscribeInput {
    fn segments(&mut self) -> anyhow::Result<Vec<PendingSegment>>;
}

/// 每轨喂 VAD 的块长(200ms @16k)。逐块喂并逐块排空 take_finished:VAD 内部环形
/// 缓冲只有 30s,整轨一次性灌入而不排空会溢出丢段。
const FEED_CHUNK: usize = 3200;

/// 单轨 PCM → 切段 → 时间轴映射(纯逻辑,分段器注入,可用 MockSegmenter 单测)。
/// 分段器必须是每轨新实例:SileroSegmenter 内部是绝对样本号计数器,跨轨复用会串轴。
pub fn collect_track_segments(
    pcm: &[f32],
    offset_ms: u64,
    source: &str,
    seg: &mut dyn Segmenter,
) -> Vec<PendingSegment> {
    let mut raw = Vec::new();
    for chunk in pcm.chunks(FEED_CHUNK) {
        seg.accept(chunk);
        raw.extend(seg.take_finished());
    }
    seg.flush();
    raw.extend(seg.take_finished());
    raw.into_iter()
        .filter(|s| !s.samples.is_empty())
        .map(|s| {
            let start_ms = offset_ms + (s.start as u64) / 16;
            let end_ms = start_ms + (s.samples.len() as u64) / 16;
            PendingSegment { source: source.to_string(), samples: s.samples, start_ms, end_ms }
        })
        .collect()
}

pub type SegmenterFactory = Box<dyn Fn() -> anyhow::Result<Box<dyn Segmenter>> + Send>;

/// 双轨入口:mic/system 各自解码切段,Source 保真。轨缺失跳过(单轨笔记合法),
/// 两轨全缺才报错。逐轨串行加载,任意时刻至多一轨全场 PCM 常驻(照 refine::embed_all)。
pub struct DualTrackInput {
    note_dir: PathBuf,
    make_segmenter: SegmenterFactory,
}

impl DualTrackInput {
    pub fn new(note_dir: PathBuf, make_segmenter: SegmenterFactory) -> Self {
        Self { note_dir, make_segmenter }
    }
}

impl TranscribeInput for DualTrackInput {
    fn segments(&mut self) -> anyhow::Result<Vec<PendingSegment>> {
        let meta = crate::store::audio::load_audio_meta(&self.note_dir);
        let mut out = Vec::new();
        let mut found = false;
        for source in ["mic", "system"] {
            let Ok(pcm) = crate::store::transcode::track_pcm(&self.note_dir, source) else {
                continue; // 轨不存在/解码失败:单轨笔记合法,另一轨兜底
            };
            found = true;
            let offset_ms = meta.tracks.get(source).map(|t| t.offset_ms).unwrap_or(0);
            let mut seg = (self.make_segmenter)()?;
            out.extend(collect_track_segments(&pcm, offset_ms, source, seg.as_mut()));
        }
        if !found {
            anyhow::bail!("mic/system 音轨均不可读,无法重转写");
        }
        Ok(out)
    }
}

/// 成品轨入口:单轨,source 恒 "mixed"。调用方须先过 mixed_untrusted 校验。
pub struct MixedInput {
    note_dir: PathBuf,
    make_segmenter: SegmenterFactory,
}

impl MixedInput {
    pub fn new(note_dir: PathBuf, make_segmenter: SegmenterFactory) -> Self {
        Self { note_dir, make_segmenter }
    }
}

impl TranscribeInput for MixedInput {
    fn segments(&mut self) -> anyhow::Result<Vec<PendingSegment>> {
        let meta = crate::store::audio::load_audio_meta(&self.note_dir);
        let pcm = crate::store::transcode::track_pcm(&self.note_dir, "mixed")?;
        let offset_ms = meta.tracks.get("mixed").map(|t| t.offset_ms).unwrap_or(0);
        let mut seg = (self.make_segmenter)()?;
        Ok(collect_track_segments(&pcm, offset_ms, "mixed", seg.as_mut()))
    }
}

/// mixed 轨完整性校验容限。起流错峰残余同量级的初值,待实测校准(spec §MixedInput)。
pub const MIXED_TOLERANCE_MS: u64 = 500;

/// mixed 轨内容是否**不可信**(None = 可信)。一期明示 mixed 存在 ≠ 完整(回滚失败/
/// 混音线程 panic 后 Drop 补合法头,均无盘上标记),消费前拿源轨 sync.track_ms 交叉
/// 核对。口径:mixed 时长应 ≈ max(各源 offset_ms + track_ms)(后启动源带前导静音,
/// 见母 spec §口径差)。不可校验(缺任何一边读数)一律判不可信——拒绝不删不改。
pub fn mixed_untrusted(meta: &AudioMeta) -> Option<String> {
    let Some(mixed) = meta.tracks.get("mixed") else {
        return Some("没有成品轨(mixed)产物".into());
    };
    let Some(dur) = mixed.duration_ms.or_else(|| {
        // 未转码的 mixed.wav 没有实测 duration_ms:退而用 sync.track_ms(录制期对账值)
        mixed.sync.as_ref().map(|s| s.track_ms)
    }) else {
        return Some("成品轨缺少时长读数,无法校验完整性".into());
    };
    let mut expected: Option<u64> = None;
    for source in ["mic", "system"] {
        let Some(t) = meta.tracks.get(source) else { continue };
        let Some(sync) = &t.sync else {
            return Some(format!("源轨 {source} 无 sync 对账记录(旧笔记),无法校验成品轨"));
        };
        let end = t.offset_ms + sync.track_ms;
        expected = Some(expected.map_or(end, |e: u64| e.max(end)));
    }
    let Some(expected) = expected else {
        return Some("没有可对账的源轨,无法校验成品轨".into());
    };
    let diff = dur.abs_diff(expected);
    if diff > MIXED_TOLERANCE_MS {
        return Some(format!(
            "成品轨时长({dur}ms)与源轨对账值({expected}ms)偏差 {diff}ms,超 {MIXED_TOLERANCE_MS}ms 容限,内容不可信"
        ));
    }
    None
}
```

`mod.rs` 暂时只有:

```rust
//! 文件重转写(三期):离线读盘上音轨重跑 ASR,覆盖 segments。
//! spec: docs/superpowers/specs/2026-08-07-retranscribe-phase3-design.md
pub mod input;
```

lib.rs 模块声明区(`mod refine;` 附近)加一行 `mod retranscribe;`。

注意:若 `MockSegmenter::new(1600)` 的实际行为与测试假设不符(比如尾样本处理),
先读 `src-tauri/src/pipeline/segmenter.rs:23` 的实现,**调测试断言就实际行为**,
不改 MockSegmenter。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test retranscribe::input`
Expected: 全绿;再跑 `cargo test` 确认无既有破坏。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/retranscribe/ src-tauri/src/lib.rs
git commit -m "feat(retrans): TranscribeInput 双实现——离线切段时间轴映射与 mixed 完整性校验"
```

---

### Task 2: `dedup.rs` — 离线回声去重

**Files:**
- Create: `src-tauri/src/retranscribe/dedup.rs`
- Modify: `src-tauri/src/retranscribe/mod.rs`(加 `pub mod dedup;`)
- Modify: `src-tauri/src/session.rs`(`fn normalize_text`、`fn text_similarity`、`fn overlap_fraction` 三个函数前加 `pub(crate)`,函数体不动)
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `crate::session::{normalize_text, text_similarity, overlap_fraction}`
- Produces:
  - `pub struct DedupSeg<'a> { pub source: &'a str, pub start_ms: u64, pub end_ms: u64, pub text: &'a str }`
  - `pub fn echo_discards(segs: &[DedupSeg]) -> Vec<usize>`(返回应弃用的下标,恒为 mic 段)
  - `pub const ECHO_OVERLAP_MIN: f32 = 0.5;` / `pub const ECHO_SIM_MIN: f32 = 0.75;`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn seg(source: &str, start: u64, end: u64, text: &str) -> DedupSeg<'_> {
        DedupSeg { source, start_ms: start, end_ms: end, text }
    }

    /// 时间重叠 + 文本相同的 mic/system 对 → 弃 mic 侧。
    #[test]
    fn overlapping_identical_pair_drops_mic() {
        let segs = [
            seg("system", 1000, 5000, "今天讨论发布计划的三个风险点"),
            seg("mic", 1200, 5200, "今天讨论发布计划的三个风险点"),
        ];
        assert_eq!(echo_discards(&segs), vec![1]);
    }

    /// 文本相似但时间不重叠(隔了很久重复同一句)→ 不弃。
    #[test]
    fn similar_text_without_overlap_kept() {
        let segs = [
            seg("system", 1000, 3000, "今天讨论发布计划的三个风险点"),
            seg("mic", 60_000, 62_000, "今天讨论发布计划的三个风险点"),
        ];
        assert!(echo_discards(&segs).is_empty());
    }

    /// 时间重叠但文本不同(真双讲)→ 不弃。
    #[test]
    fn overlapping_different_text_kept() {
        let segs = [
            seg("system", 1000, 5000, "今天讨论发布计划的三个风险点"),
            seg("mic", 1200, 5200, "我觉得数据库迁移得再排期"),
        ];
        assert!(echo_discards(&segs).is_empty());
    }

    /// mic/mic 或 system/system 同源相似不互杀;mixed 模式(无 system 段)天然零弃用。
    #[test]
    fn same_source_and_mixed_never_dropped() {
        let segs = [
            seg("mic", 1000, 3000, "同一句话"),
            seg("mic", 1100, 3100, "同一句话"),
            seg("mixed", 1000, 3000, "同一句话"),
        ];
        assert!(echo_discards(&segs).is_empty());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test retranscribe::dedup`
Expected: 编译失败

- [ ] **Step 3: 写实现**

```rust
//! 离线回声去重:实时链路的回声去重靠墙钟 hold 计时(session.rs pending/recent 环),
//! 离线时全部数据已知,直接按时间轴重叠 + 文本相似判定跨轨重复。清洗后的 mic 轨
//! 大多已无回声,此层是兜底不是主力(spec §管线)。判中弃 mic 侧:system 路是
//! 数字信号原文,mic 路的重复必是声学回灌。

use crate::session::{normalize_text, overlap_fraction, text_similarity};

/// 时间重叠占比下限(相对较短一段)。初值,单测锁死;实测误杀/漏杀后与 SIM 一并校准。
pub const ECHO_OVERLAP_MIN: f32 = 0.5;
/// 归一化文本相似度下限。
pub const ECHO_SIM_MIN: f32 = 0.75;

pub struct DedupSeg<'a> {
    pub source: &'a str,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: &'a str,
}

/// 返回应弃用的段下标(恒为 mic 段)。O(mic × system),会议级段数(千段内)无压力。
pub fn echo_discards(segs: &[DedupSeg]) -> Vec<usize> {
    let systems: Vec<&DedupSeg> = segs.iter().filter(|s| s.source == "system").collect();
    if systems.is_empty() {
        return Vec::new();
    }
    let sys_norms: Vec<String> = systems.iter().map(|s| normalize_text(s.text)).collect();
    let mut out = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        if seg.source != "mic" {
            continue;
        }
        let norm = normalize_text(seg.text);
        if norm.is_empty() {
            continue;
        }
        let hit = systems.iter().zip(&sys_norms).any(|(sys, sys_norm)| {
            overlap_fraction(seg.start_ms, seg.end_ms, sys.start_ms, sys.end_ms) >= ECHO_OVERLAP_MIN
                && text_similarity(&norm, sys_norm) >= ECHO_SIM_MIN
        });
        if hit {
            out.push(i);
        }
    }
    out
}
```

注意:先读 `src-tauri/src/session.rs:104` 的 `text_similarity` 签名确认它吃的是
**已归一化**文本还是原文——若它内部自己 normalize,则本函数不必预归一化,删掉
`norm`/`sys_norms` 直接传原文,测试不变。`overlap_fraction`(session.rs:140)的
分母语义(相对较短段/相对并集)也以实读为准,必要时只调 `ECHO_OVERLAP_MIN` 注释,
不改这两个函数。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test retranscribe::dedup && cargo test session`
Expected: 全绿(session 既有测试不受可见性调整影响)

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/retranscribe/ src-tauri/src/session.rs
git commit -m "feat(retrans): 离线回声去重——时间轴重叠+文本相似替代实时 hold 计时"
```

---

### Task 3: registry z 开关 + `attribute.rs` 声纹归属

**Files:**
- Modify: `src-tauri/src/diar/registry.rs`
- Create: `src-tauri/src/retranscribe/attribute.rs`
- Modify: `src-tauri/src/retranscribe/mod.rs`(加 `pub mod attribute;`)
- Test: 两处 inline tests

**Interfaces:**
- Consumes: `crate::diar::registry::{SpeakerRegistry, SeedCluster, SpeakerInfo, ClusterSnapshot}`、`crate::diar::SpeakerEmbedder`
- Produces:
  - registry: `pub fn disable_seed_z(&mut self)`
  - `pub struct RecSeg { pub source: String, pub text: String, pub start_ms: u64, pub end_ms: u64, pub samples: Vec<f32>, pub rms: f32 }`
  - `pub fn assign_clusters(segs: &[RecSeg], embedder: &mut Option<Box<dyn SpeakerEmbedder>>, seeds: Vec<SeedCluster>, mixed: bool) -> (Vec<Option<String>>, Vec<SpeakerInfo>, Vec<ClusterSnapshot>)`

- [ ] **Step 1: registry 失败测试**

在 `src-tauri/src/diar/registry.rs` 既有测试模块里加(先看该文件测试模块名与既有
构造种子的辅助函数,照抄搭台方式;下面以直接构造 SeedCluster 为例):

```rust
/// mixed 降级口径(三期 spec):disable_seed_z 后,z 通道对种子簇整体关闭——
/// 裸分落在 [RAW_FLOOR, SEED_ASSIGN) 灰区的段不再可能经 z 命中种子;
/// 0.68 同信道快路不受影响。
#[test]
fn disable_seed_z_closes_z_channel_but_keeps_fast_path() {
    // 4 个种子人物凑足 SNORM_MIN_COHORT,cohort 统计可用,z 通道原本开着。
    // 主种子方向 e0;干扰种子方向远离(e1/e2/e3),对探针余弦≈0 → z 值巨大,
    // 未关开关时灰区裸分必经 z 命中。
    let dim = 8;
    let unit = |i: usize| { let mut v = vec![0.0f32; dim]; v[i] = 1.0; v };
    let seeds: Vec<SeedCluster> = (0..4).map(|i| SeedCluster {
        person: format!("P{i}"), name: format!("人{i}"),
        centroid: unit(i), count: 10, source: "mic".into(),
    }).collect();
    // 探针:与 P0 余弦 0.6(灰区:≥0.50 地板、<0.68 快路)
    let mut probe = vec![0.0f32; dim];
    probe[0] = 0.6; probe[4] = 0.8; // 0.6²+0.8²=1,单位向量
    let long = SEED_MIN_SAMPLES; // 过三闸①的段长

    let mut open = SpeakerRegistry::with_seeds(&[], &seeds);
    let hit_open = open.assign_tracked(&probe, "mic", long, "mic:0");
    let mut closed = SpeakerRegistry::with_seeds(&[], &seeds);
    closed.disable_seed_z();
    let hit_closed = closed.assign_tracked(&probe, "mic", long, "mic:0");

    assert!(hit_open.is_some(), "z 通道开着时灰区高 z 段应命中种子");
    assert!(hit_closed.is_none(), "disable_seed_z 后灰区段不得再经 z 命中");

    // 快路不受影响:裸分 ≥0.68 照常命中
    let mut strong = vec![0.0f32; dim];
    strong[0] = 0.9; strong[4] = (1.0f32 - 0.81).sqrt();
    let mut closed2 = SpeakerRegistry::with_seeds(&[], &seeds);
    closed2.disable_seed_z();
    assert!(closed2.assign_tracked(&strong, "mic", long, "mic:1").is_some());
}
```

(若断言与 registry 实际数值行为不符——如 0.6 裸分因干扰种子成为 best 之外——
微调探针数值让"灰区 + 高 z"成立,原则不变:同一探针,开关是唯一变量,命中结果反转。)

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test disable_seed_z`(在 `src-tauri/`)
Expected: 编译失败(方法不存在)

- [ ] **Step 3: registry 实现**

`SpeakerRegistry` 结构体加字段(new() 里初始化 `false`,`from_snapshot`/`with_seeds`
经 new 构造无需另改;若它们不经 new,同样补初始化):

```rust
    /// 三期 mixed 降级口径:关闭种子 z 通道(assign_inner 的 z_hit 整体短路)。
    /// mixed 单轨无信道可分,AS-Norm 的"跨信道归一化"前提不成立(spec §降级口径)。
    seed_z_disabled: bool,
```

```rust
    /// 关闭种子 AS-Norm z 通道(mixed 重转写用;默认开启,实时链路不受影响)。
    pub fn disable_seed_z(&mut self) {
        self.seed_z_disabled = true;
    }
```

`assign_inner` 里 `let z_hit = seed_eligible` 一行改为:

```rust
                        let z_hit = !self.seed_z_disabled
                            && seed_eligible
                            && !fast_hit
```

(其余条件原样保留。)

- [ ] **Step 4: 跑 registry 测试**

Run: `cargo test disable_seed_z && cargo test registry`
Expected: 全绿

- [ ] **Step 5: `attribute.rs` 失败测试(assign_clusters 部分)**

新建 `src-tauri/src/retranscribe/attribute.rs`,测试:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::diar::registry::SeedCluster;
    use crate::diar::SpeakerEmbedder;

    /// 脚本嵌入器:按段顺序吐预设向量。
    struct ScriptEmbedder(Vec<Vec<f32>>, usize);
    impl SpeakerEmbedder for ScriptEmbedder {
        fn embed(&mut self, _samples: &[f32]) -> anyhow::Result<Vec<f32>> {
            let v = self.0[self.1.min(self.0.len() - 1)].clone();
            self.1 += 1;
            Ok(v)
        }
    }

    fn rec(source: &str, start: u64, dur_ms: u64) -> RecSeg {
        RecSeg {
            source: source.into(), text: "x".into(), start_ms: start, end_ms: start + dur_ms,
            samples: vec![0.1; (dur_ms * 16) as usize], rms: 0.1,
        }
    }

    fn unit(i: usize, dim: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; dim]; v[i] = 1.0; v
    }

    /// 双轨模式:同信道种子裸分命中 → 段拿到关联库人物的簇;registry 无 enroller,
    /// speakers() 带 person。
    #[test]
    fn dual_mode_seed_hit_yields_owned_cluster() {
        let seeds = vec![SeedCluster {
            person: "P1".into(), name: "张三".into(), centroid: unit(0, 8), count: 10, source: "mic".into(),
        }];
        let segs = vec![rec("mic", 0, 3000)]; // 3s=48000 样本,过 SEED_MIN_SAMPLES
        let mut emb: Option<Box<dyn SpeakerEmbedder>> = Some(Box::new(ScriptEmbedder(vec![unit(0, 8)], 0)));
        let (clusters, infos, _snaps) = assign_clusters(&segs, &mut emb, seeds, false);
        let id = clusters[0].clone().expect("裸分 1.0 必命中种子");
        let info = infos.iter().find(|i| i.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("P1"));
        assert_eq!(info.name.as_deref(), Some("张三"));
    }

    /// 无 embedder:全段无簇,infos 空——降级由 finalize 的继承兜底,不 panic。
    #[test]
    fn missing_embedder_degrades_to_no_clusters() {
        let segs = vec![rec("mic", 0, 3000)];
        let mut emb: Option<Box<dyn SpeakerEmbedder>> = None;
        let (clusters, infos, snaps) = assign_clusters(&segs, &mut emb, vec![], false);
        assert_eq!(clusters, vec![None]);
        assert!(infos.is_empty() && snaps.is_empty());
    }

    /// mixed 模式:种子 source 被改写为 "mixed",同信道 0.68 快路对 mixed 段生效。
    #[test]
    fn mixed_mode_rewrites_seed_source_for_fast_path() {
        let seeds = vec![SeedCluster {
            person: "P1".into(), name: "张三".into(), centroid: unit(0, 8), count: 10, source: "mic".into(),
        }];
        let segs = vec![rec("mixed", 0, 3000)];
        let mut emb: Option<Box<dyn SpeakerEmbedder>> = Some(Box::new(ScriptEmbedder(vec![unit(0, 8)], 0)));
        let (clusters, infos, _snaps) = assign_clusters(&segs, &mut emb, seeds, true);
        let id = clusters[0].clone().expect("source 改写后 mixed 段应走同信道快路命中");
        assert_eq!(infos.iter().find(|i| i.id == id).unwrap().person.as_deref(), Some("P1"));
    }
}
```

- [ ] **Step 6: 跑测试确认失败**

Run: `cargo test retranscribe::attribute`
Expected: 编译失败

- [ ] **Step 7: `assign_clusters` 实现**

```rust
//! 说话人归属(离线):复用实时链路的 SpeakerRegistry 在线聚类,但**只读声纹库**——
//! 不设 enroller、不 enroll_pending、不把结果回写库(三期 spec §偏离①:历史笔记的
//! 旧音频已污染过一轮库,重转写结果回写等于二次污染)。

use crate::diar::registry::{ClusterSnapshot, SeedCluster, SpeakerInfo, SpeakerRegistry};
use crate::diar::SpeakerEmbedder;

pub struct RecSeg {
    pub source: String,
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub samples: Vec<f32>,
    pub rms: f32,
}

/// 逐段嵌入 + 在线归簇(段须已按时间排序,在线聚类对顺序敏感)。收尾统一套用场内
/// 合并映射。mixed=true 时:种子 source 改写为 "mixed"(0.68 同信道快路对全部种子
/// 生效)+ 关闭 z 通道(spec §降级口径)。
pub fn assign_clusters(
    segs: &[RecSeg],
    embedder: &mut Option<Box<dyn SpeakerEmbedder>>,
    seeds: Vec<SeedCluster>,
    mixed: bool,
) -> (Vec<Option<String>>, Vec<SpeakerInfo>, Vec<ClusterSnapshot>) {
    let Some(embedder) = embedder.as_mut() else {
        return (vec![None; segs.len()], Vec::new(), Vec::new());
    };
    let seeds: Vec<SeedCluster> = if mixed {
        seeds.into_iter()
            .map(|s| SeedCluster { source: "mixed".into(), ..s })
            .collect()
    } else {
        seeds
    };
    let mut registry = SpeakerRegistry::with_seeds(&[], &seeds);
    if mixed {
        registry.disable_seed_z();
    }
    let mut clusters: Vec<Option<String>> = Vec::with_capacity(segs.len());
    for seg in segs {
        let seg_key = format!("{}:{}", seg.source, seg.start_ms);
        let assigned = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            embedder.embed(&seg.samples)
        })) {
            Ok(Ok(emb)) => registry.assign_tracked(&emb, &seg.source, seg.samples.len(), &seg_key),
            Ok(Err(err)) => {
                eprintln!("重转写声纹提取失败({}:{}ms): {err}", seg.source, seg.start_ms);
                None
            }
            Err(_) => {
                eprintln!("重转写声纹提取 panic({}:{}ms),该段无标签", seg.source, seg.start_ms);
                None
            }
        };
        clusters.push(assigned);
    }
    // 场内簇合并:把已分配的 loser id 统一映射到 winner(传递闭包)。
    let merges = registry.take_merges();
    if !merges.is_empty() {
        let resolve = |id: &str| -> String {
            let mut cur = id.to_string();
            // 链最长不过合并次数,防环兜底按次数上限走
            for _ in 0..merges.len() {
                match merges.iter().find(|(loser, _)| *loser == cur) {
                    Some((_, winner)) => cur = winner.clone(),
                    None => break,
                }
            }
            cur
        };
        for c in clusters.iter_mut() {
            if let Some(id) = c {
                *c = Some(resolve(id));
            }
        }
    }
    (clusters, registry.speakers(), registry.snapshot())
}
```

`SeedCluster` 若未派生 `Clone`(struct update 语法 `..s` 按字段 move,person/name/
centroid 均为拥有值,`..s` 直接可用,无需 Clone)。若编译器另有意见,逐字段手写构造。

- [ ] **Step 8: 跑测试确认通过**

Run: `cargo test retranscribe::attribute`
Expected: 全绿(数值断言若因 registry 阈值细节不符,微调测试向量,判定逻辑不动)

- [ ] **Step 9: 提交**

```bash
git add src-tauri/src/diar/registry.rs src-tauri/src/retranscribe/
git commit -m "feat(retrans): 离线声纹归属——只读种子命中,mixed 降级(z 关/同信道快路)"
```

---

### Task 4: `attribute.rs` — 继承兜底与 speakers 表重建

**Files:**
- Modify: `src-tauri/src/retranscribe/attribute.rs`
- Test: 同文件 tests 追加

**Interfaces:**
- Consumes: Task 3 的 `RecSeg`;`crate::store::{SegmentRecord, SpeakerMeta}`;`crate::session::overlap_fraction`;`SpeakerInfo`/`ClusterSnapshot`
- Produces:
  - `pub const INHERIT_OVERLAP_MIN: f32 = 0.3;`
  - `pub struct FinalizeStats { pub seed_matched: usize, pub inherited: usize }`
  - `pub fn finalize_speakers(segs: &[RecSeg], clusters: &[Option<String>], infos: &[SpeakerInfo], snaps: &[ClusterSnapshot], old_segs: &[SegmentRecord], old_speakers: &BTreeMap<String, SpeakerMeta>) -> (Vec<Option<String>>, BTreeMap<String, SpeakerMeta>, FinalizeStats)`

**规则(spec §说话人归属,逐条落测试):**

1. 簇关联库人物(`info.person.is_some()`)→ 段保留簇 id(seed_matched)。
2. 其余段(无簇 / 簇无 person)→ 继承兜底:在旧 segments 里找时间重叠最大的段,
   同 source 优先(mixed 段与任意 source 比);要求 `overlap_fraction ≥ INHERIT_OVERLAP_MIN`
   且旧段 speaker 的旧表记录**有人工价值**(name 非空 或 person_id 有值)。命中 → 段用
   继承 id(inherited)。
3. 都不满足 → 段保留簇 id(可能 None)。
4. speakers 表:被使用的簇 id 建 `SpeakerMeta{name: info.name 或空串, sources: info.sources,
   centroid: snap.centroid, count: snap.count, person_id: info.person}`;被继承的旧 id:
   若旧 meta.person_id 与某个已用簇的 person 相同 → **并入该簇 id**(同一人不开两行,
   相应段的 speaker 重写为簇 id);否则**重编号**为 `S<max+1>`(max 跨已用簇 id 与已
   分配新号取数字最大,防与新簇撞号),meta 从旧表整份拷贝(name/person_id/sources/
   centroid/count)。

- [ ] **Step 1: 写失败测试**

tests 模块追加:

```rust
    use crate::store::{SegmentRecord, SpeakerMeta};
    use std::collections::BTreeMap;

    fn old_seg(seq: u64, source: &str, start: u64, end: u64, speaker: Option<&str>) -> SegmentRecord {
        SegmentRecord {
            seq, source: source.into(), text: "旧".into(), start_ms: start, end_ms: end,
            speaker: speaker.map(String::from), rms: None,
        }
    }

    fn named_meta(name: &str, person: Option<&str>) -> SpeakerMeta {
        SpeakerMeta {
            name: name.into(), sources: vec!["mic".into()], centroid: Some(vec![1.0]),
            count: 5, person_id: person.map(String::from),
        }
    }

    fn info(id: &str, person: Option<&str>, name: Option<&str>) -> SpeakerInfo {
        SpeakerInfo {
            id: id.into(), sources: std::collections::BTreeSet::from(["mic".to_string()]),
            person: person.map(String::from), name: name.map(String::from),
        }
    }

    fn snap(id: &str) -> ClusterSnapshot {
        ClusterSnapshot {
            id: id.into(), centroid: vec![1.0], count: 3,
            sources: std::collections::BTreeSet::from(["mic".to_string()]),
            person: None, total_ms: 5000,
        }
    }

    /// 撞号:新簇 S1(无主)与旧 S1(张三)是不同人——继承段的旧 S1 必须重编号,
    /// 不得吞并新簇 S1,speakers 表两行并存且 meta 各归各。
    #[test]
    fn inherited_old_id_renumbered_on_collision() {
        let segs = vec![rec("mic", 0, 2000), rec("mic", 10_000, 2000)];
        // 段0 归新簇 S1(无 person);段1 无簇
        let clusters = vec![Some("S1".to_string()), None];
        let infos = vec![info("S1", None, None)];
        let snaps = vec![snap("S1")];
        let old_segs = vec![old_seg(1, "mic", 10_000, 12_000, Some("S1"))];
        let old_speakers = BTreeMap::from([("S1".to_string(), named_meta("张三", None))]);
        let (speakers, table, stats) =
            finalize_speakers(&segs, &clusters, &infos, &snaps, &old_segs, &old_speakers);
        assert_eq!(speakers[0].as_deref(), Some("S1"), "新簇段保留簇 id");
        let inherited = speakers[1].clone().expect("重叠继承应命中");
        assert_ne!(inherited, "S1", "旧 S1 与新簇 S1 撞号必须重编号");
        assert_eq!(table[&inherited].name, "张三");
        assert_eq!(table["S1"].name, "");
        assert_eq!((stats.seed_matched, stats.inherited), (0, 1));
    }

    /// 同人合流:旧说话人关联的 person 与种子命中簇相同 → 复用簇 id,不开第二行。
    #[test]
    fn inherited_speaker_unified_with_seed_cluster_by_person() {
        let segs = vec![rec("mic", 0, 3000), rec("mic", 10_000, 1000)];
        // 段0 种子命中簇 S1(person P7);段1 无簇(短段)
        let clusters = vec![Some("S1".to_string()), None];
        let infos = vec![info("S1", Some("P7"), Some("张三"))];
        let snaps = vec![snap("S1")];
        let old_segs = vec![old_seg(1, "mic", 10_000, 11_000, Some("S9"))];
        let old_speakers = BTreeMap::from([("S9".to_string(), named_meta("张三", Some("P7")))]);
        let (speakers, table, stats) =
            finalize_speakers(&segs, &clusters, &infos, &snaps, &old_segs, &old_speakers);
        assert_eq!(speakers[1].as_deref(), Some("S1"), "同 person 并入种子簇");
        assert_eq!(table.len(), 1);
        assert_eq!(table["S1"].person_id.as_deref(), Some("P7"));
        assert_eq!((stats.seed_matched, stats.inherited), (1, 1));
    }

    /// 无人工价值的旧归属(name 空且无 person)不继承——继承是保人工劳动,不是保编号。
    #[test]
    fn valueless_old_speaker_not_inherited() {
        let segs = vec![rec("mic", 0, 2000)];
        let clusters = vec![None];
        let old_segs = vec![old_seg(1, "mic", 0, 2000, Some("S3"))];
        let old_speakers = BTreeMap::from([("S3".to_string(), named_meta("", None))]);
        let (speakers, table, stats) =
            finalize_speakers(&segs, &clusters, &[], &[], &old_segs, &old_speakers);
        assert_eq!(speakers[0], None);
        assert!(table.is_empty());
        assert_eq!(stats.inherited, 0);
    }

    /// 重叠不足 30% 不继承;同 source 候选优先于跨 source。
    #[test]
    fn overlap_threshold_and_same_source_priority() {
        let segs = vec![rec("mic", 0, 4000)];
        let clusters = vec![None];
        let old_segs = vec![
            old_seg(1, "mic", 3800, 8000, Some("S1")),    // 与新段仅重叠 200ms/4000ms=5%
            old_seg(2, "system", 0, 4000, Some("S2")),    // 100% 重叠但跨 source
            old_seg(3, "mic", 500, 4000, Some("S3")),     // 同 source 87.5% 重叠
        ];
        let old_speakers = BTreeMap::from([
            ("S1".to_string(), named_meta("甲", None)),
            ("S2".to_string(), named_meta("乙", None)),
            ("S3".to_string(), named_meta("丙", None)),
        ]);
        let (speakers, table, _) =
            finalize_speakers(&segs, &clusters, &[], &[], &old_segs, &old_speakers);
        let id = speakers[0].clone().expect("87.5% 重叠应继承");
        assert_eq!(table[&id].name, "丙", "同 source 最大重叠者胜出");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test retranscribe::attribute`
Expected: 编译失败(`finalize_speakers` 未定义)

- [ ] **Step 3: 写实现**

```rust
use crate::store::{SegmentRecord, SpeakerMeta};
use std::collections::BTreeMap;

/// 继承兜底的时间重叠下限(相对判定口径同 overlap_fraction)。
pub const INHERIT_OVERLAP_MIN: f32 = 0.3;

#[derive(Debug, Default)]
pub struct FinalizeStats {
    pub seed_matched: usize,
    pub inherited: usize,
}

/// 归属定稿:种子命中保簇,未命中按时间重叠继承旧人工归属,并重建 speakers 表。
/// 规则见本文件头与三期 spec §说话人归属;撞号/同人合流的取舍在测试里逐条锁死。
pub fn finalize_speakers(
    segs: &[RecSeg],
    clusters: &[Option<String>],
    infos: &[SpeakerInfo],
    snaps: &[ClusterSnapshot],
    old_segs: &[SegmentRecord],
    old_speakers: &BTreeMap<String, SpeakerMeta>,
) -> (Vec<Option<String>>, BTreeMap<String, SpeakerMeta>, FinalizeStats) {
    let info_by_id: BTreeMap<&str, &SpeakerInfo> = infos.iter().map(|i| (i.id.as_str(), i)).collect();
    let snap_by_id: BTreeMap<&str, &ClusterSnapshot> = snaps.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut stats = FinalizeStats::default();

    // 第一遍:每段定成三种之一——Cluster(簇 id)/ Inherit(旧 speaker id)/ None。
    enum Pick { Cluster(String), Inherit(String), Nothing }
    let picks: Vec<Pick> = segs.iter().zip(clusters).map(|(seg, cluster)| {
        if let Some(id) = cluster {
            if info_by_id.get(id.as_str()).is_some_and(|i| i.person.is_some()) {
                stats.seed_matched += 1;
                return Pick::Cluster(id.clone());
            }
        }
        // 继承候选:同 source 优先(mixed 与任意 source 比),取重叠占比最大者。
        let candidate = |same_source: bool| {
            old_segs.iter()
                .filter(|o| o.speaker.is_some())
                .filter(|o| !same_source || o.source == seg.source || seg.source == "mixed")
                .filter(|o| same_source || true)
                .map(|o| (crate::session::overlap_fraction(seg.start_ms, seg.end_ms, o.start_ms, o.end_ms), o))
                .filter(|(f, _)| *f >= INHERIT_OVERLAP_MIN)
                .max_by(|(a, _), (b, _)| a.total_cmp(b))
                .map(|(_, o)| o)
        };
        let hit = candidate(true).or_else(|| candidate(false));
        if let Some(old) = hit {
            let sid = old.speaker.as_ref().unwrap();
            if old_speakers.get(sid).is_some_and(|m| !m.name.is_empty() || m.person_id.is_some()) {
                stats.inherited += 1;
                return Pick::Inherit(sid.clone());
            }
        }
        match cluster {
            Some(id) => Pick::Cluster(id.clone()),
            None => Pick::Nothing,
        }
    }).collect();

    // 第二遍:定 id 映射并建表。簇 id 原样;继承 id 先尝试按 person 并入已用簇,
    // 否则重编号避让(跨新簇与已分配号取 max)。
    let used_clusters: std::collections::BTreeSet<&str> = picks.iter()
        .filter_map(|p| match p { Pick::Cluster(id) => Some(id.as_str()), _ => None })
        .collect();
    let mut table: BTreeMap<String, SpeakerMeta> = BTreeMap::new();
    for id in &used_clusters {
        let info = info_by_id.get(id);
        let snap = snap_by_id.get(id);
        table.insert(id.to_string(), SpeakerMeta {
            name: info.and_then(|i| i.name.clone()).unwrap_or_default(),
            sources: info.map(|i| i.sources.iter().cloned().collect()).unwrap_or_default(),
            centroid: snap.map(|s| s.centroid.clone()),
            count: snap.map(|s| s.count).unwrap_or(0),
            person_id: info.and_then(|i| i.person.clone()),
        });
    }
    let numeric = |s: &str| s.strip_prefix('S').and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    let mut next = table.keys().map(|k| numeric(k)).max().unwrap_or(0) + 1;
    let mut inherit_map: BTreeMap<String, String> = BTreeMap::new();
    for p in &picks {
        let Pick::Inherit(old_id) = p else { continue };
        if inherit_map.contains_key(old_id) {
            continue;
        }
        let old_meta = &old_speakers[old_id];
        // 同人合流:旧归属关联的库人物已被某个簇命中 → 直接用那个簇 id。
        let unified = old_meta.person_id.as_ref().and_then(|pid| {
            table.iter().find(|(_, m)| m.person_id.as_deref() == Some(pid)).map(|(k, _)| k.clone())
        });
        let new_id = match unified {
            Some(id) => id,
            None => {
                let id = format!("S{next}");
                next += 1;
                table.insert(id.clone(), old_meta.clone());
                id
            }
        };
        inherit_map.insert(old_id.clone(), new_id);
    }
    let speakers = picks.iter().map(|p| match p {
        Pick::Cluster(id) => Some(id.clone()),
        Pick::Inherit(old_id) => Some(inherit_map[old_id].clone()),
        Pick::Nothing => None,
    }).collect();
    (speakers, table, stats)
}
```

(`candidate` 闭包里 `filter(|o| same_source || true)` 是赘笔——实现时删掉,
写成一次 filter:`!same_source || o.source == seg.source || seg.source == "mixed"`。)

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test retranscribe::attribute`
Expected: 全绿

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/retranscribe/attribute.rs
git commit -m "feat(retrans): 归属定稿——种子保簇/时间重叠继承人工归属/speakers 表重建"
```

---

### Task 5: `RefinedDoc.stale` + `commit.rs` 原子提交

**Files:**
- Modify: `src-tauri/src/store/refined.rs`
- Modify: `src-tauri/src/refine/mod.rs`(`run_local` 的 RefinedDoc 字面量)
- Create: `src-tauri/src/retranscribe/commit.rs`
- Modify: `src-tauri/src/retranscribe/mod.rs`(加 `pub mod commit;`)
- Test: `commit.rs` inline tests(tempfile 已是 dev-dep,若无则加)

**Interfaces:**
- Consumes: `crate::store::notelock::NoteLock`、`crate::store::{SegmentRecord, SpeakerMeta, SEGMENT_SUPPRESSIONS_FILE, write_speakers_atomic}`、`crate::store::refined::{load_refined_locked, write_refined_atomic_locked}`、`crate::store::align::ALIGN_FILE`(确认可见性,必要时改 `pub`)
- Produces:
  - `RefinedDoc` 新字段 `#[serde(default)] pub stale: bool`
  - `pub const SEGMENTS_BACKUP_FILE: &str = "segments.orig.jsonl";`
  - `pub fn commit(note_dir: &Path, lock: &NoteLock, segs: &[SegmentRecord], speakers: &BTreeMap<String, SpeakerMeta>) -> anyhow::Result<()>`

- [ ] **Step 1: 写失败测试**

`commit.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::notelock::NoteLock;
    use crate::store::{SegmentRecord, SpeakerMeta};
    use std::collections::BTreeMap;

    fn seg(seq: u64, text: &str) -> SegmentRecord {
        SegmentRecord {
            seq, source: "mic".into(), text: text.into(), start_ms: 0, end_ms: 1000,
            speaker: Some("S1".into()), rms: Some(0.1),
        }
    }

    fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("segments.jsonl"),
            "{\"seq\":1,\"source\":\"mic\",\"text\":\"旧文本\",\"start_ms\":0,\"end_ms\":500,\"speaker\":null}\n").unwrap();
        std::fs::write(dir.path().join("segment-suppressions.jsonl"), "{\"seq\":1,\"reason\":\"echo\"}\n").unwrap();
        std::fs::write(dir.path().join("align.json"), "{}").unwrap();
        dir
    }

    /// 首次提交:备份旧稿、原子写新段与说话人表、清抑制表、删 align。
    #[test]
    fn commit_backs_up_then_replaces_everything() {
        let dir = setup();
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        let speakers = BTreeMap::from([("S1".to_string(), SpeakerMeta {
            name: "张三".into(), sources: vec!["mic".into()], centroid: None, count: 0, person_id: None,
        })]);
        commit(dir.path(), &lock, &[seg(1, "新文本")], &speakers).unwrap();
        let backup = std::fs::read_to_string(dir.path().join(SEGMENTS_BACKUP_FILE)).unwrap();
        assert!(backup.contains("旧文本"), "备份应是重转写前的原稿");
        let new = std::fs::read_to_string(dir.path().join("segments.jsonl")).unwrap();
        assert!(new.contains("新文本") && !new.contains("旧文本"));
        assert!(std::fs::read_to_string(dir.path().join("speakers.json")).unwrap().contains("张三"));
        assert!(!dir.path().join("segment-suppressions.jsonl").exists(), "seq 全变,抑制表必须清");
        assert!(!dir.path().join("align.json").exists(), "旧时基映射不再适用");
    }

    /// 二次提交:备份不被覆盖——保的是「最初的原始转写」,不是上一轮重转写结果。
    #[test]
    fn backup_is_write_once() {
        let dir = setup();
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        commit(dir.path(), &lock, &[seg(1, "第一轮")], &BTreeMap::new()).unwrap();
        commit(dir.path(), &lock, &[seg(1, "第二轮")], &BTreeMap::new()).unwrap();
        let backup = std::fs::read_to_string(dir.path().join(SEGMENTS_BACKUP_FILE)).unwrap();
        assert!(backup.contains("旧文本"), "备份恒为最初原稿");
        assert!(std::fs::read_to_string(dir.path().join("segments.jsonl")).unwrap().contains("第二轮"));
    }

    /// 有 aing.json 时:标 stale + revision 进位(过期编辑器会话保存必冲突);无则不创建。
    #[test]
    fn aing_doc_marked_stale_with_revision_bump() {
        let dir = setup();
        // 用生产写入口造一份合法 aing.json(revision=3)
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        let mut doc = crate::store::refined::RefinedDoc {
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "t".into(), llm_model: None,
            stages: crate::store::refined::RefineStages {
                filter: "done".into(), recluster: "done".into(), llm: "off".into(),
                entities: "off".into(), relations: "off".into(),
            },
            discarded_seqs: vec![], entities: vec![], graph_extraction: None,
            relations: vec![], graph_support_mentions: vec![], revision: 3,
            stale: false, paragraphs: vec![],
        };
        crate::store::refined::write_refined_atomic_locked(dir.path(), &doc, &lock).unwrap();
        commit(dir.path(), &lock, &[seg(1, "新")], &BTreeMap::new()).unwrap();
        doc = crate::store::refined::load_refined_locked(dir.path(), &lock).unwrap();
        assert!(doc.stale);
        assert!(doc.revision >= 4, "revision 必须进位: {}", doc.revision);

        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(dir2.path().join("segments.jsonl"), "").unwrap();
        let lock2 = NoteLock::acquire(dir2.path()).unwrap().unwrap();
        commit(dir2.path(), &lock2, &[seg(1, "新")], &BTreeMap::new()).unwrap();
        assert!(crate::store::refined::load_refined_locked(dir2.path(), &lock2).is_none(),
            "从未 Aing 过的笔记不该被 commit 凭空造出 aing.json");
    }
}
```

(`load_refined_locked`/`write_refined_atomic_locked` 的真实签名以 `store/refined.rs`
实读为准——`run_local` 的调用方式是权威样板;若它们要求 note_id 形态的目录名,
tempdir 内建一层子目录 `notes/<id>` 再操作。`RefinedDoc` 字面量若还有本计划未列的
新字段,补默认值即可。)

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test retranscribe::commit`
Expected: 编译失败(`stale` 字段与 `commit` 未定义)

- [ ] **Step 3: 写实现**

`store/refined.rs` 的 `RefinedDoc` 在 `revision` 字段后加:

```rust
    /// 段落已被重转写整体替换:本稿引用的 source_seqs/文本基于旧段,内容过期。
    /// UI 据此提示「重新 Aing」;下一次 run_local 整写新稿时自然回 false。
    #[serde(default)]
    pub stale: bool,
```

`refine/mod.rs` `run_local` 的 `RefinedDoc { ... }` 字面量加 `stale: false,`;
全仓 `cargo build` 报出的其余 RefinedDoc 字面量构造点(含测试)一并补齐。

`commit.rs`:

```rust
//! 重转写的提交侧。破坏性覆盖三重保险(spec §提交与安全网):一次性备份、
//! 调用方提交门(本文件不管)、tmp+rename 原子切换。全部操作要求调用方持有
//! NoteLock——签名收 &NoteLock 是类型层面的持锁证明(照 write_refined_atomic_locked)。

use crate::store::notelock::NoteLock;
use crate::store::{SegmentRecord, SpeakerMeta};
use std::collections::BTreeMap;
use std::path::Path;

/// 首次重转写前的原稿备份。write-once:保「最初的原始转写」,不随后续轮次滚动。
pub const SEGMENTS_BACKUP_FILE: &str = "segments.orig.jsonl";

pub fn commit(
    note_dir: &Path,
    lock: &NoteLock,
    segs: &[SegmentRecord],
    speakers: &BTreeMap<String, SpeakerMeta>,
) -> anyhow::Result<()> {
    let live = note_dir.join("segments.jsonl");
    let backup = note_dir.join(SEGMENTS_BACKUP_FILE);
    if live.exists() && !backup.exists() {
        std::fs::copy(&live, &backup)?;
    }
    // tmp+rename(与 notes.rs write_jsonl_atomic 同哲学;那边收 JsonlLine 私有类型,
    // 这里整表全新生成、无损坏行保留需求,不共用)。
    let tmp = note_dir.join("segments.jsonl.tmp");
    let mut out = String::new();
    for s in segs {
        out.push_str(&serde_json::to_string(s)?);
        out.push('\n');
    }
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &live)?;
    crate::store::write_speakers_atomic(note_dir, speakers)?;
    // seq 全变:抑制表按旧 seq 隐藏会误伤新段,整个清掉;align.json 是旧 mic 时间戳的
    // 展示侧纠正,新段时间戳直接来自离线切段,旧映射不再适用。删除失败不致命(仅
    // 影响展示),不挡提交。
    let _ = std::fs::remove_file(note_dir.join(crate::store::SEGMENT_SUPPRESSIONS_FILE));
    let _ = std::fs::remove_file(note_dir.join(crate::store::align::ALIGN_FILE));
    // aing 标失效:revision 显式进位(与 run_local 同一套语义——过期编辑器会话的
    // 保存必因 revision 不匹配而冲突,不留 rev 窗口)。
    if let Some(mut doc) = crate::store::refined::load_refined_locked(note_dir, lock) {
        doc.stale = true;
        doc.revision = doc.revision.saturating_add(1);
        crate::store::refined::write_refined_atomic_locked(note_dir, &doc, lock)?;
    }
    Ok(())
}
```

可见性核对:`SEGMENT_SUPPRESSIONS_FILE` 已 `pub`(store/mod.rs:46);
`write_speakers_atomic` 是 `pub(crate)`(同 crate 可用);`store::align::ALIGN_FILE`
与 `load_refined_locked`/`write_refined_atomic_locked` 若非 crate 可见,把可见性
提到 `pub(crate)`(只动可见性,不动逻辑)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test retranscribe::commit && cargo test refined && cargo test refine`
Expected: 全绿

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/retranscribe/ src-tauri/src/store/refined.rs src-tauri/src/refine/mod.rs src-tauri/src/store/align.rs
git commit -m "feat(retrans): 原子提交——一次性备份/整表替换/清抑制表/aing 标 stale"
```

---

### Task 6: `mod.rs` — 编排 `run()` 与提交门

**Files:**
- Modify: `src-tauri/src/retranscribe/mod.rs`
- Modify: `src-tauri/src/session.rs`(`fn is_foreign_final`、`fn split_final`、`struct SubFinal` 及其字段改 `pub(crate)`)
- Test: `mod.rs` inline tests(tempdir 端到端)

**Interfaces:**
- Consumes: Task 1-5 全部;`crate::session::{is_foreign_final, split_final}`;`crate::asr::{Recognizer, Transcript}`
- Produces(Task 7 依赖):
  - `#[derive(Debug, Clone, serde::Serialize)] pub struct Summary { pub old_segments: usize, pub new_segments: usize, pub seed_matched: usize, pub inherited: usize, pub echo_dropped: usize, pub failed_segments: usize }`
  - `pub const ASR_FAILED_PLACEHOLDER: &str = "[识别失败]";`(与实时链路占位文本逐字一致,先在 session.rs/lib.rs 里 grep `识别失败` 核对原文,以原文为准)
  - `pub fn run(note_dir: &Path, lock: &NoteLock, input: &mut dyn TranscribeInput, recognizer: &mut dyn Recognizer, embedder: &mut Option<Box<dyn SpeakerEmbedder>>, seeds: Vec<SeedCluster>, mixed: bool, progress: &mut dyn FnMut(&str)) -> anyhow::Result<Summary>`

**编排顺序(每步一个 progress 阶段):**

1. `progress("decode")` → `input.segments()?` → 按 `(start_ms, source)` 排序(在线聚类对顺序敏感)。
2. `progress("transcribe")` → 逐段 `catch_unwind(recognize)`:成功 → `is_foreign_final(lang, text)` 命中或 text.trim 空则丢弃;否则 `split_final(...)` 切子段;失败/panic → 该段成 `ASR_FAILED_PLACEHOLDER` 占位段(failed_segments++)。每段算 rms(`(Σx²/n).sqrt()`)。产出 `Vec<RecSeg>`。
3. 双轨模式:`echo_discards` → 移除(echo_dropped = 数量)。
4. `progress("attribute")` → `assign_clusters` → `finalize_speakers`(旧段/旧表从 `NoteStore` 载入?否——直接读文件:`note_dir/segments.jsonl` 逐行 `serde_json::from_str::<SegmentRecord>` 忽略坏行,`speakers.json` 读 BTreeMap,两个私有辅助函数,不绕 NoteStore 免得引入 notes_root 形态约束)。
5. **提交门**:`new.is_empty()`,或 `failed_segments * 2 > new.len()` → `anyhow::bail!`(带人话原因),盘上不动。
6. `progress("commit")` → 组 `SegmentRecord`(seq 从 1 递增、source/text/start/end/speaker/rms)→ `commit::commit`。
7. 返回 Summary。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::{Recognizer, Transcript};
    use crate::retranscribe::input::{PendingSegment, TranscribeInput};
    use crate::store::notelock::NoteLock;

    struct StubInput(Vec<PendingSegment>);
    impl TranscribeInput for StubInput {
        fn segments(&mut self) -> anyhow::Result<Vec<PendingSegment>> {
            Ok(std::mem::take(&mut self.0))
        }
    }

    /// 按样本长度报文本的假识别器;长度 == 魔数时报错(测占位段)。
    struct LenRecognizer { fail_len: Option<usize> }
    impl Recognizer for LenRecognizer {
        fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
            if self.fail_len == Some(samples.len()) {
                anyhow::bail!("mock 识别失败");
            }
            Ok(Transcript {
                text: format!("len={}", samples.len()),
                lang: "zh".into(), tokens: vec![], timestamps: vec![],
            })
        }
    }

    fn pending(source: &str, start_ms: u64, n: usize) -> PendingSegment {
        PendingSegment {
            source: source.into(), samples: vec![0.1; n],
            start_ms, end_ms: start_ms + (n as u64) / 16,
        }
    }

    fn note_dir_with_old() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("segments.jsonl"),
            "{\"seq\":1,\"source\":\"mic\",\"text\":\"旧\",\"start_ms\":0,\"end_ms\":1000,\"speaker\":null}\n").unwrap();
        dir
    }

    /// 端到端:两段进 → 识别 → 无 embedder 降级 → 提交,segments.jsonl 换新、seq 重编、备份在。
    #[test]
    fn run_replaces_segments_end_to_end() {
        let dir = note_dir_with_old();
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        let mut input = StubInput(vec![pending("mic", 0, 16000), pending("system", 500, 32000)]);
        let mut rec = LenRecognizer { fail_len: None };
        let mut emb: Option<Box<dyn crate::diar::SpeakerEmbedder>> = None;
        let mut stages = Vec::new();
        let summary = run(dir.path(), &lock, &mut input, &mut rec, &mut emb, vec![], false,
            &mut |s| stages.push(s.to_string())).unwrap();
        assert_eq!((summary.old_segments, summary.new_segments), (1, 2));
        assert_eq!(stages, vec!["decode", "transcribe", "attribute", "commit"]);
        let text = std::fs::read_to_string(dir.path().join("segments.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"seq\":1") && lines[0].contains("len=16000"));
        assert!(lines[1].contains("\"seq\":2") && lines[1].contains("len=32000"));
        assert!(dir.path().join(crate::retranscribe::commit::SEGMENTS_BACKUP_FILE).exists());
    }

    /// 提交门:识别全灭(占位 >50%)→ 整体放弃,盘上原稿一字不动、无备份产生。
    #[test]
    fn run_aborts_when_placeholders_dominate() {
        let dir = note_dir_with_old();
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        let mut input = StubInput(vec![pending("mic", 0, 16000)]);
        let mut rec = LenRecognizer { fail_len: Some(16000) }; // 唯一一段必失败 → 100% 占位
        let mut emb: Option<Box<dyn crate::diar::SpeakerEmbedder>> = None;
        let err = run(dir.path(), &lock, &mut input, &mut rec, &mut emb, vec![], false, &mut |_| {});
        assert!(err.is_err());
        let text = std::fs::read_to_string(dir.path().join("segments.jsonl")).unwrap();
        assert!(text.contains("旧"), "放弃时原稿必须原样保留");
        assert!(!dir.path().join(crate::retranscribe::commit::SEGMENTS_BACKUP_FILE).exists());
    }

    /// 双轨回声去重进编排:mic/system 同文重叠 → mic 段被弃,echo_dropped 计数。
    #[test]
    fn run_drops_cross_track_echo_in_dual_mode() {
        let dir = note_dir_with_old();
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        // 同长 → 同文本 len=16000 → 时间重叠 → dedup 应弃 mic
        let mut input = StubInput(vec![pending("system", 0, 16000), pending("mic", 100, 16000)]);
        let mut rec = LenRecognizer { fail_len: None };
        let mut emb: Option<Box<dyn crate::diar::SpeakerEmbedder>> = None;
        let summary = run(dir.path(), &lock, &mut input, &mut rec, &mut emb, vec![], false, &mut |_| {}).unwrap();
        assert_eq!((summary.new_segments, summary.echo_dropped), (1, 1));
        let text = std::fs::read_to_string(dir.path().join("segments.jsonl")).unwrap();
        assert!(text.contains("\"system\"") && !text.contains("\"mic\""));
    }

    /// 空外语段丢弃:日语标签段不落盘(与实时链路同口径)。
    #[test]
    fn run_drops_foreign_segments() {
        let dir = note_dir_with_old();
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        struct JaRecognizer;
        impl Recognizer for JaRecognizer {
            fn recognize(&mut self, _s: &[f32]) -> anyhow::Result<Transcript> {
                Ok(Transcript { text: "こんにちは".into(), lang: "ja".into(), tokens: vec![], timestamps: vec![] })
            }
        }
        let mut input = StubInput(vec![pending("mic", 0, 16000), pending("system", 5000, 16000)]);
        let mut emb: Option<Box<dyn crate::diar::SpeakerEmbedder>> = None;
        // 两段全是日语 → 全丢 → 0 段 → 提交门放弃(顺带覆盖 new.is_empty 分支)
        let err = run(dir.path(), &lock, &mut input, &mut JaRecognizer, &mut emb, vec![], false, &mut |_| {});
        assert!(err.is_err());
    }
}
```

(`Transcript` 字段以 `asr/mod.rs:10` 实读为准——`tokens`/`timestamps` 的确切类型
照实填;若有本计划没列的字段,`..Default::default()` 或补默认值。)

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test retranscribe::` 
Expected: 编译失败(`run`/`Summary` 未定义)

- [ ] **Step 3: 写实现**

session.rs:`is_foreign_final`、`split_final`、`SubFinal`(及其四个字段)前加
`pub(crate)`。`split_final` 的调用签名(`&mut Option<Box<dyn SpeakerEmbedder>>`)
原样复用。

`mod.rs`:

```rust
//! 文件重转写(三期):离线读盘上音轨重跑 ASR,覆盖 segments。
//! spec: docs/superpowers/specs/2026-08-07-retranscribe-phase3-design.md
//! 编排链:input(解码/切段) → 识别/语言过滤/段内切分 → 回声去重(双轨) →
//! 声纹归属+继承 → 提交门 → 原子提交。全程要求调用方持 NoteLock。

pub mod attribute;
pub mod commit;
pub mod dedup;
pub mod input;

use crate::asr::Recognizer;
use crate::diar::registry::SeedCluster;
use crate::diar::SpeakerEmbedder;
use crate::store::notelock::NoteLock;
use crate::store::{SegmentRecord, SpeakerMeta};
use attribute::RecSeg;
use input::TranscribeInput;
use std::collections::BTreeMap;
use std::path::Path;

/// 与实时链路(run_asr_worker)的占位文本逐字一致——两边必须同源可 grep。
pub const ASR_FAILED_PLACEHOLDER: &str = "[识别失败]";

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Summary {
    pub old_segments: usize,
    pub new_segments: usize,
    pub seed_matched: usize,
    pub inherited: usize,
    pub echo_dropped: usize,
    pub failed_segments: usize,
}

/// 旧 segments 逐行读(坏行忽略——这里只做继承比对与计数,不负责保全坏行;
/// 坏行保全由 commit 前的一次性备份兜底)。
fn read_old_segments(note_dir: &Path) -> Vec<SegmentRecord> {
    std::fs::read_to_string(note_dir.join("segments.jsonl"))
        .map(|text| {
            text.lines()
                .filter_map(|l| serde_json::from_str::<SegmentRecord>(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn read_old_speakers(note_dir: &Path) -> BTreeMap<String, SpeakerMeta> {
    std::fs::read_to_string(note_dir.join("speakers.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    note_dir: &Path,
    lock: &NoteLock,
    input: &mut dyn TranscribeInput,
    recognizer: &mut dyn Recognizer,
    embedder: &mut Option<Box<dyn SpeakerEmbedder>>,
    seeds: Vec<SeedCluster>,
    mixed: bool,
    progress: &mut dyn FnMut(&str),
) -> anyhow::Result<Summary> {
    let mut summary = Summary::default();

    progress("decode");
    let mut pending = input.segments()?;
    pending.sort_by_key(|s| (s.start_ms, s.source.clone()));

    progress("transcribe");
    let mut recs: Vec<RecSeg> = Vec::new();
    for seg in pending {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            recognizer.recognize(&seg.samples)
        }));
        match outcome {
            Ok(Ok(t)) => {
                if t.text.trim().is_empty() || crate::session::is_foreign_final(&t.lang, &t.text) {
                    continue; // 空段/外语幻觉:与实时链路同口径,整段丢弃
                }
                let subs = crate::session::split_final(
                    seg.samples, seg.start_ms, seg.end_ms, &t, embedder, false,
                );
                for sub in subs {
                    let r = rms(&sub.samples);
                    recs.push(RecSeg {
                        source: seg.source.clone(), text: sub.text,
                        start_ms: sub.start_ms, end_ms: sub.end_ms,
                        samples: sub.samples, rms: r,
                    });
                }
            }
            Ok(Err(err)) => {
                eprintln!("重转写识别失败({}:{}ms): {err}", seg.source, seg.start_ms);
                summary.failed_segments += 1;
                let r = rms(&seg.samples);
                recs.push(RecSeg {
                    source: seg.source.clone(), text: ASR_FAILED_PLACEHOLDER.into(),
                    start_ms: seg.start_ms, end_ms: seg.end_ms, samples: seg.samples, rms: r,
                });
            }
            Err(_) => {
                eprintln!("重转写识别 panic({}:{}ms),留占位段", seg.source, seg.start_ms);
                summary.failed_segments += 1;
                let r = rms(&seg.samples);
                recs.push(RecSeg {
                    source: seg.source.clone(), text: ASR_FAILED_PLACEHOLDER.into(),
                    start_ms: seg.start_ms, end_ms: seg.end_ms, samples: seg.samples, rms: r,
                });
            }
        }
    }

    if !mixed {
        let view: Vec<dedup::DedupSeg> = recs.iter().map(|r| dedup::DedupSeg {
            source: &r.source, start_ms: r.start_ms, end_ms: r.end_ms, text: &r.text,
        }).collect();
        let drops = dedup::echo_discards(&view);
        summary.echo_dropped = drops.len();
        let dropset: std::collections::BTreeSet<usize> = drops.into_iter().collect();
        recs = recs.into_iter().enumerate()
            .filter(|(i, _)| !dropset.contains(i))
            .map(|(_, r)| r)
            .collect();
    }

    progress("attribute");
    let old_segs = read_old_segments(note_dir);
    let old_speakers = read_old_speakers(note_dir);
    summary.old_segments = old_segs.len();
    let (clusters, infos, snaps) = attribute::assign_clusters(&recs, embedder, seeds, mixed);
    let (speakers, table, stats) =
        attribute::finalize_speakers(&recs, &clusters, &infos, &snaps, &old_segs, &old_speakers);
    summary.seed_matched = stats.seed_matched;
    summary.inherited = stats.inherited;
    summary.new_segments = recs.len();

    // 提交门:结果空、或占位段过半 → 整体放弃,盘上一字不动(spec §提交与安全网)。
    if recs.is_empty() {
        anyhow::bail!("重转写产出 0 段,放弃提交(原稿保留)");
    }
    if summary.failed_segments * 2 > recs.len() {
        anyhow::bail!(
            "识别失败段过半({}/{}),放弃提交(原稿保留);检查 ASR 模型后重试",
            summary.failed_segments, recs.len()
        );
    }

    progress("commit");
    let records: Vec<SegmentRecord> = recs.iter().zip(&speakers).enumerate()
        .map(|(i, (r, sp))| SegmentRecord {
            seq: (i + 1) as u64,
            source: r.source.clone(),
            text: r.text.clone(),
            start_ms: r.start_ms,
            end_ms: r.end_ms,
            speaker: sp.clone(),
            rms: Some(r.rms),
        })
        .collect();
    commit::commit(note_dir, lock, &records, &table)?;
    Ok(summary)
}
```

实施前 grep 实时占位文本原文:`grep -rn "识别失败" src-tauri/src/session.rs` ——
若原文带方括号形态不同,以实时链路为准改常量,测试同步。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test retranscribe && cargo test`(全量)
Expected: 全绿

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/retranscribe/ src-tauri/src/session.rs
git commit -m "feat(retrans): 编排 run()——识别/过滤/去重/归属/提交门串成完整离线链路"
```

---

### Task 7: lib.rs — 命令、后台任务、事件、守卫

**Files:**
- Modify: `src-tauri/src/ipc.rs`(新增 `RetranscribeEvent`;照 `RefineEvent` 的形态与注释风格)
- Modify: `src-tauri/src/lib.rs`
- Test: lib.rs 既有测试模块加纯函数测试(守卫逻辑)

**Interfaces:**
- Consumes: Task 1-6;`AppState`(lib.rs:108)、`notes_dir`/`data_root`、`new_recognizer`(:582)、`current_asr`(:638)/`current_asr_provider`(:627)、`speaker_model_path`(:656)、`load_voiceprint_seeds`(:208)、`new_silero`(:665)、`models::root()`、`lifecycle::LifecycleHandle::is_refining`
- Produces:
  - `ipc::RetranscribeEvent { note_id: String, stage: String, state: String, message: Option<String>, summary: Option<retranscribe::Summary> }`(Serialize + Clone)
  - `AppState` 新字段 `retranscribing: Arc<Mutex<Option<(String, String)>>>`(note_id, stage;None = 空闲。**单槽即全局串行闸**)
  - `pub(crate) fn do_retranscribe(app: &AppHandle, id: &str, input: &str) -> Result<(), String>`(命令与 UDS 共用)
  - commands: `retranscribe_note(id, input)` / `retranscribe_status()` / `mixed_input_status(id)`

- [ ] **Step 1: 实现(本 Task 以集成 wiring 为主,先写实现再补守卫单测)**

ipc.rs(`RefineEvent` 旁):

```rust
/// 重转写进度事件("retranscribe")。stage: decode/transcribe/attribute/commit/all;
/// state: running/ok/error。message 仅 error 带原因;summary 仅 all/ok 带。
/// 与 refine 不同不经 lifecycle actor 直发:重转写与录制会话全局互斥(见
/// do_retranscribe 守卫),不存在与管线事件的排序耦合。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RetranscribeEvent {
    pub note_id: String,
    pub stage: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<crate::retranscribe::Summary>,
}
```

lib.rs `AppState` 加字段(构造处同步补 `retranscribing: Arc::new(Mutex::new(None)),`):

```rust
    /// 重转写在跑任务:(note_id, 当前阶段)。单槽 = 全局同时只跑一个(每任务一整套
    /// ORT 管线,与 AING_GATE 同理但直接拒绝不排队——重转写是显式修复动作,静默
    /// 排队会让用户以为卡死)。守卫链:录制中拒/Aing 中拒/槽占用拒,再由 NoteLock
    /// 兜跨进程底。
    retranscribing: Arc<Mutex<Option<(String, String)>>>,
```

守卫 + 入口(放 `refine_note` 附近):

```rust
/// 重转写守卫与启动(tauri command 与 UDS op 共用;spec §提交与安全网)。
pub(crate) fn do_retranscribe(app: &AppHandle, id: &str, input: &str) -> Result<(), String> {
    store::validate_note_id(id).map_err(|e| e.to_string())?;
    if input != "dual" && input != "mixed" {
        return Err(tr!("未知重转写来源: {input}", "Unknown retranscribe input: {input}", input = input));
    }
    let state: tauri::State<AppState> = app.state();
    // 全局互斥于录制(不限本篇):重转写与实时 ASR 各起一套 ORT 管线,叠跑抢核;
    // 且省去"另一篇在录、本篇重转写"的时序矩阵——修复动作等一等没有代价。
    if state.session.lock().unwrap().is_some() {
        return Err(tr!("录制中不能重转写,请先停止录制", "Cannot re-transcribe while recording"));
    }
    if app.state::<lifecycle::LifecycleHandle>().is_refining(id) {
        return Err(tr!("该笔记正在 Aing 中", "This note is being refined"));
    }
    let dir = notes_dir(app).map_err(|e| e.to_string())?.join(id);
    let note = store::NoteStore::new(notes_dir(app).map_err(|e| e.to_string())?)
        .load(id).map_err(|e| e.to_string())?;
    if note.meta.state != "complete" {
        return Err(tr!("笔记未完成,不能重转写", "Only completed notes can be re-transcribed"));
    }
    if input == "mixed" {
        let meta = store::audio::load_audio_meta(&dir);
        if let Some(reason) = retranscribe::input::mixed_untrusted(&meta) {
            return Err(reason);
        }
    }
    {
        let mut slot = state.retranscribing.lock().unwrap();
        if let Some((running, _)) = slot.as_ref() {
            return Err(tr!(
                "已有重转写任务在进行({running}),请等它完成",
                "A re-transcription task is already running ({running})", running = running
            ));
        }
        *slot = Some((id.to_string(), "decode".into()));
    }
    spawn_retranscribe(app.clone(), id.to_string(), input == "mixed");
    Ok(())
}

fn spawn_retranscribe(app: tauri::AppHandle, note_id: String, mixed: bool) {
    let slot = app.state::<AppState>().retranscribing.clone();
    std::thread::spawn(move || {
        let emit = |stage: &str, state: &str, message: Option<String>, summary: Option<retranscribe::Summary>| {
            let _ = app.emit("retranscribe", ipc::RetranscribeEvent {
                note_id: note_id.clone(), stage: stage.into(), state: state.into(), message, summary,
            });
        };
        emit("all", "running", None, None);
        let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<retranscribe::Summary, String> {
            let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&note_id);
            let lock = store::notelock::NoteLock::acquire(&dir)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| tr!("笔记正被占用(录制或转码中),稍后再试", "The note is busy; try again later"))?;
            // 独立识别器实例:不碰常驻 recognizer_cache 槽(那是录制会话的);
            // 恒用本地识别器,云端协议是录制期流式,不适配离线整轨(spec 已知限制 5)。
            let mut recognizer = new_recognizer(&current_asr(&app), current_asr_provider(&app))
                .map_err(|e| tr!("识别器加载失败(本地模型未下载?): {e}", "Failed to load recognizer: {e}", e = e))?;
            let mut embedder: Option<Box<dyn diar::SpeakerEmbedder>> =
                match diar::SherpaEmbedder::new(&speaker_model_path(&app)) {
                    Ok(e) => Some(Box::new(e)),
                    Err(e) => {
                        eprintln!("重转写:声纹模型不可用,归属降级为纯继承: {e}");
                        None
                    }
                };
            let seeds = load_voiceprint_seeds(&app);
            let vad_path = models::root().join("silero_vad.onnx");
            let factory: retranscribe::input::SegmenterFactory =
                Box::new(move || new_silero(&vad_path));
            let mut input: Box<dyn retranscribe::input::TranscribeInput> = if mixed {
                Box::new(retranscribe::input::MixedInput::new(dir.clone(), factory))
            } else {
                Box::new(retranscribe::input::DualTrackInput::new(dir.clone(), factory))
            };
            let slot2 = slot.clone();
            let note_id2 = note_id.clone();
            let app2 = app.clone();
            let mut progress = move |stage: &str| {
                if let Some(s) = slot2.lock().unwrap().as_mut() {
                    s.1 = stage.to_string();
                }
                let _ = app2.emit("retranscribe", ipc::RetranscribeEvent {
                    note_id: note_id2.clone(), stage: stage.into(), state: "running".into(),
                    message: None, summary: None,
                });
            };
            retranscribe::run(&dir, &lock, input.as_mut(), recognizer.as_mut(),
                &mut embedder, seeds, mixed, &mut progress)
                .map_err(|e| e.to_string())
        }));
        *slot.lock().unwrap() = None;
        match body {
            Ok(Ok(summary)) => {
                eprintln!("重转写完成({note_id}): {summary:?}");
                emit("all", "ok", None, Some(summary));
            }
            Ok(Err(e)) => {
                eprintln!("重转写失败({note_id}): {e}");
                emit("all", "error", Some(e), None);
            }
            Err(_) => {
                eprintln!("重转写 panic({note_id})");
                emit("all", "error", Some(tr!("内部错误(见日志)", "Internal error (see logs)")), None);
            }
        }
    });
}

#[tauri::command]
fn retranscribe_note(app: AppHandle, id: String, input: String) -> Result<(), String> {
    do_retranscribe(&app, &id, &input)
}

#[derive(serde::Serialize)]
struct RetranscribeStatus { note_id: String, stage: String }

#[tauri::command]
fn retranscribe_status(state: State<AppState>) -> Option<RetranscribeStatus> {
    state.retranscribing.lock().unwrap().as_ref()
        .map(|(note_id, stage)| RetranscribeStatus { note_id: note_id.clone(), stage: stage.clone() })
}

/// 成品轨入口可用性:None = 可用;Some(原因) = 置灰并提示。
#[tauri::command]
fn mixed_input_status(app: AppHandle, id: String) -> Result<Option<String>, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&id);
    Ok(retranscribe::input::mixed_untrusted(&store::audio::load_audio_meta(&dir)))
}
```

守卫补丁(两处):

1. `refine_note` 命令(lib.rs:1900)在 `request` 之前加:

```rust
    if let Some((rid, _)) = app.state::<AppState>().retranscribing.lock().unwrap().clone() {
        if rid == id {
            return Err(tr!("该笔记正在重转写中", "This note is being re-transcribed"));
        }
    }
```

(注释说明:重转写持 NoteLock,refine 的 run_local 提交时也会因锁失败——这里
提前拒绝只是把错误从「跑完才失败」提到「点下去就说清」。)

2. `do_resume_note_recording`(lib.rs:1595)在既有 refining 检查旁加同款检查
(从 `app.state::<AppState>()` 读槽;若该函数拿不到 app,把检查放它的调用方/
命令壳同位置),文案 `tr!("该笔记正在重转写中,完成后可继续录制", "...")`。
NoteLock 兜底仍在:即便漏检,`NoteWriter::resume` 会因锁失败拒绝。

注册:`invoke_handler`(lib.rs:4930)的 `generate_handler![...]` 列表加
`retranscribe_note, retranscribe_status, mixed_input_status`。

- [ ] **Step 2: 守卫单测**

lib.rs 测试模块(找 `resume_blocked` 类测试所在的 mod)加:

```rust
    /// 重转写摘要事件可序列化,None 字段不出现(前端契约)。
    #[test]
    fn retranscribe_event_serialization_shape() {
        let e = crate::ipc::RetranscribeEvent {
            note_id: "n1".into(), stage: "all".into(), state: "ok".into(),
            message: None,
            summary: Some(crate::retranscribe::Summary {
                old_segments: 10, new_segments: 8, seed_matched: 5,
                inherited: 2, echo_dropped: 1, failed_segments: 0,
            }),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"new_segments\":8"));
        assert!(!json.contains("message"));
    }
```

(do_retranscribe 的守卫链依赖 AppHandle,不强行单测——UDS 层 MockBackend 矩阵
(Task 8)与真机冒烟覆盖;守卫顺序在代码注释里写明:录制 → Aing → 完成态 →
mixed 校验 → 槽。)

- [ ] **Step 3: 编译 + 全量测试**

Run: `cargo test`
Expected: 全绿;`cargo build` 无警告新增

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/lib.rs src-tauri/src/ipc.rs
git commit -m "feat(retrans): 命令与后台任务——守卫链/单槽串行/进度事件/识别器独立实例"
```

---

### Task 8: MCP — UDS op 与工具

**Files:**
- Modify: `src-tauri/src/mcp/uds.rs`
- Modify: `src-tauri/src/mcp/server.rs`
- Test: uds.rs 既有 MockBackend 矩阵扩展;server.rs catalog 防漂移测试自动覆盖

**Interfaces:**
- Consumes: `crate::do_retranscribe`、`AppState.retranscribing`、`bridge_call`(server.rs 既有)
- Produces:
  - UDS op `"retranscribe"`(control 门控;参数 note_id + input)/ `"retranscribe_status"`(app 即可,不过 control 门)
  - MCP tools `retranscribe_note(note_id, input?)` / `retranscribe_status()`

- [ ] **Step 1: 写失败测试(uds.rs MockBackend 矩阵)**

uds.rs 测试模块:`MockBackend` 加两方法实现(照 reaing 的样式记录调用);
门控矩阵测试里(`for op in ["start", "stop", "pause", "resume", "reaing"]` 两处,
uds.rs:353/373 附近)把 `"retranscribe"` 加进列表;新增:

```rust
    /// retranscribe 过 control 门;retranscribe_status 是只读查询,不过 control 门。
    #[test]
    fn retranscribe_gated_but_status_is_not() {
        let denied = MockBackend::new(false);
        let r = dispatch_with(&denied, &Req {
            op: "retranscribe".into(), title: None, tail: None,
            note_id: Some("n1".into()), input: Some("dual".into()),
        });
        assert!(!r.ok, "control 关闭时 retranscribe 必须被拒");
        let r = dispatch_with(&denied, &Req {
            op: "retranscribe_status".into(), title: None, tail: None, note_id: None, input: None,
        });
        assert!(r.ok, "status 查询不受 control 门控");
        assert!(denied.called("retranscribe_status"));

        let allowed = MockBackend::new(true);
        let r = dispatch_with(&allowed, &Req {
            op: "retranscribe".into(), title: None, tail: None,
            note_id: Some("n1".into()), input: None,
        });
        assert!(r.ok);
        assert!(allowed.called("retranscribe:n1:dual"), "input 缺省应补 dual");
    }
```

(`Req` 的既有字段与构造形态以 uds.rs 顶部实读为准,已有测试怎么构造就怎么构造;
若 Req 构造用了 `..Default::default()` 之类,照抄。)

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test mcp::uds`
Expected: 编译失败(Req 无 input 字段等)

- [ ] **Step 3: 实现**

uds.rs:

1. `Req` 结构体加 `#[serde(default)] pub input: Option<String>`(形态照 note_id 字段)。
2. 门控行(uds.rs:126)改:
   `matches!(op, "start" | "stop" | "pause" | "resume" | "reaing" | "retranscribe")`
3. `UdsBackend` trait 加:

```rust
    /// 发起文件重转写(异步启动即返回;input 缺省 dual)。
    fn retranscribe(&self, note_id: &str, input: &str) -> Result<serde_json::Value, String>;
    /// 当前重转写任务;空闲返回 null。
    fn retranscribe_status(&self) -> serde_json::Value;
```

4. `dispatch_with` 的 match 加:

```rust
        "retranscribe" => match req.note_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => b.retranscribe(id, req.input.as_deref().unwrap_or("dual")),
            None => Err("retranscribe 需要 note_id".into()),
        },
        "retranscribe_status" => Ok(b.retranscribe_status()),
```

5. `AppBackend` 实现:

```rust
    fn retranscribe(&self, note_id: &str, input: &str) -> Result<serde_json::Value, String> {
        crate::do_retranscribe(self.0, note_id, input)?;
        Ok(serde_json::json!({ "started": true, "note_id": note_id, "input": input }))
    }

    fn retranscribe_status(&self) -> serde_json::Value {
        let state = self.0.state::<crate::AppState>();
        let slot = state.retranscribing.lock().unwrap();
        match slot.as_ref() {
            Some((note_id, stage)) => serde_json::json!({ "running": true, "note_id": note_id, "stage": stage }),
            None => serde_json::json!({ "running": false }),
        }
    }
```

6. MockBackend 对应实现(记录 `format!("retranscribe:{note_id}:{input}")` 与
   `"retranscribe_status"`)。

server.rs(照 StartParams/pause_recording 的样式):

```rust
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct RetranscribeParams {
    /// 目标笔记 id
    note_id: String,
    /// 音频来源:"dual"(双轨,默认)| "mixed"(成品轨)
    input: Option<String>,
}
```

```rust
    #[tool(
        description = "对一篇已完成的笔记发起文件重转写:离线重读盘上音轨重新跑 ASR,覆盖原始逐字稿(自动备份 segments.orig.jsonl,说话人尽量保留)。异步启动即返回,用 retranscribe_status 轮询进度;同一时刻全局只跑一个任务。需要应用运行 + 用户开启「允许 AI 控制录制」。"
    )]
    async fn retranscribe_note(
        &self,
        Parameters(p): Parameters<RetranscribeParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("retranscribe", serde_json::json!({ "note_id": p.note_id, "input": p.input })).await)
    }

    #[tool(description = "查询当前重转写任务(running/note_id/阶段);空闲返回 running=false。需要应用运行。")]
    async fn retranscribe_status(&self) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("retranscribe_status", serde_json::json!({})).await)
    }
```

catalog()(server.rs:452)加两行(gate 分别 `"control"` / `"app"`),并把
`/// \`/ai\` 页展示用的静态能力清单:MCP 十三工具` 注释里的计数改对。
`catalog_matches_tool_router` 防漂移测试会强制两处一致——跑挂了按报错补。
(bridge_call 的参数序列化把 `input: None` 传成 null,uds 侧 `unwrap_or("dual")`
接住,行为已在 Step 1 测试锁定。)

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test mcp`
Expected: 全绿(含 catalog 防漂移)

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/mcp/
git commit -m "feat(retrans): MCP 入口——UDS retranscribe/status 两 op + 工具与 catalog"
```

---

### Task 9: 前端 — 按钮、进度、stale 横幅、i18n

**Files:**
- Modify: `src/lib/events.ts`
- Modify: `src/lib/notes.ts`
- Modify: `src/routes/notes/[id]/+page.svelte`
- Modify: `src/lib/i18n/dict/notes.ts`
- Test: `npm run check` + `npm test`(i18n 哨兵测试会验 zh/en 键对齐与硬编码中文)

- [ ] **Step 1: events.ts**

`RefineEvent` 旁:

```ts
/** 文件重转写进度("retranscribe")。stage: decode/transcribe/attribute/commit/all;
 * state: running/ok/error。message 仅 error 带;summary 仅 all/ok 带。 */
export type RetranscribeSummary = {
  old_segments: number;
  new_segments: number;
  seed_matched: number;
  inherited: number;
  echo_dropped: number;
  failed_segments: number;
};
export type RetranscribeEvent = {
  note_id: string;
  stage: string;
  state: string;
  message?: string;
  summary?: RetranscribeSummary;
};
export function onRetranscribe(cb: (e: RetranscribeEvent) => void) {
  return listen<RetranscribeEvent>("retranscribe", (ev) => cb(ev.payload));
}
```

- [ ] **Step 2: notes.ts**

`refineNote` 旁:

```ts
/** 发起文件重转写(破坏性:覆盖原始逐字稿,后端自动备份)。input: "dual" | "mixed"。 */
export const retranscribeNote = (id: string, input: "dual" | "mixed") =>
  invoke<void>("retranscribe_note", { id, input });
/** 当前重转写任务;空闲 null。挂载时回填(事件只覆盖在页期间)。 */
export const retranscribeStatus = () =>
  invoke<{ note_id: string; stage: string } | null>("retranscribe_status");
/** 成品轨入口可用性:null 可用;字符串为置灰原因。 */
export const mixedInputStatus = (id: string) =>
  invoke<string | null>("mixed_input_status", { id });
```

`RefinedDoc` 类型(本文件或其引用处,grep `RefinedDoc` 定位)加 `stale?: boolean;`。

- [ ] **Step 3: +page.svelte**

状态与订阅(script 区,照 refining/onRefine 的既有写法):

```ts
let retranscribing = $state(false);
let retransStage = $state("");
let retransConfirm = $state(false);
let mixedReason = $state<string | null>(null); // null = mixed 可用

// onMount 内(既有初始化处):
retranscribeStatus().then((s) => {
  if (s && s.note_id === id) { retranscribing = true; retransStage = s.stage; }
});
mixedInputStatus(id).then((r) => (mixedReason = r)).catch(() => (mixedReason = "?"));
// 事件订阅(既有 onRefine 订阅旁,unlisten 同样进清理数组):
onRetranscribe((e) => {
  if (e.note_id !== id) return;
  if (e.state === "running") { retranscribing = true; retransStage = e.stage; }
  else {
    retranscribing = false; retransConfirm = false;
    if (e.state === "ok") { refresh(); recording.bumpNotes(); }
    else if (e.message) toast(t("notes.retrans.failed", { e: e.message }));
  }
});
```

(`toast`/错误提示机制照本页既有失败提示的做法——grep `rerunFailed` 看 refine
失败怎么展示,同款。)

发起函数:

```ts
async function startRetranscribe(input: "dual" | "mixed") {
  retransConfirm = false;
  try {
    await retranscribeNote(id, input);
    retranscribing = true;
    retransStage = "decode";
  } catch (e) {
    toast(t("notes.retrans.failed", { e: String(e) }));
  }
}
```

UI:`view-switch` 行(+page.svelte:1075 附近)、「再 Aing」按钮旁加:

```svelte
{#if retransConfirm}
  <span class="refine-warn">{t("notes.retrans.warn")}</span>
  <button class="link danger" onclick={() => startRetranscribe("dual")}>
    {t("notes.retrans.confirmDual")}
  </button>
  <button
    class="link danger"
    disabled={mixedReason !== null}
    title={mixedReason ?? ""}
    onclick={() => startRetranscribe("mixed")}
  >
    {t("notes.retrans.confirmMixed")}
  </button>
  <button class="link" onclick={() => (retransConfirm = false)}>{t("notes.cancel")}</button>
{:else}
  <button
    class="link"
    disabled={retranscribing || refining || recording.isLive || note.meta.state !== "complete"}
    title={retranscribing ? t("notes.retrans.running", { stage: retransStage }) : t("notes.retrans.hint")}
    onclick={() => (retransConfirm = true)}
  >
    {retranscribing ? t("notes.retrans.running", { stage: retransStage }) : t("notes.retrans.run")}
  </button>
{/if}
```

stale 横幅:修订稿视图(`effectiveView === "refined"` 分支,SpeakerChips 之前)加:

```svelte
{#if refined?.stale}
  <div class="banner">{t("notes.retrans.staleBanner")}</div>
{/if}
```

(`banner` 样式类照本页既有横幅——grep `banner.interrupted` 的容器写法,同款;
`refined` 是本页已有的修订稿状态变量,grep `getRefined` 确认变量名。)

- [ ] **Step 4: i18n dict**

`src/lib/i18n/dict/notes.ts`,zh 与 en 两份**同时**加(键序放 refine 组后面):

```ts
  // 详情页:文件重转写(三期)
  "notes.retrans.run": "重转写",
  "notes.retrans.hint": "用盘上音频离线重新转写全文(覆盖原始逐字稿,自动备份)",
  "notes.retrans.warn": "将覆盖原始逐字稿并重建说话人,原稿备份为 segments.orig.jsonl",
  "notes.retrans.confirmDual": "双轨重转写",
  "notes.retrans.confirmMixed": "成品轨重转写",
  "notes.retrans.running": "重转写中({stage})",
  "notes.retrans.failed": "重转写失败:{e}",
  "notes.retrans.staleBanner": "段落已重转写,本修订稿基于旧文本,请重新执行 AI。",
```

en:

```ts
  "notes.retrans.run": "Re-transcribe",
  "notes.retrans.hint": "Re-run ASR offline from the audio on disk (overwrites the raw transcript; a backup is kept)",
  "notes.retrans.warn": "This overwrites the raw transcript and rebuilds speakers. The original is backed up as segments.orig.jsonl.",
  "notes.retrans.confirmDual": "Dual-track",
  "notes.retrans.confirmMixed": "Mixed-track",
  "notes.retrans.running": "Re-transcribing ({stage})",
  "notes.retrans.failed": "Re-transcription failed: {e}",
  "notes.retrans.staleBanner": "Segments were re-transcribed. This refined doc is based on the old text — please re-run AI.",
```

- [ ] **Step 5: 验证**

Run: `npm run check && npm test`
Expected: 0 错误、测试全绿(i18n 哨兵含 zh/en 键对齐)

- [ ] **Step 6: 提交**

```bash
git add src/lib/events.ts src/lib/notes.ts src/routes/notes/ src/lib/i18n/dict/notes.ts
git commit -m "feat(retrans): 详情页重转写入口——来源二选一确认/进度/stale 横幅"
```

---

### Task 10: 全量验证、文档、PR

- [ ] **Step 1: 全量测试**

```bash
cd src-tauri && cargo test && cd .. && npm run check && npm test
```

Expected: 全绿。任何失败先修再走。

- [ ] **Step 2: 对照 spec 自查**

逐节过 `docs/superpowers/specs/2026-08-07-retranscribe-phase3-design.md`,每条
目标/安全网找到对应实现与测试;发现实现与 spec 偏离 → 要么改实现,要么在 spec
补偏离记录(小节 + 理由),不许静默漂移。

- [ ] **Step 3: PR**

```bash
git push -u origin feature/retranscribe-phase3
gh pr create --title "feat(retrans): 文件重转写(三期)——双轨/成品轨离线重转写" --body "<正文>"
```

PR 正文必含:
1. spec/plan 链接;三期动机(修复 ~40 场回声污染笔记)。
2. 偏离记录摘要(双轨只读声纹库/source 字符串/MCP 异步/本地识别器)。
3. **真机冒烟清单(合并前必做,Chromium 假通过是惯犯——PR#65/#66 前科)**:
   - [ ] 挑一场受污染历史笔记(audio.json 有 `clean` 记录)双轨重转写:文字对比旧稿肉眼改善;`segments.orig.jsonl` 生成;说话人名字/人物关联保住
   - [ ] 重转写完成后详情页自动刷新;修订稿视图出 stale 横幅;点「执行 AI」重跑后横幅消失
   - [ ] 录制中:重转写按钮置灰;后端(MCP 直打)也被拒
   - [ ] 重转写中:再 Aing 按钮照常置灰逻辑、续录被拒、第二个重转写任务被拒
   - [ ] mixed 轨笔记(开 mix_track 新录一场):成品轨重转写跑通,段 source 全为 mixed,声纹不回写库(`voiceprints.json` mtime 不变)
   - [ ] 无 mixed 轨笔记:成品轨选项置灰且 tooltip 给原因
   - [ ] MCP:`retranscribe_note` 启动 + `retranscribe_status` 轮询到 ok;关掉「允许 AI 控制录制」后被拒
   - [ ] 识别失败路径:临时改名 ASR 模型目录再重转写 → 报"识别器加载失败",原稿无损
4. 落款 `🤖 Generated with [Claude Code](https://claude.com/claude-code)`。

---

## Self-Review 记录(计划自查已做)

- **Spec 覆盖**:目标 1(双实现)→ Task 1/3;目标 2(详情页+互斥)→ Task 7/9;目标 3(MCP)→ Task 8;目标 4(归属保留)→ Task 3/4;目标 5(安全网)→ Task 5/6。降级口径(z 关/只读库)→ Task 3;提交门/备份/原子/抑制表/align/stale → Task 5/6;进度事件 → Task 7/9。
- **类型一致性**:`PendingSegment`/`RecSeg`/`Summary`/`SegmenterFactory`/`do_retranscribe` 各 Task 引用处签名一致;`stale` 贯穿 refined.rs → commit.rs → notes.ts → +page.svelte。
- **已知留白(刻意)**:`retranscribe` 不加 CLI 子命令(MCP 工具已覆盖批量驱动,YAGNI);无 `#[ignore]` 真模型集成测试(真机冒烟清单覆盖,历史笔记不进 fixtures)。
