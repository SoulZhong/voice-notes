//! 精修产物 refined.json:原始三文件之外的独立终稿,损坏/缺失时 UI 回落原始逐字稿。

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const REFINED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinedParagraph {
    pub speaker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 关联的全局声纹库人物 id(P<n>):重聚类种子命中时写入,或用户在说话人条
    /// 手动关联。有它才能把精修稿改名同步进声纹库(会议搭子)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub source_seqs: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefineStages {
    pub filter: String,
    pub recluster: String,
    pub llm: String,
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
    pub paragraphs: Vec<RefinedParagraph>,
}

pub fn write_refined_atomic(note_dir: &Path, doc: &RefinedDoc) -> anyhow::Result<()> {
    let tmp = note_dir.join("refined.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(doc)?)?;
    std::fs::rename(&tmp, note_dir.join("refined.json"))?;
    Ok(())
}

pub fn load_refined(note_dir: &Path) -> Option<RefinedDoc> {
    let bytes = std::fs::read(note_dir.join("refined.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// refined.json 编辑锁:改名/关联是 read-modify-write,无互斥的并发调用会互相覆盖
/// 丢更新(与 notes.rs EDIT_LOCK 同一哲学,独立锁——精修稿编辑与笔记编辑互不相干)。
/// 精修管线整写 refined.json 的竞争由命令层「精修中拒绝编辑」guard 挡住,不进此锁。
static REFINED_EDIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 锁内 read-modify-write 骨架:加载 → 就地修改 → 原子落盘。缺失/损坏 → Err
/// (编辑必须以「盘上有可编辑的精修稿」为前提,不能凭空造一份)。
fn update_refined(
    note_dir: &Path,
    f: impl FnOnce(&mut RefinedDoc) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let _guard = REFINED_EDIT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut doc =
        load_refined(note_dir).ok_or_else(|| anyhow::anyhow!("精修稿不存在或已损坏"))?;
    f(&mut doc)?;
    write_refined_atomic(note_dir, &doc)
}

/// 精修稿说话人改名:该 speaker 的全部段落 name 置为新名。
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
        anyhow::ensure!(hit, "精修稿中没有该说话人: {speaker_id}");
        Ok(())
    })?;
    Ok(person_id)
}

/// 把精修稿说话人关联到声纹库人物:该 speaker 的全部段落写入 person_id,
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
        anyhow::ensure!(hit, "精修稿中没有该说话人: {speaker_id}");
        Ok(())
    })
}

/// 只读 join:关联了库人物的段落,展示名跟随库中现名(会议搭子改名 → 历史精修稿
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

    #[test]
    fn roundtrip_and_corrupt_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_refined(dir.path()).is_none(), "缺失返回 None");
        let doc = RefinedDoc {
            schema_version: 1,
            generated_at: "2026-07-06T15:00:00+08:00".into(),
            llm_model: Some("deepseek-chat".into()),
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into() },
            discarded_seqs: vec![1, 2],
            paragraphs: vec![RefinedParagraph {
                speaker: "R1".into(), name: Some("张三".into()), person_id: Some("P1".into()),
                start_ms: 0, end_ms: 5000, text: "你好。".into(), source_seqs: vec![0, 3],
            }],
        };
        write_refined_atomic(dir.path(), &doc).unwrap();
        let got = load_refined(dir.path()).expect("写后可读");
        assert_eq!(got.paragraphs.len(), 1);
        assert_eq!(got.discarded_seqs, vec![1, 2]);
        assert_eq!(got.paragraphs[0].name.as_deref(), Some("张三"));
        assert_eq!(got.paragraphs[0].person_id.as_deref(), Some("P1"));
        std::fs::write(dir.path().join("refined.json"), "{broken").unwrap();
        assert!(load_refined(dir.path()).is_none(), "损坏返回 None 不 panic");
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
        }
    }

    fn write_doc(dir: &Path, paragraphs: Vec<RefinedParagraph>) {
        let doc = RefinedDoc {
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into() },
            discarded_seqs: vec![],
            paragraphs,
        };
        write_refined_atomic(dir, &doc).unwrap();
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
        // 无精修稿时同样报错,不凭空造文件。
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
    fn join_library_names_follows_current_names_and_redirects() {
        use crate::store::voiceprints::{Person, Voiceprints};
        let mut vp = Voiceprints::default();
        vp.people.insert("P1".into(), Person { name: "张三".into(), ..Default::default() });
        vp.redirects.insert("P2".into(), "P1".into());
        let mut doc = RefinedDoc {
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into() },
            discarded_seqs: vec![],
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
