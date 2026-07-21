//! Aing 全局知识图谱:从所有 aing.json 派生的 SQLite 索引(实体注册表 + 笔记↔实体边)。
//! 纯增值产物——任何失败只降级,绝不打断 Aing;丢了可从 aing.json 整库重建。

use std::path::Path;

#[allow(dead_code)] // Durable graph contract; command/API consumers land in subsequent tasks.
pub(crate) mod overrides;
#[allow(dead_code)] // Canonical graph consumers land in the following indexing/API tasks.
pub(crate) mod canonical;
#[allow(dead_code)] // Semantic index query consumers land in the following API task.
pub(crate) mod index;
#[allow(dead_code)] // Pure snapshot contract; canonical projection consumes it next.
pub(crate) mod resolve;

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
CREATE TABLE IF NOT EXISTS entity_redirects (
  old_id TEXT PRIMARY KEY,
  new_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS entity_name_overrides (
  id   TEXT PRIMARY KEY,
  name TEXT NOT NULL
);
";

/// 沿重定向链追到底(改名/合并留下的 old_id→new_id 映射),供解析层与查询层统一收口。
/// **不随 rebuild_all 清空**——它是用户显式改名/合并的留痕,不是从 aing.json 可重建的派生
/// 数据,必须跨重建存活,否则改名下次启动就被冲掉。步数上限防环。
fn resolve_redirect(conn: &rusqlite::Connection, id: &str) -> String {
    let mut cur = id.to_string();
    for _ in 0..8 {
        let next: Option<String> = conn
            .query_row("SELECT new_id FROM entity_redirects WHERE old_id = ?1", [&cur], |r| r.get(0))
            .ok();
        match next {
            Some(n) if n != cur => cur = n,
            _ => return cur,
        }
    }
    cur
}

/// 打开(必要时创建)图谱库,建表 + 设 WAL/busy_timeout。幂等。
pub(crate) fn open(data_root: &Path) -> anyhow::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(data_root.join(GRAPH_FILE))?;
    conn.busy_timeout(std::time::Duration::from_millis(3000))?;
    // v2 是由 index::rebuild_atomic 整库替换的派生快照。兼容查询暂时复用本函数，
    // 但不能在读到 v2 后补建 v1 治理表；治理真值已经迁到 knowledge-overrides.json。
    let schema_version = conn
        .query_row("SELECT schema_version FROM graph_meta LIMIT 1", [], |row| {
            row.get::<_, u32>(0)
        })
        .ok();
    if schema_version == Some(index::GRAPH_SCHEMA_VERSION) {
        return Ok(conn);
    }
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
    let conn = open(data_root)?;
    Ok(doc
        .entities
        .iter()
        .filter(|e| !e.name.trim().is_empty())
        .map(|e| {
            let (gid, is_person) = resolve_global_id(&vp, e);
            let gid = resolve_redirect(&conn, &gid);
            (e.id.clone(), gid, is_person)
        })
        .collect())
}

/// 把一篇笔记的实体/提及写入图谱:整篇替换该笔记的边(先删后插,幂等)。
/// 调用方负责"失败只 eprintln,不打断 Aing"。
#[allow(dead_code)] // v1 compatibility helper; production indexing now uses index::RebuildScheduler.
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
        // 过一遍改名/合并留下的重定向:旧名下次 Aing/整库重建也收口到用户纠正后的规范 id。
        let gid = resolve_redirect(&tx, &gid);
        // 显示名覆盖:用户手动改过名的实体,重建/重灌时不用 aing.json 里的原始提取名,
        // 否则 rebuild_all 清表重插会把改好的名字冲回原样(id 靠 redirect 收口了,名字不会)。
        let name_override: Option<String> = tx
            .query_row("SELECT name FROM entity_name_overrides WHERE id = ?1", [&gid], |r| r.get(0))
            .ok();
        let display_name = name_override.as_deref().unwrap_or(&e.name);
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
        if name_override.is_some() {
            // 有显示名覆盖:名字也强制跟着覆盖走(不是"首见胜"),否则改名后半路又混进旧名。
            tx.execute(
                "INSERT INTO entities(id, kind, name, aliases, is_person, updated_at) VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(id) DO UPDATE SET name=?3, aliases=?4, updated_at=?6",
                rusqlite::params![gid, e.kind, display_name, merged, is_person as i64, doc.generated_at],
            )?;
        } else {
            tx.execute(
                "INSERT INTO entities(id, kind, name, aliases, is_person, updated_at) VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(id) DO UPDATE SET aliases=?4, updated_at=?6",
                rusqlite::params![gid, e.kind, e.name, merged, is_person as i64, doc.generated_at],
            )?;
        }
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

/// 实体改名结果:new_id 是改名后的规范 id(人实体 id 不变,非人实体 id 随名字重算);
/// merged=true 表示撞上已存在的同名实体,已自动合并(边计数相加、别名并集、旧行删除)。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RenameOutcome {
    pub new_id: String,
    pub merged: bool,
}

/// 改实体显示名。人实体(id 不以 e: 开头)委托 VoiceprintStore::rename——那是全应用统一
/// 的人物改名入口(会议搭子/精修稿改名走的同一处),id 本身不变,图这边顺带把 entities.name
/// 现地更新一份便于图谱页即时反映(声纹库仍是真值源)。非人实体 id=e:<规范名>,改名即换 id:
/// 若新规范名与某个已存在的**不同**实体撞了,自动合并(边计数相加/别名并集/删旧行);否则
/// 就是纯改名(迁移该实体的所有边到新 id)。两种情形都在 entity_redirects 留一条旧→新的
/// 记录,保证下次 Aing/rebuild_all 时旧名的提及依然收口到这个新规范 id,不会改了又被冲回去。
pub(crate) fn rename_entity(data_root: &Path, old_id: &str, new_name: &str) -> anyhow::Result<RenameOutcome> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        anyhow::bail!("名字不能为空");
    }
    if !old_id.starts_with("e:") {
        // 人实体:id 是 person_id,不随名字变;真值源是声纹库。
        store::VoiceprintStore::new(data_root.to_path_buf()).rename(old_id, new_name)?;
        let _guard = GRAPH_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let conn = open(data_root)?;
        // 现地更新图谱展示名,失败不影响改名本身(声纹库已是真值源,图谱下次 upsert 会追上)。
        let _ = conn.execute(
            "UPDATE entities SET name = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![new_name, chrono::Local::now().to_rfc3339(), old_id],
        );
        return Ok(RenameOutcome { new_id: old_id.to_string(), merged: false });
    }

    let new_id = format!("e:{}", norm(new_name));
    let _guard = GRAPH_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut conn = open(data_root)?;
    let now = chrono::Local::now().to_rfc3339();

    if new_id == old_id {
        // 规范名不变(仅大小写/首尾空白差异):纯换展示名,id 不变;仍记覆盖,防 rebuild 冲回。
        conn.execute(
            "UPDATE entities SET name = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![new_name, now, old_id],
        )?;
        conn.execute(
            "INSERT INTO entity_name_overrides(id, name) VALUES(?1, ?2)
             ON CONFLICT(id) DO UPDATE SET name = ?2",
            rusqlite::params![old_id, new_name],
        )?;
        return Ok(RenameOutcome { new_id, merged: false });
    }

    let tx = conn.transaction()?;
    let target_exists: bool = tx
        .query_row("SELECT 1 FROM entities WHERE id = ?1", [&new_id], |_| Ok(()))
        .is_ok();

    if target_exists {
        // 合并:old 的边并入 new(计数相加,与 upsert_note 的求和公式一致),别名并集,删旧行。
        tx.execute(
            "INSERT INTO note_entities(note_id, entity_id, mention_count)
             SELECT note_id, ?1, mention_count FROM note_entities WHERE entity_id = ?2
             ON CONFLICT(note_id, entity_id) DO UPDATE SET mention_count = mention_count + excluded.mention_count",
            rusqlite::params![new_id, old_id],
        )?;
        tx.execute("DELETE FROM note_entities WHERE entity_id = ?1", [old_id])?;
        let old_name: Option<String> =
            tx.query_row("SELECT name FROM entities WHERE id = ?1", [old_id], |r| r.get(0)).ok();
        let old_aliases: Option<String> =
            tx.query_row("SELECT aliases FROM entities WHERE id = ?1", [old_id], |r| r.get(0)).ok();
        let existing_aliases: Option<String> =
            tx.query_row("SELECT aliases FROM entities WHERE id = ?1", [&new_id], |r| r.get(0)).ok();
        let mut merged_aliases = existing_aliases.clone().unwrap_or_else(|| "[]".into());
        if let Some(on) = &old_name {
            merged_aliases = merge_aliases(Some(&merged_aliases), on, &[]);
        }
        if let Some(oa) = old_aliases.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()) {
            let target_name: String =
                tx.query_row("SELECT name FROM entities WHERE id = ?1", [&new_id], |r| r.get(0))?;
            merged_aliases = merge_aliases(Some(&merged_aliases), &target_name, &oa);
        }
        tx.execute(
            "UPDATE entities SET name = ?1, aliases = ?2, updated_at = ?3 WHERE id = ?4",
            rusqlite::params![new_name, merged_aliases, now, new_id],
        )?;
        tx.execute("DELETE FROM entities WHERE id = ?1", [old_id])?;
    } else {
        // 纯改名:整行连同它的边一起迁到新 id(new_id 尚不存在,不会撞主键)。
        tx.execute(
            "UPDATE entities SET id = ?1, name = ?2, updated_at = ?3 WHERE id = ?4",
            rusqlite::params![new_id, new_name, now, old_id],
        )?;
        tx.execute(
            "UPDATE note_entities SET entity_id = ?1 WHERE entity_id = ?2",
            rusqlite::params![new_id, old_id],
        )?;
    }
    tx.execute(
        "INSERT INTO entity_redirects(old_id, new_id) VALUES(?1, ?2)
         ON CONFLICT(old_id) DO UPDATE SET new_id = ?2",
        rusqlite::params![old_id, new_id],
    )?;
    // 路径压缩:曾经重定向到 old_id 的旧记录,一并转向新的 new_id,避免链越拖越长。
    tx.execute(
        "UPDATE entity_redirects SET new_id = ?1 WHERE new_id = ?2 AND old_id != ?2",
        rusqlite::params![new_id, old_id],
    )?;
    tx.execute(
        "INSERT INTO entity_name_overrides(id, name) VALUES(?1, ?2)
         ON CONFLICT(id) DO UPDATE SET name = ?2",
        rusqlite::params![new_id, new_name],
    )?;
    tx.commit()?;
    Ok(RenameOutcome { new_id, merged: target_exists })
}

/// 清表后遍历 notes 根下所有笔记目录,逐篇 load_refined 重灌。返回入图笔记数。
/// 损坏/无 aing.json/无实体的笔记静默跳过(全仓损坏容忍)。
#[allow(dead_code)] // v1 compatibility helper; startup now requests an atomic v2 rebuild.
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

/// 笔记节点(文章视角力导图):每篇笔记 + 它含多少个不同实体(节点大小)+ 总提及数。
/// 返回 (note_id, entity_count, mention_total),entity_count 降序。标题由上层 join。
pub(crate) fn note_nodes(data_root: &Path) -> anyhow::Result<Vec<(String, i64, i64)>> {
    let conn = open(data_root)?;
    let mut stmt = conn.prepare(
        "SELECT note_id, COUNT(DISTINCT entity_id) AS ecount, COALESCE(SUM(mention_count),0) AS mtotal
         FROM note_entities GROUP BY note_id ORDER BY ecount DESC, note_id ASC",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 笔记共享边(文章视角,实体共现图的对偶):两篇笔记共享实体即连边,边权 = 共享的
/// 不同实体数。`a < b` 去重,降序返回。只共享 0 个实体的笔记对不产生边。
pub(crate) fn note_shared_edges(data_root: &Path) -> anyhow::Result<Vec<(String, String, i64)>> {
    let conn = open(data_root)?;
    let mut stmt = conn.prepare(
        "SELECT ne1.note_id, ne2.note_id, COUNT(DISTINCT ne1.entity_id) AS shared
         FROM note_entities ne1
         JOIN note_entities ne2 ON ne1.entity_id = ne2.entity_id AND ne1.note_id < ne2.note_id
         GROUP BY ne1.note_id, ne2.note_id
         ORDER BY shared DESC, ne1.note_id ASC, ne2.note_id ASC",
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
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "done".into(), entities: "done".into(), relations: "off".into() },
            discarded_seqs: vec![], entities, graph_extraction: None, relations: vec![], paragraphs: paras,
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
            vec![para("灯塔计划下周启动", vec![Mention { id: String::new(), entity: "ent_1".into(), start: 0, end: 4 }])],
        );
        let d2 = doc_with(
            vec![ent("ent_1", "project", "灯塔计划", &[])],
            vec![para("继续灯塔计划,灯塔计划排期", vec![
                Mention { id: String::new(), entity: "ent_1".into(), start: 2, end: 6 },
                Mention { id: String::new(), entity: "ent_1".into(), start: 7, end: 11 },
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
        let d = doc_with(vec![ent("ent_1", "term", "AB", &[])], vec![para("AB", vec![Mention { id: String::new(), entity: "ent_1".into(), start: 0, end: 2 }])]);
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
                Mention { id: String::new(), entity: "ent_1".into(), start: 0, end: 2 },
                Mention { id: String::new(), entity: "ent_1".into(), start: 2, end: 4 },
                Mention { id: String::new(), entity: "ent_1".into(), start: 4, end: 6 },
                Mention { id: String::new(), entity: "ent_2".into(), start: 6, end: 8 },
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
        upsert_note(root.path(), "n1", &doc_with(vec![ent("ent_1","project","灯塔计划",&[])], vec![para("灯塔计划",vec![Mention{id:String::new(),entity:"ent_1".into(),start:0,end:4}])])).unwrap();
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
    fn note_graph_nodes_and_edges_are_entity_graph_dual() {
        let root = tempfile::tempdir().unwrap();
        // n1={A,B};n2={A,B};n3={A,C}——与 cooccurrence 用例同布局,验证对偶关系。
        let mk = |names: &[&str]| doc_with(
            names.iter().enumerate().map(|(i, nm)| ent(&format!("ent_{}", i + 1), "term", nm, &[])).collect(),
            vec![para("x", vec![])],
        );
        upsert_note(root.path(), "n1", &mk(&["A", "B"])).unwrap();
        upsert_note(root.path(), "n2", &mk(&["A", "B"])).unwrap();
        upsert_note(root.path(), "n3", &mk(&["A", "C"])).unwrap();

        // 节点:每篇笔记 + 它含的不同实体数(节点大小信号)。
        let nodes = note_nodes(root.path()).unwrap();
        let n1 = nodes.iter().find(|(id, ..)| id == "n1").expect("n1");
        assert_eq!(n1.1, 2, "n1 含 A、B 两个实体");
        let n3 = nodes.iter().find(|(id, ..)| id == "n3").expect("n3");
        assert_eq!(n3.1, 2, "n3 含 A、C 两个实体");

        // 边:两篇笔记共享实体即连边,边权=共享的不同实体数。
        let edges = note_shared_edges(root.path()).unwrap();
        // n1、n2 都是 {A,B} → 共享 2 个实体;n1、n3 共享 A → 1;n2、n3 共享 A → 1。
        assert!(edges.iter().any(|(a, b, w)| a == "n1" && b == "n2" && *w == 2), "n1,n2 共享 2");
        assert!(edges.iter().any(|(a, b, w)| a == "n1" && b == "n3" && *w == 1), "n1,n3 共享 1");
        assert!(edges.iter().any(|(a, b, w)| a == "n2" && b == "n3" && *w == 1), "n2,n3 共享 1");
        assert!(edges.iter().all(|(a, b, _)| a < b), "a<b 去重");
    }

    #[test]
    fn entity_detail_aggregates_notes_and_related() {
        let root = tempfile::tempdir().unwrap();
        // n1={A(2 提及),B};n2={A(1 提及),C}
        let mk = |names: &[&str], a_mentions: usize| {
            let ents: Vec<_> = names.iter().enumerate()
                .map(|(i, nm)| ent(&format!("ent_{}", i + 1), "term", nm, &[])).collect();
            let ms = (0..a_mentions).map(|_| Mention { id: String::new(), entity: "ent_1".into(), start: 0, end: 1 }).collect();
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

    #[test]
    fn rename_entity_pure_rename_migrates_id_and_edges() {
        let root = tempfile::tempdir().unwrap();
        upsert_note(
            root.path(), "n1",
            &doc_with(vec![ent("ent_1", "term", "B端", &[])], vec![para("B端", vec![Mention { id: String::new(), entity: "ent_1".into(), start: 0, end: 2 }])]),
        ).unwrap();
        let outcome = rename_entity(root.path(), "e:b端", "B 端").unwrap();
        assert_eq!(outcome.new_id, "e:b 端");
        assert!(!outcome.merged);
        let ents = list_entities(root.path()).unwrap();
        assert_eq!(ents.len(), 1, "旧行迁移不重复");
        assert_eq!(ents[0].id, "e:b 端");
        assert_eq!(ents[0].name, "B 端");
        assert_eq!(ents[0].note_count, 1, "边跟着迁,不丢");
    }

    #[test]
    fn rename_entity_merges_on_collision_and_sums_counts() {
        let root = tempfile::tempdir().unwrap();
        upsert_note(root.path(), "n1", &doc_with(vec![ent("ent_1", "term", "B端", &["旧名"])], vec![para("x", vec![])])).unwrap();
        upsert_note(root.path(), "n2", &doc_with(vec![ent("ent_1", "term", "B 端", &[])], vec![para("x", vec![])])).unwrap();
        let outcome = rename_entity(root.path(), "e:b端", "B 端").unwrap();
        assert_eq!(outcome.new_id, "e:b 端");
        assert!(outcome.merged, "撞上已存在的 B 端 → 合并");
        let ents = list_entities(root.path()).unwrap();
        assert_eq!(ents.len(), 1, "旧行被删,只剩合并后的一个");
        assert_eq!(ents[0].note_count, 2, "两篇笔记都算上");
        assert!(ents[0].aliases.iter().any(|a| a == "旧名"), "旧实体的别名并入");
    }

    #[test]
    fn rename_entity_redirect_survives_rebuild_all() {
        let root = tempfile::tempdir().unwrap();
        let d = doc_with(vec![ent("ent_1", "term", "B端", &[])], vec![para("x", vec![])]);
        write_note(root.path(), "n1", &d); // rebuild_all 从磁盘 aing.json 扫,upsert_note 本身不落盘
        upsert_note(root.path(), "n1", &d).unwrap();
        rename_entity(root.path(), "e:b端", "B 端").unwrap();
        // 整库重建(从 aing.json 重灌,aing.json 里存的还是旧名"B端")
        rebuild_all(root.path()).unwrap();
        let ents = list_entities(root.path()).unwrap();
        assert_eq!(ents.len(), 1, "重建后没有裂成新旧两个实体");
        assert_eq!(ents[0].id, "e:b 端", "旧名经重定向收口到改名后的规范 id");
        assert_eq!(ents[0].name, "B 端");
    }

    #[test]
    fn rename_entity_person_delegates_to_voiceprint_store() {
        let root = tempfile::tempdir().unwrap();
        write_vp(root.path(), r#"{"schema_version":1,"next_person":2,"people":{"P1":{"name":"张三"}}}"#);
        upsert_note(root.path(), "n1", &doc_with(vec![ent("ent_1", "person", "张三", &[])], vec![para("x", vec![])])).unwrap();
        let outcome = rename_entity(root.path(), "P1", "张三丰").unwrap();
        assert_eq!(outcome.new_id, "P1", "人实体 id 不变");
        assert!(!outcome.merged);
        let vp = store::VoiceprintStore::new(root.path().to_path_buf()).load();
        assert_eq!(vp.people["P1"].name, "张三丰", "声纹库才是人物改名的真值源");
    }

    #[test]
    fn rename_entity_rejects_empty_name() {
        let root = tempfile::tempdir().unwrap();
        upsert_note(root.path(), "n1", &doc_with(vec![ent("ent_1", "term", "X", &[])], vec![para("x", vec![])])).unwrap();
        assert!(rename_entity(root.path(), "e:x", "   ").is_err());
    }
}
