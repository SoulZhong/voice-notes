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

/// 与实时链路(run_asr_worker/FinalSink)的占位文本逐字一致——已 grep session.rs
/// 核对原文「[识别失败]」,两边同源,不得各自维护一份字符串。
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

/// 离线重转写主编排:解码 → 识别/过滤/切分 → 回声去重(双轨) → 声纹归属+继承 →
/// 提交门 → 原子提交。任何一步失败(除识别单段失败——那走占位段兜底)整体
/// `bail`,盘上不动;调用方负责持有 `NoteLock` 贯穿整个函数生命周期。
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
    // 在线聚类(assign_clusters)对到达顺序敏感,先按时间排序再识别/归属。
    pending.sort_by_key(|s| (s.start_ms, s.source.clone()));

    progress("transcribe");
    let mut recs: Vec<RecSeg> = Vec::new();
    for seg in pending {
        // catch_unwind:单段识别 panic 不能拖垮整场重转写,退化为占位段。
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            recognizer.recognize(&seg.samples)
        }));
        match outcome {
            Ok(Ok(t)) => {
                if t.text.trim().is_empty() || crate::session::is_foreign_final(&t.lang, &t.text) {
                    continue; // 空段/外语幻觉:与实时链路同口径,整段丢弃(不计入 recs)
                }
                let subs = crate::session::split_final(
                    seg.samples, seg.start_ms, seg.end_ms, &t, embedder, false,
                );
                for sub in subs {
                    let r = rms(&sub.samples);
                    recs.push(RecSeg {
                        source: seg.source.clone(),
                        text: sub.text,
                        start_ms: sub.start_ms,
                        end_ms: sub.end_ms,
                        samples: sub.samples,
                        rms: r,
                    });
                }
            }
            Ok(Err(err)) => {
                eprintln!("重转写识别失败({}:{}ms): {err}", seg.source, seg.start_ms);
                summary.failed_segments += 1;
                let r = rms(&seg.samples);
                recs.push(RecSeg {
                    source: seg.source.clone(),
                    text: ASR_FAILED_PLACEHOLDER.into(),
                    start_ms: seg.start_ms,
                    end_ms: seg.end_ms,
                    samples: seg.samples,
                    rms: r,
                });
            }
            Err(_) => {
                eprintln!("重转写识别 panic({}:{}ms),留占位段", seg.source, seg.start_ms);
                summary.failed_segments += 1;
                let r = rms(&seg.samples);
                recs.push(RecSeg {
                    source: seg.source.clone(),
                    text: ASR_FAILED_PLACEHOLDER.into(),
                    start_ms: seg.start_ms,
                    end_ms: seg.end_ms,
                    samples: seg.samples,
                    rms: r,
                });
            }
        }
    }

    // 双轨模式下跨轨回声去重;混音(mixed)只有一条轨,无跨轨可比,天然跳过。
    if !mixed {
        let view: Vec<dedup::DedupSeg> = recs
            .iter()
            .map(|r| dedup::DedupSeg {
                source: &r.source,
                start_ms: r.start_ms,
                end_ms: r.end_ms,
                text: &r.text,
            })
            .collect();
        let drops = dedup::echo_discards(&view);
        summary.echo_dropped = drops.len();
        let dropset: std::collections::BTreeSet<usize> = drops.into_iter().collect();
        recs = recs
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !dropset.contains(i))
            .map(|(_, r)| r)
            .collect();
    }

    progress("attribute");
    // 不绕 NoteStore:直接读文件,免得引入 notes_root 形态约束(spec 明确要求)。
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
            summary.failed_segments,
            recs.len()
        );
    }

    progress("commit");
    let records: Vec<SegmentRecord> = recs
        .iter()
        .zip(&speakers)
        .enumerate()
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

    /// NoteLock 互斥断言:同目录持锁期间再次 acquire 必须拿到 Ok(None)(而非 Err 或
    /// 阻塞)。重转写 worker 全程持这把锁(见 spawn_retranscribe),正是靠这条互斥语义
    /// 挡住"录制/编辑中还能重转写"或"重转写中还能录制/编辑"——spec §测试「持锁时
    /// 发起 → 拒绝」在单元级锁死这个前提,不必每次都跑一遍完整的 worker 集成场景。
    #[test]
    fn notelock_acquire_rejects_while_already_held() {
        let dir = tempfile::tempdir().unwrap();
        let held = NoteLock::acquire(dir.path()).unwrap().expect("首次持锁应成功");
        let second = NoteLock::acquire(dir.path()).unwrap();
        assert!(second.is_none(), "持锁期间二次 acquire 必须返回 Ok(None),不得拿到锁");
        drop(held);
        assert!(NoteLock::acquire(dir.path()).unwrap().is_some(), "释放后应可重新持锁");
    }

    /// 端到端:两段进 → 识别 → 无 embedder 降级 → 提交,segments.jsonl 换新、seq 重编、备份在。
    #[test]
    fn run_replaces_segments_end_to_end() {
        let dir = note_dir_with_old();
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        // system 段起点特意错开到 mic 段结束(1000ms)之后、不与其时间重叠:
        // 若两段重叠,"len=16000"/"len=32000" 编辑距离恰好 0.778 ≥ ECHO_SIM_MIN(0.75)、
        // 重叠占比恰好 0.5 == ECHO_OVERLAP_MIN,会被 dedup 误判成回声——那是 dedup 的
        // 独立职责,本测试测的是编排顺序,不该被这份 fixture 巧合带偏(dedup 有专门
        // 的端到端测试 run_drops_cross_track_echo_in_dual_mode 覆盖)。
        let mut input = StubInput(vec![pending("mic", 0, 16000), pending("system", 3000, 32000)]);
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
