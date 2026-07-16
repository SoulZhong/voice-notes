# Aing Phase 1 · Plan 4 — SQLite 全局知识图谱 + 实体解析 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development 逐任务实现。步骤用 `- [ ]` 勾选。

**Goal:** 建一个**从所有 `aing.json` 派生、可整库重建**的 SQLite 知识图谱:全局实体注册表 + 笔记↔实体边;把每篇笔记 Plan 3 抽出的实体解析到全局(非人按规范名跨笔记去重、人按精确名匹配复用声纹库 `person_id`),支撑"实体浏览 / 某实体出现在哪些笔记 / 相关笔记(共享实体)"查询。**纯增值产物**:图谱任何失败只 eprintln 不影响 Aing / 本地稿;`graph.sqlite` 丢了可整库重建、不丢数据。

**Architecture:** 新 `src-tauri/src/graph/` 模块。SQLite 两表 `entities`(全局实体)+ `note_entities`(笔记↔实体边,带 mention_count);实体↔实体共现、相关笔记都是**查询**不建表(真派生)。`rusqlite`(bundled)短连接 `Connection::open(<data_root>/graph.sqlite)` + `static GRAPH_LOCK: Mutex<()>` 串行写(Connection 非 Sync,沿用全仓"每次操作现开文件 + 进程内互斥"惯例)。解析层:非人实体全局 id = `e:<规范名>`(确定性→跨笔记去重 + 重建幂等);人实体按**精确规范名**匹配声纹库 person→复用 `person_id`,匹配不上退化为图内局部实体(不强行模糊合并)。`upsert_note` 整篇替换该笔记的边(幂等);`rebuild_all` 清表后遍历所有 `aing.json` 重灌。挂在 `spawn_refine` 尾部 `stages.llm=="done"` 后 upsert;app 启动后台 `rebuild_all` 兜底存量笔记。

**Tech Stack:** Rust + `rusqlite = { version = "0.32", features = ["bundled"] }`(bundled 静态编译 libsqlite3,避免依赖系统库,与全仓 bundled 原生依赖风格一致)。

## Global Constraints

- **核心红线**:图谱是纯增值产物。`graph::upsert_note`/`rebuild_all`/查询的任何失败(SQLite 锁死、文件损坏、解析失败)**只 eprintln 记录、绝不 `?` 向上传播打断 Aing 主流程或本地成稿**;`spawn_refine` 里的图谱调用必须 `if let Err(e) = ... { eprintln!(...) }` 风格(与 refined.json 落盘失败同款姿态)。`graph.sqlite` 可从所有 `aing.json` 整库重建,损坏即重建、不丢数据。
- **派生/幂等**:`upsert_note` 重跑同一篇不产生重复边/节点(先 `DELETE FROM note_entities WHERE note_id=?` 再插;实体 `ON CONFLICT(id) DO UPDATE`)。`rebuild_all` 清表后从 `read_dir(notes 根)`+`store::load_refined` 逐篇重灌,跑两遍结果一致。
- **实体解析规则**:①非人实体全局 id = `format!("e:{}", 规范名)`(规范名 = `name.trim().to_lowercase()`),跨笔记同规范名归一为一个全局实体;②人实体(`kind=="person"`):`Entity.name`(或其 alias)的规范名**精确等于**某声纹库 person 的 `Person.name` 规范名(且该 person 经 `VoiceprintStore::resolve` 归一后是自身、非 loser、name 非空)时,全局 id = 该 `person_id`、`is_person=1`;匹配不上则按非人同规则用 `e:<规范名>`、`is_person=0`(不强行模糊匹配错误 person_id——错并比不并更难纠)。别名跨笔记做并集存储,但**不**用别名做跨笔记 id 归一(疑似重复合并是后续带用户确认的功能,本 plan 不做)。
- **已知范围(显式声明,勿假设对称)**:当前 Agent Aing 路径不产出 entities(`apply_refined_texts` 只写 text/stages.llm),故走 Agent 的笔记 `entities` 恒空→不产生图节点;本 plan 只覆盖 HTTP LLM 路径产出实体的笔记。扩展 Agent 侧抽实体不在本 plan。
- **不改稳定契约**:hook key、Tauri 命令/事件名、MCP 工具名、`aing.json` 结构、`stages.*`、`Person`/`Voiceprints` 结构、`refine`/`store` 既有 API —— 全不动。图谱只**读** `aing.json` 与 `voiceprints.json`,新增 `graph.sqlite`。本 plan **不加 Tauri 命令 / 不动前端**(图谱查询命令与图谱页 UI 归 Phase 2);查询函数是 `pub(crate)` Rust,供测试与 Phase 2 消费。
- 复用 `store::load_refined`(迁移感知)读每篇、`store::VoiceprintStore::new(data_root).load()` + `store::VoiceprintStore::resolve` 读人物、`store::validate_note_id` 防穿越;不自己重复解析 JSON。
- `Cargo.lock` 随 `Cargo.toml` 一并提交(加依赖后 `cargo check` 刷新 lock)。
- git 提交**不加** `Co-Authored-By`/Claude/Generated 署名。
- 验证:`cargo test --lib` 全绿;`cargo check` 无 error;`npm run check` 0/0(前端不改,回归确认)。

## File Structure

- **Create** `src-tauri/src/graph/mod.rs` — 连接/建表(`open`)、`GRAPH_LOCK`、解析(`resolve_global_id`)、`upsert_note`、`rebuild_all`、查询(`list_entities`/`entity_notes`/`related_notes`)、类型 `EntityRow`;`#[cfg(test)]` 测试。
- **Modify** `src-tauri/Cargo.toml` — 加 `rusqlite`(bundled);`Cargo.lock` 随之更新。
- **Modify** `src-tauri/src/lib.rs` — 顶部 `mod graph;`(紧邻 `mod refine;`);`spawn_refine` 尾部接 `graph::upsert_note`;app setup 后台 `graph::rebuild_all` 兜底。
- **Modify** `src-tauri/src/store/mod.rs` — 若 graph 需 `validate_note_id` 等 `pub(crate)`,确认可见(已是 `pub(crate)`,graph 是 crate 内模块可直接 `crate::store::validate_note_id`,无需改 re-export)。

---

### Task 1: rusqlite 依赖 + graph 模块骨架(连接 + 建表)

**Files:**
- Modify: `src-tauri/Cargo.toml`(`[dependencies]` `Cargo.toml:20-51`)
- Create: `src-tauri/src/graph/mod.rs`
- Modify: `src-tauri/src/lib.rs`(模块声明块 `lib.rs:1-21`)

**Interfaces:**
- Produces:
  - `pub(crate) fn open(data_root: &Path) -> anyhow::Result<rusqlite::Connection>`（打开 `<data_root>/graph.sqlite`,设 `PRAGMA journal_mode=WAL`、`busy_timeout=3000`,`CREATE TABLE IF NOT EXISTS` 两表 + 索引;幂等)
  - `static GRAPH_LOCK: std::sync::Mutex<()>`（写路径进程内串行）
- Consumes: 无。

- [ ] **Step 1: 加依赖**

在 `src-tauri/Cargo.toml` `[dependencies]`(`realfft = "3"` 一带之后、`[dev-dependencies]` 之前)加:

```toml
# Aing 知识图谱:全局实体注册表 + 笔记↔实体边,从所有 aing.json 派生可整库重建。
# bundled 静态编译 libsqlite3,不依赖系统库(与本仓其余原生依赖 bundled 风格一致)。
rusqlite = { version = "0.32", features = ["bundled"] }
```

Run: `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | tail -3`
Expected: 编译通过(rusqlite 及 libsqlite3-sys 下载编译),`Cargo.lock` 更新。**首次编译 bundled sqlite 较慢,耐心等**。

- [ ] **Step 2: 注册模块**

`src-tauri/src/lib.rs` 模块声明块(`lib.rs:17` `mod refine;` 之后)加:

```rust
mod graph;
```

- [ ] **Step 3: 写失败测试——open 建表且幂等**

新建 `src-tauri/src/graph/mod.rs`,先放测试:

```rust
//! Aing 全局知识图谱:从所有 aing.json 派生的 SQLite 索引(实体注册表 + 笔记↔实体边)。
//! 纯增值产物——任何失败只降级,绝不打断 Aing;丢了可从 aing.json 整库重建。

use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;

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
}
```

- [ ] **Step 4: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph::tests::open_creates_tables 2>&1 | tail -20`
Expected: 编译失败(`open` 未定义)。

- [ ] **Step 5: 实现 open + schema**

在 `graph/mod.rs`(测试模块之前)加:

```rust
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
```

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph 2>&1 | tail -6`
Expected: 过。
Run: `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | tail -2` → 无 error(会有 upsert/rebuild/查询未实现前的 dead_code 警告——本任务尚未加它们,故暂无;`open`/`GRAPH_LOCK` 已被测试消费)。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/graph/mod.rs src-tauri/src/lib.rs
git commit -m "Aing 知识图谱骨架:rusqlite(bundled)依赖 + graph 模块 + SQLite schema(entities/note_entities)+ open(WAL/建表幂等)+ GRAPH_LOCK;mod graph 注册"
```

---

### Task 2: 实体解析 + upsert_note + rebuild_all

**Files:**
- Modify: `src-tauri/src/graph/mod.rs`(解析 + 写入 + 重建 + 测试)
- Modify: `src-tauri/src/store/mod.rs`(**新增一行 re-export** `pub use voiceprints::Voiceprints;`——`VoiceprintStore` 已导出但结构体 `Voiceprints` 未导出,`resolve_global_id` 签名要命名它;`VoiceprintStore::resolve` 是 `impl VoiceprintStore` 的关联函数[已核 lib.rs:1445 同款调用],`validate_note_id` 是 `pub(crate)`,均无需再改可见性)

**Interfaces:**
- Consumes: Task 1 的 `open`/`GRAPH_LOCK`;`store::{RefinedDoc, Entity, load_refined, write_refined_atomic, VoiceprintStore, Voiceprints}`、关联函数 `store::VoiceprintStore::resolve(&vp, id)`、`store::validate_note_id`。
- Produces:
  - `pub(crate) fn resolve_global_id(vp: &store::Voiceprints, e: &store::Entity) -> (String, bool)`（→ (全局 id, is_person);人精确名匹配复用 person_id,否则 `e:<规范名>`）
  - `pub(crate) fn upsert_note(data_root: &Path, note_id: &str, doc: &store::RefinedDoc) -> anyhow::Result<()>`（整篇替换该笔记的边 + upsert 实体;调用方负责"失败只 eprintln")
  - `pub(crate) fn rebuild_all(data_root: &Path) -> anyhow::Result<usize>`（清表后遍历所有 aing.json 重灌,返回入图笔记数）

- [ ] **Step 1: 写失败测试——解析/去重/person 匹配/幂等重建**

在 `graph/mod.rs` 测试模块追加(fixture:tempdir 当 data_root,手工 `notes/<id>/aing.json` + `voiceprints.json`):

```rust
use crate::store::{self, Entity, Mention, RefineStages, RefinedDoc, RefinedParagraph};

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
```

（`list_entities` 由 Task 3 实现——但为让 Task 2 测试可编译可跑,**本任务先实现 `list_entities` 的最小版本**放这里,Task 3 再补 `entity_notes`/`related_notes`。见下 Step 3。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph::tests::upsert 2>&1 | tail -20`
Expected: 编译失败(`resolve_global_id`/`upsert_note`/`rebuild_all`/`list_entities`/`EntityRow` 未定义)。

- [ ] **Step 3: 实现解析 + 写入 + 重建(+ 最小 list_entities)**

先在 `src-tauri/src/store/mod.rs` re-export 区(`pub use voiceprints::VoiceprintStore;` `store/mod.rs:17` 之后)加一行:

```rust
pub use voiceprints::Voiceprints; // graph::resolve_global_id 命名此类型(人实体→person_id 匹配)。
```

再在 `graph/mod.rs` 加:

```rust
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
             ON CONFLICT(note_id, entity_id) DO UPDATE SET mention_count=?3",
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph 2>&1 | tail -8`
Expected: Task 1 + Task 2 全部 graph 测试过(去重/person 匹配/幂等/重建)。
Run: `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | tail -2` → 无 error(`entity_notes`/`related_notes` 未实现前,`open` 等已被消费;无未用告警或仅极少)。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/graph/mod.rs src-tauri/src/store/mod.rs
git commit -m "图谱解析+写入:resolve_global_id(非人 e:<规范名>跨笔记去重、人精确名匹配复用 person_id)、upsert_note(整篇替换边幂等、别名并集、mention 计数)、rebuild_all(清表遍历所有 aing.json 重灌)、list_entities 查询;store 导出 Voiceprints 供 graph 命名"
```

---

### Task 3: 查询补全(entity_notes/related_notes)+ spawn_refine 接线 + 启动重建

**Files:**
- Modify: `src-tauri/src/graph/mod.rs`(两个查询 + 测试)
- Modify: `src-tauri/src/lib.rs`(`spawn_refine` 尾部接线 `lib.rs:386` 一带;app setup 后台 rebuild)

**Interfaces:**
- Consumes: Task 2 的 `upsert_note`/`rebuild_all`/`list_entities`;`lib.rs::data_root`。
- Produces:
  - `pub(crate) fn entity_notes(data_root: &Path, entity_id: &str) -> anyhow::Result<Vec<String>>`（某实体出现在哪些笔记,按 mention_count 降序）
  - `pub(crate) fn related_notes(data_root: &Path, note_id: &str) -> anyhow::Result<Vec<(String, i64)>>`（与该笔记共享实体的其他笔记 + 共享实体数,降序）
  - `spawn_refine` 在 `stages.llm=="done"` 后 upsert 该笔记(失败只 eprintln);app 启动后台 `rebuild_all` 兜底存量。

- [ ] **Step 1: 写失败测试——两个查询**

`graph/mod.rs` 测试追加:

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph::tests::entity_notes graph::tests::related_notes 2>&1 | tail -15`
Expected: 编译失败(两函数未定义)。

- [ ] **Step 3: 实现两个查询**

`graph/mod.rs` 加:

```rust
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
```

- [ ] **Step 4: 跑查询测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib graph 2>&1 | tail -6`
Expected: 全部 graph 测试过。

- [ ] **Step 5: spawn_refine 接线(失败只 eprintln)**

`src-tauri/src/lib.rs`,`spawn_refine` 里 `report("llm", &doc.stages.llm);`(`lib.rs:386` 一带,HTTP/Agent 两分支汇合后)**之后**插入:

```rust
        // 图谱是纯增值产物:仅成功 Aing 的笔记入图,失败只记日志不打断本轮 Aing。
        if doc.stages.llm == "done" {
            match data_root(&app) {
                Ok(root) => {
                    if let Err(e) = graph::upsert_note(&root, &note_id, &doc) {
                        eprintln!("graph: upsert {note_id} 失败(不影响 Aing): {e}");
                    }
                }
                Err(e) => eprintln!("graph: data_root 不可用,跳过入图: {e}"),
            }
        }
```

（注:此处在 `catch_unwind` 闭包内,`app`/`note_id`/`doc` 均在作用域;`upsert_note` 内部若 panic 会被 catch_unwind 吞——但 `upsert_note` 走 `?`+`Result` 不 panic,SQLite 错误以 `Err` 返回被 `if let Err` 接住,符合红线。)

- [ ] **Step 6: app 启动后台兜底重建**

在 `src-tauri/src/lib.rs` 的 tauri `Builder::...setup(|app| { ... })` 闭包里(与其它启动任务并列),加一段后台线程做存量重建(找不到 setup 就近的启动位置亦可,只要在 app 就绪后):

```rust
            // 存量笔记(升级前已有 aing.json)入图:后台整库重建一次,兜底 upsert 只覆盖本次会话新 Aing 的空白。
            let app_for_graph = app.handle().clone();
            std::thread::spawn(move || {
                if let Ok(root) = data_root(&app_for_graph) {
                    match graph::rebuild_all(&root) {
                        Ok(n) => eprintln!("graph: 启动重建完成,{n} 篇入图"),
                        Err(e) => eprintln!("graph: 启动重建失败(不影响运行): {e}"),
                    }
                }
            });
```

（若 `setup` 闭包的 app 参数名/handle 取法与此不同,按该文件现有 setup 里其它 `app.handle()`/`app.state()` 用法适配;关键是拿到一个 `AppHandle` 克隆进后台线程,`data_root(&handle)` 求根后 `rebuild_all`。找不到合适位置就问,不要硬塞。）

- [ ] **Step 7: 全量回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib 2>&1 | tail -3`
Expected: 全量 `ok`(= 基线 494 + 本 plan graph 测试数)。
Run: `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | tail -2` → 无 error;graph 模块无 dead_code(函数经测试 + spawn_refine/setup 消费)。
Run: `npm run check 2>&1 | tail -2` → 0/0。

- [ ] **Step 8: 提交**

```bash
git add src-tauri/src/graph/mod.rs src-tauri/src/lib.rs
git commit -m "图谱查询 entity_notes/related_notes + spawn_refine 尾部 upsert_note 接线(仅 stages.llm==done,失败只 eprintln 不打断 Aing)+ 启动后台 rebuild_all 兜底存量笔记入图"
```

---

## Self-Review
- **Spec 覆盖**:覆盖架构 spec Phase 1 的「SQLite 全局图谱(实体注册表 + 笔记↔实体边)+ 实体解析(人复用 person_id、非人去重)+ 从所有 aing.json 派生可整库重建 + 相关笔记(共享实体)」。**明确不做**(归后续):别名跨笔记疑似重复合并(带用户确认,类比声纹 merge)、entity↔entity 显式关系表(本 plan 用共现查询代替)、图谱 Tauri 命令与图谱页 UI(Phase 2)、Agent 路径抽实体(现状 Agent 笔记无实体→无节点,已声明)、实时增量(Phase 3)。
- **红线可验**:`upsert_note`/`rebuild_all` 走 `Result`+`?` 不 panic;`spawn_refine` 接线 `if let Err(e){eprintln!}` 不 `?`;查询失败也返回 Err 由调用方(Phase 2)决定降级;`graph.sqlite` 可 `rebuild_all` 整库重建。
- **幂等可验**:`upsert_note` 先删本笔记边再插 + 实体 ON CONFLICT;测试 `upsert_is_idempotent`/`rebuild_all` 跑两遍断言一致。
- **占位符**:schema/解析/写入/重建/查询/接线均给完整 SQL + Rust + 测试代码;唯 Step 6 setup 接线位置要求实现者按现有 `setup` 闭包适配(给了取 handle 的模式,找不到就问)——非 TBD。
- **类型一致**:`resolve_global_id(&Voiceprints,&Entity)->(String,bool)`、`upsert_note(&Path,&str,&RefinedDoc)`、`rebuild_all(&Path)->usize`、`EntityRow{id,kind,name,aliases,is_person,note_count,mention_total}`、`list_entities/entity_notes/related_notes` 签名 Task 2/3 一致;消费 `store::{RefinedDoc,Entity,Mention,load_refined,write_refined_atomic,VoiceprintStore,Voiceprints,validate_note_id}` 均为现有 `pub`/`pub(crate)` API(第 3、9 节已核 `VoiceprintStore::resolve`、`validate_note_id` 可见)。
- **契约不变**:不改任何 hook key/命令-事件名/MCP 工具名/aing.json 结构/Person 结构;新增 `graph.sqlite`(派生)与 graph 模块;不加 Tauri 命令、不动前端。
