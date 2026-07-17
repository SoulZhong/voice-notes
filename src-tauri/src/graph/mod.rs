//! Aing 全局知识图谱:从所有 aing.json 派生的 SQLite 索引(实体注册表 + 笔记↔实体边)。
//! 纯增值产物——任何失败只降级,绝不打断 Aing;丢了可从 aing.json 整库重建。

use std::path::Path;

pub(crate) const GRAPH_FILE: &str = "graph.sqlite";

/// 写路径进程内串行(rusqlite::Connection 非 Sync;沿用全仓"每次操作现开连接 + Mutex 门禁"惯例)。
pub(crate) static GRAPH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS entities (
  id         TEXT PRIMARY KEY,
  kind       TEXT NOT NULL,
  name       TEXT NOT NULL,
  aliases    TEXT NOT NULL DEFAULT '[]',
  is_person  INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT
);
CREATE TABLE IF NOT EXISTS note_entities (
  note_id       TEXT NOT NULL,
  entity_id     TEXT NOT NULL,
  mention_count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (note_id, entity_id)
);
CREATE INDEX IF NOT EXISTS idx_ne_entity ON note_entities(entity_id);
";

/// 打开(必要时创建)图谱库,建表 + 设 WAL/busy_timeout。幂等。
pub(crate) fn open(data_root: &Path) -> anyhow::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(data_root.join(GRAPH_FILE))?;
    conn.busy_timeout(std::time::Duration::from_millis(3000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

use crate::store;

/// 规范名:trim + 小写(跨笔记去重 / person 名匹配的归一键)。
fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// 实体 → (全局 id, is_person)。人实体按精确规范名匹配声纹库 person 复用 person_id;
/// 否则(含匹配不上的人)按非人规则用 e:<规范名>。别名也参与人匹配(取任一命中)。
pub(crate) fn resolve_global_id(vp: &store::Voiceprints, e: &store::Entity) -> (String, bool) {
    if e.kind == "person" {
        let keys: Vec<String> = std::iter::once(norm(&e.name)).chain(e.aliases.iter().map(|a| norm(a))).collect();
        for (pid, person) in vp.people.iter() {
            if person.name.trim().is_empty() {
                continue;
            }
            // 只认归一后是自身的规范 person(排除已被合并的 loser)
            if store::VoiceprintStore::resolve(vp, pid) != Some(pid.as_str()) {
                continue;
            }
            if keys.iter().any(|k| k == &norm(&person.name)) {
                return (pid.clone(), true);
            }
        }
    }
    (format!("e:{}", norm(&e.name)), false)
}

/// 把一篇笔记 aing.json 里的局部实体(ent_N)逐个解析成全局 id,供笔记页高亮点击导航用。
/// 无 aing.json/无实体 → 空;名为空的实体跳过。读盘失败不 panic(load_refined 返回 None)。
pub(crate) fn resolve_local_ids(data_root: &Path, note_id: &str) -> anyhow::Result<Vec<(String, String, bool)>> {
    store::validate_note_id(note_id)?;
    let dir = data_root.join("notes").join(note_id);
    let Some(doc) = store::load_refined(&dir) else { return Ok(Vec::new()) };
    let vp = store::VoiceprintStore::new(data_root.to_path_buf()).load();
    Ok(doc
        .entities
        .iter()
        .filter(|e| !e.name.trim().is_empty())
        .map(|e| {
            let (gid, is_person) = resolve_global_id(&vp, e);
            (e.id.clone(), gid, is_person)
        })
        .collect())
}

/// 把一篇笔记的实体/提及写入图谱:整篇替换该笔记的边(先删后插,幂等)。
/// 调用方负责"失败只 eprintln,不打断 Aing"。
pub(crate) fn upsert_note(data_root: &Path, note_id: &str, doc: &store::RefinedDoc) -> anyhow::Result<()> {
    store::validate_note_id(note_id)?;
    let vp = store::VoiceprintStore::new(data_root.to_path_buf()).load();
    let _guard = GRAPH_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut conn = open(data_root)?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM note_entities WHERE note_id = ?1", [note_id])?;
    for e in &doc.entities {
        if e.name.trim().is_empty() {
            continue;
        }
        let (gid, is_person) = resolve_global_id(&vp, e);
        // mention_count:本篇 paragraphs 里引用该实体**局部 id** 的提及数
        let count: i64 = doc
            .paragraphs
            .iter()
            .flat_map(|p| p.mentions.iter())
            .filter(|m| m.entity == e.id)
            .count() as i64;
        // upsert 实体:name 保留首见(稳定),别名并集
        let existing_aliases: Option<String> = tx
            .query_row("SELECT aliases FROM entities WHERE id = ?1", [&gid], |r| r.get(0))
            .ok();
        let merged = merge_aliases(existing_aliases.as_deref(), &e.name, &e.aliases);
        tx.execute(
            "INSERT INTO entities(id, kind, name, aliases, is_person, updated_at) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(id) DO UPDATE SET aliases=?4, updated_at=?6",
            rusqlite::params![gid, e.kind, e.name, merged, is_person as i64, doc.generated_at],
        )?;
        tx.execute(
            "INSERT INTO note_entities(note_id, entity_id, mention_count) VALUES(?1,?2,?3)
             ON CONFLICT(note_id, entity_id) DO UPDATE SET mention_count = mention_count + excluded.mention_count",
            rusqlite::params![note_id, gid, count],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// 别名并集(existing JSON ∪ {新实体别名};排除等于 name 的),返回 JSON 数组字符串。
fn merge_aliases(existing_json: Option<&str>, name: &str, new_aliases: &[String]) -> String {
    let mut set: Vec<String> = existing_json
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .unwrap_or_default();
    let name_key = norm(name);
    for a in new_aliases {
        let a = a.trim();
        if !a.is_empty() && norm(a) != name_key && !set.iter().any(|x| norm(x) == norm(a)) {
            set.push(a.to_string());
        }
    }
    serde_json::to_string(&set).unwrap_or_else(|_| "[]".into())
}

/// 清表后遍历 notes 根下所有笔记目录,逐篇 load_refined 重灌。返回入图笔记数。
/// 损坏/无 aing.json/无实体的笔记静默跳过(全仓损坏容忍)。
pub(crate) fn rebuild_all(data_root: &Path) -> anyhow::Result<usize> {
    {
        let _guard = GRAPH_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let conn = open(data_root)?;
        conn.execute_batch("DELETE FROM note_entities; DELETE FROM entities;")?;
    }
    let notes_root = data_root.join("notes");
    let mut n = 0usize;
    let Ok(rd) = std::fs::read_dir(&notes_root) else { return Ok(0) };
    for e in rd.flatten().filter(|e| e.path().is_dir()) {
        let Some(id) = e.file_name().to_str().map(|s| s.to_string()) else { continue };
        if store::validate_note_id(&id).is_err() {
            continue;
        }
        if let Some(doc) = store::load_refined(&e.path()) {
            if doc.entities.is_empty() {
                continue;
            }
            if let Err(err) = upsert_note(data_root, &id, &doc) {
                eprintln!("graph rebuild: 跳过 {id}: {err}");
                continue;
            }
            n += 1;
        }
    }
    Ok(n)
}

/// 一个全局实体 + 聚合计数(实体浏览用)。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EntityRow {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub is_person: bool,
    pub note_count: i64,
    pub mention_total: i64,
}

/// 与某实体共现的实体(实体详情面板「相关实体」用)。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CoEntity {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub shared_notes: i64,
}

/// 单个实体的详情:聚合行 + 出现笔记(含提及数)+ 共现实体。
#[derive(Debug, Clone)]
pub(crate) struct EntityDetail {
    pub row: EntityRow,
    pub notes: Vec<(String, i64)>,
    pub related: Vec<CoEntity>,
}

/// 列全部有边的实体,按出现笔记数降序(孤儿实体——无边——不列)。
pub(crate) fn list_entities(data_root: &Path) -> anyhow::Result<Vec<EntityRow>> {
    let conn = open(data_root)?;
    let mut stmt = conn.prepare(
        "SELECT e.id, e.kind, e.name, e.aliases, e.is_person,
                COUNT(ne.note_id) AS note_count, COALESCE(SUM(ne.mention_count),0) AS mention_total
         FROM entities e JOIN note_entities ne ON e.id = ne.entity_id
         GROUP BY e.id ORDER BY note_count DESC, e.name ASC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let aliases_json: String = r.get(3)?;
            Ok(EntityRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                name: r.get(2)?,
                aliases: serde_json::from_str(&aliases_json).unwrap_or_default(),
                is_person: r.get::<_, i64>(4)? != 0,
                note_count: r.get(5)?,
                mention_total: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 某实体出现在哪些笔记(按该笔记内提及数降序)。
pub(crate) fn entity_notes(data_root: &Path, entity_id: &str) -> anyhow::Result<Vec<String>> {
    let conn = open(data_root)?;
    let mut stmt = conn.prepare(
        "SELECT note_id FROM note_entities WHERE entity_id = ?1 ORDER BY mention_count DESC, note_id ASC",
    )?;
    let rows = stmt
        .query_map([entity_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 与给定笔记共享实体的其他笔记 + 共享实体数,降序(相关笔记)。
pub(crate) fn related_notes(data_root: &Path, note_id: &str) -> anyhow::Result<Vec<(String, i64)>> {
    let conn = open(data_root)?;
    let mut stmt = conn.prepare(
        "SELECT ne2.note_id, COUNT(*) AS shared
         FROM note_entities ne1 JOIN note_entities ne2 ON ne1.entity_id = ne2.entity_id
         WHERE ne1.note_id = ?1 AND ne2.note_id != ?1
         GROUP BY ne2.note_id ORDER BY shared DESC, ne2.note_id ASC",
    )?;
    let rows = stmt
        .query_map([note_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 实体共现边:两实体在同一笔记出现即连边,边权 = 共同出现的笔记数。`a < b` 去重,
/// 降序返回。力导图用。孤立实体(无任何共现)不产生边,由列表视图承载。
pub(crate) fn cooccurrence_edges(data_root: &Path) -> anyhow::Result<Vec<(String, String, i64)>> {
    let conn = open(data_root)?;
    let mut stmt = conn.prepare(
        "SELECT ne1.entity_id, ne2.entity_id, COUNT(DISTINCT ne1.note_id) AS shared
         FROM note_entities ne1
         JOIN note_entities ne2 ON ne1.note_id = ne2.note_id AND ne1.entity_id < ne2.entity_id
         GROUP BY ne1.entity_id, ne2.entity_id
         ORDER BY shared DESC, ne1.entity_id ASC, ne2.entity_id ASC",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 查单个实体详情。实体不存在 → Ok(None)。只读,不加锁。
pub(crate) fn entity_detail(data_root: &Path, gid: &str) -> anyhow::Result<Option<EntityDetail>> {
    let conn = open(data_root)?;
    // 基本聚合行(LEFT JOIN 容错;正常每实体都有边)
    let mut stmt = conn.prepare(
        "SELECT e.id, e.kind, e.name, e.aliases, e.is_person,
                COUNT(ne.note_id), COALESCE(SUM(ne.mention_count),0)
         FROM entities e LEFT JOIN note_entities ne ON e.id = ne.entity_id
         WHERE e.id = ?1 GROUP BY e.id",
    )?;
    let row = stmt
        .query_map([gid], |r| {
            let aliases_json: String = r.get(3)?;
            Ok(EntityRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                name: r.get(2)?,
                aliases: serde_json::from_str(&aliases_json).unwrap_or_default(),
                is_person: r.get::<_, i64>(4)? != 0,
                note_count: r.get(5)?,
                mention_total: r.get(6)?,
            })
        })?
        .next()
        .transpose()?;
    let Some(row) = row else { return Ok(None) };
    drop(stmt);
    // 出现笔记(提及降序)
    let mut s2 = conn.prepare(
        "SELECT note_id, mention_count FROM note_entities WHERE entity_id = ?1
         ORDER BY mention_count DESC, note_id ASC",
    )?;
    let notes = s2
        .query_map([gid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(s2);
    // 共现实体(共享笔记数降序)
    let mut s3 = conn.prepare(
        "SELECT e2.id, e2.kind, e2.name, COUNT(DISTINCT ne1.note_id) AS shared
         FROM note_entities ne1
         JOIN note_entities ne2 ON ne1.note_id = ne2.note_id AND ne2.entity_id != ne1.entity_id
         JOIN entities e2 ON e2.id = ne2.entity_id
         WHERE ne1.entity_id = ?1
         GROUP BY e2.id ORDER BY shared DESC, e2.name ASC",
    )?;
    let related = s3
        .query_map([gid], |r| {
            Ok(CoEntity { id: r.get(0)?, kind: r.get(1)?, name: r.get(2)?, shared_notes: r.get(3)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(EntityDetail { row, notes, related }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Entity, Mention, RefineStages, RefinedDoc, RefinedParagraph};

    fn ent(id: &str, kind: &str, name: &str, aliases: &[&str]) -> Entity {
        Entity { id: id.into(), kind: kind.into(), name: name.into(), aliases: aliases.iter().map(|s| s.to_string()).collect() }
    }
    fn doc_with(entities: Vec<Entity>, paras: Vec<RefinedParagraph>) -> RefinedDoc {
        RefinedDoc {
            schema_version: 1, generated_at: "t".into(), llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "done".into(), entities: "done".into() },
            discarded_seqs: vec![], entities, paragraphs: paras,
        }
    }
    fn para(text: &str, mentions: Vec<Mention>) -> RefinedParagraph {
        RefinedParagraph { speaker: "R1".into(), name: None, person_id: None, start_ms: 0, end_ms: 1, text: text.into(), source_seqs: vec![0], mentions }
    }
    fn write_note(root: &Path, id: &str, doc: &RefinedDoc) {
        let dir = root.join("notes").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        store::write_refined_atomic(&dir, doc).unwrap();
    }
    fn write_vp(root: &Path, json: &str) {
        std::fs::write(root.join("voiceprints.json"), json).unwrap();
    }

    #[test]
    fn upsert_dedups_nonperson_across_notes_and_counts_mentions() {
        let root = tempfile::tempdir().unwrap();
        let d1 = doc_with(
            vec![ent("ent_1", "project", "灯塔计划", &["Lighthouse"])],
            vec![para("灯塔计划下周启动", vec![Mention { entity: "ent_1".into(), start: 0, end: 4 }])],
        );
        let d2 = doc_with(
            vec![ent("ent_1", "project", "灯塔计划", &[])],
            vec![para("继续灯塔计划,灯塔计划排期", vec![
                Mention { entity: "ent_1".into(), start: 2, end: 6 },
                Mention { entity: "ent_1".into(), start: 7, end: 11 },
            ])],
        );
        write_note(root.path(), "n1", &d1);
        write_note(root.path(), "n2", &d2);
        upsert_note(root.path(), "n1", &d1).unwrap();
        upsert_note(root.path(), "n2", &d2).unwrap();

        let ents = list_entities(root.path()).unwrap();
        assert_eq!(ents.len(), 1, "同规范名跨笔记归一为一个全局实体");
        assert_eq!(ents[0].id, "e:灯塔计划");
        assert_eq!(ents[0].note_count, 2);
        assert_eq!(ents[0].mention_total, 3, "n1 一次 + n2 两次");
        assert!(ents[0].aliases.contains(&"Lighthouse".to_string()), "别名跨笔记并集");
    }

    #[test]
    fn upsert_person_matches_voiceprint_by_exact_name() {
        let root = tempfile::tempdir().unwrap();
        write_vp(root.path(), r#"{"schema_version":1,"next_person":2,"people":{"P1":{"name":"张三"},"P2":{"name":""}}}"#);
        let d = doc_with(
            vec![ent("ent_1", "person", "张三", &[]), ent("ent_2", "person", "李四", &[])],
            vec![para("张三和李四开会", vec![])],
        );
        write_note(root.path(), "n1", &d);
        upsert_note(root.path(), "n1", &d).unwrap();
        let ids: std::collections::HashSet<String> = list_entities(root.path()).unwrap().into_iter().map(|e| e.id).collect();
        assert!(ids.contains("P1"), "张三 精确匹配声纹库 person → 复用 P1");
        assert!(ids.contains("e:李四"), "李四 无匹配 → 图内局部实体");
    }

    #[test]
    fn upsert_is_idempotent_and_replaces_note_edges() {
        let root = tempfile::tempdir().unwrap();
        let d = doc_with(vec![ent("ent_1", "term", "AB", &[])], vec![para("AB", vec![Mention { entity: "ent_1".into(), start: 0, end: 2 }])]);
        write_note(root.path(), "n1", &d);
        upsert_note(root.path(), "n1", &d).unwrap();
        upsert_note(root.path(), "n1", &d).unwrap(); // 重跑
        let ents = list_entities(root.path()).unwrap();
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].note_count, 1, "重跑不产生重复边");
    }

    #[test]
    fn upsert_sums_mentions_when_two_locals_resolve_to_same_global() {
        let root = tempfile::tempdir().unwrap();
        write_vp(root.path(), r#"{"schema_version":1,"next_person":2,"people":{"P1":{"name":"张三"}}}"#);
        // ent_1 名"张三"(3 提及);ent_2 名"老张" alias"张三"(1 提及)——都精确匹配 P1
        let d = doc_with(
            vec![ent("ent_1", "person", "张三", &[]), ent("ent_2", "person", "老张", &["张三"])],
            vec![para("张三张三张三老张", vec![
                Mention { entity: "ent_1".into(), start: 0, end: 2 },
                Mention { entity: "ent_1".into(), start: 2, end: 4 },
                Mention { entity: "ent_1".into(), start: 4, end: 6 },
                Mention { entity: "ent_2".into(), start: 6, end: 8 },
            ])],
        );
        write_note(root.path(), "n1", &d);
        upsert_note(root.path(), "n1", &d).unwrap();
        let ents = list_entities(root.path()).unwrap();
        // 两个局部实体都归到 P1,mention 求和 = 4(不是被覆盖成 1)
        let p1 = ents.iter().find(|e| e.id == "P1").expect("张三/老张 都归 P1");
        assert_eq!(p1.mention_total, 4, "两个局部实体的提及应求和,不被覆盖");
        // 幂等:再 upsert 一次仍是 4(DELETE 先清)
        upsert_note(root.path(), "n1", &d).unwrap();
        assert_eq!(list_entities(root.path()).unwrap().iter().find(|e| e.id == "P1").unwrap().mention_total, 4, "重跑幂等,不翻倍");
    }

    #[test]
    fn rebuild_all_scans_all_aing_json() {
        let root = tempfile::tempdir().unwrap();
        write_note(root.path(), "n1", &doc_with(vec![ent("ent_1", "org", "Acme", &[])], vec![para("Acme", vec![])]));
        write_note(root.path(), "n2", &doc_with(vec![ent("ent_1", "org", "Acme", &[])], vec![para("Acme", vec![])]));
        let n = rebuild_all(root.path()).unwrap();
        assert_eq!(n, 2, "两篇入图");
        assert_eq!(list_entities(root.path()).unwrap()[0].note_count, 2);
        // 幂等:再 rebuild 一次结果一致
        rebuild_all(root.path()).unwrap();
        assert_eq!(list_entities(root.path()).unwrap().len(), 1);
    }

    #[test]
    fn entity_notes_lists_notes_for_entity() {
        let root = tempfile::tempdir().unwrap();
        upsert_note(root.path(), "n1", &doc_with(vec![ent("ent_1","project","灯塔计划",&[])], vec![para("灯塔计划",vec![Mention{entity:"ent_1".into(),start:0,end:4}])])).unwrap();
        upsert_note(root.path(), "n2", &doc_with(vec![ent("ent_1","project","灯塔计划",&[])], vec![para("灯塔计划",vec![])])).unwrap();
        let notes = entity_notes(root.path(), "e:灯塔计划").unwrap();
        assert_eq!(notes.len(), 2);
        assert!(notes.contains(&"n1".to_string()) && notes.contains(&"n2".to_string()));
    }

    #[test]
    fn related_notes_ranks_by_shared_entities() {
        let root = tempfile::tempdir().unwrap();
        // n1: {灯塔计划, Acme};n2: {灯塔计划, Acme};n3: {灯塔计划}
        let mk = |names: &[&str]| doc_with(
            names.iter().enumerate().map(|(i,nm)| ent(&format!("ent_{}",i+1), "term", nm, &[])).collect(),
            vec![para("x", vec![])],
        );
        upsert_note(root.path(), "n1", &mk(&["灯塔计划","Acme"])).unwrap();
        upsert_note(root.path(), "n2", &mk(&["灯塔计划","Acme"])).unwrap();
        upsert_note(root.path(), "n3", &mk(&["灯塔计划"])).unwrap();
        let rel = related_notes(root.path(), "n1").unwrap();
        assert_eq!(rel[0].0, "n2", "n2 共享 2 个实体排最前");
        assert_eq!(rel[0].1, 2);
        assert!(rel.iter().any(|(id, c)| id == "n3" && *c == 1));
        assert!(!rel.iter().any(|(id, _)| id == "n1"), "不含自己");
    }

    #[test]
    fn open_creates_tables_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        // 两表存在
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('entities','note_entities')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
        drop(conn);
        // 再开一次不报错(IF NOT EXISTS)
        let _ = open(dir.path()).unwrap();
        assert!(dir.path().join("graph.sqlite").exists());
    }

    #[test]
    fn cooccurrence_edges_dedup_and_weight() {
        let root = tempfile::tempdir().unwrap();
        // 每篇给一组实体(各一次提及);n1/n2={A,B},n3={A,C}
        let mk = |names: &[&str]| doc_with(
            names.iter().enumerate().map(|(i, nm)| ent(&format!("ent_{}", i + 1), "term", nm, &[])).collect(),
            vec![para("x", vec![])],
        );
        upsert_note(root.path(), "n1", &mk(&["A", "B"])).unwrap();
        upsert_note(root.path(), "n2", &mk(&["A", "B"])).unwrap();
        upsert_note(root.path(), "n3", &mk(&["A", "C"])).unwrap();
        let edges = cooccurrence_edges(root.path()).unwrap();
        // (e:a,e:b) 共现 2 篇;(e:a,e:c) 共现 1 篇;B、C 从不共现
        assert!(edges.iter().any(|(a, b, w)| a == "e:a" && b == "e:b" && *w == 2), "A,B 共享 2");
        assert!(edges.iter().any(|(a, b, w)| a == "e:a" && b == "e:c" && *w == 1), "A,C 共享 1");
        assert!(!edges.iter().any(|(a, b, _)| (a == "e:b" && b == "e:c") || (a == "e:c" && b == "e:b")), "B,C 不共现");
        assert!(edges.iter().all(|(a, b, _)| a < b), "a<b 去重,不出现反向对");
    }

    #[test]
    fn entity_detail_aggregates_notes_and_related() {
        let root = tempfile::tempdir().unwrap();
        // n1={A(2 提及),B};n2={A(1 提及),C}
        let mk = |names: &[&str], a_mentions: usize| {
            let ents: Vec<_> = names.iter().enumerate()
                .map(|(i, nm)| ent(&format!("ent_{}", i + 1), "term", nm, &[])).collect();
            let ms = (0..a_mentions).map(|_| Mention { entity: "ent_1".into(), start: 0, end: 1 }).collect();
            doc_with(ents, vec![para("x", ms)])
        };
        upsert_note(root.path(), "n1", &mk(&["A", "B"], 2)).unwrap();
        upsert_note(root.path(), "n2", &mk(&["A", "C"], 1)).unwrap();

        let d = entity_detail(root.path(), "e:a").unwrap().expect("A 存在");
        assert_eq!(d.row.note_count, 2, "出现在 2 篇");
        assert_eq!(d.row.mention_total, 3, "提及 2+1");
        assert_eq!(d.notes.len(), 2);
        assert_eq!(d.notes[0].1, 2, "提及最多的笔记排前(n1=2)");
        let rel: Vec<&str> = d.related.iter().map(|r| r.name.as_str()).collect();
        assert!(rel.contains(&"B") && rel.contains(&"C"), "共现 B 与 C");
        assert!(d.related.iter().all(|r| r.shared_notes == 1), "各共享 1 篇");
        assert!(entity_detail(root.path(), "e:zzz").unwrap().is_none(), "不存在实体 → None");
    }

    #[test]
    fn resolve_local_ids_maps_person_and_nonperson() {
        let root = tempfile::tempdir().unwrap();
        write_vp(root.path(), r#"{"schema_version":1,"next_person":2,"people":{"P1":{"name":"张三"}}}"#);
        let d = doc_with(
            vec![
                ent("ent_1", "person", "张三", &[]),
                ent("ent_2", "project", "灯塔计划", &[]),
                ent("ent_3", "person", "李四", &[]),
            ],
            vec![para("张三推进灯塔计划,李四列席", vec![])],
        );
        write_note(root.path(), "n1", &d);
        let links = resolve_local_ids(root.path(), "n1").unwrap();
        assert!(links.iter().any(|(l, g, p)| l == "ent_1" && g == "P1" && *p), "张三→P1 人");
        assert!(links.iter().any(|(l, g, p)| l == "ent_2" && g == "e:灯塔计划" && !*p), "灯塔计划→非人");
        assert!(links.iter().any(|(l, g, p)| l == "ent_3" && g == "e:李四" && !*p), "李四无匹配→退化非人");
        assert!(resolve_local_ids(root.path(), "no-such-note").unwrap().is_empty(), "无 aing.json → 空");
    }
}
