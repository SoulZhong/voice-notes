//! Aing 产物 refined.json:原始三文件之外的独立终稿,损坏/缺失时 UI 回落原始逐字稿。

use serde::{Deserialize, Serialize};
use std::path::Path;

use super::notelock::NoteLock;

pub const REFINED_SCHEMA_VERSION: u32 = 2;

/// 每笔记修订稿产物文件名(人读真值)。
pub const AING_DOC_FILE: &str = "aing.json";
/// 旧文件名:一次性迁移到 `AING_DOC_FILE`,迁移后保留供回滚。
pub const LEGACY_REFINED_FILE: &str = "refined.json";

fn stage_off() -> String {
    "off".into()
}

/// 实体在段落正文中的一次提及(笔记页高亮 + 图谱建边用)。`start`/`end` 是本段
/// `text` 的字符(char)下标,半开区间 [start, end);`entity` 引用本篇
/// `RefinedDoc.entities[].id`。Plan 3 由大模型产出,本 plan 恒为空。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mention {
    #[serde(default)]
    pub id: String,
    pub entity: String,
    pub start: usize,
    pub end: usize,
}

/// 本篇出现的一个实体(人读真值;全局知识图谱由所有 aing.json 派生、可整库重建)。
/// `id`:人实体复用全局 `person_id`(P<n>),非人实体为新分配 `ent_id`。
/// `kind`:person/org/project/term/decision/task/place/date… 用字符串免枚举迁移。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub id: String,
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinedParagraph {
    pub speaker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 关联的全局声纹库人物 id(P<n>):重聚类种子命中时写入,或用户在说话人条
    /// 手动关联。有它才能把修订稿改名同步进声纹库(会议搭子)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub source_seqs: Vec<u64>,
    /// 本段实体提及区间(Plan 3 填,本 plan 恒空)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Mention>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefineStages {
    pub filter: String,
    pub recluster: String,
    pub llm: String,
    /// 实体抽取阶段:off/running/done/partial/failed(Plan 3 用,本 plan 恒 off)。
    #[serde(default = "stage_off")]
    pub entities: String,
    #[serde(default = "stage_off")]
    pub relations: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinedDoc {
    pub schema_version: u32,
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_model: Option<String>,
    pub stages: RefineStages,
    #[serde(default)]
    pub discarded_seqs: Vec<u64>,
    /// 本篇实体清单(Plan 3 填,本 plan 恒空)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub graph_extraction: Option<super::aing_graph::GraphExtraction>,
    #[serde(default)]
    pub relations: Vec<super::aing_graph::RelationFact>,
    /// 仅供旧关系保持端点归属/证据拆分的 mention ids。它们仍存在于段落 mentions
    /// 以通过图谱结构校验，但不是当前正文的 live mentions，不得进入 UI/搜索索引。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graph_support_mentions: Vec<String>,
    pub paragraphs: Vec<RefinedParagraph>,
}

/// 已持有该笔记 `NoteLock` 时使用的底层原子写。把锁凭据作为参数，避免调用者
/// 意外绕开跨进程互斥，也避免 Aing 管线在锁内重入公共 writer。
pub(crate) fn write_refined_atomic_locked(
    note_dir: &Path,
    doc: &RefinedDoc,
    _lock: &NoteLock,
) -> anyhow::Result<()> {
    let mut doc = doc.clone();
    let note_id = note_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("修订稿目录缺少有效笔记 id"))?;
    crate::store::aing_graph::ensure_graph_ids(note_id, &mut doc);
    let tmp = note_dir.join("aing.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)?;
    std::fs::rename(&tmp, note_dir.join(AING_DOC_FILE))?;
    Ok(())
}

/// 公共整份写入同样服从笔记级跨进程锁；所有 aing.json writer 因而共享一条
/// 串行化边界，固定的 `.tmp` 文件也不会被并发写者竞写。
pub fn write_refined_atomic(note_dir: &Path, doc: &RefinedDoc) -> anyhow::Result<()> {
    let lock = NoteLock::acquire(note_dir)?
        .ok_or_else(|| anyhow::anyhow!("笔记正在被另一进程修改，请稍后重试"))?;
    write_refined_atomic_locked(note_dir, doc, &lock)
}

fn ensure_ids(note_dir: &Path, mut doc: RefinedDoc) -> Option<RefinedDoc> {
    let note_id = note_dir.file_name()?.to_str()?;
    crate::store::aing_graph::ensure_graph_ids(note_id, &mut doc);
    Some(doc)
}

/// `Some(None)` 表示文件不存在；`None` 表示文件存在但读/解析失败。后者不能回退
/// 旧文件，否则损坏的新真值会被旧快照静默覆盖。
fn load_aing_file(note_dir: &Path) -> Option<Option<RefinedDoc>> {
    match std::fs::read(note_dir.join(AING_DOC_FILE)) {
        Ok(bytes) => {
            let doc: RefinedDoc = serde_json::from_slice(&bytes).ok()?;
            Some(Some(ensure_ids(note_dir, doc)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

fn load_legacy_file(note_dir: &Path) -> Option<RefinedDoc> {
    let bytes = std::fs::read(note_dir.join(LEGACY_REFINED_FILE)).ok()?;
    let doc: RefinedDoc = serde_json::from_slice(&bytes).ok()?;
    ensure_ids(note_dir, doc)
}

/// 已持有 NoteLock 的加载路径。旧稿迁移也复用同一锁内 writer，避免重入。
pub(crate) fn load_refined_locked(note_dir: &Path, lock: &NoteLock) -> Option<RefinedDoc> {
    match load_aing_file(note_dir)? {
        Some(doc) => Some(doc),
        None => {
            let doc = load_legacy_file(note_dir)?;
            // 迁移落盘失败不致命(下次加载再试),旧文件不删。
            let _ = write_refined_atomic_locked(note_dir, &doc, lock);
            Some(doc)
        }
    }
}

/// 读修订稿:优先 `aing.json`;缺失时从旧 `refined.json` 一次性迁移(读旧格式→写
/// aing.json,旧文件保留供回滚)。两者皆无或损坏 → None(UI 回落原始逐字稿)。
pub fn load_refined(note_dir: &Path) -> Option<RefinedDoc> {
    match load_aing_file(note_dir)? {
        Some(doc) => Some(doc),
        None => match NoteLock::acquire(note_dir) {
            Ok(Some(lock)) => load_refined_locked(note_dir, &lock),
            // 另一 writer 正持锁时仍可读已有的完整原子快照；若它尚未产生
            // aing.json，则暂时返回旧稿但绝不在无锁状态迁移。
            _ => match load_aing_file(note_dir)? {
                Some(doc) => Some(doc),
                None => load_legacy_file(note_dir),
            },
        },
    }
}

/// aing.json 或旧 refined.json 是否存在(供「是否有修订稿」判断,迁移感知)。
pub fn aing_exists(note_dir: &Path) -> bool {
    note_dir.join(AING_DOC_FILE).exists() || note_dir.join(LEGACY_REFINED_FILE).exists()
}

/// 同进程 read-modify-write 串行锁。跨进程边界始终是 NoteLock；两把锁同时需要
/// 时固定按 NoteLock → REFINED_EDIT_LOCK 获取，杜绝反序死锁。
static REFINED_EDIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 锁内 read-modify-write 骨架:加载 → 就地修改 → 原子落盘。缺失/损坏 → Err
/// (编辑必须以「盘上有可编辑的修订稿」为前提,不能凭空造一份)。
fn update_refined(
    note_dir: &Path,
    f: impl FnOnce(&mut RefinedDoc) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let note_lock = NoteLock::acquire(note_dir)?
        .ok_or_else(|| anyhow::anyhow!("笔记正在被另一进程修改，请稍后重试"))?;
    let _process_guard = REFINED_EDIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut doc = load_refined_locked(note_dir, &note_lock)
        .ok_or_else(|| anyhow::anyhow!("修订稿不存在或已损坏"))?;
    f(&mut doc)?;
    write_refined_atomic_locked(note_dir, &doc, &note_lock)
}

/// 修订稿说话人改名:该 speaker 的全部段落 name 置为新名。
/// 返回该说话人已关联的 person_id(若有),供调用方把改名同步进声纹库。
pub fn rename_refined_speaker(
    note_dir: &Path,
    speaker_id: &str,
    name: &str,
) -> anyhow::Result<Option<String>> {
    let mut person_id = None;
    update_refined(note_dir, |doc| {
        let mut hit = false;
        for p in doc.paragraphs.iter_mut().filter(|p| p.speaker == speaker_id) {
            hit = true;
            p.name = Some(name.to_string());
            if person_id.is_none() {
                person_id = p.person_id.clone();
            }
        }
        anyhow::ensure!(hit, "修订稿中没有该说话人: {speaker_id}");
        Ok(())
    })?;
    Ok(person_id)
}

/// 把修订稿说话人关联到声纹库人物:该 speaker 的全部段落写入 person_id,
/// name 采用库中现名(空名传 None,展示端按「说话人 N」兜底)。
pub fn assign_refined_person(
    note_dir: &Path,
    speaker_id: &str,
    person_id: &str,
    person_name: &str,
) -> anyhow::Result<()> {
    update_refined(note_dir, |doc| {
        let mut hit = false;
        for p in doc.paragraphs.iter_mut().filter(|p| p.speaker == speaker_id) {
            hit = true;
            p.person_id = Some(person_id.to_string());
            p.name = if person_name.is_empty() { None } else { Some(person_name.to_string()) };
        }
        anyhow::ensure!(hit, "修订稿中没有该说话人: {speaker_id}");
        Ok(())
    })
}

/// Agent Aing 写回:按段落下标批量替换 text,并把 stages.llm 置 "done"、记录 llm_model。
/// 约束式写入——只能改文本,说话人/时间戳/段落数一概不可动,这是把「外部 Agent 可写」
/// 的面收到最小的关键:哪怕 Agent 行为失常,最坏也只是文本变差,结构不会被破坏。
/// 任一下标越界或文本为空即整体拒绝(不落盘半份结果)。updates 为空是合法输入,
/// 语义为「已审阅,确无需要修订之处」——同样把 llm 置 done,否则干净稿会被误报失败。
pub fn apply_refined_texts(
    note_dir: &Path,
    updates: &[(usize, String)],
    llm_model: &str,
) -> anyhow::Result<usize> {
    update_refined(note_dir, |doc| {
        for (i, text) in updates {
            anyhow::ensure!(
                *i < doc.paragraphs.len(),
                "段落下标越界: {i}(共 {} 段)",
                doc.paragraphs.len()
            );
            anyhow::ensure!(!text.trim().is_empty(), "第 {i} 段修订文本为空");
        }
        for (i, text) in updates {
            doc.paragraphs[*i].text = text.clone();
        }
        doc.stages.llm = "done".into();
        doc.llm_model = Some(llm_model.to_string());
        Ok(())
    })?;
    Ok(updates.len())
}

/// 只读 join:关联了库人物的段落,展示名跟随库中现名(会议搭子改名 → 历史修订稿
/// 跟着变),person_id 经 redirects 归一到 winner。只改返回值,不落盘——与
/// notes.rs join_person_names 同一哲学。库中无名/人已删除时保留段落原 name。
pub fn join_library_names(doc: &mut RefinedDoc, vp: &super::voiceprints::Voiceprints) {
    for p in doc.paragraphs.iter_mut() {
        let Some(pid) = &p.person_id else { continue };
        let Some(resolved) = super::VoiceprintStore::resolve(vp, pid).map(str::to_string) else {
            continue;
        };
        if let Some(person) = vp.people.get(&resolved) {
            if !person.name.is_empty() {
                p.name = Some(person.name.clone());
            }
        }
        p.person_id = Some(resolved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ensure_graph_ids, evidence_id};

    #[test]
    fn v2_writes_synthesize_stable_mention_ids_without_a_repair_read() {
        let dir = tempfile::tempdir().unwrap();
        let doc = RefinedDoc {
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "2026-07-21T00:00:00+08:00".into(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "done".into(), entities: "done".into(), relations: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            paragraphs: vec![RefinedParagraph {
                speaker: "S1".into(), name: None, person_id: None,
                start_ms: 0, end_ms: 1000, text: "灯塔计划启动".into(), source_seqs: vec![7],
                mentions: vec![Mention { id: String::new(), entity: "ent_1".into(), start: 0, end: 4 }],
            }],
        };

        write_refined_atomic(dir.path(), &doc).unwrap();
        let first: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join(AING_DOC_FILE)).unwrap()).unwrap();
        let first_id = first["paragraphs"][0]["mentions"][0]["id"].as_str().unwrap().to_string();
        write_refined_atomic(dir.path(), &doc).unwrap();
        let second: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join(AING_DOC_FILE)).unwrap()).unwrap();

        assert!(first_id.starts_with("mn_"));
        assert_eq!(first_id.len(), 27);
        assert_eq!(second["paragraphs"][0]["mentions"][0]["id"].as_str(), Some(first_id.as_str()));
        assert_eq!(first.get("graph_extraction"), Some(&serde_json::Value::Null));
        assert_eq!(first.get("relations"), Some(&serde_json::json!([])));
        assert!(first.get("graph_support_mentions").is_none());
    }

    #[test]
    fn schema_v1_defaults_graph_fields() {
        let legacy = r#"{
            "schema_version": 1,
            "generated_at": "2026-07-01T09:00:00+08:00",
            "stages": { "filter": "done", "recluster": "done", "llm": "done" },
            "discarded_seqs": [],
            "paragraphs": [{
                "speaker": "S1", "start_ms": 0, "end_ms": 500,
                "text": "灯塔计划启动", "source_seqs": [7],
                "mentions": [{ "entity": "ent_1", "start": 0, "end": 4 }]
            }]
        }"#;
        let mut doc: RefinedDoc = serde_json::from_str(legacy).unwrap();

        ensure_graph_ids("note-1", &mut doc);
        let first_id = doc.paragraphs[0].mentions[0].id.clone();
        ensure_graph_ids("note-1", &mut doc);

        assert_eq!(doc.stages.relations, "off");
        assert!(doc.graph_extraction.is_none());
        assert!(doc.relations.is_empty());
        assert!(doc.graph_support_mentions.is_empty());
        assert!(doc.paragraphs[0].mentions[0].id.starts_with("mn_"));
        assert_eq!(doc.paragraphs[0].mentions[0].id, first_id);
        assert_eq!(doc.paragraphs[0].mentions[0].id.len(), 27);
    }

    #[test]
    fn evidence_ids_include_normalized_quote() {
        let first = evidence_id("note-1", &[7], 1, 3, "  灯塔   计划 ");
        let same_normalized = evidence_id("note-1", &[7], 1, 3, "灯塔 计划");
        let changed_quote = evidence_id("note-1", &[7], 1, 3, "灯塔项目");

        assert_eq!(first, same_normalized);
        assert_ne!(first, changed_quote);
    }

    #[test]
    fn roundtrip_and_corrupt_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_refined(dir.path()).is_none(), "缺失返回 None");
        let doc = RefinedDoc {
            schema_version: 1,
            generated_at: "2026-07-06T15:00:00+08:00".into(),
            llm_model: Some("deepseek-chat".into()),
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into(), entities: "off".into(), relations: "off".into() },
            discarded_seqs: vec![1, 2],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            paragraphs: vec![RefinedParagraph {
                speaker: "R1".into(), name: Some("张三".into()), person_id: Some("P1".into()),
                start_ms: 0, end_ms: 5000, text: "你好。".into(), source_seqs: vec![0, 3],
                mentions: vec![],
            }],
        };
        write_refined_atomic(dir.path(), &doc).unwrap();
        let got = load_refined(dir.path()).expect("写后可读");
        assert_eq!(got.paragraphs.len(), 1);
        assert_eq!(got.discarded_seqs, vec![1, 2]);
        assert_eq!(got.paragraphs[0].name.as_deref(), Some("张三"));
        assert_eq!(got.paragraphs[0].person_id.as_deref(), Some("P1"));
        std::fs::write(dir.path().join(AING_DOC_FILE), "{broken").unwrap();
        assert!(load_refined(dir.path()).is_none(), "损坏返回 None 不 panic");
    }

    #[test]
    fn legacy_refined_json_migrates_to_aing_json_on_load() {
        let dir = tempfile::tempdir().unwrap();
        // 只有旧 refined.json,没有 aing.json
        let legacy = r#"{
            "schema_version": 1,
            "generated_at": "2026-07-01T09:00:00+08:00",
            "stages": { "filter": "done", "recluster": "done", "llm": "done" },
            "discarded_seqs": [],
            "paragraphs": [
                { "speaker": "S1", "start_ms": 0, "end_ms": 500, "text": "旧稿", "source_seqs": [0] }
            ]
        }"#;
        std::fs::write(dir.path().join("refined.json"), legacy).unwrap();
        assert!(!dir.path().join("aing.json").exists());

        let doc = load_refined(dir.path()).expect("应从旧 refined.json 迁移出");
        assert_eq!(doc.paragraphs[0].text, "旧稿");
        // 迁移把 aing.json 落盘,旧文件保留供回滚
        assert!(dir.path().join("aing.json").exists(), "迁移应写出 aing.json");
        assert!(dir.path().join("refined.json").exists(), "旧文件保留");
    }

    #[test]
    fn aing_json_takes_precedence_over_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |text: &str| format!(
            r#"{{"schema_version":1,"generated_at":"t","stages":{{"filter":"done","recluster":"done","llm":"done"}},"discarded_seqs":[],"paragraphs":[{{"speaker":"S1","start_ms":0,"end_ms":1,"text":"{text}","source_seqs":[0]}}]}}"#
        );
        std::fs::write(dir.path().join("aing.json"), mk("新稿")).unwrap();
        std::fs::write(dir.path().join("refined.json"), mk("旧稿")).unwrap();
        assert_eq!(load_refined(dir.path()).unwrap().paragraphs[0].text, "新稿");
    }

    #[test]
    fn aing_exists_considers_both_filenames() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!aing_exists(dir.path()));
        std::fs::write(dir.path().join("refined.json"), "{}").unwrap();
        assert!(aing_exists(dir.path()), "只有旧文件也算有");
        std::fs::remove_file(dir.path().join("refined.json")).unwrap();
        std::fs::write(dir.path().join("aing.json"), "{}").unwrap();
        assert!(aing_exists(dir.path()));
    }

    #[test]
    fn aing_fields_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let doc = RefinedDoc {
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "2026-07-16T10:00:00+08:00".into(),
            llm_model: None,
            stages: RefineStages {
                filter: "done".into(),
                recluster: "done".into(),
                llm: "off".into(),
                entities: "off".into(),
                relations: "off".into(),
            },
            discarded_seqs: vec![],
            entities: vec![Entity {
                id: "ent_1".into(),
                kind: "project".into(),
                name: "灯塔计划".into(),
                aliases: vec!["Lighthouse".into()],
            }],
            paragraphs: vec![RefinedParagraph {
                speaker: "S1".into(),
                name: None,
                person_id: None,
                start_ms: 0,
                end_ms: 1000,
                text: "灯塔计划下周启动".into(),
                source_seqs: vec![0],
                mentions: vec![Mention { id: "mn_000000000000000000000000".into(), entity: "ent_1".into(), start: 0, end: 4 }],
            }],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
        };
        write_refined_atomic(dir.path(), &doc).unwrap();
        let back = load_refined(dir.path()).unwrap();
        assert_eq!(back.entities, doc.entities);
        assert_eq!(back.paragraphs[0].mentions, doc.paragraphs[0].mentions);
        assert_eq!(back.stages.entities, "off");
    }

    #[test]
    fn old_doc_without_aing_fields_still_loads_with_empty_defaults() {
        // 旧 refined.json:没有 entities / mentions / stages.entities 键
        let dir = tempfile::tempdir().unwrap();
        let old = r#"{
            "schema_version": 1,
            "generated_at": "2026-07-01T09:00:00+08:00",
            "stages": { "filter": "done", "recluster": "done", "llm": "done" },
            "discarded_seqs": [],
            "paragraphs": [
                { "speaker": "S1", "start_ms": 0, "end_ms": 500, "text": "你好", "source_seqs": [0] }
            ]
        }"#;
        std::fs::write(dir.path().join("refined.json"), old).unwrap();
        let doc = load_refined(dir.path()).expect("旧结构应能加载");
        assert!(doc.entities.is_empty());
        assert!(doc.paragraphs[0].mentions.is_empty());
        assert!(doc.graph_support_mentions.is_empty());
        assert_eq!(doc.stages.entities, "off", "缺 stages.entities 键默认 off");
    }

    /// 旧版 refined.json(无 person_id 字段)必须照常解析——字段缺省为 None。
    #[test]
    fn old_schema_without_person_id_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("refined.json"),
            r#"{"schema_version":1,"generated_at":"t",
                "stages":{"filter":"done","recluster":"done","llm":"off"},
                "paragraphs":[{"speaker":"R1","start_ms":0,"end_ms":1000,"text":"嗯。","source_seqs":[0]}]}"#,
        )
        .unwrap();
        let doc = load_refined(dir.path()).expect("旧 schema 可读");
        assert!(doc.paragraphs[0].person_id.is_none());
        assert!(doc.paragraphs[0].name.is_none());
    }

    fn para(speaker: &str, name: Option<&str>, person: Option<&str>, start: u64) -> RefinedParagraph {
        RefinedParagraph {
            speaker: speaker.into(),
            name: name.map(str::to_string),
            person_id: person.map(str::to_string),
            start_ms: start,
            end_ms: start + 1000,
            text: "内容。".into(),
            source_seqs: vec![start / 1000],
            mentions: vec![],
        }
    }

    fn write_doc(dir: &Path, paragraphs: Vec<RefinedParagraph>) {
        let doc = RefinedDoc {
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into(), entities: "off".into(), relations: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            paragraphs,
        };
        write_refined_atomic(dir, &doc).unwrap();
    }

    #[test]
    fn every_writer_respects_note_lock_and_locked_writer_can_commit() {
        let dir = tempfile::tempdir().unwrap();
        write_doc(dir.path(), vec![para("R1", None, None, 0)]);
        let original = std::fs::read(dir.path().join(AING_DOC_FILE)).unwrap();
        let guard = crate::store::notelock::NoteLock::try_exclusive(dir.path())
            .unwrap()
            .unwrap();

        let mut replacement = load_refined(dir.path()).unwrap();
        replacement.paragraphs[0].text = "整份替换。".into();
        assert!(
            write_refined_atomic(dir.path(), &replacement).is_err(),
            "公共整写也必须服从 NoteLock"
        );
        assert!(
            apply_refined_texts(dir.path(), &[(0, "局部替换。".into())], "m").is_err(),
            "公共 read-modify-write 必须服从同一把 NoteLock"
        );
        assert!(
            rename_refined_speaker(dir.path(), "R1", "新名字").is_err(),
            "改名 writer 必须服从同一把 NoteLock"
        );
        assert!(
            assign_refined_person(dir.path(), "R1", "P1", "人物").is_err(),
            "人物关联 writer 必须服从同一把 NoteLock"
        );
        assert_eq!(
            std::fs::read(dir.path().join(AING_DOC_FILE)).unwrap(),
            original,
            "抢锁失败的写入不能触碰盘上真值"
        );

        write_refined_atomic_locked(dir.path(), &replacement, &guard).unwrap();
        drop(guard);
        assert_eq!(
            load_refined(dir.path()).unwrap().paragraphs[0].text,
            "整份替换。"
        );
    }

    #[test]
    fn rename_updates_all_paragraphs_of_speaker_and_returns_linked_person() {
        let dir = tempfile::tempdir().unwrap();
        write_doc(
            dir.path(),
            vec![para("R1", None, Some("P3"), 0), para("R2", Some("李四"), None, 1000), para("R1", None, Some("P3"), 2000)],
        );
        let pid = rename_refined_speaker(dir.path(), "R1", "张三").unwrap();
        assert_eq!(pid.as_deref(), Some("P3"), "返回关联人物供调用方同步声纹库");
        let doc = load_refined(dir.path()).unwrap();
        assert_eq!(doc.paragraphs[0].name.as_deref(), Some("张三"));
        assert_eq!(doc.paragraphs[2].name.as_deref(), Some("张三"));
        assert_eq!(doc.paragraphs[1].name.as_deref(), Some("李四"), "别的说话人不受影响");
    }

    #[test]
    fn rename_unknown_speaker_errors_and_leaves_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        write_doc(dir.path(), vec![para("R1", None, None, 0)]);
        assert!(rename_refined_speaker(dir.path(), "R9", "张三").is_err());
        let doc = load_refined(dir.path()).unwrap();
        assert!(doc.paragraphs[0].name.is_none(), "未命中不落盘任何修改");
        // 无修订稿时同样报错,不凭空造文件。
        let empty = tempfile::tempdir().unwrap();
        assert!(rename_refined_speaker(empty.path(), "R1", "张三").is_err());
    }

    #[test]
    fn assign_person_links_and_adopts_library_name() {
        let dir = tempfile::tempdir().unwrap();
        write_doc(dir.path(), vec![para("R1", Some("旧名"), None, 0), para("R1", None, None, 1000)]);
        assign_refined_person(dir.path(), "R1", "P7", "王五").unwrap();
        let doc = load_refined(dir.path()).unwrap();
        for p in &doc.paragraphs {
            assert_eq!(p.person_id.as_deref(), Some("P7"));
            assert_eq!(p.name.as_deref(), Some("王五"));
        }
        // 关联未命名人物:name 清为 None,展示端按「说话人 N」兜底。
        assign_refined_person(dir.path(), "R1", "P8", "").unwrap();
        let doc = load_refined(dir.path()).unwrap();
        assert!(doc.paragraphs[0].name.is_none());
        assert_eq!(doc.paragraphs[0].person_id.as_deref(), Some("P8"));
    }

    #[test]
    fn apply_refined_texts_updates_and_marks_llm_done() {
        let dir = tempfile::tempdir().unwrap();
        write_doc(dir.path(), vec![para("R1", None, None, 0), para("R2", None, None, 1000)]);
        let n = apply_refined_texts(dir.path(), &[(1, "修订后。".into())], "claude-agent").unwrap();
        assert_eq!(n, 1);
        let doc = load_refined(dir.path()).unwrap();
        assert_eq!(doc.paragraphs[0].text, "内容。", "未提交的段落不动");
        assert_eq!(doc.paragraphs[1].text, "修订后。");
        assert_eq!(doc.stages.llm, "done");
        assert_eq!(doc.llm_model.as_deref(), Some("claude-agent"));
        assert_eq!(doc.paragraphs.len(), 2, "段落数不可变");
    }

    #[test]
    fn apply_refined_texts_empty_updates_means_reviewed_clean() {
        let dir = tempfile::tempdir().unwrap();
        write_doc(dir.path(), vec![para("R1", None, None, 0)]);
        assert_eq!(apply_refined_texts(dir.path(), &[], "m").unwrap(), 0);
        let doc = load_refined(dir.path()).unwrap();
        assert_eq!(doc.stages.llm, "done", "空 updates = 已审阅无需修订,同样算完成");
        assert_eq!(doc.paragraphs[0].text, "内容。");
    }

    #[test]
    fn apply_refined_texts_rejects_bad_input_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        write_doc(dir.path(), vec![para("R1", None, None, 0)]);
        assert!(apply_refined_texts(dir.path(), &[(9, "x".into())], "m").is_err(), "下标越界");
        assert!(apply_refined_texts(dir.path(), &[(0, "  ".into())], "m").is_err(), "空文本");
        // 混合提交里带一个坏项:整体拒绝,好项也不落盘
        assert!(apply_refined_texts(dir.path(), &[(0, "好的。".into()), (5, "x".into())], "m").is_err());
        let doc = load_refined(dir.path()).unwrap();
        assert_eq!(doc.paragraphs[0].text, "内容。", "整体拒绝,未落盘任何修改");
        assert_eq!(doc.stages.llm, "off");
        // 无修订稿时报错,不凭空造文件
        let empty = tempfile::tempdir().unwrap();
        assert!(apply_refined_texts(empty.path(), &[(0, "x".into())], "m").is_err());
    }

    #[test]
    fn join_library_names_follows_current_names_and_redirects() {
        use crate::store::voiceprints::{Person, Voiceprints};
        let mut vp = Voiceprints::default();
        vp.people.insert("P1".into(), Person { name: "张三".into(), ..Default::default() });
        vp.redirects.insert("P2".into(), "P1".into());
        let mut doc = RefinedDoc {
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into(), entities: "off".into(), relations: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            paragraphs: vec![
                para("R1", Some("旧快照名"), Some("P2"), 0), // 已被合并的引用:归一到 P1 且跟随现名
                para("R2", Some("现场名"), None, 1000),      // 未关联:原样保留
                para("R3", Some("留名"), Some("P9"), 2000),  // 悬空引用:容忍,保留原 name
            ],
        };
        join_library_names(&mut doc, &vp);
        assert_eq!(doc.paragraphs[0].person_id.as_deref(), Some("P1"));
        assert_eq!(doc.paragraphs[0].name.as_deref(), Some("张三"));
        assert_eq!(doc.paragraphs[1].name.as_deref(), Some("现场名"));
        assert_eq!(doc.paragraphs[2].name.as_deref(), Some("留名"));
    }
}
