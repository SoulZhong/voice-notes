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
