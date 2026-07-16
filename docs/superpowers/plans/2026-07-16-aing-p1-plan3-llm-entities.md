# Aing Phase 1 · Plan 3 — HTTP 结构化批函数(修订 + 实体抽取)+ 填充 aing.json 实体 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development 逐任务实现。步骤用 `- [ ]` 勾选。

**Goal:** 让**同一次** HTTP 大模型调用在产出修订文本的同时抽取关键实体,把实体(`entities`)与每段提及区间(`mentions`)填进 Plan 2 定型的 `aing.json`;实体维度**独立降级**——任何实体环节失败都不影响修订文本、不影响本地成稿。仅 HTTP 引擎产实体;Agent 引擎保持只修订(实体 off)。

**Architecture:** 扩展 `refine/llm.rs`:`SYSTEM_PROMPT` 增第五件事「抽取实体」,响应 JSON 增 `entities` 字段(`[{name,kind,aliases}]`,**不含字符偏移**);`do_call_chunk`/`call_chunk` 宽松解析并返回原始实体,`polish` 跨块累积并返回 `(LlmOutcome, Vec<RawEntity>)`。新纯函数:`resolve_note_entities`(按规范名去重 + 合并别名 + 分配本篇局部 id `ent_N`)与 `compute_mentions`(在**修订后**段落文本里按 name+alias 子串搜索,产出 char 下标区间,非重叠)。`refine/mod.rs::run_llm` 编排:polish → 解析实体 → 算 mentions → 写 `doc.entities` / `doc.paragraphs[].mentions` / `stages.entities`,实体环节用 `catch`/降级包住,失败只置 `stages.entities="failed"` 不动修订。

**Tech Stack:** Rust(serde_json、ureq,现有栈);无新依赖。**mention 用确定性子串搜索**而非让大模型报偏移(大模型数不准字符)。

## Global Constraints

- **核心红线**:实体抽取/解析/mention 计算的任何失败(响应缺 entities、JSON 坏、解析 panic)**只降级不塌**——修订文本(`stages.llm`)与本地成稿(段落/说话人/时长)永远照常;`doc.entities` 退化为空、`stages.entities="failed"`/`"partial"`,笔记依旧可用。
- **实体仅 HTTP 引擎产**;Agent 引擎(`lib.rs` spawn_refine 的 agent 分支、`refine/agent.rs`)本 plan **不动**,`stages.entities` 保持默认 `"off"`。
- **不改稳定契约**:`stages.llm` 的既有值域与语义、hook key、Tauri 命令/事件名、MCP 工具名、`aing.json` 既有 JSON 键 —— 全不动;只**写入** Plan 2 已定型的 `entities`/`mentions`/`stages.entities` 三键(值域同 llm:`off/running/done/partial/failed`)。
- **mention 是 char 下标半开区间** `[start,end)`(Plan 2 定义),对多字节中文必须按 `chars()` 计数,不能用字节偏移。
- **同一次调用产出修订+实体**(spec:成本约束,不额外发一轮请求)。
- 实体 `kind` 接受任意字符串(prompt 给建议词表,代码不做枚举校验,免未来加类型迁移)。
- git 提交信息**不加** `Co-Authored-By` 或任何 Claude/Generated 署名。
- 验证:`cargo test --manifest-path src-tauri/Cargo.toml --lib` 全绿;`cargo check` 无 error;`npm run check` 0/0(前端本 plan 不改,回归确认)。

## File Structure

- **Modify** `src-tauri/src/refine/llm.rs` — `SYSTEM_PROMPT` 加实体指令;`RawEntity` 类型;`do_call_chunk`/`call_chunk` 解析并返回 entities;`polish` 累积并改返回类型为 `(LlmOutcome, Vec<RawEntity>)`;更新 llm.rs 内既有 polish 测试。
- **Modify** `src-tauri/src/refine/mod.rs` — 新纯函数 `resolve_note_entities`、`compute_mentions`(+ 私有 `find_char_spans`);`run_llm` 编排实体填充与独立降级;新增单测。
- **Modify** `src-tauri/src/refine/agent.rs`(仅测试)— 若 agent 测试断言依赖 polish 旧签名则同步(预期不涉及;agent 不调 polish)。实际大概率无需改。

无前端改动(实体消费是 Phase 2 UI);`aing.json` 的 `entities`/`mentions` 键已在 Plan 2 定型,前端忽略未知字段。

---

### Task 1: 结构化实体抽取(prompt + 解析 + polish 累积)

**Files:**
- Modify: `src-tauri/src/refine/llm.rs`(`SYSTEM_PROMPT` `llm.rs:11`;`do_call_chunk` `llm.rs:163-196`;`call_chunk` `llm.rs:114-160`;`polish` `llm.rs:274-313`;测试模块 `llm.rs:315+`)

**Interfaces:**
- Produces:
  - `pub struct RawEntity { pub name: String, pub kind: String, pub aliases: Vec<String> }`(大模型每块吐的原始实体,未去重、未分配 id)
  - `polish(cfg, paragraphs, log) -> (LlmOutcome, Vec<RawEntity>)`（**返回类型变更**:原 `LlmOutcome` → 元组,第二元是跨块累积的原始实体,失败块不贡献实体）
- Consumes: Plan 2 的 `store::RefinedParagraph`(已含 `mentions` 字段,本任务不写它)。

- [ ] **Step 1: 写失败测试——响应带 entities 时解析出来 + 宽松降级**

在 `llm.rs` 测试模块(`mod tests`)追加。**复用该模块现有 helper**(已确认存在,`llm.rs:315+`):`mock_server(responses: Vec<String>) -> String`(返回 base URL,`responses` 是每次连接回的 body)、`para(text) -> RefinedParagraph`(已含 `mentions: vec![]`);`LlmConfig` 在测试里 inline 构造 `LlmConfig { base_url: base, model: "m".into(), api_key: "k".into() }`。现有的 `chat_body(texts, glossary)` **不含 entities**,故本任务的实体响应**内联整段 body 字符串**(如下),不改 `chat_body`。

```rust
#[test]
fn parses_entities_from_response() {
    let body = r#"{"choices":[{"message":{"content":"{\"glossary\":{},\"texts\":[\"灯塔计划下周启动\"],\"entities\":[{\"name\":\"灯塔计划\",\"kind\":\"project\",\"aliases\":[\"Lighthouse\"]}]}"}}]}"#;
    let base = mock_server(vec![body.to_string()]);
    let cfg = LlmConfig { base_url: base, model: "m".into(), api_key: "k".into() };
    let mut ps = vec![para("灯塔计划下周启动")];
    let (outcome, ents) = polish(&cfg, &mut ps, None);
    assert!(matches!(outcome, LlmOutcome::Done));
    assert_eq!(ps[0].text, "灯塔计划下周启动");
    assert_eq!(ents.len(), 1);
    assert_eq!(ents[0].name, "灯塔计划");
    assert_eq!(ents[0].kind, "project");
    assert_eq!(ents[0].aliases, vec!["Lighthouse".to_string()]);
}

#[test]
fn missing_entities_key_degrades_to_empty_without_failing_texts() {
    let body = r#"{"choices":[{"message":{"content":"{\"glossary\":{},\"texts\":[\"你好\"]}"}}]}"#;
    let base = mock_server(vec![body.to_string()]);
    let cfg = LlmConfig { base_url: base, model: "m".into(), api_key: "k".into() };
    let mut ps = vec![para("你好")];
    let (outcome, ents) = polish(&cfg, &mut ps, None);
    assert!(matches!(outcome, LlmOutcome::Done), "缺 entities 不影响 texts 成败");
    assert!(ents.is_empty());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib refine::llm::tests::parses_entities_from_response 2>&1 | tail -20`
Expected: 编译失败——`polish` 返回的是 `LlmOutcome` 不是元组、`RawEntity` 未定义。

- [ ] **Step 3: 加 `RawEntity` + prompt 加实体指令**

替换 `SYSTEM_PROMPT`(`llm.rs:11`)为(在原四件事后加抽取实体,输出 JSON 增 entities):

```rust
const SYSTEM_PROMPT: &str = "你是会议逐字稿精修助手。对输入的每个段落做四件事,除此之外禁止任何改动:\n1. 纠正同音/近音错字(如「肯计→肯定」),不确定时保留原文,禁止改写句式或语义;\n2. 实体归一:同一人名/产品名/术语全文统一为最常见或术语表给定的写法;\n3. 轻度清理口头语:删除无意义的「嗯」「呃」及紧邻重复(「我们我们→我们」),保留语气词「吧」「啊」等;\n4. 英文与数字排版:英文词组与中文之间加空格,产品名保持原大小写。\n此外,抽取本批出现的关键实体(不改动正文),用修订后的规范名。\n输出 JSON:{\"glossary\":{\"错误写法\":\"统一写法\"},\"texts\":[\"段落1修订文\",\"段落2修订文\"],\"entities\":[{\"name\":\"规范名\",\"kind\":\"person|org|project|term|decision|task|place|date\",\"aliases\":[\"别名\"]}]}。\ntexts 数组长度必须与输入段落数一致,顺序一致。glossary 只收实体类归一项。entities 没有可给空数组,aliases 可省略。";
```

在 `LlmOutcome` 附近(`llm.rs:23` 后)加类型:

```rust
/// 大模型每块吐出的原始实体(未去重、未分配 id)。解析层(refine/mod.rs)再规范化。
#[derive(Debug, Clone, PartialEq)]
pub struct RawEntity {
    pub name: String,
    pub kind: String,
    pub aliases: Vec<String>,
}
```

- [ ] **Step 4: `do_call_chunk` 宽松解析 entities**

在 `do_call_chunk`(`llm.rs:163-196`)里,`texts_out` 长度校验通过后、`Ok(...)` 之前,加实体解析(宽松:缺/坏 → 空,不报错);把返回签名从 `Result<(String, Value, Vec<String>), ChunkErr>` 改为携带实体:

```rust
    let entities = parse_raw_entities(&parsed["entities"]);
    Ok((resp_text, parsed["glossary"].clone(), texts_out, entities))
```

新增私有宽松解析函数(放 `do_call_chunk` 上方):

```rust
/// 宽松解析实体数组:非数组 → 空;逐项跳过缺 name 的;kind 缺省 "term";aliases 缺省空。
/// 绝不返回错误——实体是增值层,坏数据只当没有,不拖垮 texts。
fn parse_raw_entities(v: &Value) -> Vec<RawEntity> {
    let Some(arr) = v.as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|e| {
            let name = e["name"].as_str()?.trim();
            if name.is_empty() {
                return None;
            }
            let kind = e["kind"].as_str().unwrap_or("term").trim();
            let kind = if kind.is_empty() { "term" } else { kind };
            let aliases = e["aliases"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            Some(RawEntity { name: name.to_string(), kind: kind.to_string(), aliases })
        })
        .collect()
}
```

- [ ] **Step 5: `call_chunk` 透传 entities**

`call_chunk`(`llm.rs:114-160`)返回类型从 `Result<(Value, Vec<String>), ChunkErr>` 改为 `Result<(Value, Vec<String>, Vec<RawEntity>), ChunkErr>`;日志分支里 `Ok((raw, _, _))` 改成 `Ok((raw, _, _, _))`(多一个绑定);结尾 `result.map(|(_, glossary, texts)| (glossary, texts))` 改为 `result.map(|(_, glossary, texts, ents)| (glossary, texts, ents))`。

- [ ] **Step 6: `polish` 累积实体 + 改返回类型**

`polish`(`llm.rs:274-313`)签名改为 `-> (LlmOutcome, Vec<RawEntity>)`;新增 `let mut all_entities: Vec<RawEntity> = Vec::new();`;`Ok` 分支解构改为 `Ok((g, outs, ents))`,在应用 texts 后 `all_entities.extend(ents);`;三个 return 点改为返回元组:

```rust
    // 早退空块
    if chunks.is_empty() {
        return (LlmOutcome::Done, Vec::new());
    }
    ...
    let outcome = if failed == 0 {
        LlmOutcome::Done
    } else if network_failed == chunks.len() {
        LlmOutcome::Failed
    } else {
        LlmOutcome::Partial(failed)
    };
    (outcome, all_entities)
```

失败块不 extend(其 entities 根本没解析出来),天然只累积成功块的实体。

- [ ] **Step 7: 更新 llm.rs 内既有 polish 测试的解构**

`llm.rs:315+` 里凡 `let outcome = polish(...)` 或 `assert!(matches!(polish(...), ...))` 改为 `let (outcome, _ents) = polish(...)`。用 `cargo test --lib refine::llm 2>&1 | grep -n "error\[" ` 逐个编译错误定位修正。**不改这些测试的断言语义**,只适配元组返回。

- [ ] **Step 8: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib refine::llm 2>&1 | tail -6`
Expected: 新增 2 测过 + 原 llm 测(polish/chunk/probe 等)过。
Run: `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | tail -2`
Expected: 无 error。

- [ ] **Step 9: 提交**

```bash
git add src-tauri/src/refine/llm.rs
git commit -m "HTTP 结构化输出增实体抽取:SYSTEM_PROMPT 加第五件事、响应 entities 宽松解析(缺/坏→空不拖垮 texts)、polish 跨块累积返回 (LlmOutcome, Vec<RawEntity>);同一次调用产修订+实体"
```

---

### Task 2: 实体解析(去重+局部 id)与 mention 计算(确定性子串搜索)

**Files:**
- Modify: `src-tauri/src/refine/mod.rs`(新增纯函数与其单测;引用 `crate::store::{Entity, Mention, RefinedParagraph}` 与 `super::llm::RawEntity`)

**Interfaces:**
- Consumes: Task 1 的 `llm::RawEntity`;Plan 2 的 `store::Entity { id, kind, name, aliases }`、`store::Mention { entity, start, end }`。
- Produces:
  - `pub(crate) fn resolve_note_entities(raw: Vec<llm::RawEntity>) -> Vec<store::Entity>`（按规范名不分大小写去重、合并别名、按首次出现顺序分配局部 id `ent_1`/`ent_2`…；**本 plan 不做全局 person 归并**——那是 Plan 4)
  - `pub(crate) fn compute_mentions(paragraphs: &[store::RefinedParagraph], entities: &[store::Entity]) -> Vec<Vec<store::Mention>>`（对每段,在**修订后** `text` 里按各实体 name+aliases 子串搜索,产 char 下标区间,单段内非重叠;返回与 `paragraphs` 等长、逐段对齐的 mentions)

- [ ] **Step 1: 写失败测试——去重合并 + mention char 偏移(含中文多字节)**

在 `refine/mod.rs` 测试模块追加(mod.rs 测试模块**没有**造 `RefinedParagraph` 的 helper——本任务先加一个局部 `para`):

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib refine::tests::resolve_dedups 2>&1 | tail -20`
Expected: 编译失败(`resolve_note_entities`/`compute_mentions` 未定义)。

- [ ] **Step 3: 实现三函数**

在 `refine/mod.rs`(顶部 `use` 区确保有 `use crate::store::{self, Entity, Mention, RefinedParagraph};` 或按现有引用风格补;`llm::RawEntity` 经 `super::llm` 或已在 scope)加:

```rust
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
                if !a.is_empty() && a.to_lowercase() != key && !e.aliases.iter().any(|x| x.to_lowercase() == a.to_lowercase()) {
                    e.aliases.push(a);
                }
            }
        } else {
            let id = format!("ent_{}", out.len() + 1);
            let mut aliases: Vec<String> = Vec::new();
            for a in r.aliases {
                let a = a.trim().to_string();
                if !a.is_empty() && a.to_lowercase() != key && !aliases.iter().any(|x| x.to_lowercase() == a.to_lowercase()) {
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib refine::tests::resolve_dedups refine::tests::compute_mentions 2>&1 | tail -8`
Expected: 3 测全过。特别核对 `compute_mentions_finds_name_and_alias_by_char_offset` 的 char 偏移(中文/空格/英文混排)正确。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/refine/mod.rs
git commit -m "实体解析 + mention 计算:resolve_note_entities(规范名去重/合并别名/局部 ent_N id,不做全局归并)、compute_mentions(修订后正文子串搜索→char 下标区间,单段内非重叠长匹配优先)"
```

---

### Task 3: run_llm 编排——填充 entities/mentions/stages.entities,实体独立降级

**Files:**
- Modify: `src-tauri/src/refine/mod.rs`(`run_llm` `mod.rs:195-214`;测试模块)

**Interfaces:**
- Consumes: Task 1 的 `polish -> (LlmOutcome, Vec<RawEntity>)`;Task 2 的 `resolve_note_entities`/`compute_mentions`;Plan 2 的 `doc.entities`/`doc.paragraphs[].mentions`/`doc.stages.entities`。
- Produces:
  - `pub(crate) fn fill_entities(doc: &mut RefinedDoc, raw: Vec<llm::RawEntity>, text_state: &str)`——把原始实体解析+算 mention 落进 `doc`,`stages.entities` 置为 `text_state`(与文本 outcome 同源)。**抽成纯函数以便无网络单测**(run_llm 的网络+实体端到端已由 Task 1 的 polish 测试覆盖)。
  - `run_llm` 在 HTTP 路径调 `polish` 后调 `fill_entities`;签名不变(`note_dir, doc, cfg, llm_model, log`),`lib.rs` 调用点无需改。

- [ ] **Step 1: 写失败测试——fill_entities 纯函数(无网络)+ run_llm 薄接线**

在 `refine/mod.rs` 测试模块追加(用 Task 2 已加的局部 `para` helper;`fill_entities` 纯函数不触网,直接喂 `RawEntity`):

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib refine::tests::fill_entities 2>&1 | tail -20`
Expected: 编译失败(`fill_entities` 未定义)。

- [ ] **Step 3: 加 `fill_entities` + `run_llm` 调它**

在 `refine/mod.rs`(Task 2 的两函数附近)加纯函数:

```rust
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
```

替换 `run_llm`(`mod.rs:195-214`)体为(polish 新返回元组 + 调 fill_entities + 写盘失败双阶段降级):

```rust
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
```

（注:`fill_entities` 只写 entities/mentions/stages.entities,从不碰 `text`;文本失败时 `state="failed"` → entities 空、stages.entities "failed",符合「同源」语义。`zip` 天然按 paragraphs 长度对齐,`compute_mentions` 恒等长,不会漏段或越界。)

- [ ] **Step 4: 跑测试确认通过 + 全量回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib refine 2>&1 | tail -6`
Expected: 新增 3 测 + Task 1/2 测 + 原 refine 测全过。
Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib 2>&1 | tail -3`
Expected: 全量 `ok`(= 基线 485 + 本 plan 新增数)。
Run: `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | tail -2` → 无 error。
Run: `npm run check 2>&1 | tail -2` → 0/0。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/refine/mod.rs
git commit -m "run_llm 编排实体填充:polish 后 resolve_note_entities + compute_mentions 落 doc.entities/paragraphs[].mentions/stages.entities(跟随文本 outcome);实体环节独立降级——失败只清空标 failed 不回退修订;写盘失败双阶段降级"
```

---

## Self-Review
- **Spec 覆盖**:覆盖架构 spec Phase 1 的「可复用批函数:一批文本+上下文→结构化(修订+实体)」「同一次调用产出结构化 JSON」「实体入 aing.json + 每段提及区间」「HTTP 产实体、Agent 退化无实体」「实体失败只降级不塌」。**不做**(后续/别处):全局 person 归并与非人实体去重、SQLite 图谱、笔记间关联(全 Plan 4);实时分批(Phase 3);内部标识符改名(独立 cosmetic plan)。
- **红线可验**:Task 3 三测覆盖「实体落盘」「无实体也 done」「文本失败保原文且实体空」;`compute_mentions` 长度不齐降级分支 + 写盘失败双降级,保证实体环节永不回退修订/本地稿。
- **占位符**:prompt/解析/解析层/mention/编排/测试均给完整 Rust 代码,含实测的 helper 名——llm.rs 用现有 `mock_server(vec![body])->url`/`para(text)`/inline `LlmConfig`(已核 `llm.rs:315+`),mod.rs 测试新加局部 `para`/`doc_with`(mod.rs 无现成 RefinedParagraph builder,故自带);无 TBD。`fill_entities` 抽纯函数使实体编排无网络可测,规避了 mod.rs 拿不到 llm.rs 私有 `mock_server` 的问题。
- **类型一致**:`RawEntity{name,kind,aliases}`(Task 1)= Task 2/3 引用一致;`resolve_note_entities(Vec<RawEntity>)->Vec<Entity>`、`compute_mentions(&[RefinedParagraph],&[Entity])->Vec<Vec<Mention>>`(Task 2 定义)= Task 3 调用签名一致;`polish` 新返回 `(LlmOutcome, Vec<RawEntity>)`(Task 1)= Task 3 解构一致;`store::Entity{id,kind,name,aliases}`/`store::Mention{entity,start,end}` 沿用 Plan 2 定型。
- **契约不变**:`stages.llm` 值域/语义、hook key、Tauri 命令-事件名、MCP 工具名、既有 JSON 键全未改;只写入 Plan 2 定型的 entities/mentions/stages.entities。Agent 路径与 lib.rs 未改。
