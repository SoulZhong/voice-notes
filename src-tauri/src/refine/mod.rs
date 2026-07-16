//! 会后 Aing 管线编排:过滤(A3)→重聚类(A1)→段落化,可选 LLM Aing(A2)。
//! 原始三文件只读;一切产物写 refined.json。

pub mod agent;
pub mod filter;
pub mod llm;
pub mod recluster;

use crate::diar::registry::SeedCluster;
use crate::diar::SpeakerEmbedder;
use crate::store::{
    self, write_refined_atomic, Entity, Mention, RefineStages, RefinedDoc, RefinedParagraph,
    SegmentRecord, SpeakerMeta,
};
use std::collections::BTreeMap;
use std::path::Path;

/// 同说话人合并段落的时长上限(对齐豆包排版粒度)。
pub const MAX_PARA_MS: u64 = 60_000;

pub fn run_local(
    note_dir: &Path,
    segs: &[SegmentRecord],
    speakers: &BTreeMap<String, SpeakerMeta>,
    embedder: Option<&mut dyn SpeakerEmbedder>,
    seeds: &[SeedCluster],
    generated_at: &str,
) -> RefinedDoc {
    let discarded = filter::discarded_seqs(segs);
    let kept: Vec<&SegmentRecord> = segs.iter().filter(|s| !discarded.contains(&s.seq)).collect();

    let inputs: Vec<recluster::SegInput> = kept
        .iter()
        .map(|s| recluster::SegInput {
            seq: s.seq,
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            source: s.source.clone(),
            old_speaker: s.speaker.clone(),
        })
        .collect();

    let (assign, recluster_state) = match embedder {
        Some(e) => match embed_all(note_dir, &kept, e) {
            Ok(embs) => (recluster::recluster(&inputs, &embs, seeds), "done"),
            Err(err) => {
                eprintln!("refine: 嵌入失败,重聚类降级: {err}");
                (fallback_assign(&inputs), "failed")
            }
        },
        None => (fallback_assign(&inputs), "skipped"),
    };

    let paragraphs = build_paragraphs(segs, &discarded, &assign, speakers);
    let doc = RefinedDoc {
        schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
        generated_at: generated_at.to_string(),
        llm_model: None,
        stages: RefineStages {
            filter: "done".into(),
            recluster: recluster_state.into(),
            llm: "off".into(),
            entities: "off".into(),
        },
        discarded_seqs: discarded,
        entities: vec![],
        paragraphs,
    };
    if let Err(e) = write_refined_atomic(note_dir, &doc) {
        eprintln!("refine: refined.json 写盘失败: {e}");
    }
    doc
}

/// 时间轴 ms 区间 → 该轨文件内的 PCM 样本下标区间(16kHz 单声道,1ms=16 样本)。
///
/// F2 修复:每条轨道的文件内 0 点并不总是笔记时间轴的 0 点——续录/中途授权的轨道
/// audio.json 里记着非零 offset_ms(不变式:文件内毫秒 + offset_ms == 段时间轴毫秒,
/// 见 store/audio.rs `TrackMeta::offset_ms` 文档)。故切片前须先把时间轴 ms 减掉该轨
/// offset_ms 换算回文件内 ms,而不能假设轨道 0 时刻就是笔记 0 点。
/// 越界一律 clamp 到 pcm_len;换算后 end<=start(轨道 offset 之后、或段整体早于该轨
/// 出现时刻)返回 None,调用方按"该段在这轨没有可用音频"处理。
fn slice_range(start_ms: u64, end_ms: u64, offset_ms: u64, pcm_len: usize) -> Option<(usize, usize)> {
    let a = ((start_ms.saturating_sub(offset_ms)) as usize * 16).min(pcm_len);
    let b = ((end_ms.saturating_sub(offset_ms)) as usize * 16).min(pcm_len);
    if b <= a {
        None
    } else {
        Some((a, b))
    }
}

/// 按 source 分组、每次只惰性加载一轨全场 PCM,算完该轨全部段的嵌入即整轨 drop
/// (F3 修复:避免双轨全场 f32 同时常驻内存——3h 双轨会议可达 ~1.4GB+)。
/// 结果按 kept 的原始下标回填,输出顺序/长度与 kept 保持一一对应,行为不变。
fn embed_all(
    note_dir: &Path,
    kept: &[&SegmentRecord],
    embedder: &mut dyn SpeakerEmbedder,
) -> anyhow::Result<Vec<Option<Vec<f32>>>> {
    let audio_meta = crate::store::audio::load_audio_meta(note_dir);
    let mut out: Vec<Option<Vec<f32>>> = vec![None; kept.len()];

    let mut by_source: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, s) in kept.iter().enumerate() {
        by_source.entry(s.source.as_str()).or_default().push(i);
    }

    for (source, idxs) in by_source {
        // 该轨全是短段(< MIN_EMBED_MS)时无需读它的 PCM,省一次磁盘 I/O。
        if !idxs.iter().any(|&i| {
            let s = kept[i];
            s.end_ms.saturating_sub(s.start_ms) >= recluster::MIN_EMBED_MS
        }) {
            continue;
        }
        let offset_ms = audio_meta.tracks.get(source).map(|t| t.offset_ms).unwrap_or(0);
        let pcm = crate::store::transcode::track_pcm(note_dir, source)?;
        for &i in &idxs {
            let s = kept[i];
            let dur = s.end_ms.saturating_sub(s.start_ms);
            if dur < recluster::MIN_EMBED_MS {
                continue;
            }
            if let Some((a, b)) = slice_range(s.start_ms, s.end_ms, offset_ms, pcm.len()) {
                out[i] = embedder.embed(&pcm[a..b]).ok();
            }
        }
        // `pcm` 在此 for 迭代结束时被 drop:下一轮换轨才会再分配一份全场 PCM,
        // 任意时刻至多一轨全场 PCM 常驻内存。
    }
    Ok(out)
}

fn fallback_assign(inputs: &[recluster::SegInput]) -> Vec<recluster::Assignment> {
    inputs
        .iter()
        .map(|i| recluster::Assignment {
            seq: i.seq,
            speaker: i.old_speaker.clone().unwrap_or_else(|| "R1".into()),
            name: None,
            person: None,
        })
        .collect()
}

pub(crate) fn build_paragraphs(
    segs: &[SegmentRecord],
    discarded: &[u64],
    assign: &[recluster::Assignment],
    speakers: &BTreeMap<String, SpeakerMeta>,
) -> Vec<RefinedParagraph> {
    let by_seq: BTreeMap<u64, &recluster::Assignment> = assign.iter().map(|a| (a.seq, a)).collect();
    let mut out: Vec<RefinedParagraph> = Vec::new();
    for s in segs {
        if discarded.contains(&s.seq) {
            continue;
        }
        let Some(a) = by_seq.get(&s.seq) else { continue };
        let old_meta = s.speaker.as_ref().and_then(|old| speakers.get(old));
        let name = a.name.clone().or_else(|| {
            old_meta.filter(|m| !m.name.is_empty()).map(|m| m.name.clone())
        });
        // 人物关联:重聚类种子命中优先;降级路径(沿用旧 S 标签)继承该说话人在
        // speakers.json 里已有的关联——与 name 同一套兜底逻辑。
        let person_id = a.person.clone().or_else(|| old_meta.and_then(|m| m.person_id.clone()));
        let merge = out.last().map_or(false, |p: &RefinedParagraph| {
            p.speaker == a.speaker && s.end_ms.saturating_sub(p.start_ms) <= MAX_PARA_MS
        });
        if merge {
            let p = out.last_mut().unwrap();
            p.text.push_str(&s.text);
            p.end_ms = s.end_ms;
            p.source_seqs.push(s.seq);
        } else {
            out.push(RefinedParagraph {
                speaker: a.speaker.clone(),
                name,
                person_id,
                start_ms: s.start_ms,
                end_ms: s.end_ms,
                text: s.text.clone(),
                source_seqs: vec![s.seq],
                mentions: vec![],
            });
        }
    }
    out
}

/// F4 修复:落盘失败时把 `doc.stages.llm` 降级为 "failed",不留在 polish 算出的
/// "done"/"partial"——否则内存态(以及 lib.rs 随后 `emit("llm", &doc.stages.llm)`
/// 发出的事件)会报告"完成",但盘上 refined.json 因写失败仍是旧值(通常 "off"),
/// 前后端状态互相矛盾。落盘失败一律视为"本轮 Aing 没能生效"，与其它阶段"没能
/// 落成盘就不算完成"的语义看齐;错误照样返回给调用方记日志,不吞掉。
pub fn run_llm(
    note_dir: &Path,
    doc: &mut RefinedDoc,
    cfg: &llm::LlmConfig,
    llm_model: &str,
    log: Option<&crate::ailog::Ctx>,
) -> anyhow::Result<()> {
    let (text_outcome, raw_entities) = llm::polish(cfg, &mut doc.paragraphs, log);
    let state = match text_outcome {
        llm::LlmOutcome::Done => "done",
        llm::LlmOutcome::Partial(_) => "partial",
        llm::LlmOutcome::Failed => "failed",
    };
    doc.stages.llm = state.into();
    doc.llm_model = Some(llm_model.to_string());
    // 实体维度:与文本同一批调用产出,stages.entities 跟随 state;实体环节绝不回退修订文本。
    fill_entities(doc, raw_entities, state);
    if let Err(e) = write_refined_atomic(note_dir, doc) {
        doc.stages.llm = "failed".into();
        doc.stages.entities = "failed".into();
        return Err(e);
    }
    Ok(())
}

/// 把原始实体落进 doc:解析规范实体 + 逐段算 mention,`stages.entities` 置为 text_state
/// (与文本 outcome 同源——同一批调用产出)。抽成纯函数便于无网络单测,也隔离实体环节:
/// 无论实体多寡,都不触碰 doc.paragraphs[].text(修订文本已由 polish 定稿)。
pub(crate) fn fill_entities(doc: &mut RefinedDoc, raw: Vec<llm::RawEntity>, text_state: &str) {
    doc.stages.entities = text_state.into();
    let entities = resolve_note_entities(raw);
    let mentions = compute_mentions(&doc.paragraphs, &entities);
    for (p, m) in doc.paragraphs.iter_mut().zip(mentions) {
        p.mentions = m;
    }
    doc.entities = entities;
}

/// 大模型原始实体 → 本篇规范实体:按规范名(trim + 不分大小写)去重,合并别名,
/// 按首次出现顺序分配局部 id `ent_N`。首现的原名即规范名。**不做全局 person 归并**
/// (跨笔记/声纹匹配是 Plan 4 的解析层)。
pub(crate) fn resolve_note_entities(raw: Vec<llm::RawEntity>) -> Vec<Entity> {
    let mut out: Vec<Entity> = Vec::new();
    for r in raw {
        let name = r.name.trim();
        if name.is_empty() {
            continue;
        }
        let key = name.to_lowercase();
        if let Some(e) = out.iter_mut().find(|e| e.name.to_lowercase() == key) {
            for a in r.aliases {
                let a = a.trim().to_string();
                if !a.is_empty()
                    && a.to_lowercase() != key
                    && !e.aliases.iter().any(|x| x.to_lowercase() == a.to_lowercase())
                {
                    e.aliases.push(a);
                }
            }
        } else {
            let id = format!("ent_{}", out.len() + 1);
            let mut aliases: Vec<String> = Vec::new();
            for a in r.aliases {
                let a = a.trim().to_string();
                if !a.is_empty()
                    && a.to_lowercase() != key
                    && !aliases.iter().any(|x| x.to_lowercase() == a.to_lowercase())
                {
                    aliases.push(a);
                }
            }
            out.push(Entity { id, kind: r.kind.trim().to_string(), name: name.to_string(), aliases });
        }
    }
    out
}

/// 在 hay 中找 needle 的所有非重叠出现,返回 char 下标半开区间 [start,end)。
/// 空 needle 返回空。用于把实体名/别名映射到修订后正文的高亮区间。
fn find_char_spans(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let needle_chars = needle.chars().count();
    let mut spans = Vec::new();
    let mut byte = 0usize;
    while let Some(pos) = hay[byte..].find(needle) {
        let abs = byte + pos;
        let char_start = hay[..abs].chars().count();
        spans.push((char_start, char_start + needle_chars));
        byte = abs + needle.len(); // 非重叠推进
    }
    spans
}

/// 对每段,在修订后 `text` 里按各实体 name+aliases 子串搜索,产出提及区间。
/// 单段内所有实体的命中合一起按 (start 升序, 长度降序) 贪心去重叠,保证高亮不交叠、
/// 长匹配优先(别名「灯塔」与全名「灯塔计划」重叠时留全名)。返回与 paragraphs 逐段对齐。
pub(crate) fn compute_mentions(paragraphs: &[RefinedParagraph], entities: &[Entity]) -> Vec<Vec<Mention>> {
    paragraphs
        .iter()
        .map(|p| {
            // 收集 (start, end, entity_id)
            let mut hits: Vec<(usize, usize, &str)> = Vec::new();
            for e in entities {
                for needle in std::iter::once(&e.name).chain(e.aliases.iter()) {
                    for (s, en) in find_char_spans(&p.text, needle) {
                        hits.push((s, en, e.id.as_str()));
                    }
                }
            }
            // start 升序、长度降序;贪心保留不与已选重叠者
            hits.sort_by(|a, b| a.0.cmp(&b.0).then((b.1 - b.0).cmp(&(a.1 - a.0))));
            let mut chosen: Vec<Mention> = Vec::new();
            let mut last_end = 0usize;
            let mut first = true;
            for (s, en, id) in hits {
                if first || s >= last_end {
                    chosen.push(Mention { entity: id.to_string(), start: s, end: en });
                    last_end = en;
                    first = false;
                }
            }
            chosen
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SegmentRecord, SpeakerMeta};
    use std::collections::BTreeMap;

    fn para(text: &str) -> crate::store::RefinedParagraph {
        crate::store::RefinedParagraph {
            speaker: "R1".into(),
            name: None,
            person_id: None,
            start_ms: 0,
            end_ms: 1000,
            text: text.into(),
            source_seqs: vec![0],
            mentions: vec![],
        }
    }

    fn doc_with(texts: &[&str]) -> RefinedDoc {
        RefinedDoc {
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into(), entities: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            paragraphs: texts.iter().map(|t| para(t)).collect(),
        }
    }

    #[test]
    fn resolve_dedups_by_name_case_insensitive_and_merges_aliases() {
        let raw = vec![
            llm::RawEntity { name: "灯塔计划".into(), kind: "project".into(), aliases: vec!["Lighthouse".into()] },
            llm::RawEntity { name: "灯塔计划".into(), kind: "project".into(), aliases: vec!["灯塔".into()] },
            llm::RawEntity { name: "Acme".into(), kind: "org".into(), aliases: vec![] },
            llm::RawEntity { name: "acme".into(), kind: "org".into(), aliases: vec!["ACME 公司".into()] },
        ];
        let ents = resolve_note_entities(raw);
        assert_eq!(ents.len(), 2, "灯塔计划 与 Acme 各归一为一个");
        assert_eq!(ents[0].id, "ent_1");
        assert_eq!(ents[0].name, "灯塔计划");
        // 合并别名(去重、顺序稳定)
        assert!(ents[0].aliases.contains(&"Lighthouse".to_string()));
        assert!(ents[0].aliases.contains(&"灯塔".to_string()));
        assert_eq!(ents[1].id, "ent_2");
        assert_eq!(ents[1].name, "Acme", "首现写法为规范名");
        assert!(ents[1].aliases.contains(&"ACME 公司".to_string()));
    }

    #[test]
    fn compute_mentions_finds_name_and_alias_by_char_offset() {
        let ents = vec![store::Entity {
            id: "ent_1".into(),
            kind: "project".into(),
            name: "灯塔计划".into(),
            aliases: vec!["Lighthouse".into()],
        }];
        // 段落0:开头是「灯塔计划」(char 0..4);段落1:含别名 Lighthouse
        let ps = vec![
            para("灯塔计划下周启动"),
            para("我们叫它 Lighthouse 吧"), // "我们叫它 " 是 5 个 char(含空格),Lighthouse 从 char 5 起
        ];
        let ms = compute_mentions(&ps, &ents);
        assert_eq!(ms[0], vec![store::Mention { entity: "ent_1".into(), start: 0, end: 4 }]);
        assert_eq!(ms[1], vec![store::Mention { entity: "ent_1".into(), start: 5, end: 15 }]);
    }

    #[test]
    fn compute_mentions_non_overlapping_and_empty_when_absent() {
        let ents = vec![store::Entity { id: "ent_1".into(), kind: "term".into(), name: "AB".into(), aliases: vec![] }];
        let ps = vec![para("无关文本")];
        assert!(compute_mentions(&ps, &ents)[0].is_empty());
    }

    /// 假嵌入器:按调用顺序(=段 seq 序)依次返回预置方向向量。
    struct SeqEmbedder {
        dirs: Vec<[f32; 3]>,
        i: usize,
    }
    impl crate::diar::SpeakerEmbedder for SeqEmbedder {
        fn embed(&mut self, _s: &[f32]) -> anyhow::Result<Vec<f32>> {
            let d = self.dirs[self.i.min(self.dirs.len() - 1)];
            self.i += 1;
            Ok(vec![d[0], d[1], d[2]])
        }
    }

    fn seg(seq: u64, source: &str, text: &str, start: u64, end: u64, spk: &str) -> SegmentRecord {
        SegmentRecord {
            seq,
            source: source.into(),
            text: text.into(),
            start_ms: start,
            end_ms: end,
            speaker: Some(spk.into()),
            rms: Some(0.02),
        }
    }

    fn write_wav(dir: &std::path::Path, name: &str, secs: u32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(dir.join(name), spec).unwrap();
        for _ in 0..(16000 * secs) {
            w.write_sample(2000i16).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn run_local_filters_reclusters_and_builds_paragraphs() {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "mic.wav", 30);
        let segs = vec![
            seg(0, "mic", "大家好,今天讲三点。", 0, 5000, "S1"),
            seg(1, "mic", "噪声。", 5000, 5600, "S9"), // 应被过滤(合成占位:≤2 有效字短段)
            seg(2, "mic", "第一点是架构。", 6000, 11_000, "S2"), // 与 seq0 同人(嵌入同向)
            seg(3, "mic", "我有个问题。", 12_000, 20_000, "S3"), // 另一人(≥MIN_CLUSTER_MS,不被判碎片吞并)
        ];
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        let mut e = SeqEmbedder { dirs: vec![a, a, b], i: 0 };
        let doc = run_local(
            dir.path(),
            &segs,
            &BTreeMap::new(),
            Some(&mut e),
            &[],
            "2026-07-06T15:00:00+08:00",
        );
        assert_eq!(doc.discarded_seqs, vec![1]);
        assert_eq!(doc.stages.filter, "done");
        assert_eq!(doc.stages.recluster, "done");
        assert_eq!(doc.stages.llm, "off");
        assert_eq!(doc.paragraphs.len(), 2, "seq0+seq2 并段,seq3 独立");
        assert_eq!(doc.paragraphs[0].source_seqs, vec![0, 2]);
        assert_ne!(doc.paragraphs[0].speaker, doc.paragraphs[1].speaker);
        assert!(crate::store::load_refined(dir.path()).is_some(), "run_local 已落盘");
    }

    #[test]
    fn run_local_without_embedder_skips_recluster_keeps_old_labels() {
        let dir = tempfile::tempdir().unwrap();
        let mut speakers = BTreeMap::new();
        speakers.insert(
            "S1".into(),
            SpeakerMeta {
                name: "老板".into(),
                sources: vec!["mic".into()],
                centroid: None,
                count: 1,
                person_id: Some("P2".into()),
            },
        );
        let segs = vec![seg(0, "mic", "就这样定了。", 0, 4000, "S1")];
        let doc = run_local(dir.path(), &segs, &speakers, None, &[], "t");
        assert_eq!(doc.stages.recluster, "skipped");
        assert_eq!(doc.paragraphs[0].speaker, "S1");
        assert_eq!(doc.paragraphs[0].name.as_deref(), Some("老板"), "旧标签沿用用户改名");
        assert_eq!(doc.paragraphs[0].person_id.as_deref(), Some("P2"), "降级路径继承既有人物关联");
    }

    #[test]
    fn paragraphs_split_at_max_duration() {
        let segs: Vec<SegmentRecord> = (0..5)
            .map(|i| seg(i, "mic", "内容。", i * 20_000, (i + 1) * 20_000, "S1"))
            .collect();
        let assign: Vec<_> = (0..5)
            .map(|i| recluster::Assignment { seq: i, speaker: "R1".into(), name: None, person: None })
            .collect();
        let ps = build_paragraphs(&segs, &[], &assign, &BTreeMap::new());
        assert!(ps.len() >= 2, "100s 同人内容必须按 MAX_PARA_MS 切段");
        assert!(ps.iter().all(|p| p.end_ms - p.start_ms <= MAX_PARA_MS + 20_000));
    }

    #[test]
    fn slice_range_covers_offset_bounds_and_inversion() {
        // offset=0:直接按 ms*16 换算,不做任何偏移。
        assert_eq!(slice_range(1000, 3000, 0, 1_000_000), Some((16_000, 48_000)));
        // offset=60_000(续录/中途授权轨道,从第 60s 才出现):时间轴 ms 须先减掉 offset
        // 才是文件内 ms,直接拿时间轴 ms 当文件内 ms 用是 F2 的 bug。
        assert_eq!(slice_range(61_000, 63_000, 60_000, 1_000_000), Some((16_000, 48_000)));
        // 越界:换算后终点超过 pcm 实际长度,clamp 到 pcm_len;clamp 后 start==end(=pcm_len)
        // 时说明这段完全落在文件外,必须 None。
        assert_eq!(slice_range(0, 1_000_000, 0, 1_000), Some((0, 1_000)));
        assert_eq!(slice_range(100_000, 200_000, 0, 1_000), None, "clamp 后 start==end→None");
        // 倒置:end<=start(含 offset 换算后倒置)一律 None,绝不倒切片。
        assert_eq!(slice_range(3000, 1000, 0, 1_000_000), None);
    }

    #[test]
    fn embed_all_accounts_for_track_offset_ms() {
        // F2 回归:轨道文件的 0 点不总是笔记时间轴的 0 点——续录/中途授权的轨道
        // audio.json 记着非零 offset_ms。mic.wav 只覆盖轨道出现之后的 10s
        // (文件内 0ms == 时间轴 60_000ms),文件内第 1~3 秒写入区别于其余静音的
        // marker 采样值,供嵌入器捕获校验 embed_all 是否先减掉 offset 再切片。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("audio.json"),
            r#"{"schema_version":1,"tracks":{"mic":{"offset_ms":60000}}}"#,
        )
        .unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(dir.path().join("mic.wav"), spec).unwrap();
        for ms in 0..10_000u32 {
            let v: i16 = if (1000..3000).contains(&ms) { 5000 } else { 0 };
            for _ in 0..16 {
                w.write_sample(v).unwrap();
            }
        }
        w.finalize().unwrap();

        // 段时间轴 61_000..63_000ms == 文件内 1000..3000ms(marker 区间)。
        let s = seg(0, "mic", "第一点。", 61_000, 63_000, "S1");
        let kept: Vec<&SegmentRecord> = vec![&s];

        struct CaptureEmbedder(Vec<f32>);
        impl crate::diar::SpeakerEmbedder for CaptureEmbedder {
            fn embed(&mut self, s: &[f32]) -> anyhow::Result<Vec<f32>> {
                self.0.push(s.first().copied().unwrap_or(0.0));
                Ok(vec![0.0])
            }
        }
        let mut e = CaptureEmbedder(Vec::new());
        let out = embed_all(dir.path(), &kept, &mut e).unwrap();
        assert!(out[0].is_some(), "offset 换算后应落在轨道有效范围内,必须产出嵌入");
        assert_eq!(
            e.0[0],
            5000.0 / 32768.0,
            "切片起点须是文件内 1000ms 处的 marker 采样,而非误用时间轴 61_000ms 直接当文件内 ms"
        );
    }

    #[test]
    fn run_llm_success_sets_stage_done_and_persists() {
        // 空段落 → llm::polish 内部 chunk_indices 为空,直接判 Done,不发起网络请求;
        // 验证正常路径下 stages.llm/llm_model 按预期置位且成功落盘。
        let dir = tempfile::tempdir().unwrap();
        let mut doc = RefinedDoc {
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into(), entities: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            paragraphs: vec![],
        };
        let cfg = llm::LlmConfig { base_url: "http://127.0.0.1:1".into(), model: "m".into(), api_key: "k".into() };
        run_llm(dir.path(), &mut doc, &cfg, "m", None).expect("空段落路径不应触网,写盘应成功");
        assert_eq!(doc.stages.llm, "done");
        assert_eq!(doc.llm_model.as_deref(), Some("m"));
        assert!(crate::store::load_refined(dir.path()).is_some(), "已落盘");
    }

    #[test]
    fn run_llm_write_failure_downgrades_stage_to_failed() {
        // F4 回归:落盘失败(此处用不存在的目录模拟——std::fs::write 必然 ENOENT)时,
        // stages.llm 不能停留在 polish 算出的 "done"/"partial"(此处为空段落→Done),
        // 必须降级为 "failed",否则内存态与随后的 emit 会跟磁盘上的旧值(通常 "off")
        // 互相矛盾。错误仍须原样返回给调用方。
        let base = tempfile::tempdir().unwrap();
        let missing_dir = base.path().join("does-not-exist");
        let mut doc = RefinedDoc {
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into(), entities: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            paragraphs: vec![],
        };
        let cfg = llm::LlmConfig { base_url: "http://127.0.0.1:1".into(), model: "m".into(), api_key: "k".into() };
        let err = run_llm(&missing_dir, &mut doc, &cfg, "m", None);
        assert!(err.is_err(), "目录不存在,写盘必须失败");
        assert_eq!(doc.stages.llm, "failed", "写盘失败必须把内存态降级为 failed");
    }

    #[test]
    fn fill_entities_populates_entities_and_mentions() {
        let mut doc = doc_with(&["灯塔计划下周启动"]);
        let raw = vec![llm::RawEntity { name: "灯塔计划".into(), kind: "project".into(), aliases: vec![] }];
        fill_entities(&mut doc, raw, "done");
        assert_eq!(doc.stages.entities, "done");
        assert_eq!(doc.entities.len(), 1);
        assert_eq!(doc.entities[0].id, "ent_1");
        assert_eq!(doc.paragraphs[0].mentions, vec![store::Mention { entity: "ent_1".into(), start: 0, end: 4 }]);
    }

    #[test]
    fn fill_entities_empty_raw_sets_stage_but_no_entities() {
        let mut doc = doc_with(&["你好"]);
        fill_entities(&mut doc, vec![], "done");
        assert_eq!(doc.stages.entities, "done", "成功抽取但无实体也是 done");
        assert!(doc.entities.is_empty());
        assert!(doc.paragraphs[0].mentions.is_empty());
    }

    #[test]
    fn fill_entities_follows_text_state_on_failure() {
        let mut doc = doc_with(&["原文"]);
        fill_entities(&mut doc, vec![], "failed"); // 文本失败 → 实体也 failed、空
        assert_eq!(doc.stages.entities, "failed");
        assert!(doc.entities.is_empty());
    }

    #[test]
    fn run_llm_wires_entities_stage_no_network() {
        // 空段落 → polish 早退 Done 不触网(沿用现有 run_llm 测试同款零网络路径),
        // 验证 run_llm 确实调了 fill_entities:stages.entities 被置位(done)、entities 空。
        let dir = tempfile::tempdir().unwrap();
        let mut doc = doc_with(&[]);
        let cfg = llm::LlmConfig { base_url: "http://127.0.0.1:1".into(), model: "m".into(), api_key: "k".into() };
        run_llm(dir.path(), &mut doc, &cfg, "m", None).unwrap();
        assert_eq!(doc.stages.llm, "done");
        assert_eq!(doc.stages.entities, "done", "run_llm 应经 fill_entities 置位 stages.entities");
        let reloaded = crate::store::load_refined(dir.path()).unwrap();
        assert_eq!(reloaded.stages.entities, "done", "落盘也带上 entities 阶段");
    }

    fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let to = dst.join(entry.file_name());
            if ty.is_dir() {
                copy_dir_all(&entry.path(), &to)?;
            } else {
                std::fs::copy(entry.path(), &to)?;
            }
        }
        Ok(())
    }

    /// golden 校准工具(长期保留,非一次性):对真实会议样本跑本地 Aing 管线,
    /// 产物落在临时工作目录下供 scripts/refine_golden.py 核验聚类/过滤指标。
    /// 源目录只读——先整份拷到 `$TMPDIR/vn-golden-work/<note_id>`,绝不碰用户原始数据。
    ///
    /// 用法(在 src-tauri/ 下):
    /// ```text
    /// VN_MODELS=<模型目录> VN_GOLDEN_NOTE=<会话目录> \
    ///   cargo test --lib refine::tests::golden_generate_refined -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn golden_generate_refined() {
        let src = std::env::var("VN_GOLDEN_NOTE").expect("设置 VN_GOLDEN_NOTE 为 golden 会话目录");
        let src = std::path::PathBuf::from(src);
        let note_id = src
            .file_name()
            .expect("VN_GOLDEN_NOTE 需指向具体会话目录")
            .to_string_lossy()
            .to_string();

        let work_root = std::env::temp_dir().join("vn-golden-work");
        let _ = std::fs::remove_dir_all(&work_root);
        let dst = work_root.join(&note_id);
        copy_dir_all(&src, &dst).expect("拷贝 golden 会话目录到临时工作目录失败");

        let note = crate::store::NoteStore::new(work_root.clone())
            .load(&note_id)
            .expect("加载拷贝目录的 segments/speakers 失败");

        let model_path =
            crate::models::root().join("3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx");
        let mut embedder = crate::diar::SherpaEmbedder::new(&model_path)
            .expect("加载声纹模型失败(设置 VN_MODELS 指向模型目录)");

        let doc = run_local(
            &dst,
            &note.segments,
            &note.speakers,
            Some(&mut embedder as &mut dyn crate::diar::SpeakerEmbedder),
            &[],
            "golden",
        );

        let labels: std::collections::BTreeSet<&str> =
            doc.paragraphs.iter().map(|p| p.speaker.as_str()).collect();
        println!("聚类标签数: {}", labels.len());
        println!("段落数: {}", doc.paragraphs.len());
        println!("discarded 数: {}", doc.discarded_seqs.len());
        println!("工作目录: {}", dst.display());

        assert!(crate::store::load_refined(&dst).is_some(), "refined.json 应已生成");
    }
}
