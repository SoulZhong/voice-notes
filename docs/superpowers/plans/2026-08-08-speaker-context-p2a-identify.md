# 说话人上下文推断 P2a(identify 只读期)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 spec P2a:refine 管线新增 identify 阶段(LLM 用会议文本推断「R 簇→真实身份」),经程序侧四道裁决后**只出建议卡/报告,零自动写入**;建议卡进整理收件箱,人工确认后落地关联并经 P1 的 feedback 链路回灌质心;每场推断结果落 identify.json,为 P2b 开闸积累评测样本。

**Architecture:** recluster 增导出簇统计(质心/时长/成员 seq/信道分布);新模块 `refine/identify.rs` 承载输入打包(候选 Top-K 召回 + 轻量人名预筛采样)、输出解析与四道裁决(逐字区间校验/冲突/声学门/分档)。执行体仿 `RelationExecutor` 模式建 `IdentifyExecutor` trait,P2a 先交付 HTTP 实现(Agent 实现独立成尾部任务,可拆后续 PR)。产物落笔记目录 `identify.json`(绑 revision+source_hash+簇指纹,状态机 suggested/applied/rejected);挂进 `spawn_refine` 于 run_local 之后。IPC 三条(列建议/确认/拒绝)+ 触发重试 `identify_note`;MCP 只读工具返回最近推断结果。前端收件箱加 `identify` 卡片类型。

**Tech Stack:** Rust(Tauri 2)+ SvelteKit;无新 crate 依赖(sha2/hex/serde 已有)。

**Spec:** `docs/superpowers/specs/2026-08-08-speaker-context-inference-design.md`(rev2)
**基线:** 分支 `feat/speaker-context-p2a`,叠在 `feat/speaker-context-p1`(PR #79)之上——复用 feedback 回灌与 speaker_eval。

## Global Constraints

- **零自动写入**:P2a 一切 identify 结论只落 identify.json + 建议卡;唯一落地路径是用户点确认(`apply_identify_suggestion`)。
- identify 失败绝不阻塞 Aing 其余阶段:`RefineStages.identify` 独立置值(off/running/done/failed/skipped),报错只写 stage + ailog。
- 与 spec 的三处**有意收窄**(实现时保持,并已在本计划各任务注明):① P2a 的拒绝/状态记录内嵌 identify.json(绑簇指纹),独立 identify_journal 推迟到 P2b 自动应用需要 intent 补偿时再建;② 声学门 P2a 用同信道裸余弦 ≥ `SEED_ASSIGN_THRESHOLD`(0.68)正向确认,AS-Norm z 通道推迟到 P2b 调优(P2a 全员建议卡,声学门只影响档位记录);③ MCP 工具返回**最近一次已落盘的推断结果**(只读),重新推断入口是 IPC `identify_note`。
- 模型门禁沿用:`voiceprints.embedding_model != settings.speaker_model` 时声学门直接判「无声学确认」(不碰质心比较),文本档位照常。
- 新 serde 字段一律 `#[serde(default)]`;`RefineStages` 结构体字面量构造点全量修复(已知:`refine/mod.rs:289,758` + `refine/mod.rs` 测试 1547/1587 + `store/refined.rs` 测试 1009/1160/1374/1522 附近,以编译器报错为准)。
- **不跑全量 `cargo fmt`**(仓库非 rustfmt 全量干净);新文件可单独 `rustfmt src-tauri/src/refine/identify.rs`。
- 新增 MCP 工具必须同步更新 `mcp/server.rs` 三处工具计数断言(L489/L564/L584 附近);新增 IPC 命令留意 `lib.rs:5868-5872`、`lib.rs:5990-5994` 两处解析 invoke_handler 源码文本的测试。
- 每任务收尾:`cd src-tauri && cargo test --lib <过滤>`,提交;文案 `tr!("中文","English")` 双语。

---

### Task 1: recluster 导出簇统计

**Files:**
- Modify: `src-tauri/src/refine/recluster.rs`(主函数签名 :61、输出段 :140-204、5 个单测 :229-278)
- Modify: `src-tauri/src/refine/mod.rs:273-282`(唯一调用点)

**Interfaces:**
- Produces:

```rust
/// 会后簇统计:identify 的声学输入。R 号与 Assignment 一致(时长降序编号)。
#[derive(Debug, Clone)]
pub struct ClusterStat {
    pub speaker: String,                     // "R1"
    pub centroid: Vec<f32>,                  // 单位向量(merge_centroid 已归一)
    pub total_ms: u64,
    pub member_seqs: Vec<u64>,               // 升序;簇指纹的原料
    pub sources: BTreeMap<String, u64>,      // 信道 -> 该信道时长 ms
    pub seed: Option<(String, String, f32)>, // 命中的库种子 (person_id, name, cosine)
}

pub fn recluster(inputs: &[SegInput], embs: &[Option<Vec<f32>>], seeds: &[SeedCluster])
    -> (Vec<Assignment>, Vec<ClusterStat>);
```

- [ ] **Step 1: 改签名与实现**。在 :140 排序后、:189 输出 Assignment 的同一循环里,同步产出 `ClusterStat`:`member_seqs` 取 `cl.members` 映射回 `inputs[i].seq` 后排序;`sources` 按成员段 `inputs[i].source` 累加各自时长;`seed` 取 :145-155 种子命名循环算出的最佳 `(person, name, sim)`(即便 `sim < 0.68` 未采纳命名,也把最佳值记进 stat 供裁决层参考——采纳与否由 `Assignment.person` 是否 Some 区分)。无簇路径(全场无嵌入)返回 `(assign, vec![])`。
- [ ] **Step 2: 修调用点**:`refine/mod.rs:275` 改为 `let (assign, cluster_stats) = recluster::recluster(...)`,`cluster_stats` 暂以 `let _ =` 接住(Task 6 接线),fallback/None 两分支给 `vec![]`。
- [ ] **Step 3: 修 5 个单测**(解构元组第 0 位),并新增一测:

```rust
#[test]
fn recluster_exports_cluster_stats_with_sources_and_members() {
    // 两段同簇(相同嵌入向量),一段 mic 一段 system:
    // stats 应 1 簇、member_seqs=[0,1]、sources 两键各记各自时长。
    let inputs = vec![seg_input(0, "mic", 0, 3000), seg_input(1, "system", 3000, 6000)];
    let embs = vec![Some(unit(0)), Some(unit(0))];
    let (assign, stats) = recluster(&inputs, &embs, &[]);
    assert_eq!(assign.len(), 2);
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].member_seqs, vec![0, 1]);
    assert_eq!(stats[0].sources["mic"], 3000);
    assert_eq!(stats[0].sources["system"], 3000);
    assert_eq!(stats[0].total_ms, 6000);
}
```

(`seg_input`/`unit` 按本文件既有测试工厂写法;若无则新建同款。)
- [ ] **Step 4**: `cargo test --lib recluster` 全绿;提交 `feat(recluster): 导出簇统计(质心/成员/信道分布/种子命中)供 identify 使用`。

---

### Task 2: RefineStages.identify 字段

**Files:**
- Modify: `src-tauri/src/store/refined.rs:68-77` + 全部构造点

- [ ] **Step 1**: `RefineStages` 加 `#[serde(default = "stage_off")] pub identify: String,`(放 recluster 与 llm 之间,与管线顺序一致)。
- [ ] **Step 2**: `cargo check --lib 2>&1 | grep E0063` 列出全部缺字段构造点,逐个补 `identify: "off".into()`(`run_local` :289 处即初值 `"off"`)。
- [ ] **Step 3**: 新增回归测试(refined.rs tests):旧 aing.json(无 identify 字段)反序列化后 `identify == "off"`:

```rust
#[test]
fn stages_identify_defaults_to_off_for_legacy_docs() {
    let legacy = r#"{"filter":"done","recluster":"done","llm":"off","entities":"off","relations":"off"}"#;
    let s: RefineStages = serde_json::from_str(legacy).unwrap();
    assert_eq!(s.identify, "off");
}
```

- [ ] **Step 4**: `cargo test --lib` 全绿;提交 `feat(store): RefineStages 增 identify 阶段位(serde default 向后兼容)`。

---

### Task 3: identify 输入打包(候选召回 + 采样 + 指纹)

**Files:**
- Create: `src-tauri/src/refine/identify.rs`(`refine/mod.rs` 加 `pub mod identify;`)
- Modify: `src-tauri/src/feedback.rs`(`scope_key` 改 `pub(crate) fn seq_fingerprint(seqs: &BTreeSet<u64>) -> String` 并导出,feedback 内部改调它;identify 复用同一指纹算法,保证 P1 账本与 P2a 指纹口径一致)

**Interfaces:**
- Consumes: `RefinedDoc.paragraphs`、Task 1 `ClusterStat`、`Voiceprints`(people/redirects)、`crate::store::source_hash(&doc.paragraphs)`(agent.rs :658 同款)。
- Produces:

```rust
pub const MAX_CANDIDATES: usize = 30;      // 候选人上限(全量人名不进 prompt)
pub const ACOUSTIC_TOP_K: usize = 10;      // 声学近邻召回
pub const RECENT_TOP_K: usize = 10;        // last_seen 最近召回
pub const SAMPLE_CHAR_BUDGET: usize = 6000; // 采样段落总字符预算

pub struct Candidate { pub person_id: String, pub name: String }

/// LLM 输入包:序列化后即 user prompt 的 JSON 主体。
pub struct IdentifyContext {
    pub note_id: String,
    pub revision: u64,
    pub source_hash: String,
    pub clusters: Vec<ClusterBrief>,   // 每簇:R号/时长/主信道/现有关联/指纹
    pub candidates: Vec<Candidate>,
    pub sampled: Vec<SampledParagraph>, // {paragraph_index, speaker(R号), text}
}

pub fn build_context(
    note_id: &str, doc: &RefinedDoc, stats: &[ClusterStat], vp: &Voiceprints,
) -> IdentifyContext;
```

- [ ] **Step 1: 写失败测试**(identify.rs 内):

```rust
#[test]
fn candidates_come_from_acoustic_neighbors_recent_and_seed_hits() {
    // 库 3 人:A(质心与簇0余弦最高)、B(last_seen 最近)、C(不相关且久远)。
    // MAX_CANDIDATES 足够大时 A、B 必在候选,顺序 A(声学) 先于 B(时近)。
    // C 在 K 收紧到 1+1 时被挤出。
}

#[test]
fn sampling_picks_name_hits_intro_patterns_and_cluster_openings() {
    // 段落:簇 R1 开场段、含候选人名"张伟"的段、含"我是"句式的段、无关长段。
    // 预算内前三者必入选,无关段在预算耗尽时被裁;总字符 <= SAMPLE_CHAR_BUDGET。
}

#[test]
fn fingerprint_matches_feedback_scope_key_algorithm() {
    let seqs: std::collections::BTreeSet<u64> = [3u64, 1, 2].into_iter().collect();
    assert_eq!(cluster_fingerprint(&seqs), crate::feedback::seq_fingerprint(&seqs));
}
```

- [ ] **Step 2: 实现**。要点(按 spec「输入打包与候选召回」):
  - 候选三路召回取并集,去重后截 `MAX_CANDIDATES`:① 声学近邻:每簇质心 × 每人各信道主质心裸余弦,取全局 top `ACOUSTIC_TOP_K` 人(经 redirects 归一,只收有名人物——无名 P<n> 对起名无意义);② `last_seen` 降序 top `RECENT_TOP_K` 有名人物;③ recluster 种子命中人(`ClusterStat.seed`)无条件收入;
  - 采样(**不依赖知识图谱**,spec 循环依赖修正):优先级 a) 每簇开场 2 段;b) 文本含任一候选人名的段;c) 自报句式正则命中段(`我是|我叫|这边是|我这边是`——用 `str::contains` 多模式即可,不引 regex crate);d) 簇切换边界前后各 1 段;按优先级填充至 `SAMPLE_CHAR_BUDGET`,超预算的段截前 200 字符;
  - `ClusterBrief`:`{ speaker, fingerprint: cluster_fingerprint(&member_seqs), total_ms, dominant_source(时长最大信道), mixed(信道>1 且次信道占比>20%), linked: Option<(person_id,name)>(来自 Assignment/seed 采纳) }`;
  - mic 先验:`dominant_source=="mic"` 的簇在 brief 里带 `is_mic: true`(prompt 告知「mic 簇大概率是『我』」)。
- [ ] **Step 3**: `cargo test --lib identify::` 全绿;提交 `feat(identify): 输入打包——候选三路召回、轻量人名采样、簇指纹与 P1 账本同口径`。

---

### Task 4: 输出解析与四道裁决

**Files:**
- Modify: `src-tauri/src/refine/identify.rs`(追加)

**Interfaces:**

```rust
/// LLM 原始输出(json_object 解析目标;宽松:坏条目跳过不拖垮整批)。
#[derive(Deserialize)]
pub struct RawIdentify { pub assignments: Vec<RawAssignment> }
#[derive(Deserialize)]
pub struct RawAssignment {
    pub cluster: String,                    // "R2"
    pub person_id: Option<String>,          // 库内已有人(二选一)
    pub new_name: Option<String>,           // 新名字(二选一)
    pub confidence: String,                 // high|medium|low(自报,仅参考)
    pub evidence: Vec<RawIdentifyEvidence>,
}
#[derive(Deserialize)]
pub struct RawIdentifyEvidence {
    pub paragraph_index: usize,
    pub start: usize, pub end: usize,       // Unicode scalar 半开区间
    pub quote: String,
    pub r#type: String,                     // self_intro|addressed_reply|third_person_exclusion|role_topic
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone, Copy)]
pub enum Tier { High, Medium, Low }

/// 裁决产物(落盘用,Task 6 定义完整 IdentifyDoc)。
pub struct Verdict {
    pub tier: Tier,
    pub acoustic: Option<(String, f32)>,    // (信道, 与目标人同信道余弦)
    pub reject_reason: Option<String>,      // Some = 该条被整条丢弃(不落建议)
}

pub fn adjudicate(
    raw: &RawAssignment, ctx: &IdentifyContext, doc: &RefinedDoc,
    stats: &[ClusterStat], vp: &Voiceprints, model_gate_ok: bool,
    taken: &BTreeMap<String, String>,       // 已裁决通过的 cluster -> person 键(冲突检测)
) -> Verdict;
```

- [ ] **Step 1: 写失败测试**(每关一测):

```rust
#[test]
fn evidence_quote_and_range_must_match_paragraph_text() {
    // 区间校验:doc.paragraphs[i].text 的 char 区间子串必须逐字等于 quote;
    // 越界/不等 → reject_reason=Some("evidence-mismatch")。
}
#[test]
fn two_clusters_pointing_to_same_person_degrade_to_medium() {
    // taken 里已有同 person → tier 最高 Medium,不 reject(建议卡让人裁)。
}
#[test]
fn acoustic_gate_requires_same_source_cosine() {
    // 目标人有同 dominant_source 质心且余弦>=0.68 → acoustic Some 且可 High;
    // 余弦不足/跨信道混合簇/目标人无同信道质心/model_gate_ok=false → 最高 Medium。
}
#[test]
fn tier_high_requires_self_intro_all_gates() {
    // self_intro 证据 + 校验过 + 无冲突 + 声学过 → High;
    // 仅 addressed_reply → Medium;仅 role_topic → Low;
    // person_id 与 new_name 都空/都有 → reject。
}
```

- [ ] **Step 2: 实现**。裁决顺序(spec 四道):① 结构合法(person_id XOR new_name;person_id 经 resolve 存在,悬空 → reject);② 逐字区间校验(`paragraphs[i].text.chars()` 按 scalar 取 `[start,end)`,与 quote 全等;任一 evidence 不过 → 整条 reject);③ 冲突:`taken` 已含同 person 或同 cluster → 降 Medium;④ 声学门:`model_gate_ok && !mixed && 目标人存在 dominant_source 主质心 && cos >= 0.68` → 记 acoustic,否则 None;⑤ 分档:有 `self_intro` 证据且②④全过且无冲突 → High;有 `self_intro|addressed_reply` → Medium;只剩 `role_topic|third_person_exclusion` → Low。`new_name` 目标无质心可比:声学门恒 None,最高 Medium(新人必须人工拍板,spec「新建 Person 没有设计」的回应——建人发生在确认时)。
- [ ] **Step 3**: `cargo test --lib identify::` 全绿;提交 `feat(identify): 输出解析与四道裁决(逐字区间/冲突/声学门/分档)`。

---

### Task 5: IdentifyExecutor trait + HTTP 实现

**Files:**
- Modify: `src-tauri/src/refine/identify.rs`(trait + prompt)
- Modify: `src-tauri/src/refine/llm.rs`(HTTP 实现,仿 `HttpRelationExecutor` :22)
- Modify: `src-tauri/src/lib.rs`(分派函数,放 `relation_executor` :2321 旁)

**Interfaces:**

```rust
// identify.rs
pub trait IdentifyExecutor: Send + Sync {
    fn provider(&self) -> &str;
    fn model(&self) -> &str;
    fn infer(&self, ctx: &IdentifyContext, log: Option<&crate::ailog::Ctx>)
        -> anyhow::Result<RawIdentify>;
}
pub const IDENTIFY_SYSTEM_PROMPT: &str = "…";   // Step 2 全文

// llm.rs
pub struct HttpIdentifyExecutor { cfg: LlmConfig }   // new() 校验三字段非空,同 HttpRelationExecutor

// lib.rs
fn identify_executor(settings: &settings::Settings)
    -> anyhow::Result<Box<dyn refine::identify::IdentifyExecutor>>
// "openai" => HttpIdentifyExecutor;"agent" => bail!("identify 的 Agent 执行体见 Task 11/后续 PR")
```

- [ ] **Step 1: SYSTEM_PROMPT 全文**(中文单行常量,要点必须全含):角色=会议说话人身份推断器;输入=簇列表(R号/时长/主信道/现有关联/is_mic)+候选人列表(person_id+name)+采样段落(paragraph_index+speaker+text);任务=为**无名或存疑**簇推断身份;证据类型四种及各自含义;**只允许引用逐字存在的证据**,start/end 为 Unicode scalar 半开区间、quote 逐字符相等;mic 簇大概率是「我」;参考候选但允许 new_name(候选外的名字必须有 self_intro 级证据);输出 JSON `{"assignments":[{"cluster":"R2","person_id":"P3"或null,"new_name":null或"张伟","confidence":"high|medium|low","evidence":[{"paragraph_index":0,"start":0,"end":5,"quote":"我是张伟","type":"self_intro"}]}]}`;没有可靠推断输出空数组;**禁止为已明确关联且无矛盾证据的簇输出条目**。
- [ ] **Step 2: HTTP 实现**:单次请求(不分块),`temperature 0.1` + `response_format json_object` + `apply_thinking_off`,`REQ_TIMEOUT_S` 沿用 60s;ailog `kind: "identify"`(request 全量/response 原文,仿 `call_chunk` :285-305);解析失败归 `ChunkErr::Content` 同语义(返回 Err,由调用方标 failed)。
- [ ] **Step 3: mock 测试**(复用 Task 1/P1 的 `mock_server_capturing`):请求体含 `assignments` schema 说明与候选人名;响应解析回 RawIdentify。
- [ ] **Step 4**: `cargo test --lib` 全绿;提交 `feat(identify): IdentifyExecutor trait 与 HTTP 实现(单次请求+ailog)`。

---

### Task 6: identify.json 落盘 + spawn_refine 挂钩 + identify_note IPC

**Files:**
- Modify: `src-tauri/src/refine/identify.rs`(IdentifyDoc + run_identify)
- Modify: `src-tauri/src/refine/mod.rs:273-306`(cluster_stats 从 run_local 带出——`RefinedDoc` 不动,改 `run_local` 返回 `(RefinedDoc, Vec<ClusterStat>)`,调用点两处:`spawn_refine` 与测试)
- Modify: `src-tauri/src/lib.rs`(`spawn_refine` :402-424 之间插入;新命令 `identify_note`;invoke_handler 注册)

**Interfaces:**

```rust
// identify.rs — 落盘结构(笔记目录 identify.json,原子写 tmp+rename)
pub const IDENTIFY_FILE: &str = "identify.json";

#[derive(Serialize, Deserialize)]
pub struct IdentifyDoc {
    pub schema_version: u32,               // 1
    pub generated_at: String,
    pub provider: String, pub model: String,
    pub revision: u64,                     // 生成时 RefinedDoc.revision
    pub source_hash: String,               // 生成时段落 hash(过期判定)
    pub assignments: Vec<IdentifyAssignment>,
}
#[derive(Serialize, Deserialize)]
pub struct IdentifyAssignment {
    pub fingerprint: String,               // 簇指纹(绑定身份,不绑 R 号)
    pub cluster: String,                   // 生成时的 R 号(展示用)
    pub person_id: Option<String>, pub new_name: Option<String>,
    pub tier: Tier, pub llm_confidence: String,
    pub acoustic: Option<(String, f32)>,
    pub evidence: Vec<StoredEvidence>,     // 通过校验的证据(原样存)
    pub status: String,                    // suggested | applied | rejected
    #[serde(default)] pub decided_at: Option<String>,
}

pub fn run_identify(
    note_dir: &Path, note_id: &str, doc: &RefinedDoc, stats: &[ClusterStat],
    vp: &Voiceprints, model_gate_ok: bool,
    executor: &dyn IdentifyExecutor, log: Option<&crate::ailog::Ctx>, now: &str,
) -> anyhow::Result<IdentifyDoc>;
pub fn load_identify(note_dir: &Path) -> Option<IdentifyDoc>;
pub fn save_identify(note_dir: &Path, doc: &IdentifyDoc) -> anyhow::Result<()>;
```

- [ ] **Step 1: run_identify 实现**:build_context → executor.infer → 逐条 adjudicate(reject 的丢弃仅 eprintln;Low 丢弃仅 ailog 留痕)→ 与旧 identify.json 合并:**旧稿里同指纹且 status != suggested 的决策(applied/rejected)保留不覆盖**(用户已拍板的不重复打扰);已明确关联的簇(`ClusterBrief.linked` 为 Some 且无矛盾)不出建议 → 组 IdentifyDoc 落盘。单测:mock executor 返回固定 RawIdentify,验证合并保留 rejected、High/Medium 落 suggested、Low 不落。
- [ ] **Step 2: spawn_refine 挂钩**(lib.rs :402 report recluster 之后、:424 provider 分派之前):

```rust
// identify(P2a 只读期):失败只标 stage,绝不影响后续 llm/agent 精修。
doc.stages.identify = "running".into();
report("identify", &doc.stages.identify);
let identify_state = (|| -> anyhow::Result<&'static str> {
    let s = /* 已加载的 settings */;
    let executor = match identify_executor(&s) {
        Ok(e) => e,
        Err(_) => return Ok("skipped"),           // agent provider 等:本期跳过
    };
    let vp = open_voiceprint_store(&app).map_err(anyhow::Error::msg)?.load();
    let gate_ok = vp.embedding_model == s.speaker_model;
    let now = chrono::Local::now().to_rfc3339();
    let log = /* ailog ctx,与 run_llm 同源 */;
    refine::identify::run_identify(&note_dir, &note_id, &doc, &cluster_stats, &vp, gate_ok, executor.as_ref(), log, &now)?;
    Ok("done")
})()
.unwrap_or_else(|e| { eprintln!("identify 失败(不阻塞精修): {e}"); "failed" });
doc.stages.identify = identify_state.into();
report("identify", &doc.stages.identify);
// stage 值随 doc 在后续 run_llm/write 里落盘;identify.json 已独立落盘。
```

  `run_local` 返回值改 `(RefinedDoc, Vec<ClusterStat>)`,mod.rs 与 lib.rs 调用点、mod.rs 既有测试同步解构。
- [ ] **Step 3: `identify_note` 命令**(单独触发/重试,读盘现稿而非重跑 recluster——簇统计从现稿重建:按 `RefinedParagraph.speaker` 分组 `source_seqs`,质心经 `embed_all` 同款逐段重嵌入取均值;为控制篇幅该重建函数 `stats_from_doc(note_dir, doc, embedder)` 放 identify.rs,单测覆盖分组正确性):

```rust
#[tauri::command]
async fn identify_note(app: AppHandle, id: String) -> Result<(), String>
// 守卫:validate_note_id + is_refining 拒绝 + 录制中拒绝(reject_if_active 同款语义);
// spawn_blocking 内:load_refined → stats_from_doc → identify_executor → run_identify;
// 完成后 emit("identify_done", note_id)(前端收件箱 refresh 用)。
```

  注册进 invoke_handler(:5439 宏块),检查两处源码解析测试。
- [ ] **Step 4**: `cargo test --lib` 全绿;提交 `feat(identify): identify.json 落盘、Aing 管线挂钩与 identify_note 重试命令`。

---

### Task 7: 建议动作 IPC(list / apply / reject)

**Files:**
- Modify: `src-tauri/src/lib.rs`(三条命令 + 注册)
- Modify: `src-tauri/src/ipc.rs`(`IdentifySuggestion` 视图类型)
- Modify: `src-tauri/src/store/voiceprints.rs`(`create_person(name, now) -> String`:VP_LOCK 内分配 `P<next_person>`,空质心空样本——质心随 P1 feedback 回灌自然长出)

**Interfaces:**

```rust
// ipc.rs
#[derive(Serialize, Deserialize, Clone)]
pub struct IdentifySuggestion {
    pub note_id: String, pub note_title: String,
    pub cluster: String, pub fingerprint: String,
    pub person_id: Option<String>, pub person_name: String, // 库内人现名或 new_name
    pub is_new: bool, pub tier: String,
    pub quote: String, pub evidence_type: String,           // 首条证据(卡片引文)
    pub generated_at: String,
}

// lib.rs
#[tauri::command] fn list_identify_suggestions(app: AppHandle) -> Result<Vec<ipc::IdentifySuggestion>, String>
// 扫 notes 目录各 identify.json(NoteStore::list 已有列举),收 status=="suggested",
// 过期过滤:identify.source_hash != 当前稿 source_hash 的整篇跳过(稿已变,建议不可信);
// person_id 经 resolve,悬空跳过;generated_at 降序,cap 50。

#[tauri::command] async fn apply_identify_suggestion(app: AppHandle, note_id: String, fingerprint: String) -> Result<(), String>
// spawn_blocking:load_identify 找 fingerprint 且 status=suggested(否则 tr! 报"建议已失效");
// 复核 source_hash 与现稿一致、R 簇仍含该指纹的 seq 集合(load_refined 按 speaker 重算指纹比对);
// person_id 分支:直接调 assign_refined_person 的**内部逻辑**(resolve+name+store::assign_refined_person),
//   随后按 P1 同款 spawn_feedback(SegFilter::Seqs(该簇 source_seqs), prior, resolved) 回灌;
// new_name 分支:vp.create_person(name) 得新 id,再走同一 assign+回灌(prior=None → Reinforce,质心由回灌长出);
// 成功后 identify.json 该条 status="applied"+decided_at,原子回写。

#[tauri::command] fn reject_identify_suggestion(app: AppHandle, note_id: String, fingerprint: String) -> Result<(), String>
// status="rejected"+decided_at 回写;同指纹永不再建议(run_identify Step1 的合并规则保证)。
```

- [ ] **Step 1**: `create_person` + 单测(id 递增、重名允许、空名拒绝)。
- [ ] **Step 2**: 三条命令 + 注册;`apply` 的指纹复核逻辑抽 `fingerprint_still_valid(doc, fingerprint) -> Option<String/*R号*/>` 纯函数放 identify.rs,单测:重聚类换 R 号但成员不变 → 仍 valid;成员变了 → invalid。
- [ ] **Step 3**: `cargo test --lib` 全绿;提交 `feat(identify): 建议列表/确认/拒绝命令——确认走 assign+P1 回灌,新名建人`。

---

### Task 8: MCP 只读工具 `identify_speakers`

**Files:**
- Modify: `src-tauri/src/mcp/server.rs`(参数结构 + 工具方法 + **三处计数断言**)
- Modify: `src-tauri/src/mcp/tools.rs`(业务实现)

- [ ] **Step 1**: tools.rs 加 `pub fn identify_speakers(roots, note_id) -> anyhow::Result<Value>`:AnchoredRefinedDir 同款防护读 identify.json,返回 `{note_id, generated_at, provider, model, revision, source_hash, assignments:[{cluster, fingerprint, person_id, new_name, tier, status, evidence}]}`;无文件 → `bail!(tr!("该笔记尚无身份推断结果,先运行 Aing 或 identify_note","..."))`。
- [ ] **Step 2**: server.rs 样板(仿 get_aing_context :365-377):`IdentifySpeakersParams { note_id }`,`#[tool(description="读取笔记最近一次说话人身份推断结果(只读;裁决与证据齐全)。重新推断请在应用内触发。")]`。**不加入** `AGENT_TOOL_NAMES`(spec:MCP 仅只读副产品,Agent 沙箱白名单本期不动)。
- [ ] **Step 3**: 更新三处工具计数断言;`cargo test --lib mcp` + `cargo test --test mcp_stdio` 全绿;提交 `feat(mcp): identify_speakers 只读工具`。

---

### Task 9: 前端收件箱 identify 建议卡

**Files:**
- Modify: `src/lib/people.ts`(类型 + 三个 invoke 绑定)
- Modify: `src/lib/tidyQueue.ts`(`TidyItem` 加 `{ kind: "identify"; suggestion: IdentifySuggestion }`;`tidyItemKey` 加 `i:<note_id>:<fingerprint>`;`buildTidyQueue` 增入参,排序位:回执之后、合并建议之前——身份建议时效性最强)
- Modify: `src/lib/tidy.svelte.ts`(state 增 `identify: IdentifySuggestion[]`;`doRefresh` 并行加 `listIdentifySuggestions()`;`visible` 同款 dismissed 过滤——注意 identify 的"忽略"走 `reject_identify_suggestion`(后端真值),**不写 dismiss_tidy_item**,本地 Set 仅作乐观移除)
- Modify: `src/routes/speakers/+page.svelte`(`{:else if item.kind === "identify"}` 卡片分支 + `doApplyIdentify`/`doRejectIdentify` 经 `act()` 包装)

- [ ] **Step 1**: people.ts:

```ts
export interface IdentifySuggestion {
  note_id: string; note_title: string; cluster: string; fingerprint: string;
  person_id: string | null; person_name: string; is_new: boolean;
  tier: string; quote: string; evidence_type: string; generated_at: string;
}
export const listIdentifySuggestions = () => invoke<IdentifySuggestion[]>("list_identify_suggestions");
export const applyIdentifySuggestion = (noteId: string, fingerprint: string) =>
  invoke<void>("apply_identify_suggestion", { noteId, fingerprint });
export const rejectIdentifySuggestion = (noteId: string, fingerprint: string) =>
  invoke<void>("reject_identify_suggestion", { noteId, fingerprint });
```

- [ ] **Step 2**: 卡片内容(与既有建议卡同构、粉彩风格随现有):标题「这可能是 {person_name}」+ tier 徽标(high 描边强调);正文:笔记标题 + R 号 + 证据引文(`「{quote}」`,evidence_type 中文化:自我介绍/称呼应答/主题线索);按钮:「就是 TA」(apply)/「不是」(reject);`is_new` 时标题改「新面孔:{person_name}?」且确认文案「建档并关联」。
- [ ] **Step 3**: 前端检查:`npm run check`(svelte-check)通过;如仓库有前端测试脚本按现状跑。提交 `feat(ui): 整理收件箱身份建议卡(确认即关联+回灌,拒绝即永久静默)`。

---

### Task 10: speaker_eval 分档评测扩展

**Files:**
- Modify: `src-tauri/src/bin/speaker_eval.rs`

- [ ] **Step 1**: `run` 增读每笔记 `identify.json`:truth 中 R 层条目若在 identify assignments 里有对应簇(按 fingerprint 无从对——truth 用 R 号,按 `cluster` 字段匹配且 `source_hash` 与现稿一致才算),按 tier 分桶另算三套 Metrics(High/Medium/整体),输出:

```
[identify] high:   标注 N 正确 X 误认 Y 未识别 Z  precision P% recall R%
[identify] medium: …
[identify] all:    …
```

  P2b 开闸验收线看 `[identify] high` 的 precision(spec:样本 ≥50 且误认 ≤1%)。
- [ ] **Step 2**: 纯函数测试:构造 identify.json fixture(High 命中、Medium 误认、过期 source_hash 被排除)断言分桶计数;e2e fixture 测试补 identify.json 路径。
- [ ] **Step 3**: `cargo test --bin speaker_eval` 全绿;提交 `feat(eval): identify 分档 precision/recall(P2b 开闸数据门)`。

---

### Task 11(可拆后续 PR): Agent 执行体

**Files:** `src-tauri/src/refine/agent.rs`(`AgentIdentifyExecutor` 仿 `AgentRelationExecutor` :434;identify 指令模板;沙箱白名单加只读 `get_aing_context` 已够用——识别输入由 Rust 侧打包进 prompt,Agent 不需新工具)、`src-tauri/src/lib.rs`(`identify_executor` 的 "agent" 分支)

- [ ] 指令模板:把 `IdentifyContext` 序列化 JSON 内嵌 prompt(Agent 不读库),要求输出与 HTTP 同 schema 的 JSON 到 stdout;解析与裁决完全复用。`configured_provider` 同款只允许 Claude/Gemini。测试仿 `refine_command_claude_has_strict_mcp_and_allowlist`。
- [ ] 若本任务拆出,`identify_executor` 的 agent 分支保持 bail,identify stage 记 "skipped"——HTTP 用户(当前默认)不受影响。

---

## 收尾核对

- [ ] `cd src-tauri && cargo test`(lib+bins)全绿;`cargo clippy --all-targets` 无新增告警;`npm run check` 通过
- [ ] 真机冒烟清单(PR 描述):① Aing 一场含自我介绍的多人笔记 → identify.json 生成,收件箱出现建议卡带引文;② 点「就是 TA」→ R 段落显示真名,日志出现 feedback 回灌;③ 点「不是」→ 卡片消失且重跑 Aing 不再出现;④ 新面孔确认 → 会议搭子出现新人,下一场同人命中;⑤ agent provider 用户:identify stage 显示 skipped,精修不受影响;⑥ MCP `identify_speakers` 返回裁决结果
- [ ] PR 描述注明:P2a 零自动写入;三处对 spec 的收窄(journal 内嵌/声学门简化/MCP 只读)及理由;Task 11 是否随本 PR
