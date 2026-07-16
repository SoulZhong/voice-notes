# Aing Phase 1 · Plan 2 — 数据模型定型 + refined.json→aing.json 迁移 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development 逐任务实现。步骤用 `- [ ]` 勾选。

**Goal:** 把每笔记的修订稿产物一次性升级为 **`aing.json`**——在现有 `refined.json` 格式之上,增量加上「实体清单 + 每段实体提及区间 + 实体抽取阶段」三块 Aing 数据字段(一次定型,后续 Plan 3/Phase 3 填充无需再迁移),并把落盘文件从 `refined.json` 改名为 `aing.json`,旧文件一次性迁移。**纯后端、零行为变更、无新依赖**;字段现在全空,Aing 引擎(Plan 3)才填。

**Architecture:** 在 `store/refined.rs` 的 `RefinedDoc`/`RefinedParagraph`/`RefineStages` 上**增量加字段**(全部 `#[serde(default)]`,向前兼容:旧 `refined.json` 缺这些键照常加载,新键取默认空值)。落盘文件名引入共享常量 `AING_DOC_FILE = "aing.json"`;`load_refined` 改为「优先 aing.json,缺失时从旧 refined.json 一次性迁移(读旧→写 aing.json,旧文件保留供回滚)」。所有判断「是否有修订稿」的点走迁移感知的 `aing_exists`。JSON 键名全部不变(仅新增键),故**前端零改动**。

**Tech Stack:** Rust(serde/serde_json,现有栈);无新 crate 依赖(rusqlite 属 Plan 4)。

## Global Constraints

- **零行为变更**:新字段现在恒为空(`entities=[]`、`mentions=[]`、`stages.entities="off"`);本 plan 不产出任何实体、不改修订/重聚类/LLM 逻辑。
- **JSON 键名保持不变,只增不改**:`schema_version`/`generated_at`/`llm_model`/`stages`/`filter`/`recluster`/`llm`/`discarded_seqs`/`paragraphs`/`speaker`/`name`/`person_id`/`start_ms`/`end_ms`/`text`/`source_seqs` 一律原样;新增键 `entities`(doc 级)、`mentions`(段级)、`stages.entities`。**不得**给旧字段加 `serde(rename)` 或改名。前端读的 `stages.llm`、`paragraphs` 等键因此不受影响。
- **外部字符串契约全程不动**(它们跨进程/用户边界,cosmetic 改名延后到独立 plan):hook 事件 key `refine_started`/`refine_finished`;Tauri 命令名 `refine_note`/`get_refined`;Tauri 事件名 `"refine"`;MCP 工具名 `apply_refined_texts`。**内部 Rust 标识符**(`RefinedDoc`/`RefinedParagraph`/`RefineStages`/`spawn_refine`/`RefineState`/`refine` 模块)本 plan **也保持不变**——内部改名是纯 cosmetic、高噪声、与上述字符串契约纠缠,单列一个机械 plan 另做。
- **旧 `refined.json` 迁移后保留在盘上**(不删),供回滚;迁移是幂等的(aing.json 已存在即不再迁移)。
- 迁移/加载失败沿用现有语义:`load_refined` 返回 `Option`,损坏/缺失 → `None`(UI 回落原始逐字稿)。
- 每个新增字段必须 `#[serde(default)]`(或带默认函数),保证旧文件与未来加字段都不需要再迁移。
- git 提交信息**不加** `Co-Authored-By` 或任何 Claude/Generated 署名。
- 验证:`cargo test --manifest-path src-tauri/Cargo.toml --lib` 全绿;`cargo check` 无 error;`npm run check` 0/0(前端不该受影响,作回归确认)。

## File Structure

- **Modify** `src-tauri/src/store/refined.rs` — 加 `Entity`/`Mention` 类型;给 `RefinedParagraph`+`mentions`、`RefinedDoc`+`entities`、`RefineStages`+`entities`;文件名常量 + 迁移感知的 `load_refined`/`write_refined_atomic`/`aing_exists`;新增单测。
- **Modify** `src-tauri/src/store/mod.rs` — 重导出新类型 `Entity`/`Mention` 与新函数 `aing_exists`、常量。
- **Modify** `src-tauri/src/refine/mod.rs` — 构造 `RefineStages { .. }` 的字面量补 `entities` 字段(编译器会逐处报错定位)。
- **Modify** `src-tauri/src/refine/agent.rs` — 同上补 `RefineStages` 字面量;基线文件路径 `refined.json` → 走 `AING_DOC_FILE`。
- **Modify** `src-tauri/src/mcp/tools.rs` — `dir.join("refined.json").exists()` → `store::aing_exists(&dir)`(迁移感知)。
- 其余含 `RefineStages { .. }` 字面量的测试文件(`store/refined.rs`、`refine/mod.rs`、`refine/agent.rs` 内 `#[cfg(test)]`)由编译器驱动补字段。

---

### Task 1: aing.json 数据模型定型(增量加实体/提及/阶段字段)

**Files:**
- Modify: `src-tauri/src/store/refined.rs`(类型定义 `refined.rs:8-57`;`REFINED_SCHEMA_VERSION` 常量在 `refined.rs:5`;测试模块 `refined.rs:162+`)
- Modify: `src-tauri/src/store/mod.rs`(重导出 `refined.rs:5-15` 一带)
- Modify: `src-tauri/src/refine/mod.rs`(`RefineStages { .. }` 字面量 `mod.rs:254-258`)
- Modify: `src-tauri/src/refine/agent.rs`(测试内 `RefineStages` 字面量,若有)

**Interfaces:**
- Produces:
  - `pub struct Mention { pub entity: String, pub start: usize, pub end: usize }`(段级提及区间,`start`/`end` 是段落 `text` 的 char 下标半开区间)
  - `pub struct Entity { pub id: String, pub kind: String, pub name: String, pub aliases: Vec<String> }`(本篇实体;`id` 人实体复用 `person_id`,非人实体为 `ent_id`;`kind` 用字符串免枚举迁移)
  - `RefinedParagraph.mentions: Vec<Mention>`(新增,`#[serde(default)]`)
  - `RefinedDoc.entities: Vec<Entity>`(新增,`#[serde(default)]`)
  - `RefineStages.entities: String`(新增,`#[serde(default = "stage_off")]`,值域同 `llm`:`off/running/done/partial/failed`)
- Consumes: 无(首任务)。

- [ ] **Step 1: 写失败测试——新字段 roundtrip + 旧文件向前兼容**

在 `src-tauri/src/store/refined.rs` 的 `#[cfg(test)] mod tests` 内追加(放到现有 `roundtrip_and_corrupt_returns_none` 附近):

```rust
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
            mentions: vec![Mention { entity: "ent_1".into(), start: 0, end: 4 }],
        }],
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
    std::fs::write(dir.path().join("aing.json"), old).unwrap();
    let doc = load_refined(dir.path()).expect("旧结构应能加载");
    assert!(doc.entities.is_empty());
    assert!(doc.paragraphs[0].mentions.is_empty());
    assert_eq!(doc.stages.entities, "off", "缺 stages.entities 键默认 off");
}
```

（注:`Mention`/`Entity` 需 `#[derive(PartialEq)]` 才能 `assert_eq!`；`tempfile` 已是本 crate dev-dependency——现有测试 `refined.rs` 已用 `tempfile::tempdir()`。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib store::refined::tests::aing_fields_roundtrip 2>&1 | tail -20`
Expected: 编译失败——`RefineStages` 无 `entities` 字段、`RefinedParagraph` 无 `mentions`、`RefinedDoc` 无 `entities`、`Mention`/`Entity` 未定义。

- [ ] **Step 3: 加类型与字段**

在 `src-tauri/src/store/refined.rs` 顶部(`RefinedParagraph` 定义之前)加默认函数与两个新类型:

```rust
fn stage_off() -> String {
    "off".into()
}

/// 实体在段落正文中的一次提及(笔记页高亮 + 图谱建边用)。`start`/`end` 是本段
/// `text` 的字符(char)下标,半开区间 [start, end);`entity` 引用本篇
/// `RefinedDoc.entities[].id`。Plan 3 由大模型产出,本 plan 恒为空。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mention {
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
```

给 `RefinedParagraph` 追加字段(在 `source_seqs` 之后):

```rust
    /// 本段实体提及区间(Plan 3 填,本 plan 恒空)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Mention>,
```

给 `RefineStages` 追加字段(在 `llm` 之后):

```rust
    /// 实体抽取阶段:off/running/done/partial/failed(Plan 3 用,本 plan 恒 off)。
    #[serde(default = "stage_off")]
    pub entities: String,
```

给 `RefinedDoc` 追加字段(在 `paragraphs` 之前或之后皆可,建议 `paragraphs` 之前):

```rust
    /// 本篇实体清单(Plan 3 填,本 plan 恒空)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<Entity>,
```

- [ ] **Step 4: 编译器驱动补齐所有 `RefineStages { .. }` 字面量**

`RefineStages` 现有 4 字段后,每处硬构造都缺 `entities` 会编译报错。逐处补 `entities: "off".into(),`:
- `src-tauri/src/refine/mod.rs:254-258`(`run_local` 里,生产路径)——`entities: "off".into()`。
- `src-tauri/src/refine/mod.rs`、`src-tauri/src/store/refined.rs`、`src-tauri/src/refine/agent.rs` 内 `#[cfg(test)]` 的 `RefineStages { .. }` 字面量——同样补 `entities: "off".into()`。

用 `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | grep -n "missing field \`entities\`"` 定位每一处,改到编译通过。**不改任何 `stages.entities` 的运行时赋值逻辑**(保持 off)。

- [ ] **Step 5: 重导出新类型(mod.rs)**

在 `src-tauri/src/store/mod.rs` 现有 `pub use refined::{... RefineStages, RefinedDoc, RefinedParagraph};` 一行里追加 `Entity, Mention`:

```rust
pub use refined::{load_refined, write_refined_atomic, Entity, Mention, RefineStages, RefinedDoc, RefinedParagraph};
```

- [ ] **Step 6: 跑测试确认通过 + 全量回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib store::refined 2>&1 | tail -8`
Expected: 新增两测过 + 原 9 测过。
Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib 2>&1 | tail -3`
Expected: 全量 `test result: ok`,数量 = 旧基线 + 2。
Run: `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | tail -2`
Expected: 无 error。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/store/refined.rs src-tauri/src/store/mod.rs src-tauri/src/refine/mod.rs src-tauri/src/refine/agent.rs
git commit -m "aing.json 数据模型定型:RefinedDoc 增 entities、RefinedParagraph 增 mentions、RefineStages 增 entities 阶段(全 serde default,旧文件向前兼容);新增 Entity/Mention 类型;字段现恒空,Plan 3 填充"
```

---

### Task 2: 落盘文件 refined.json → aing.json + 旧文件一次性迁移

**Files:**
- Modify: `src-tauri/src/store/refined.rs`(`write_refined_atomic`/`load_refined` `refined.rs:42-52`;新增常量与 `aing_exists`;测试)
- Modify: `src-tauri/src/store/mod.rs`(重导出 `aing_exists` 与常量)
- Modify: `src-tauri/src/mcp/tools.rs`(`refined.json` 存在性检查 `tools.rs:56`、`tools.rs:169`)
- Modify: `src-tauri/src/refine/agent.rs`(基线文件路径 `refined.json` `agent.rs:647` 一带)

**Interfaces:**
- Consumes: Task 1 的 `RefinedDoc`(含新字段)。
- Produces:
  - `pub const AING_DOC_FILE: &str = "aing.json"`;`pub const LEGACY_REFINED_FILE: &str = "refined.json"`
  - `pub fn aing_exists(note_dir: &Path) -> bool`(aing.json 或旧 refined.json 存在即真;迁移感知)
  - `load_refined` 语义升级:优先 aing.json;缺失且旧 refined.json 在 → 迁移(读旧→写 aing.json,返回 doc);两者皆无或损坏 → `None`。
  - `write_refined_atomic` 落盘目标改为 `aing.json`(临时件 `aing.json.tmp`)。

- [ ] **Step 1: 写失败测试——迁移 + 优先级 + 存在性**

在 `store/refined.rs` 测试模块追加:

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib store::refined::tests::legacy_refined_json_migrates 2>&1 | tail -20`
Expected: 编译失败(`aing_exists` 未定义)或断言失败(当前 `write_refined_atomic` 仍写 refined.json、`load_refined` 不迁移)。

- [ ] **Step 3: 改常量 + 迁移感知的读写 + aing_exists**

在 `src-tauri/src/store/refined.rs` 顶部常量区(`REFINED_SCHEMA_VERSION` 附近)加:

```rust
/// 每笔记修订稿产物文件名(人读真值)。
pub const AING_DOC_FILE: &str = "aing.json";
/// 旧文件名:一次性迁移到 `AING_DOC_FILE`,迁移后保留供回滚。
pub const LEGACY_REFINED_FILE: &str = "refined.json";
```

把 `write_refined_atomic`/`load_refined` 改为(替换 `refined.rs:42-52` 两函数):

```rust
pub fn write_refined_atomic(note_dir: &Path, doc: &RefinedDoc) -> anyhow::Result<()> {
    let tmp = note_dir.join("aing.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(doc)?)?;
    std::fs::rename(&tmp, note_dir.join(AING_DOC_FILE))?;
    Ok(())
}

/// 读修订稿:优先 `aing.json`;缺失时从旧 `refined.json` 一次性迁移(读旧格式→写
/// aing.json,旧文件保留供回滚)。两者皆无或损坏 → None(UI 回落原始逐字稿)。
pub fn load_refined(note_dir: &Path) -> Option<RefinedDoc> {
    if let Ok(bytes) = std::fs::read(note_dir.join(AING_DOC_FILE)) {
        return serde_json::from_slice(&bytes).ok();
    }
    let bytes = std::fs::read(note_dir.join(LEGACY_REFINED_FILE)).ok()?;
    let doc: RefinedDoc = serde_json::from_slice(&bytes).ok()?;
    // 迁移落盘;失败不致命(下次加载再试),旧文件不删
    let _ = write_refined_atomic(note_dir, &doc);
    Some(doc)
}

/// aing.json 或旧 refined.json 是否存在(供「是否有修订稿」判断,迁移感知)。
pub fn aing_exists(note_dir: &Path) -> bool {
    note_dir.join(AING_DOC_FILE).exists() || note_dir.join(LEGACY_REFINED_FILE).exists()
}
```

- [ ] **Step 4: 跑迁移测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib store::refined 2>&1 | tail -8`
Expected: 新增 3 测过 + Task 1 两测过 + 原测过。

- [ ] **Step 5: 重导出 + 更新所有旧文件名字面量的读点**

在 `src-tauri/src/store/mod.rs` 追加重导出:

```rust
pub use refined::{aing_exists, AING_DOC_FILE, LEGACY_REFINED_FILE};
```

`src-tauri/src/mcp/tools.rs`——把两处存在性检查(`tools.rs:56` 的 `let has_refined = dir.join("refined.json").exists();`、`tools.rs:169` 附近同样的 `dir.join("refined.json").exists()`)改为迁移感知:

```rust
let has_refined = store::aing_exists(&dir);
```
（`tools.rs:169` 那处若在断言/守卫上下文,同改为 `store::aing_exists(&dir)`。）

`src-tauri/src/refine/agent.rs`——Agent 读的「基线修订稿」路径(`agent.rs:647` 一带,当前 `note_dir.join("refined.json")` 或类似)改为 `note_dir.join(crate::store::AING_DOC_FILE)`。因为管线在 Agent 跑之前已 `run_local` 写出 aing.json(见 `lib.rs::spawn_refine` 流程),基线恒为 aing.json;但若该处也可能面对未迁移的旧笔记,改用 `crate::store::load_refined(note_dir).is_some()` 做「有无基线」判断更稳妥(迁移感知)。**保留** `agent.rs` 里 `#[cfg(test)]` 用 shell heredoc 造 `refined.json` 的测试 fixture——它验证的是历史路径,可一并改造成 `aing.json`,或保留旧名验证迁移;二选一但要让该文件测试仍绿(编译器/测试驱动)。

- [ ] **Step 6: 全量回归 + 前端零回归确认**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib 2>&1 | tail -3`
Expected: 全量 `ok`(= 基线 + Task1 2 + Task2 3)。
Run: `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | tail -2`
Expected: 无 error。
Run: `grep -rn '"refined.json"' src-tauri/src | grep -v '#\[cfg(test)\]' | grep -v LEGACY_REFINED_FILE`
Expected: 仅剩常量定义处 `LEGACY_REFINED_FILE` 与(若保留的)测试 fixture;无生产读写点再裸用字面量。
Run: `npm run check 2>&1 | tail -2`
Expected: 0/0(前端不读文件、JSON 键未变,应无影响)。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/store/refined.rs src-tauri/src/store/mod.rs src-tauri/src/mcp/tools.rs src-tauri/src/refine/agent.rs
git commit -m "落盘文件 refined.json→aing.json + 旧文件一次性迁移:AING_DOC_FILE 常量、load_refined 迁移感知(读旧→写 aing.json,旧文件保留回滚)、aing_exists;mcp/tools 与 agent 基线走迁移感知路径;JSON 键未变前端零改动"
```

---

## Self-Review

- **Spec 覆盖**:本 plan 覆盖架构 spec Phase 1 的「aing.json 一次定型(修订段落 + 提及区间 + 本篇实体)」与「refined.json→aing.json 迁移」。**明确不做**(归后续 plan,spec 允许拆):HTTP 结构化批函数与实体抽取(Plan 3)、SQLite 全局图谱与实体解析(Plan 4)、内部 Rust 标识符改名(独立 cosmetic plan)、`stages.llm`→`stages.aing` 的 JSON 键改名(会破前端,判定不做)。
- **定型不返工论证**:所有新字段 `#[serde(default)]`,Plan 3 填 `entities`/`mentions`/`stages.entities`、Phase 3 加更多字段都靠 serde default 兼容,无需再迁移。`kind` 用 String 免枚举迁移。
- **占位符**:类型/迁移/测试均给完整 Rust 代码;`RefineStages` 字面量补齐由编译器 `missing field` 逐处驱动,非 TBD。
- **类型一致性**:`Mention`/`Entity` 定义(Task 1)= 测试与后续 Task 引用一致;`RefinedDoc.entities: Vec<Entity>`、`RefinedParagraph.mentions: Vec<Mention>`、`RefineStages.entities: String` 三处签名全程一致;`aing_exists`/`AING_DOC_FILE`/`LEGACY_REFINED_FILE`(Task 2 定义)= mod.rs 重导出与 mcp/agent 引用一致。
- **契约不变式**:hook keys、Tauri 命令/事件名、MCP 工具名、内部 Rust 标识符、所有旧 JSON 键——全程未改;仅新增 3 个 JSON 键 + 1 个文件名 + 迁移。
