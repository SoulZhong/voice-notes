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

    /// mic 轨的电平交叉门压低区间(player_gate::GateSpan,时间轴 16k 样本域)。
    /// 仅双轨入口(DualTrackInput)在 segments() 调用之后可用——mic/system 两轨
    /// 都存在时才有交叉可比,单轨(MixedInput、缺一轨的双轨笔记)默认空表降级=
    /// 不开门,与 player_gate 自身"任一轨读不出即空表"的降级口径一致。
    fn mic_gate_spans(&self) -> Vec<crate::player_gate::GateSpan> {
        Vec::new()
    }
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
/// 两轨全缺才报错。
///
/// 内存口径(2026-08 电平交叉门接入后有变化):切段阶段仍是逐轨串行——本轨切完
/// 段就不再需要它的 PCM;但两轨都存在时,门控(build_gate_from_pcm)需要两条
/// 全场 PCM 同时在手才能逐帧比电平,故本实现把两轨 PCM 暂存到 segments() 返回
/// 前才释放,峰值内存两轨并存(不再是"至多一轨常驻")。这与 player_gate 自身
/// mmap 两条 canonical WAV 的量级一致,且只发生在重转写这种一次性离线任务里,
/// 已被接受(不是回放热路径的常驻开销)。
pub struct DualTrackInput {
    note_dir: PathBuf,
    make_segmenter: SegmenterFactory,
    gate_spans: Vec<crate::player_gate::GateSpan>,
}

impl DualTrackInput {
    pub fn new(note_dir: PathBuf, make_segmenter: SegmenterFactory) -> Self {
        Self { note_dir, make_segmenter, gate_spans: Vec::new() }
    }
}

impl TranscribeInput for DualTrackInput {
    fn segments(&mut self) -> anyhow::Result<Vec<PendingSegment>> {
        let meta = crate::store::audio::load_audio_meta(&self.note_dir);
        let mut out = Vec::new();
        let mut found = false;
        // 暂存两轨的 (offset_ms, PCM),供切段循环结束后计算门控用;单轨笔记里
        // 缺的那一路始终是 None,门控随即降级为空表(见下方 if let)。
        let mut mic_track: Option<(u64, Vec<f32>)> = None;
        let mut sys_track: Option<(u64, Vec<f32>)> = None;
        for source in ["mic", "system"] {
            // 「轨不存在」与「轨存在但解码失败」不是同一件事,不能同等 continue:
            // m4a 损坏、或转码 worker 恰在删 wav 的窗口撞上,都会让 track_pcm 返回 Err，
            // 若在此静默跳过,单轨结果会在没有任何警示的情况下覆盖全文——用户以为
            // 拿到了双轨完整重转写,实际上丢了一整路内容。文件确实都不存在才是
            // 「单轨笔记合法」的场景,此时才允许 continue;文件存在但解不出来，
            // 必须整体 bail,把盘上一字不动地留住,逼用户看到错误再决定重试。
            if !track_exists(&self.note_dir, source) {
                continue;
            }
            let pcm = crate::store::transcode::track_pcm(&self.note_dir, source).map_err(|e| {
                anyhow::anyhow!(
                    "音轨 {source} 存在但解码失败,拒绝用另一轨静默覆盖全文(排查后重试): {e}"
                )
            })?;
            found = true;
            let offset_ms = meta.tracks.get(source).map(|t| t.offset_ms).unwrap_or(0);
            let mut seg = (self.make_segmenter)()?;
            out.extend(collect_track_segments(&pcm, offset_ms, source, seg.as_mut()));
            match source {
                "mic" => mic_track = Some((offset_ms, pcm)),
                "system" => sys_track = Some((offset_ms, pcm)),
                _ => unreachable!("循环只枚举 mic/system"),
            }
        }
        if !found {
            anyhow::bail!("mic/system 音轨均不可读,无法重转写");
        }
        if let (Some((mic_off, mic_pcm)), Some((sys_off, sys_pcm))) = (&mic_track, &sys_track) {
            self.gate_spans =
                crate::player_gate::build_gate_from_pcm(mic_pcm, *mic_off, sys_pcm, *sys_off);
        }
        Ok(out)
    }

    fn mic_gate_spans(&self) -> Vec<crate::player_gate::GateSpan> {
        self.gate_spans.clone()
    }
}

/// 纯逻辑判定:`<source>.wav`/`<source>.m4a` 是否至少一个存在于笔记目录。抽成独立
/// 函数是为了让"轨不存在 vs 轨存在但解码失败"这条分支判定本身可脱离 afconvert
/// 子进程单测(见下方 tests::track_exists_*)。
fn track_exists(note_dir: &std::path::Path, source: &str) -> bool {
    note_dir.join(format!("{source}.wav")).exists() || note_dir.join(format!("{source}.m4a")).exists()
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
/// regen 轨的放宽容限(codex 第二轮 P1):补生成经 player_align 重采样,mic 长度
/// 相对源轨读数改变 0.5~0.9s 是 spec 已知限制 1 记录的量级,按严格档必被误拒。
pub const MIXED_REGEN_TOLERANCE_MS: u64 = 2_000;

/// mixed 轨内容是否**不可信**(None = 可信)。一期明示 mixed 存在 ≠ 完整(回滚失败/
/// 混音线程 panic 后 Drop 补合法头,均无盘上标记),消费前拿源轨 sync.track_ms 交叉
/// 核对。口径:mixed 时长应 ≈ max(各源 offset_ms + track_ms)(后启动源带前导静音,
/// 见母 spec §口径差)。不可校验(缺任何一边读数)一律判不可信——拒绝不删不改。
pub fn mixed_untrusted(meta: &AudioMeta) -> Option<String> {
    let Some(mixed) = meta.tracks.get("mixed") else {
        return Some("没有成品轨(mixed)产物".into());
    };
    let Some(dur) = mixed
        .duration_ms
        // 未转码的 mixed.wav 没有实测 duration_ms:退而用 sync.track_ms(录制期对账值)。
        // 但 mixed 实际从不落 sync(record_sync 只遍历 mic/system 的 health),这条
        // 回退保留只为向后兼容口径;真正兜住未转码场景的是第三读数 mix.track_ms
        // (二期起定稿即写,Windows 无转码路径全靠它)。
        .or_else(|| mixed.sync.as_ref().map(|s| s.track_ms))
        .or_else(|| mixed.mix.as_ref().map(|m| m.track_ms))
    else {
        return Some("成品轨缺少时长读数,无法校验完整性".into());
    };
    let mut expected: Option<u64> = None;
    for source in ["mic", "system"] {
        let Some(t) = meta.tracks.get(source) else { continue };
        // Fix 3(codex 第二轮):源轨终点优先用 duration_ms(整文件实测时长,转码后必有)
        // + offset_ms——sync.track_ms 是"本场净时长",set_track_sync 每场覆盖,续录
        // 笔记只留最后一场;拿它当源轨终点会把前面所有场次都漏掉,续录笔记必被误判
        // untrusted。没有 duration_ms(未转码)才回退 offset_ms+sync.track_ms;该回退
        // 对续录笔记会低估终点、可能误拒——宁可误拒不可误信,故仍保留而不是放行。
        let end = if let Some(d) = t.duration_ms {
            t.offset_ms + d
        } else {
            let Some(sync) = &t.sync else {
                return Some(format!("源轨 {source} 无 sync 对账记录(旧笔记),无法校验成品轨"));
            };
            t.offset_ms + sync.track_ms
        };
        expected = Some(expected.map_or(end, |e: u64| e.max(end)));
    }
    let Some(expected) = expected else {
        return Some("没有可对账的源轨,无法校验成品轨".into());
    };
    // 口径对齐(codex 第二轮 P1):dur 是文件本地时长,expected 是全局时间轴终点,
    // 比较前把 mixed 自己的 offset_ms 加回去——否则 origin>0 的合法轨被整个
    // offset 量级误拒。容限按来源分档:regen 经对齐重采样,长度相对源轨读数改变
    // 0.5~0.9s 是已知残余(spec 已知限制 1),放宽到 2s;live/无标记维持严格档。
    let mixed_end = mixed.offset_ms + dur;
    let tolerance = if mixed.mix.as_ref().is_some_and(|m| m.origin == "regen") {
        MIXED_REGEN_TOLERANCE_MS
    } else {
        MIXED_TOLERANCE_MS
    };
    let diff = mixed_end.abs_diff(expected);
    if diff > tolerance {
        return Some(format!(
            "成品轨终点({mixed_end}ms)与源轨对账值({expected}ms)偏差 {diff}ms,超 {tolerance}ms 容限,内容不可信"
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
            ..Default::default()
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

    /// codex 第二轮 P1:mixed 的时长读数是**文件本地**口径,源轨终点是**全局时间轴**
    /// 口径——比较必须把 mixed 自己的 offset_ms 加回去,否则 origin>0 的合法成品轨
    /// (续录/系统先开等 min(源 offset)>0 的场景)会被整个 offset 的量级误拒。
    #[test]
    fn mixed_with_nonzero_offset_compares_on_global_timeline() {
        let mut m = meta_with(Some(59_000), 60_000, 58_000, 1_200);
        // 两源 offset 分别 1200/…,mixed origin=1000:文件时长 59_000,全局终点 60_000。
        m.tracks.get_mut("mic").unwrap().offset_ms = 1_000;
        m.tracks.get_mut("mixed").unwrap().offset_ms = 1_000;
        // expected = max(1000+60000, 1200+58000) = 61_000;mixed 全局终点 = 1000+59_000 = 60_000
        // 偏差 1000ms>500 会拒——把 mixed 时长调到 60_200,全局终点 61_200,偏差 200 应放行。
        m.tracks.get_mut("mixed").unwrap().duration_ms = Some(60_200);
        assert_eq!(mixed_untrusted(&m), None, "全局口径下 offset>0 的合法轨应放行");
        // 反证:若实现忘加 mixed.offset_ms,60_200 vs 61_000 偏差 800 会被误拒。
    }

    /// codex 第二轮 P1(第二半):regen 轨经 player_align 重采样,mic 长度改变
    /// 0.5~0.9s 是 spec 已知限制 1 的量级,而源轨读数仍是对齐前的——对 regen 轨
    /// 按已知残余放宽容限,live 轨维持 500ms 严格档。
    #[test]
    fn regen_origin_gets_alignment_tolerance() {
        let mk = |origin: &str| {
            let mut m = meta_with(None, 60_000, 59_000, 1_200); // expected=60_200
            m.tracks.get_mut("mixed").unwrap().mix = Some(crate::store::audio::MixInfo {
                origin: origin.into(),
                seek_offset_ms: Default::default(),
                track_ms: 61_000, // 偏差 800ms:超 500 严格档,在 2000 放宽档内
                clipped_samples: 0,
                limited_samples: 0,
                limit_metered: true,
            });
            m
        };
        assert_eq!(mixed_untrusted(&mk("regen")), None, "regen 轨按对齐残余放宽");
        assert!(mixed_untrusted(&mk("live")).is_some(), "live 轨维持严格容限");
    }

    /// 未转码的 mixed.wav(Windows 恒如此;macOS 转码失败降级)没有 duration_ms,
    /// mixed 又永远没有 sync(record_sync 只遍历 mic/system)——三期在这种笔记上
    /// 恒判「缺少时长读数」。二期起 MixInfo.track_ms(定稿时量出)作第三读数来源。
    #[test]
    fn mix_info_track_ms_serves_as_duration_fallback() {
        let mut m = meta_with(None, 60_000, 59_000, 1_200); // 无 duration_ms、无 sync
        assert!(mixed_untrusted(&m).is_some(), "三读数全缺时必须拒");
        m.tracks.get_mut("mixed").unwrap().mix = Some(crate::store::audio::MixInfo {
            origin: "live".into(),
            seek_offset_ms: Default::default(),
            track_ms: 60_400, // 对账值 max(60000, 1200+59000)=60200,差 200ms ≤ 容限
            clipped_samples: 0,
            limited_samples: 0,
            limit_metered: true,
        });
        assert_eq!(mixed_untrusted(&m), None, "有 MixInfo.track_ms 即可校验通过");
    }

    /// 续录笔记(Fix 3,codex 第二轮):源轨 duration_ms 是整文件实测时长(跨全部场次),
    /// sync.track_ms 只是最后一场的净时长(set_track_sync 每场覆盖)。mixed 时长对齐
    /// 全长 duration_ms 时应判可信——若仍按 offset+sync.track_ms(只有末场)算终点,
    /// 会把前面场次全部漏掉,偏差远超容限,续录笔记必被误判 untrusted。
    #[test]
    fn mixed_trusted_for_continued_recording_using_source_duration_ms() {
        let sync = |track_ms: u64| SyncInfo {
            wall_ms: track_ms, samples: 1, track_ms, drift_ms: 0, silence_ms: 0, gaps: 0, rate_fixes: 0,
            ..Default::default()
        };
        let mut m = AudioMeta::default();
        // 两场录制,mic 全长 10_000ms,但 sync.track_ms 只留了最后一场的 3_000ms
        // ——若按旧回退口径算终点是 3_000ms,离真实全长差 7_000ms,远超容限。
        m.tracks.insert("mic".into(), TrackMeta {
            duration_ms: Some(10_000), sync: Some(sync(3_000)), ..Default::default()
        });
        m.tracks.insert("mixed".into(), TrackMeta { duration_ms: Some(10_100), ..Default::default() });
        assert_eq!(mixed_untrusted(&m), None, "duration_ms 全长口径下续录笔记应判可信");
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

    /// track_exists 纯逻辑:wav/m4a 任一存在即真,两者皆无为假。不依赖 afconvert,
    /// 全平台可跑——这是 Fix1「轨不存在 vs 解码失败」分支判定的可测部分。
    #[test]
    fn track_exists_checks_wav_or_m4a_presence() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!track_exists(dir.path(), "mic"), "两者皆无");
        std::fs::write(dir.path().join("mic.wav"), b"x").unwrap();
        assert!(track_exists(dir.path(), "mic"), "wav 存在");
        std::fs::remove_file(dir.path().join("mic.wav")).unwrap();
        std::fs::write(dir.path().join("mic.m4a"), b"x").unwrap();
        assert!(track_exists(dir.path(), "mic"), "m4a 存在");
    }

    /// DualTrackInput 端到端(macOS 子进程可行,直接测真实分支而非只测纯函数):
    /// system 轨完全不存在(单轨笔记合法,应 continue);mic 轨的 m4a 存在但是垃圾
    /// 字节,afconvert 解码必失败——此时不许静默跳过,必须整体 bail 且错误带轨名。
    #[cfg(target_os = "macos")]
    #[test]
    fn dual_track_bails_when_file_exists_but_decode_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mic.m4a"), b"not a real m4a").unwrap();
        let make_seg: SegmenterFactory =
            Box::new(|| Ok(Box::new(MockSegmenter::new(1600)) as Box<dyn Segmenter>));
        let mut input = DualTrackInput::new(dir.path().to_path_buf(), make_seg);
        let err = match input.segments() {
            Err(e) => e,
            Ok(_) => panic!("解码失败必须整体 bail,不得静默产出结果"),
        };
        assert!(err.to_string().contains("mic"), "错误须带轨名,实际: {err}");
    }
}
