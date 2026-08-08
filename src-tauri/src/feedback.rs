//! 纠错回灌(spec rev2 P1-2):人工指认「这个说话人是库里的谁」之后,把该
//! 说话人的发声段重新嵌入并并入那个人的质心——让整理动作变成训练信号
//! (准确度分析文档:轻量纠错回灌文献口径相对 DER -32%)。
//! 本文件先落纯逻辑核:不碰磁盘不碰库,输入段+PCM,输出各信道嵌入统计;
//! 磁盘壳(账本/门禁/解码/入库)与 IPC 挂钩在同文件后续任务追加。

use std::collections::BTreeMap;

use crate::diar::SpeakerEmbedder;
use crate::store::SegmentRecord;

/// 单段最短 1.5s:与 registry::MIN_CENTROID_UPDATE_SAMPLES(24_000 采样)同口径,
/// 更短的段嵌入不稳定,进质心是污染不是信号。
pub const MIN_SEG_MS: u64 = 1_500;
/// 单次回灌每信道最多嵌入的段数(时长降序取):指认是用户动作触发的后台任务,
/// 超长会议不该嵌几百段——每信道最长的 30 段已足够代表这个人;分信道限额是
/// 因为全局截断会让长会议的次要信道颗粒无收。
pub const MAX_SEGS_PER_SOURCE: usize = 30;

/// 某信道的回灌统计:单位化质心 + 段数 + 实际嵌入时长。
pub struct SourceStat {
    pub centroid: Vec<f32>,
    pub count: u64,
    pub total_ms: u64,
}

/// 纯逻辑核:给定待回灌段 + 各轨 PCM(16kHz f32,带轨起点 offset_ms),逐段
/// 切片→嵌入→按信道求单位均值。嵌入/切片/非有限值失败一律静默跳段——回灌是
/// 增值层,任何失败都不该冒泡成用户可见错误。
pub fn build_source_stats(
    segs: &[&SegmentRecord],
    pcm_by_source: &BTreeMap<String, (u64, Vec<f32>)>,
    embedder: &mut dyn SpeakerEmbedder,
) -> BTreeMap<String, SourceStat> {
    // 分信道分组,各自(时长降序,seq 升序)稳定排序后限额——排序键里带 seq
    // 是为了让选择结果与调用方的加载顺序无关(NoteStore::load 会按时间重排)。
    let mut by_source_segs: BTreeMap<&str, Vec<&SegmentRecord>> = BTreeMap::new();
    for s in segs {
        if s.end_ms.saturating_sub(s.start_ms) >= MIN_SEG_MS {
            by_source_segs.entry(s.source.as_str()).or_default().push(s);
        }
    }

    struct Acc {
        sum: Vec<f32>,
        count: u64,
        total_ms: u64,
    }
    let mut out_acc: BTreeMap<String, Acc> = BTreeMap::new();
    for (source, mut list) in by_source_segs {
        let Some((offset_ms, pcm)) = pcm_by_source.get(source) else {
            continue;
        };
        list.sort_by_key(|s| {
            (
                std::cmp::Reverse(s.end_ms.saturating_sub(s.start_ms)),
                s.seq,
            )
        });
        list.truncate(MAX_SEGS_PER_SOURCE);
        for s in list {
            let start = (s.start_ms.saturating_sub(*offset_ms) as usize).saturating_mul(16);
            let end =
                ((s.end_ms.saturating_sub(*offset_ms) as usize).saturating_mul(16)).min(pcm.len());
            if start >= end {
                continue;
            }
            // 实际切片时长再过一次门:账面 2s 但轨尾截断到几十 ms 的段,嵌入
            // 不稳定且会虚报 total_ms。
            let actual_ms = ((end - start) / 16) as u64;
            if actual_ms < MIN_SEG_MS {
                continue;
            }
            let Ok(vec) = embedder.embed(&pcm[start..end]) else {
                continue;
            };
            if vec.is_empty() || vec.iter().any(|x| !x.is_finite()) {
                continue;
            }
            let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm <= f32::EPSILON {
                continue;
            }
            let acc = out_acc.entry(source.to_string()).or_insert_with(|| Acc {
                sum: vec![0.0; vec.len()],
                count: 0,
                total_ms: 0,
            });
            if acc.sum.len() != vec.len() {
                continue; // 维度漂移只可能是模型异常,弃段
            }
            for (a, b) in acc.sum.iter_mut().zip(&vec) {
                *a += b / norm; // 逐向量归一化后累加(registry 同口径)
            }
            acc.count += 1;
            acc.total_ms += actual_ms;
        }
    }

    out_acc
        .into_iter()
        .filter_map(|(source, acc)| {
            let norm: f32 = acc.sum.iter().map(|x| x * x).sum::<f32>().sqrt();
            if !norm.is_finite() || norm <= f32::EPSILON || acc.count == 0 {
                return None;
            }
            Some((
                source,
                SourceStat {
                    centroid: acc.sum.iter().map(|x| x / norm).collect(),
                    count: acc.count,
                    total_ms: acc.total_ms,
                },
            ))
        })
        .collect()
}

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 指认范围:原始稿按 S 号(speakers.json 域),修订稿按 source_seqs(R 段落
/// 已把 seq 集合显式落盘,不必反查 R→S 映射)。
pub enum SegFilter {
    Speakers(BTreeSet<String>),
    Seqs(BTreeSet<u64>),
}

#[derive(Debug, PartialEq)]
pub enum ReinforceResult {
    /// source -> 实际嵌入段数。
    Applied { per_source: BTreeMap<String, u64> },
    SkippedModelMismatch,
    SkippedNoSegments,
    SkippedAlreadyDone,
    SkippedUnknownPerson,
}

/// P2b 撤销结果:三态——还原成功 / 不可还原(带原因,持久化到 op 记录)/ 无账。
#[derive(Debug, PartialEq)]
pub enum UndoOutcome {
    Restored,
    NotRevertible(&'static str),
    NoEntry,
}

/// 按操作 id 撤销一次自动回灌:scope 账本条目必须是**同一 op** 写的(后续人工
/// 回灌会覆盖同 scope 条目,凭 scope 撤销会错撤人工作业);person 经 redirects
/// 归一比较(合并后 id 变身);restore 被后续写挡下时如实报不可还原。
pub fn undo_reinforce_op(
    note_dir: &Path,
    seqs: &BTreeSet<u64>,
    expect_person: &str,
    op_id: &str,
    vp: &crate::store::VoiceprintStore,
) -> anyhow::Result<UndoOutcome> {
    let key = seq_fingerprint(seqs);
    let mut ledger = load_ledger(note_dir);
    let Some(entry) = ledger.entries.get(&key) else {
        return Ok(UndoOutcome::NoEntry);
    };
    if entry.op_id.as_deref() != Some(op_id) {
        return Ok(UndoOutcome::NotRevertible("superseded"));
    }
    let lib = vp.load();
    let entry_person = crate::store::VoiceprintStore::resolve(&lib, &entry.person_id);
    let expect = crate::store::VoiceprintStore::resolve(&lib, expect_person);
    if entry_person.is_none() || entry_person != expect {
        return Ok(UndoOutcome::NotRevertible("person-mismatch"));
    }
    let restored = vp.restore_feedback(&entry.person_id, &entry.before, &entry.after)?;
    if !restored {
        return Ok(UndoOutcome::NotRevertible("touched-since"));
    }
    ledger.entries.remove(&key);
    if let Err(e) = save_ledger(note_dir, &ledger) {
        eprintln!("feedback: 撤销后账本清理失败(无害): {e}");
    }
    Ok(UndoOutcome::Restored)
}

/// 指认后的回灌分派决策(纯函数,可单测;IPC 挂钩壳只做 IO)。
/// prior = 该说话人指认前已关联的库人物(resolve 后的 id 与名字)。
#[derive(Debug, PartialEq)]
pub enum FeedbackAction {
    /// 先前关联的是**无名**自动人物(停录时 AUTO_ENROLL 自动建的 P<n>):
    /// journaled 合并 prior→target——质心已在库,合并比重嵌入干净(不重复计
    /// 时长),且可经收件箱撤销/拆回;也消掉了"同一声音两个身份"的种子竞争。
    MergePrior { prior: String },
    /// 无先前关联,或先前是有名人物(用户纠正认错):段重嵌入回灌 target。
    /// 有名 prior 在停录时并入的净增量 P1 无法撤销(spec P2 identify_journal)。
    Reinforce,
    /// 已指认给同一人:无事可做。
    Noop,
}

pub fn plan_action(prior: Option<(&str, &str)>, target: &str) -> FeedbackAction {
    match prior {
        Some((id, _)) if id == target => FeedbackAction::Noop,
        Some((id, name)) if name.trim().is_empty() => FeedbackAction::MergePrior { prior: id.to_string() },
        _ => FeedbackAction::Reinforce,
    }
}

const LEDGER_FILE: &str = "feedback.json";

/// 笔记级回灌账本:幂等(同段集合同人只灌一次)+ 纠错还原凭据。
#[derive(Default, Serialize, Deserialize)]
struct FeedbackLedger {
    /// key = 段集合指纹(排序 seq 的 sha256 前 16 hex)。
    #[serde(default)]
    entries: BTreeMap<String, LedgerEntry>,
}

#[derive(Serialize, Deserialize)]
struct LedgerEntry {
    person_id: String,
    at: String,
    /// 还原凭据(reinforce_feedback 返回的前后快照)。
    before: String,
    after: String,
    /// P2b:自动应用的操作 id——撤销按 op 对账,scope 键会被后续人工回灌覆盖,
    /// 不能只凭 scope 判断"这是我那次写的"。人工路径为 None(serde 兼容 P1 数据)。
    #[serde(default)]
    op_id: Option<String>,
}

/// 段集合指纹:sha256(升序 seq 的 LE 字节) 前 8 字节 hex。P1 回灌账本与
/// P2a identify 的簇指纹共用同一口径(speaker_eval 里有复刻,改动需三处同步)。
pub(crate) fn seq_fingerprint(seqs: &BTreeSet<u64>) -> String {
    let mut h = Sha256::new();
    for s in seqs {
        h.update(s.to_le_bytes());
    }
    hex::encode(&h.finalize()[..8])
}

fn load_ledger(note_dir: &Path) -> FeedbackLedger {
    std::fs::read_to_string(note_dir.join(LEDGER_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_ledger(note_dir: &Path, ledger: &FeedbackLedger) -> anyhow::Result<()> {
    let tmp = note_dir.join(format!("{LEDGER_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(ledger)?)?;
    std::fs::rename(&tmp, note_dir.join(LEDGER_FILE))?;
    Ok(())
}

/// 磁盘壳:模型门禁 → 选段 → 幂等/纠错账本 → 逐轨解码 → 纯逻辑核 →
/// reinforce_feedback 入库。解码失败/轨缺失只收窄不报错——回灌拿得到多少
/// 信号用多少;门禁不一致整体跳过(不同模型向量空间不可比,错灌比不灌糟)。
#[allow(clippy::too_many_arguments)]
pub fn reinforce_person(
    note_dir: &Path,
    segs: &[SegmentRecord],
    filter: &SegFilter,
    person_id: &str,
    vp: &crate::store::VoiceprintStore,
    library_model: &str,
    expected_model: &str,
    embedder: &mut dyn SpeakerEmbedder,
    now: &str,
    op_id: Option<&str>,
) -> anyhow::Result<ReinforceResult> {
    // 门禁:与种子注入同一严格语义(lib.rs 模型门禁)。
    if library_model != expected_model {
        return Ok(ReinforceResult::SkippedModelMismatch);
    }
    let wanted: Vec<&SegmentRecord> = segs
        .iter()
        .filter(|s| match filter {
            SegFilter::Speakers(ids) => s.speaker.as_ref().is_some_and(|sp| ids.contains(sp)),
            SegFilter::Seqs(seqs) => seqs.contains(&s.seq),
        })
        .collect();
    if wanted.is_empty() {
        return Ok(ReinforceResult::SkippedNoSegments);
    }
    {
        // 悬空人物在解码/嵌入之前就短路:嵌入白做还会污染账本。
        let lib = vp.load();
        if crate::store::VoiceprintStore::resolve(&lib, person_id).is_none() {
            return Ok(ReinforceResult::SkippedUnknownPerson);
        }
    }

    let seq_set: BTreeSet<u64> = wanted.iter().map(|s| s.seq).collect();
    let key = seq_fingerprint(&seq_set);
    let mut ledger = load_ledger(note_dir);
    if let Some(prev) = ledger.entries.get(&key) {
        if prev.person_id == person_id {
            return Ok(ReinforceResult::SkippedAlreadyDone);
        }
        // 纠错:上一次灌错了人。未被动过就还原;动过则宁留污染不覆盖新信息。
        match vp.restore_feedback(&prev.person_id, &prev.before, &prev.after) {
            Ok(true) => eprintln!("feedback: 已还原 {} 的上次回灌(纠错)", prev.person_id),
            Ok(false) => eprintln!("feedback: {} 已被其它写动过,跳过还原", prev.person_id),
            Err(e) => eprintln!("feedback: 还原失败(忽略): {e}"),
        }
        ledger.entries.remove(&key);
    }

    let meta = crate::store::audio::load_audio_meta(note_dir);
    let mut pcm_by_source: BTreeMap<String, (u64, Vec<f32>)> = BTreeMap::new();
    for source in wanted.iter().map(|s| s.source.as_str()).collect::<BTreeSet<_>>() {
        match crate::store::transcode::track_pcm(note_dir, source) {
            Ok(pcm) => {
                let offset = meta.tracks.get(source).map(|t| t.offset_ms).unwrap_or(0);
                pcm_by_source.insert(source.to_string(), (offset, pcm));
            }
            Err(e) => eprintln!("feedback: 音轨 {source} 解码失败,该轨不回灌: {e}"),
        }
    }

    let stats = build_source_stats(&wanted, &pcm_by_source, embedder);
    if stats.is_empty() {
        return Ok(ReinforceResult::SkippedNoSegments);
    }
    let tuples: Vec<(String, Vec<f32>, u64, u64)> = stats
        .iter()
        .map(|(s, st)| (s.clone(), st.centroid.clone(), st.count, st.total_ms))
        .collect();
    let applied = vp.reinforce_feedback(person_id, &tuples, now)?;
    ledger.entries.insert(
        key,
        LedgerEntry {
            person_id: person_id.to_string(),
            at: now.to_string(),
            before: applied.person_before,
            after: applied.person_after,
            op_id: op_id.map(str::to_string),
        },
    );
    if let Err(e) = save_ledger(note_dir, &ledger) {
        eprintln!("feedback: 账本写入失败(下次可能重复回灌): {e}");
    }
    Ok(ReinforceResult::Applied {
        per_source: stats.iter().map(|(s, st)| (s.clone(), st.count)).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diar::MockEmbedder;

    fn seg(seq: u64, source: &str, start_ms: u64, end_ms: u64) -> SegmentRecord {
        SegmentRecord {
            seq,
            source: source.into(),
            text: String::new(),
            start_ms,
            end_ms,
            speaker: Some("S1".into()),
            rms: Some(0.0),
        }
    }

    /// 16kHz:1ms = 16 采样。
    fn pcm_ms(ms: u64) -> Vec<f32> {
        vec![0.1; (ms * 16) as usize]
    }

    fn unit(i: usize) -> Vec<f32> {
        let mut v = vec![0.0; 4];
        v[i] = 1.0;
        v
    }

    #[test]
    fn short_and_truncated_segments_are_skipped() {
        let s_short = seg(0, "mic", 0, 800); // 账面 <1.5s
                                             // 账面 2s 但 PCM 只覆盖到 2200ms:切到 1200ms,实际 <1.5s,同样跳过——
                                             // 账面时长骗不过实际样本数。
        let s_trunc = seg(1, "mic", 1000, 3000);
        let s_ok = seg(2, "mic", 0, 2000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(2200)));
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        let stats = build_source_stats(&[&s_short, &s_trunc, &s_ok], &pcm, &mut emb);
        let mic = stats.get("mic").expect("s_ok 应产出 mic 统计");
        assert_eq!(mic.count, 1);
        assert_eq!(mic.total_ms, 2000, "只计实际嵌入段的实际切片时长");
    }

    #[test]
    fn stats_split_by_source_and_centroid_is_unit_mean_of_unit_vectors() {
        let m = seg(0, "mic", 0, 2000);
        let s = seg(1, "system", 0, 2000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(2000)));
        pcm.insert("system".to_string(), (0u64, pcm_ms(2000)));
        // 故意给未归一化向量:实现必须先逐向量归一化再累加。
        let mut emb = MockEmbedder::new(vec![
            Ok(vec![3.0, 0.0, 0.0, 0.0]),
            Ok(vec![0.0, 5.0, 0.0, 0.0]),
        ]);
        let stats = build_source_stats(&[&m, &s], &pcm, &mut emb);
        assert_eq!(stats.keys().collect::<Vec<_>>(), vec!["mic", "system"]);
        for st in stats.values() {
            let norm: f32 = st.centroid.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-5, "质心必须单位化");
        }
        assert!((stats["mic"].centroid[0] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn non_finite_embeddings_and_errors_degrade_to_skip() {
        let a = seg(0, "mic", 0, 2000);
        let b = seg(1, "mic", 2000, 4000);
        let c = seg(2, "system", 0, 2000); // system 轨 PCM 缺失
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(4000)));
        let mut emb = MockEmbedder::new(vec![
            Ok(vec![f32::NAN, 1.0, 0.0, 0.0]),
            Err(anyhow::anyhow!("boom")),
        ]);
        let stats = build_source_stats(&[&a, &b, &c], &pcm, &mut emb);
        assert!(stats.is_empty(), "NaN/报错/缺轨全部静默跳过");
    }

    #[test]
    fn per_source_cap_with_stable_order() {
        // mic 造 MAX_SEGS_PER_SOURCE+2 个等长段:限额按信道各自生效,
        // 等长时按 seq 升序稳定取。
        let segs: Vec<SegmentRecord> = (0..(MAX_SEGS_PER_SOURCE as u64 + 2))
            .map(|i| seg(i, "mic", i * 2000, i * 2000 + 2000))
            .collect();
        let refs: Vec<&SegmentRecord> = segs.iter().collect();
        let mut pcm = BTreeMap::new();
        pcm.insert(
            "mic".to_string(),
            (0u64, pcm_ms((MAX_SEGS_PER_SOURCE as u64 + 2) * 2000)),
        );
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        let stats = build_source_stats(&refs, &pcm, &mut emb);
        assert_eq!(stats["mic"].count as usize, MAX_SEGS_PER_SOURCE);
    }

    #[test]
    fn offset_is_respected_when_slicing() {
        // 轨 offset 1000ms:段 [1000,3000)ms 应切 PCM [0,32000) 采样;
        // PCM 只有 2000ms 长,不减 offset 会切出空区间。
        let m = seg(0, "mic", 1000, 3000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (1000u64, pcm_ms(2000)));
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        let stats = build_source_stats(&[&m], &pcm, &mut emb);
        assert_eq!(stats["mic"].total_ms, 2000);
    }
    fn write_wav(path: &std::path::Path, ms: u64) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for _ in 0..(ms * 16) {
            w.write_sample(3000i16).unwrap();
        }
        w.finalize().unwrap();
    }

    fn seeded_store(root: &std::path::Path, dir_vec: Vec<f32>) -> (crate::store::VoiceprintStore, String) {
        let store = crate::store::VoiceprintStore::new(root.to_path_buf());
        let key = format!("seed-{}", store.load().next_person);
        let links = store
            .upsert_from_session(
                &[crate::diar::registry::ClusterSnapshot {
                    id: key.clone(),
                    centroid: dir_vec,
                    count: 4,
                    sources: std::collections::BTreeSet::from(["mic".to_string()]),
                    person: None,
                    total_ms: 12_000,
                }],
                "t0",
            )
            .unwrap();
        let pid = links.get(&key).unwrap().clone();
        (store, pid)
    }

    #[test]
    fn reinforce_is_idempotent_per_scope_and_person() {
        let note = tempfile::tempdir().unwrap();
        write_wav(&note.path().join("mic.wav"), 4000);
        let vp_root = tempfile::tempdir().unwrap();
        let (store, pid) = seeded_store(vp_root.path(), vec![1.0, 0.0, 0.0, 0.0]);
        let segs = vec![seg(0, "mic", 0, 2000), seg(1, "mic", 2000, 4000)];
        let filter = SegFilter::Speakers(BTreeSet::from(["S1".to_string()]));
        let model = store.load().embedding_model.clone();

        let mut emb = MockEmbedder::new(vec![Ok(unit(1)), Ok(unit(1))]);
        let r1 = reinforce_person(note.path(), &segs, &filter, &pid, &store, &model, &model, &mut emb, "t1", None).unwrap();
        assert!(matches!(r1, ReinforceResult::Applied { .. }), "{r1:?}");
        let total_after_first = store.load().people[&pid].total_ms;

        let mut emb2 = MockEmbedder::new(vec![Ok(unit(1)), Ok(unit(1))]);
        let r2 = reinforce_person(note.path(), &segs, &filter, &pid, &store, &model, &model, &mut emb2, "t2", None).unwrap();
        assert_eq!(r2, ReinforceResult::SkippedAlreadyDone, "同段集合同人重复指认不得重复加权");
        assert_eq!(store.load().people[&pid].total_ms, total_after_first);
    }

    #[test]
    fn correction_restores_previous_person_when_untouched() {
        let note = tempfile::tempdir().unwrap();
        write_wav(&note.path().join("mic.wav"), 4000);
        let vp_root = tempfile::tempdir().unwrap();
        let (store, pid_a) = seeded_store(vp_root.path(), vec![1.0, 0.0, 0.0, 0.0]);
        let (_, pid_b) = {
            // 同一库再造一个 B(seeded_store 会复用同一目录的库)。
            let store_b = crate::store::VoiceprintStore::new(vp_root.path().to_path_buf());
            let links = store_b
                .upsert_from_session(
                    &[crate::diar::registry::ClusterSnapshot {
                        id: "seed-b".into(),
                        centroid: vec![0.0, 0.0, 1.0, 0.0],
                        count: 4,
                        sources: std::collections::BTreeSet::from(["mic".to_string()]),
                        person: None,
                        total_ms: 12_000,
                    }],
                    "t0",
                )
                .unwrap();
            (store_b, links.get("seed-b").unwrap().clone())
        };

        let segs = vec![seg(0, "mic", 0, 2000), seg(1, "mic", 2000, 4000)];
        let filter = SegFilter::Speakers(BTreeSet::from(["S1".to_string()]));
        let model = store.load().embedding_model.clone();

        let mut emb = MockEmbedder::new(vec![Ok(unit(1)), Ok(unit(1))]);
        reinforce_person(note.path(), &segs, &filter, &pid_a, &store, &model, &model, &mut emb, "t1", None).unwrap();
        let a_total_polluted = store.load().people[&pid_a].total_ms;
        assert!(a_total_polluted > 12_000);

        // 纠错:同段集合改指 B → A 的上次回灌应被还原(未被动过),B 获得回灌。
        let mut emb2 = MockEmbedder::new(vec![Ok(unit(1)), Ok(unit(1))]);
        let r = reinforce_person(note.path(), &segs, &filter, &pid_b, &store, &model, &model, &mut emb2, "t2", None).unwrap();
        assert!(matches!(r, ReinforceResult::Applied { .. }), "{r:?}");
        assert_eq!(store.load().people[&pid_a].total_ms, 12_000, "A 的回灌应还原");
        assert!(store.load().people[&pid_b].total_ms > 12_000, "B 获得回灌");
    }

    #[test]
    fn model_mismatch_and_unknown_person_short_circuit() {
        let note = tempfile::tempdir().unwrap();
        let vp_root = tempfile::tempdir().unwrap();
        let (store, pid) = seeded_store(vp_root.path(), vec![1.0, 0.0, 0.0, 0.0]);
        let segs = vec![seg(0, "mic", 0, 2000)];
        let filter = SegFilter::Speakers(BTreeSet::from(["S1".to_string()]));
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        // 门禁:严格相等,库侧默认模型 vs 期望 "eres2netv2" → 跳过。
        let r = reinforce_person(note.path(), &segs, &filter, &pid, &store, "campplus", "eres2netv2", &mut emb, "t", None)
            .unwrap();
        assert_eq!(r, ReinforceResult::SkippedModelMismatch);
        // 悬空人物:在解码/嵌入之前就短路。
        write_wav(&note.path().join("mic.wav"), 2000);
        let model = store.load().embedding_model.clone();
        let r2 = reinforce_person(note.path(), &segs, &filter, "P999", &store, &model, &model, &mut emb, "t", None).unwrap();
        assert_eq!(r2, ReinforceResult::SkippedUnknownPerson);
    }
    #[test]
    fn plan_action_dispatches_by_prior_state() {
        assert_eq!(plan_action(None, "P2"), FeedbackAction::Reinforce);
        assert_eq!(plan_action(Some(("P2", "张伟")), "P2"), FeedbackAction::Noop);
        assert_eq!(
            plan_action(Some(("P7", "")), "P2"),
            FeedbackAction::MergePrior { prior: "P7".into() },
            "无名自动人物并入目标,不重嵌入"
        );
        assert_eq!(
            plan_action(Some(("P7", "李雷")), "P2"),
            FeedbackAction::Reinforce,
            "有名先前人物=纠错,只灌新人"
        );
    }
    #[test]
    fn undo_reinforce_op_accounts_by_op_id() {
        let note = tempfile::tempdir().unwrap();
        write_wav(&note.path().join("mic.wav"), 4000);
        let vp_root = tempfile::tempdir().unwrap();
        let (store, pid) = seeded_store(vp_root.path(), vec![1.0, 0.0, 0.0, 0.0]);
        let segs = vec![seg(0, "mic", 0, 2000), seg(1, "mic", 2000, 4000)];
        let filter = SegFilter::Speakers(BTreeSet::from(["S1".to_string()]));
        let model = store.load().embedding_model.clone();
        let seqs: BTreeSet<u64> = [0u64, 1].into_iter().collect();

        let mut emb = MockEmbedder::new(vec![Ok(unit(1)), Ok(unit(1))]);
        reinforce_person(note.path(), &segs, &filter, &pid, &store, &model, &model, &mut emb, "t1", Some("iop-9"))
            .unwrap();
        // 错 op id:superseded,不撤。
        assert_eq!(
            undo_reinforce_op(note.path(), &seqs, &pid, "iop-other", &store).unwrap(),
            UndoOutcome::NotRevertible("superseded")
        );
        // 对 op id:还原成功,total_ms 回落,账本清空。
        assert_eq!(
            undo_reinforce_op(note.path(), &seqs, &pid, "iop-9", &store).unwrap(),
            UndoOutcome::Restored
        );
        assert_eq!(store.load().people[&pid].total_ms, 12_000);
        assert_eq!(
            undo_reinforce_op(note.path(), &seqs, &pid, "iop-9", &store).unwrap(),
            UndoOutcome::NoEntry,
            "重复撤销无账"
        );
    }

}