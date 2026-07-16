# Aing Phase 2b · Plan A — 图谱查询命令 + 全局 id 解析 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为图谱页/实体导航暴露后端能力:共现边查询、实体详情聚合、笔记局部实体→全局 id 解析,以及四个 Tauri 命令(纯后端,无 UI、无新依赖)。

**Architecture:** 在既有 `graph/mod.rs`(SQLite 知识图谱)上加三个纯 SQL/解析函数,配 `ipc.rs` 可序列化镜像结构 + `lib.rs` 四个薄命令(照 `note_related` 模板:失败/空一律降级返回空,绝不 `Err`)。查询走只读短连接(`open`),不加锁(与既有 `list_entities`/`related_notes` 同);解析复用既有 `resolve_global_id`。

**Tech Stack:** Rust、rusqlite(bundled,已有)、tauri command、serde。**本 plan 不引入任何新依赖**(d3-force 属 Plan C)。

## Global Constraints

- **核心红线**:图谱是纯增值派生索引。所有新命令查询失败/panic 只 `eprintln!` + 返回空(空 `Vec` / `GraphData{nodes:vec![],edges:vec![]}` / `None`),`Ok(...)` 不 `Err`,绝不塌页。graph 层纯函数按需返回 `anyhow::Result`,由命令层兜底。
- **可见性**:graph 层新函数/结构 `pub(crate)`;ipc 结构 `pub` + `#[derive(Debug, Clone, Serialize)]`。
- **全局 id 语义**(既有 `resolve_global_id`,不改):人实体精确规范名匹配声纹库非-loser person → 复用 `person_id`;否则(含匹配不上的人)→ `e:<规范名(trim+lower)>`、`is_person=false`。
- **规范名** `norm(s)` = `s.trim().to_lowercase()`(graph 私有,已有)。测试里 `"A"` 的全局 id 是 `"e:a"`。
- **无 emoji / 无 UI**。gates:`cargo test --lib`、`cargo check --manifest-path src-tauri/Cargo.toml`、`npm run check` 0/0(本 plan 不动前端,check 应保持绿)。
- **分支**:`feature/aing-p2b-plan-a-graph-queries`,从 master(含 `79f2c06`)开。`git add` 只加明确路径,**禁 `-A`**。提交信息**不加** Co-Authored-By / Generated 尾注。

---

## File Structure

- **Modify** `src-tauri/src/graph/mod.rs`:加 `cooccurrence_edges`、`entity_detail`(+ 结构 `EntityDetail`/`CoEntity`)、`resolve_local_ids` 三函数 + 各自单测。
- **Modify** `src-tauri/src/ipc.rs`:加 `EntitySummary`、`EdgeRow`、`GraphData`、`EntityNoteRef`、`RelatedEntity`、`EntityDetail`、`EntityLink` 七个可序列化结构。
- **Modify** `src-tauri/src/lib.rs`:加 `graph_entities`、`graph_data`、`entity_detail`、`note_entity_links` 四命令 + 注册进 `generate_handler!`。

既有可复用(勿重写):`graph::open`、`graph::norm`、`graph::EntityRow`、`graph::list_entities`、`graph::resolve_global_id`、`graph::upsert_note`(测试用)、`store::load_refined`、`store::VoiceprintStore::new(PathBuf).load()`、`store::NoteStore::new(root).list()`、`store::validate_note_id`、`data_root`/`notes_dir`。

---

## Task 1: `graph::cooccurrence_edges` — 实体共现边

**Files:**
- Modify: `src-tauri/src/graph/mod.rs`(在 `related_notes` 之后加函数;测试加进文件末尾 `mod tests`)

**Interfaces:**
- Consumes: `open(data_root) -> anyhow::Result<Connection>`(已有);测试用 `upsert_note`、`doc_with`、`ent`、`para`(已有 test helper)。
- Produces: `pub(crate) fn cooccurrence_edges(data_root: &Path) -> anyhow::Result<Vec<(String, String, i64)>>` —— 每条 `(entity_a, entity_b, shared_notes)`,`a < b` 去重,`shared_notes` = 两实体共同出现的笔记数,按共享数降序。

- [ ] **Step 1: 写失败测试**（加到 `graph/mod.rs` 的 `mod tests` 内）

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph::tests::cooccurrence_edges_dedup_and_weight`
Expected: 编译失败 `cannot find function cooccurrence_edges`。

- [ ] **Step 3: 实现**（加到 `graph/mod.rs`,`related_notes` 函数之后）

```rust
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph::tests::cooccurrence_edges_dedup_and_weight`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/graph/mod.rs
git commit -m "graph: 加 cooccurrence_edges(实体共现边,a<b 去重/边权=共享笔记数)"
```

---

## Task 2: `graph::entity_detail` — 实体详情聚合

**Files:**
- Modify: `src-tauri/src/graph/mod.rs`(结构 + 函数 + 测试)

**Interfaces:**
- Consumes: `open`、`EntityRow`(已有);测试用 `upsert_note`/`doc_with`/`ent`/`para`/`Mention`(已有,`Mention` 已在 `mod tests` 的 `use` 里)。
- Produces:
  - `pub(crate) struct CoEntity { pub id: String, pub kind: String, pub name: String, pub shared_notes: i64 }`
  - `pub(crate) struct EntityDetail { pub row: EntityRow, pub notes: Vec<(String, i64)>, pub related: Vec<CoEntity> }`（`notes` = `(note_id, mention_count)` 按提及降序;`related` = 共现实体按共享笔记数降序）
  - `pub(crate) fn entity_detail(data_root: &Path, gid: &str) -> anyhow::Result<Option<EntityDetail>>`（实体不存在 → `None`）

- [ ] **Step 1: 写失败测试**（加到 `mod tests`）

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph::tests::entity_detail_aggregates_notes_and_related`
Expected: 编译失败 `cannot find function entity_detail` / `cannot find type EntityDetail`。

- [ ] **Step 3: 实现**（结构加在 `EntityRow` 定义之后;函数加在 `related_notes`/`cooccurrence_edges` 之后）

```rust
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph::tests::entity_detail_aggregates_notes_and_related`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/graph/mod.rs
git commit -m "graph: 加 entity_detail(实体聚合 + 出现笔记 + 共现实体)"
```

---

## Task 3: `graph::resolve_local_ids` — 笔记局部实体 → 全局 id

**Files:**
- Modify: `src-tauri/src/graph/mod.rs`(函数 + 测试)

**Interfaces:**
- Consumes: `resolve_global_id`(已有)、`store::load_refined`、`store::VoiceprintStore::new(PathBuf).load()`、`store::validate_note_id`;测试用 `write_note`、`write_vp`、`doc_with`、`ent`、`para`(已有 test helper)。
- Produces: `pub(crate) fn resolve_local_ids(data_root: &Path, note_id: &str) -> anyhow::Result<Vec<(String, String, bool)>>` —— 每项 `(local_id ent_N, global_id, is_person)`。无 `aing.json`/无实体 → 空 `Vec`;名为空的实体跳过。

- [ ] **Step 1: 写失败测试**（加到 `mod tests`）

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph::tests::resolve_local_ids_maps_person_and_nonperson`
Expected: 编译失败 `cannot find function resolve_local_ids`。

- [ ] **Step 3: 实现**（加到 `graph/mod.rs`,`resolve_global_id` 之后）

```rust
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
```

- [ ] **Step 4: 跑测试确认通过 + 全 graph 模块回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph::`
Expected: 本模块全部 PASS(既有 + 3 新测)。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/graph/mod.rs
git commit -m "graph: 加 resolve_local_ids(笔记局部实体→全局 person_id/e:名)"
```

---

## Task 4: `ipc` 镜像结构 + 四个 Tauri 命令

命令是薄封装(照 `note_related` 模板),Tauri 命令无法脱离 AppHandle 单测——由 `cargo check` 保证编译、由 Task 1–3 的纯函数测试保证逻辑。故本任务无新增单测。

**Files:**
- Modify: `src-tauri/src/ipc.rs`(七结构,加在 `RelatedNote` 之后)
- Modify: `src-tauri/src/lib.rs`(四命令,加在 `note_related` 之后;注册进 `generate_handler!`,`note_related,` 那行之后)

**Interfaces:**
- Consumes: `graph::{list_entities, cooccurrence_edges, entity_detail, resolve_local_ids, EntityRow, EntityDetail, CoEntity}`;`store::NoteStore`、`data_root`/`notes_dir`。
- Produces(前端 Plan B/C 依赖的命令契约):
  - `graph_entities() -> Vec<ipc::EntitySummary>`
  - `graph_data() -> ipc::GraphData`
  - `entity_detail(id: String) -> Option<ipc::EntityDetail>`
  - `note_entity_links(id: String) -> Vec<ipc::EntityLink>`

- [ ] **Step 1: 加 ipc 结构**（`src-tauri/src/ipc.rs`,`RelatedNote` 之后）

```rust
/// 图谱实体摘要(列表 / 力导图节点)。镜像 graph::EntityRow(后者无 Serialize)。
#[derive(Debug, Clone, Serialize)]
pub struct EntitySummary {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub is_person: bool,
    pub note_count: i64,
    pub mention_total: i64,
}

/// 力导图一条共现边(a<b,weight=共享笔记数)。
#[derive(Debug, Clone, Serialize)]
pub struct EdgeRow {
    pub a: String,
    pub b: String,
    pub weight: i64,
}

/// 力导图数据:节点(全部实体)+ 边(共现)。
#[derive(Debug, Clone, Serialize)]
pub struct GraphData {
    pub nodes: Vec<EntitySummary>,
    pub edges: Vec<EdgeRow>,
}

/// 实体详情面板里「出现的笔记」一项(联查了标题)。
#[derive(Debug, Clone, Serialize)]
pub struct EntityNoteRef {
    pub id: String,
    pub title: String,
    pub started_at: String,
    pub mention_count: i64,
}

/// 实体详情面板里「相关(共现)实体」一项。
#[derive(Debug, Clone, Serialize)]
pub struct RelatedEntity {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub shared_notes: i64,
}

/// 实体详情(右侧面板)。
#[derive(Debug, Clone, Serialize)]
pub struct EntityDetail {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub is_person: bool,
    pub note_count: i64,
    pub mention_total: i64,
    pub notes: Vec<EntityNoteRef>,
    pub related: Vec<RelatedEntity>,
}

/// 笔记页高亮点击导航:局部实体 id → 全局 id(+是否人实体)。
#[derive(Debug, Clone, Serialize)]
pub struct EntityLink {
    pub local_id: String,
    pub global_id: String,
    pub is_person: bool,
}
```

- [ ] **Step 2: 加四个命令**（`src-tauri/src/lib.rs`,`note_related` 函数之后）

```rust
/// 图谱全部实体(列表视图),按出现笔记数降序。图谱失败/空 → 空列表,不 Err。
#[tauri::command]
fn graph_entities(app: AppHandle) -> Result<Vec<ipc::EntitySummary>, String> {
    let Ok(root) = data_root(&app) else { return Ok(vec![]) };
    let rows = match graph::list_entities(&root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("graph_entities: 查询失败,返回空: {e}");
            return Ok(vec![]);
        }
    };
    Ok(rows
        .into_iter()
        .map(|r| ipc::EntitySummary {
            id: r.id,
            kind: r.kind,
            name: r.name,
            aliases: r.aliases,
            is_person: r.is_person,
            note_count: r.note_count,
            mention_total: r.mention_total,
        })
        .collect())
}

/// 力导图数据:节点(全部实体)+ 共现边。任一子查询失败 → 该部分空,整体不 Err。
#[tauri::command]
fn graph_data(app: AppHandle) -> Result<ipc::GraphData, String> {
    let Ok(root) = data_root(&app) else { return Ok(ipc::GraphData { nodes: vec![], edges: vec![] }) };
    let nodes = graph::list_entities(&root)
        .unwrap_or_else(|e| {
            eprintln!("graph_data: 实体查询失败,返回空: {e}");
            vec![]
        })
        .into_iter()
        .map(|r| ipc::EntitySummary {
            id: r.id,
            kind: r.kind,
            name: r.name,
            aliases: r.aliases,
            is_person: r.is_person,
            note_count: r.note_count,
            mention_total: r.mention_total,
        })
        .collect();
    let edges = graph::cooccurrence_edges(&root)
        .unwrap_or_else(|e| {
            eprintln!("graph_data: 共现边查询失败,返回空: {e}");
            vec![]
        })
        .into_iter()
        .map(|(a, b, weight)| ipc::EdgeRow { a, b, weight })
        .collect();
    Ok(ipc::GraphData { nodes, edges })
}

/// 单个实体详情(右侧面板)。实体不存在/图谱失败 → None,不 Err。
#[tauri::command]
fn entity_detail(app: AppHandle, id: String) -> Result<Option<ipc::EntityDetail>, String> {
    let Ok(root) = data_root(&app) else { return Ok(None) };
    let detail = match graph::entity_detail(&root, &id) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("entity_detail: 查询失败,返回 None: {e}");
            return Ok(None);
        }
    };
    let Some(d) = detail else { return Ok(None) };
    // 联查笔记标题(NoteStore.list);查不到标题的笔记跳过
    let by_id: std::collections::HashMap<String, store::NoteSummary> = match notes_dir(&app) {
        Ok(nr) => store::NoteStore::new(nr).list().into_iter().map(|n| (n.id.clone(), n)).collect(),
        Err(_) => std::collections::HashMap::new(),
    };
    let notes = d
        .notes
        .into_iter()
        .filter_map(|(nid, cnt)| {
            by_id.get(&nid).map(|n| ipc::EntityNoteRef {
                id: n.id.clone(),
                title: n.title.clone(),
                started_at: n.started_at.clone(),
                mention_count: cnt,
            })
        })
        .collect();
    let related = d
        .related
        .into_iter()
        .map(|r| ipc::RelatedEntity { id: r.id, kind: r.kind, name: r.name, shared_notes: r.shared_notes })
        .collect();
    Ok(Some(ipc::EntityDetail {
        id: d.row.id,
        kind: d.row.kind,
        name: d.row.name,
        aliases: d.row.aliases,
        is_person: d.row.is_person,
        note_count: d.row.note_count,
        mention_total: d.row.mention_total,
        notes,
        related,
    }))
}

/// 笔记页高亮点击导航:该笔记局部实体 → 全局 id(+是否人)。失败/无实体 → 空。
#[tauri::command]
fn note_entity_links(app: AppHandle, id: String) -> Result<Vec<ipc::EntityLink>, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let Ok(root) = data_root(&app) else { return Ok(vec![]) };
    match graph::resolve_local_ids(&root, &id) {
        Ok(v) => Ok(v
            .into_iter()
            .map(|(local_id, global_id, is_person)| ipc::EntityLink { local_id, global_id, is_person })
            .collect()),
        Err(e) => {
            eprintln!("note_entity_links: 解析失败,返回空: {e}");
            Ok(vec![])
        }
    }
}
```

- [ ] **Step 3: 注册命令**（`src-tauri/src/lib.rs` 的 `generate_handler![` 里,`note_related,` 那行之后加四行）

```rust
            note_related,
            graph_entities,
            graph_data,
            entity_detail,
            note_entity_links,
```

- [ ] **Step 4: 编译 + 全库测试 + 前端 check**

Run:
```bash
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml --lib
npm run check
```
Expected: `cargo check` 无 error;`cargo test --lib` 全绿(既有 502 + 本 plan 3 新测,应 505);`npm run check` 0/0(未动前端)。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/ipc.rs src-tauri/src/lib.rs
git commit -m "ipc+命令: graph_entities/graph_data/entity_detail/note_entity_links(图谱查询接线,失败降级空)"
```

---

## Self-Review(对照 spec)

- **spec §后端** 三 graph 查询(cooccurrence_edges / entity_detail / resolve_local_ids)→ Task 1/2/3 ✓;四命令 + ipc 镜像 → Task 4 ✓。
- **红线**(失败返回空不 Err)→ 四命令均 `unwrap_or_else`/`match…return Ok(空)` ✓。
- **顺手修 read_dir 顺序依赖**:`graph_entities`/`graph_data` 走 `list_entities` 的 `ORDER BY note_count DESC, name ASC` 稳定序 ✓(SQL 排序,不依赖 read_dir)。
- **无新依赖** ✓(d3-force 属 Plan C);**无 UI** ✓(不动前端,`npm run check` 应保持绿)。
- 类型一致性:`graph::EntityDetail.row/notes/related`(Task 2)与 `entity_detail` 命令(Task 4)消费一致;`ipc::EntitySummary` 字段与 `EntityRow` 逐字对应 ✓。
- 命令名 `graph_entities/graph_data/entity_detail/note_entity_links` 在 Task 4 定义并在同任务注册,无跨任务漂移 ✓。
- **无 placeholder**:每步含完整代码/命令/期望输出 ✓。
