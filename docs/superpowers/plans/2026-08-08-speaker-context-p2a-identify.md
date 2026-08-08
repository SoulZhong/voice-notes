# 说话人上下文推断 P2a(identify 只读期)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

rev2:消化 Codex 计划审查(22 P1 + 18 P2)。骨架级修正:① **identify 移到精修完成之后**运行(证据锚定最终正文,source_hash 不再被精修立即作废;spec 的"identify 在 llm 前"连同其收益一起推迟到 P2b 自动应用时重估);② **放弃 `RefineStages.identify` 字段**——P2a 状态唯一事实源是「identify.json 是否存在且新鲜」,消掉双事实源与 stage 落盘时序问题;③ **簇指纹取自最终 RefinedDoc**(按 R 分组 source_seqs),ClusterStat 仅供质心且**按信道分开导出**;④ 门禁复用 `refine_llm_ready` 全套(绝不绕过用户关闭精修的授权);⑤ apply 并发用「锁外 assign + 前后两段锁内校验/置位」+ 命令级串行门,不嵌套 NoteLock;⑥ 拒绝键 = 指纹+目标身份(拒绝"是张伟"不封杀该簇的其它候选)。

**Goal:** refine 管线在精修完成后新增 identify 推断(LLM 用会议文本推断「R 簇→真实身份」),经程序侧裁决后**只出建议卡,零自动写入**;人工确认走既有 assign+P1 回灌链路;identify.json 为 P2b 积累评测样本。

**Architecture:** recluster 导出按信道分组的簇质心;新模块 `refine/identify.rs`:输入打包(候选三路召回+轻量采样)、`IdentifyExecutor` trait(P2a 交付 HTTP 实现)、输出宽松解析、五道裁决、identify.json 读写。挂进 `spawn_refine` 尾部(精修成功后);手动重试 `identify_note` 复用 P1 `feedback::build_source_stats` 重建簇质心。IPC 三条 + MCP 只读工具;前端收件箱加 identify 卡。

**Tech Stack:** Rust(Tauri 2)+ SvelteKit;无新 crate(sha2/hex/serde 已有,bin 可直接依赖 sha2)。

**Spec:** `docs/superpowers/specs/2026-08-08-speaker-context-inference-design.md`(rev2)
**基线:** 分支 `feat/speaker-context-p2a` 叠在 `feat/speaker-context-p1`(PR #79)上。

## Global Constraints

- **零自动写入**;唯一落地路径 `apply_identify_suggestion`(人工)。
- **门禁**:identify 只在 `refine_llm_ready(&settings)` 为真(用户开启精修+HTTP 配置齐全)时运行;agent provider 或精修关闭 → 不跑、无痕。手动 `identify_note` 同一门禁。模型门禁(`vp.embedding_model != settings.speaker_model`)时:**声学召回路与声学门都关闭**,只走时近召回+文本档(声学 None,最高 Medium)。
- **状态模型**:无 RefineStages 改动。`identify.json` 携带生成时 `source_hash`;「新鲜」= 与当前稿 source_hash 相等;精修/编辑后旧文件即过期,由下一次 Aing 或手动 identify_note 覆盖。UI 事件:完成后 `emit("identify_done", note_id)`。
- **与 spec 的收窄(P2a 有效)**:拒绝/状态内嵌 identify.json(独立 journal 推迟 P2b);声学门用同信道裸余弦 ≥0.68(AS-Norm 推迟 P2b);MCP 只读返回最近结果+stale 标志;identify 时机在精修后(spec 管线图的"identify 在 llm 前"推迟 P2b 重估)。
- 裁决产物**不可变**:assignments 的 tier/evidence 生成后不再改;人工动作只改 `status` 字段(评测读原始 tier,不被人工决策污染)。
- 嵌入并发:手动路径复用 P1 `FEEDBACK_GATE`;管线路径天然在 `AING_GATE` 内。
- 新 MCP 工具:同步更新 server.rs 工具计数断言、**README 的 MCP 工具表**(`mcp_stdio` 测试校验 README 含全部注册工具)、`AGENT_TOOL_NAMES` **不动**(白名单计数断言不变)。
- 新 IPC:留意 `lib.rs` 两处解析 invoke_handler 源码的测试(~5868/~5990)。
- 前端:`tidyQueue.ts` 的 `itemIds` 等 **switch 全分支穷举**必须补 identify 支;`tidyQueue.test.ts` 及 `buildTidyQueue` 全部调用点同步更新;卡片文案接入现有 i18n 机制(看 `+page.svelte` 既有卡片的文案写法,同款处理;若现状即中文硬编码则跟随现状,不自创第二套)。
- 不跑全量 `cargo fmt`;每任务 `cargo test --lib <过滤>` 后提交;`tr!` 双语用于后端用户可见文案。

---

### Task 1: recluster 导出按信道分组的簇统计

**Files:**
- Modify: `src-tauri/src/refine/recluster.rs`(:61 签名、:140-204 输出段、既有单测)
- Modify: `src-tauri/src/refine/mod.rs`(调用点 :275 + 模块内所有直接调用 recluster/run_local 的测试,以编译错误为准)

**Interfaces:**

```rust
#[derive(Debug, Clone)]
pub struct ClusterStat {
    pub speaker: String,                          // "R1"(与 Assignment 同序同名)
    /// 按信道分组的单位质心:成员段先按 source 分组,各组内已归一嵌入求均值再归一。
    /// 跨信道混合质心没有声学意义(与库内按信道存质心同理),不导出。
    pub centroids: BTreeMap<String, Vec<f32>>,
    pub total_ms: u64,
    pub source_ms: BTreeMap<String, u64>,         // 信道 -> 时长
    /// AHC 后、传播前的核心成员(仅调试参考;身份指纹一律取最终 doc,见 Task 3)。
    pub core_seqs: Vec<u64>,
    /// 最佳库种子近邻:(person_id, name, cosine, adopted)。adopted=命名被采纳(>=0.68)。
    pub seed: Option<(String, String, f32, bool)>,
}

pub fn recluster(inputs: &[SegInput], embs: &[Option<Vec<f32>>], seeds: &[SeedCluster])
    -> (Vec<Assignment>, Vec<ClusterStat>);
```

- [ ] **Step 1: 实现**。在输出循环(:140-204)同步构建:`centroids` 按成员 `inputs[i].source` 分组,组内把(已归一的)嵌入求均值后再归一;`source_ms` 同组累计时长;`seed` 记最佳近邻并带 adopted 标记。无簇路径返回 `(assign, vec![])`。
- [ ] **Step 2: 调用点**:`mod.rs:275` 解构元组;fallback/None 分支给 `vec![]`;`run_local` 签名改 `-> anyhow::Result<(RefinedDoc, Vec<ClusterStat>)>`,`lib.rs::spawn_refine` 与 mod.rs 全部测试同步解构(测试数量以 `cargo check` 报错为准,不按行号清单)。
- [ ] **Step 3: 新增单测**:

```rust
#[test]
fn recluster_exports_per_source_centroids() {
    // 同簇两段,一 mic 一 system,嵌入向量不同方向:
    // centroids 必须两键、各自等于该信道段的单位向量,不得跨信道平均。
    let inputs = vec![seg_input(0, "mic", 0, 3000), seg_input(1, "system", 3000, 6000)];
    let embs = vec![Some(unit(0)), Some(unit(1))];
    let (_, stats) = recluster(&inputs, &embs, &[]);
    assert_eq!(stats.len(), 1);
    assert!((stats[0].centroids["mic"][0] - 1.0).abs() < 1e-5);
    assert!((stats[0].centroids["system"][1] - 1.0).abs() < 1e-5);
    assert_eq!(stats[0].source_ms["mic"], 3000);
}
```

(注意 AHC_THRESHOLD=0.68:正交向量不会被合并——构造时用**同段强制同簇**的场景需 sim>=0.68,可给两向量 0.9 相似再断言分组;实现本测试时以"两段进同簇"为前提调整向量,保持断言精神:**分信道质心不混合**。)
- [ ] **Step 4**: `cargo test --lib recluster` 全绿;提交 `feat(recluster): 导出分信道簇质心与种子近邻(adopted 标记)`。

---

### Task 2: identify 输入打包(候选召回 + 采样 + 指纹)

**Files:**
- Create: `src-tauri/src/refine/identify.rs`(`refine/mod.rs` 加 `pub mod identify;`)
- Modify: `src-tauri/src/feedback.rs`(`scope_key` 更名 `pub(crate) fn seq_fingerprint(&BTreeSet<u64>) -> String` 导出,内部调用点同步)

**Interfaces:**

```rust
pub const MAX_CANDIDATES: usize = 30;
pub const ACOUSTIC_TOP_K_PER_CLUSTER: usize = 5;   // 每簇 Top-K 后取并集(全局 Top-K 会被单簇占满)
pub const RECENT_TOP_K: usize = 10;
pub const SAMPLE_CHAR_BUDGET: usize = 6000;
pub const NAME_HIT_MIN_CHARS: usize = 2;           // 单字名不做 contains 召回(误命中海量)

#[derive(Serialize)]
pub struct Candidate { pub person_id: String, pub name: String }

#[derive(Serialize)]
pub struct ClusterBrief {
    pub speaker: String, pub fingerprint: String,
    pub total_ms: u64, pub dominant_source: String, pub mixed: bool, pub is_mic: bool,
    pub linked: Option<(String, String)>,          // 已采纳关联 (person_id, name)
}

#[derive(Serialize)]
pub struct SampledParagraph { pub paragraph_index: usize, pub speaker: String, pub text: String }

#[derive(Serialize)]
pub struct IdentifyContext {
    pub note_id: String,
    pub revision: u64, pub source_hash: String,
    pub clusters: Vec<ClusterBrief>,
    pub candidates: Vec<Candidate>,
    pub sampled: Vec<SampledParagraph>,
}

/// 指纹口径与 P1 feedback 账本一致(sha256(LE seq 序列) 前 8 字节 hex)。
pub fn cluster_fingerprint(seqs: &BTreeSet<u64>) -> String;   // 直接转发 feedback::seq_fingerprint

/// 从最终稿重建每个 R 簇的成员 seq 集(source_seqs 并集)。指纹一律以此为准——
/// recluster 的 core_seqs 不含无嵌入传播段,与最终稿不一致,绝不能当指纹。
pub fn cluster_members_from_doc(doc: &RefinedDoc) -> BTreeMap<String, BTreeSet<u64>>;

pub fn build_context(
    note_id: &str, doc: &RefinedDoc, stats: &[ClusterStat], vp: &Voiceprints,
    acoustic_enabled: bool,                        // 模型门禁不一致时 false:关闭声学召回路
) -> IdentifyContext;
```

- [ ] **Step 1: 失败测试**(内部 helper 带参数化 K,常量只是默认值,测试可传小 K):

```rust
#[test]
fn fingerprint_matches_feedback_and_members_come_from_doc() {
    // doc 两个 R 段落(R1: seqs [0,1] 含无嵌入传播段;R2: [5]):
    // cluster_members_from_doc 按 speaker 聚合;指纹与 feedback::seq_fingerprint 相等。
}
#[test]
fn candidates_per_cluster_topk_and_recent_union_dedup() {
    // helper: recall_candidates(stats, vp, acoustic_enabled, k_acoustic, k_recent)
    // 库 4 有名人 + 1 无名人:声学近邻按簇各取 top-k(不被单簇占满);
    // last_seen 最近补齐;无名人绝不入候选;acoustic_enabled=false 时只剩时近路。
}
#[test]
fn sampling_dedups_and_respects_budget() {
    // 同一段同时命中"开场+人名+自报句式"只入选一次;
    // 预算收紧后低优先级段被裁,总字符 <= budget(截断段计截断后长度)。
}
```

- [ ] **Step 2: 实现**。候选三路:① 声学(`acoustic_enabled` 时):每簇的 `centroids` 与每个有名人物同信道主质心裸余弦(维度不一致跳过、非有限值跳过),**每簇** top `ACOUSTIC_TOP_K_PER_CLUSTER` 取并集;② `last_seen` 降序 top `RECENT_TOP_K` 有名人物;③ `stat.seed` 中 **adopted==true** 的人。并集去重(经 redirects 归一)截 `MAX_CANDIDATES`。采样:优先级(每簇开场 2 段 → 含 ≥`NAME_HIT_MIN_CHARS` 字符候选人名的段 → 自报句式段(`我是/我叫/这边是/我这边是` 的 `contains`)→ 簇边界前后段),**BTreeSet<paragraph_index> 去重**,按优先级序填充,超预算段截前 200 字符且**按截断后长度计入预算**,预算耗尽即停。`ClusterBrief.linked` 只取 adopted 种子或段落既有 person_id。
- [ ] **Step 3**: `cargo test --lib identify::` 全绿;提交 `feat(identify): 输入打包——分簇声学召回、去重采样、指纹取自最终稿`。

---

### Task 3: 输出宽松解析与五道裁决

**Files:**
- Modify: `src-tauri/src/refine/identify.rs`

**Interfaces:**

```rust
pub struct RawAssignment {
    pub cluster: String,
    pub person_id: Option<String>, pub new_name: Option<String>,
    pub confidence: String,
    pub evidence: Vec<RawIdentifyEvidence>,        // {paragraph_index,start,end,quote,type}
}

/// 宽松解析:整体先解成 Value,assignments 数组逐条 from_value,坏条目跳过并计数。
pub fn parse_raw_identify(content: &str) -> anyhow::Result<(Vec<RawAssignment>, usize /*skipped*/)>;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier { High, Medium, Low }

pub struct Verdict {
    pub tier: Tier,
    pub acoustic: Option<(String, f32)>,
    pub reject_reason: Option<&'static str>,       // Some = 整条丢弃
}

pub fn adjudicate(
    raw: &RawAssignment, doc: &RefinedDoc, members: &BTreeMap<String, BTreeSet<u64>>,
    stats: &[ClusterStat], vp: &Voiceprints, acoustic_enabled: bool,
    taken: &BTreeMap<String, String>,
) -> Verdict;
```

- [ ] **Step 1: 失败测试**(逐关):

```rust
#[test] fn parse_skips_bad_entries_keeps_good() { /* 三条里一条类型错:返回 2 条 + skipped=1 */ }
#[test] fn evidence_quote_and_char_range_must_match() { /* 越界/不等 → reject "evidence-mismatch";CJK 按 scalar 数 */ }
#[test] fn self_intro_quote_must_contain_target_name() {
    // 铁证防偷梁换柱:quote "我是李雷" + person_id 指向张伟 → self_intro 证据无效
    //(降级为无 self_intro 处理);new_name 分支同理比对 new_name。
}
#[test] fn addressed_reply_needs_call_and_reply_pair() {
    // 需 >=2 条 evidence:至少一条落在目标簇段落(应答),至少一条落在其它簇段落且
    // 含目标名(称呼)。单条只算 role_topic 弱证据。
}
#[test] fn conflicts_and_acoustic_gate_cap_tier() { /* taken 冲突→Medium;混合簇/无同信道质心/门禁关→声学 None 上限 Medium */ }
#[test] fn structural_rejects() { /* person_id XOR new_name;悬空 person_id;未知 evidence type;new_name trim 后为空/超 32 字符 → reject */ }
```

- [ ] **Step 2: 实现**。裁决序:① 结构(XOR;resolve 悬空 → reject;`new_name` trim/去控制字符/≤32 字符;未知 evidence type 丢该条 evidence,全部丢光 → reject;confidence 非法按 "low");② 区间逐字校验(scalar 半开区间,任一 evidence 不过 → 整条 reject);③ **证据-身份一致性**:self_intro 的 quote 必须含目标名(person 现名或 new_name),否则该 evidence 降为 role_topic;addressed_reply 按上述配对规则,不满足降 role_topic;④ 冲突(taken 同 person 或同 cluster → cap Medium);⑤ 声学门:`acoustic_enabled && !brief.mixed && 目标人有 dominant_source 主质心 && 维度一致 && cos 有限 && cos>=0.68` → Some,否则 None;⑥ 分档:有效 self_intro+②③⑤过+无冲突 → High;有效 self_intro|addressed_reply → Medium;仅 role_topic/third_person_exclusion → Low。`new_name` 恒无声学 → 最高 Medium。
- [ ] **Step 3**: 全绿提交 `feat(identify): 宽松解析与五道裁决(含证据-身份一致性)`。

---

### Task 4: IdentifyExecutor + HTTP 实现

**Files:**
- Modify: `src-tauri/src/refine/identify.rs`(trait + SYSTEM_PROMPT 全文)
- Modify: `src-tauri/src/refine/llm.rs`(`HttpIdentifyExecutor`)
- Modify: `src-tauri/src/lib.rs`(`identify_executor(settings)` 分派,仿 `relation_executor` :2321;"agent"/其它 → Err,调用方按「跳过」处理)

- [ ] **Step 1**: trait:

```rust
pub trait IdentifyExecutor: Send + Sync {
    fn provider(&self) -> &str;
    fn model(&self) -> &str;
    fn infer(&self, ctx: &IdentifyContext, log: Option<&crate::ailog::Ctx>)
        -> anyhow::Result<(Vec<RawAssignment>, usize)>;   // parse_raw_identify 的产物
}
```

- [ ] **Step 2**: `IDENTIFY_SYSTEM_PROMPT` 全文(单行中文常量,必须全含):角色=会议说话人身份推断器;输入字段释义(clusters/candidates/sampled,is_mic 簇大概率是「我」);任务=只为无名或存疑簇推断;四种证据类型定义,addressed_reply 必须给"称呼段+应答段"两条证据;**只允许逐字存在的证据**,start/end 为 Unicode scalar 半开区间、quote 逐字符相等;self_intro 的 quote 必须包含所指认的名字;优先从 candidates 选(给 person_id),候选外允许 new_name 但必须有 self_intro 级证据;输出 JSON schema(与 RawAssignment 字段一一对应,给完整示例);无可靠推断输出 `{"assignments":[]}`;禁止为已明确关联且无矛盾的簇输出条目。
- [ ] **Step 3**: `HttpIdentifyExecutor`(仿 `HttpRelationExecutor`):单请求,`temperature 0.1`+`response_format json_object`+`apply_thinking_off`,超时 `REQ_TIMEOUT_S`;user 内容 = `serde_json::to_string(ctx)`;ailog `kind:"identify"` 全量记录;响应经 `parse_raw_identify`。mock 测试(复用 `mock_server_capturing`):请求体含 `"clusters"` 与候选名;响应解析正确。
- [ ] **Step 4**: 全绿提交 `feat(identify): IdentifyExecutor 与 HTTP 实现`。

---

### Task 5: identify.json 读写 + 管线挂钩 + identify_note

**Files:**
- Modify: `src-tauri/src/refine/identify.rs`(IdentifyDoc/run_identify/load/save)
- Modify: `src-tauri/src/lib.rs`(`spawn_refine` 尾部挂钩;`identify_note` 命令;注册)

**Interfaces:**

```rust
pub const IDENTIFY_FILE: &str = "identify.json";
pub const IDENTIFY_SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct IdentifyDoc {
    pub schema_version: u32,
    pub generated_at: String, pub provider: String, pub model: String,
    pub revision: u64, pub source_hash: String,
    pub assignments: Vec<IdentifyAssignment>,
    /// 拒绝记录跨代保留:key=(fingerprint, 目标键)。目标键=resolve 后 person_id 或
    /// "name:<new_name>"。拒绝"是张伟"不封杀该簇其它候选。
    #[serde(default)]
    pub rejected: BTreeMap<String, String>,        // "fp|target" -> decided_at
}

#[derive(Serialize, Deserialize)]
pub struct IdentifyAssignment {
    pub fingerprint: String, pub cluster: String,
    pub person_id: Option<String>, pub new_name: Option<String>,
    pub tier: Tier, pub llm_confidence: String,
    pub acoustic: Option<(String, f32)>,
    pub evidence: Vec<StoredEvidence>,
    pub status: String,                            // suggested|applied|rejected
    #[serde(default)] pub decided_at: Option<String>,
}

pub fn run_identify(...上下文同 rev1,增 members 参数...) -> anyhow::Result<IdentifyDoc>;
pub fn load_identify(note_dir: &Path) -> Option<IdentifyDoc>;
pub fn save_identify(note_dir: &Path, doc: &IdentifyDoc) -> anyhow::Result<()>;  // tmp+rename
```

- [ ] **Step 1: run_identify**:build_context → infer → 逐条 adjudicate(taken 随过程累积)→ 过滤:reject 丢弃(eprintln);Low 丢弃(ailog);目标命中 `rejected` 名单(fp|target)不再生成;已 linked 且无矛盾的簇不生成 → 新 IdentifyDoc(**继承旧稿 rejected 全表**;旧稿 assignments 里 status==applied 的按 fp 保留原条不覆盖)。单测:rejected 继承并拦截同目标再建议、不同目标放行;applied 保留。
- [ ] **Step 2: spawn_refine 挂钩**(HTTP 分支 `run_llm` 成功返回之后、`report("llm",..)` 附近):

```rust
// identify(P2a 只读):精修定稿后推断,证据锚定最终正文。失败只留日志。
if let Ok(executor) = identify_executor(&s) {
    let r = (|| -> anyhow::Result<()> {
        let vp = open_voiceprint_store(&app).map_err(anyhow::Error::msg)?.load();
        let acoustic_enabled = vp.embedding_model == s.speaker_model;
        let members = refine::identify::cluster_members_from_doc(&doc);
        let now = chrono::Local::now().to_rfc3339();
        let idoc = refine::identify::run_identify(
            &note_dir, &note_id, &doc, &cluster_stats, &members, &vp,
            acoustic_enabled, executor.as_ref(), ailog_ctx.as_ref(), &now,
        )?;
        refine::identify::save_identify(&note_dir, &idoc)?;
        let _ = app.emit("identify_done", &note_id);
        Ok(())
    })();
    if let Err(e) = r { eprintln!("identify 失败(不影响精修结果): {e}"); }
}
```

  前置:`identify_executor` 内部先查 `refine_llm_ready(&s)`,不 ready 返回 Err(挂钩处静默跳过)——**绝不绕过用户关闭精修的授权**。`doc` 用 run_llm 之后的内存稿(与盘上一致);`cluster_stats` 来自 run_local 返回元组。agent 分支不挂(执行体 Err 即跳过)。
- [ ] **Step 3: identify_note 命令**(手动触发/重试):

```rust
#[tauri::command]
async fn identify_note(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    if app.state::<lifecycle::LifecycleHandle>().is_refining(&id) { return Err(tr!("该笔记正在 Aing 中","This note is being refined")); }
    reject_if_active(&state, &id)?;
    // spawn_blocking 内(持 FEEDBACK_GATE 全程——嵌入与 track_pcm 竞争都收敛于此):
    //   settings 门禁 → identify_executor;
    //   load_refined 现稿 → cluster_members_from_doc;
    //   簇质心重建:NoteStore::load 取 segments,对每簇成员 seq 调 P1 的
    //   feedback::build_source_stats(SegFilter::Seqs 语义的筛选自己做:按 seq 集过滤段),
    //   得到该簇按信道的 SourceStat.centroid → 组装 ClusterStat(seed 置 None,
    //   dominant/mixed 由 source_ms 算);嵌入器临时新建 SherpaEmbedder;
    //   run_identify + save + emit("identify_done")。
}
```

  `build_source_stats` 签名已可复用(`segs: &[&SegmentRecord]` 直接传该簇成员段)。注册 invoke_handler,过两处源码解析测试。
- [ ] **Step 4**: 单测(mock executor)+ `cargo test --lib` 全绿;提交 `feat(identify): identify.json 生成/继承规则、管线尾部挂钩与手动重试`。

---

### Task 6: 建议动作 IPC(list / apply / reject)

**Files:**
- Modify: `src-tauri/src/lib.rs`、`src-tauri/src/ipc.rs`、`src-tauri/src/store/voiceprints.rs`

**Interfaces:**

```rust
// voiceprints.rs
/// 建空人物(P2a 新面孔确认用):VP_LOCK 内分配 P<next_person>;空名报错;
/// 质心为空——由确认后的 P1 feedback 回灌自然长出。
pub fn create_person(&self, name: &str, now: &str) -> anyhow::Result<String>;
/// 补偿:仅当此人仍为空质心空样本时删除(apply 半途失败的孤儿清理)。
pub fn delete_person_if_empty(&self, id: &str) -> anyhow::Result<bool>;

// ipc.rs — IdentifySuggestion 同 rev1(字段:note_id/note_title/cluster/fingerprint/
// person_id/person_name/is_new/tier/quote/evidence_type/generated_at)

// lib.rs
#[tauri::command] fn list_identify_suggestions(app: AppHandle) -> Result<Vec<ipc::IdentifySuggestion>, String>
#[tauri::command] async fn apply_identify_suggestion(app: AppHandle, note_id: String, fingerprint: String) -> Result<(), String>
#[tauri::command] fn reject_identify_suggestion(app: AppHandle, note_id: String, fingerprint: String) -> Result<(), String>
```

- [ ] **Step 1**: `create_person`/`delete_person_if_empty` + 单测(递增 id、空名 Err、非空人不删)。
- [ ] **Step 2**: `list_identify_suggestions`:NoteStore::list 遍历,load_identify,收 `status=="suggested"` 且 ① `source_hash` == 当前稿(`load_refined_for_display` 后 `store::source_hash`)② 指纹仍与现稿某簇成员集相符 ③ 该簇**当前未关联**其它人物(用户已手动关联的不再打扰)④ person_id resolve 存在;`generated_at` 降序 cap 50。
- [ ] **Step 3**: **apply 并发协议**(不嵌套 NoteLock,命令级串行):

```rust
static IDENTIFY_ACT_GATE: Mutex<()> = Mutex::new(());   // 双击/并发确认拒绝的 UI 竞争收敛
// spawn_blocking 内,持 IDENTIFY_ACT_GATE:
// ① 读 identify.json:找 fingerprint 且 status=suggested,否则 tr!("建议已失效");
//    复核指纹仍匹配现稿(cluster_members_from_doc 重算),不匹配 → 置 status=rejected(过期)并报错;
// ② is_new:vp.create_person(name)?;
// ③ 调 assign_refined_person 的内部逻辑(与既有命令同函数复用,自取 NoteLock)——
//    此刻不持任何笔记锁,无嵌套;P1 的 spawn_feedback 挂钩自动跟随(prior=None → Reinforce);
// ④ 失败补偿:③ Err 且 is_new → vp.delete_person_if_empty(new_id)(best-effort),原样返回 Err;
// ⑤ 成功:重读 identify.json 置该条 status=applied+decided_at 原子回写(读改写在 ①-⑤ 的
//    同一 IDENTIFY_ACT_GATE 内,list 只读不受影响;管线重跑按 fp 保留 applied,见 Task 5)。
```

  reject:同门内读改写:该条 `status=rejected+decided_at`,并写入 `rejected["fp|target"]`。
- [ ] **Step 4**: 复用测试:apply 的 ①⑤ 逻辑抽 `mark_applied(idoc, fp) -> Result<()>`、`mark_rejected(idoc, fp, target)` 纯函数放 identify.rs 单测;命令壳无自动化测试(与仓库现状一致),真机冒烟覆盖。
- [ ] **Step 5**: 注册 + `cargo test --lib` 全绿;提交 `feat(identify): 建议列表/确认/拒绝——锁外 assign、新人补偿、拒绝键含目标身份`。

---

### Task 7: MCP 只读工具 `identify_speakers`

**Files:**
- Modify: `src-tauri/src/store/refined.rs`(`AnchoredRefinedDir` 增 `pub(crate) fn load_identify(&self) -> Option<identify JSON Value>`——锚定目录内读 `identify.json`,与 `load_current` 同款防护;identify.rs 的 load 也可换用它)
- Modify: `src-tauri/src/mcp/tools.rs` + `src-tauri/src/mcp/server.rs`
- Modify: `README.md`(MCP 工具表补一行)

- [ ] **Step 1**: tools.rs `identify_speakers(roots, note_id)`:AnchoredRefinedDir::open → load_identify;同时算当前稿 source_hash,返回附 `"stale": bool`(与 UI 口径一致,消除两入口矛盾);无文件 → bail 提示先运行 Aing。
- [ ] **Step 2**: server.rs 工具方法(仿 get_aing_context);`AGENT_TOOL_NAMES` **不加**;更新全量工具计数断言(以实际断言写法为准,agent 白名单断言不动);README 工具表加行。
- [ ] **Step 3**: `cargo test --lib mcp && cargo test --test mcp_stdio` 全绿;提交 `feat(mcp): identify_speakers 只读工具(带 stale 标志)`。

---

### Task 8: 前端收件箱 identify 卡

**Files:**
- Modify: `src/lib/people.ts`(类型 + 三绑定,同 rev1)
- Modify: `src/lib/tidyQueue.ts`(`TidyItem` 加 identify 支;`tidyItemKey` → `i:<note_id>:<fingerprint>`;**`itemIds` 等全部按 kind 分支的函数穷举补齐**——先 `rg "kind ===" src/lib src/routes` 列全再改;`buildTidyQueue` 增参,排序:回执后、合并建议前)
- Modify: `src/lib/tidyQueue.test.ts`(如存在:全部 buildTidyQueue 调用补参 + identify 用例:出现在正确位置、key 格式、dismissed 过滤)
- Modify: `src/lib/tidy.svelte.ts`(state.identify + doRefresh 并行拉取 + `identify_done` 事件监听 refresh;忽略动作走 `rejectIdentifySuggestion`(后端真值),本地乐观移除,不写 dismiss_tidy_item)
- Modify: `src/routes/speakers/+page.svelte`(卡片分支 + `doApplyIdentify`/`doRejectIdentify` 经 `act()`;文案跟随本文件既有卡片的 i18n 处理方式)

- [ ] **Step 1**: 卡片:标题「这可能是 {person_name}」(is_new:「新面孔:{person_name}?」),tier=high 加强调徽标;正文 = 笔记标题 + {cluster} + 证据引文「{quote}」+ 证据类型中文标签;按钮「就是 TA」/「不是」(is_new 确认文案「建档并关联」)。
- [ ] **Step 2**: `npm run check` 通过 + 前端测试(若有 tidyQueue.test.ts)全绿;提交 `feat(ui): 收件箱身份建议卡`。

---

### Task 9: speaker_eval identify 分档评测

**Files:**
- Modify: `src-tauri/src/bin/speaker_eval.rs`(bin 可直接 `use sha2`,与库无耦合地复刻指纹:sha256(LE u64 序列) 前 8 字节 hex——加一条与固定向量比对的单测锁死算法,防与 feedback::seq_fingerprint 漂移)

- [ ] **Step 1**: run 增读每笔记 identify.json:R 层 truth 条目按「当前稿该 R 簇成员集指纹」映射到 assignments(**按 fingerprint 匹配,不按 R 号**——重聚类会重编号;**不检查 source_hash**——正文编辑不改变身份真值);分三桶输出:

```
[identify] high:        标注 N 正确 X 误认 Y 未识别 Z  precision P% recall R%
[identify] high+medium: …
[identify] all:         …
```

  评测用生成时 tier 与推断目标(status 无关——人工决策不污染模型原始准确率);`new_name` 条目按名字比对并在输出尾注计数(同名风险自担)。P2b 验收线看 `[identify] high` 的 precision。
- [ ] **Step 2**: fixture 测试:识别 fingerprint 匹配(R 号变化仍命中)、tier 分桶、指纹算法固定向量。
- [ ] **Step 3**: `cargo test --bin speaker_eval` 全绿;提交 `feat(eval): identify 分档评测(指纹匹配,人工决策不污染样本)`。

---

### Task 10(独立后续 PR,本计划仅立项): Agent 执行体

不解析 CLI stdout(与现有 Agent 架构冲突——各家输出格式不可信)。设计方向:仿 relation 路径,Agent 在隔离 scratch 目录经文件交换(prompt 内嵌 IdentifyContext JSON,要求把结果 JSON 写入指定路径),Rust 侧读文件解析+裁决复用。`configured_provider` 同款仅 Claude/Gemini。**详细设计与实现在 P2a 落地并跑通评测后另立计划**;在此之前 agent provider 用户 identify 不运行(无痕跳过)。

---

## 收尾核对

- [ ] `cd src-tauri && cargo test`(lib+bins+mcp_stdio)全绿;`cargo clippy --all-targets` 无新增告警;`npm run check` 通过
- [ ] 真机冒烟(PR 描述):① Aing 一场含自我介绍的多人笔记 → identify.json 生成(source_hash=精修后稿),收件箱出卡带引文;② 「就是 TA」→ R 段落显真名 + 日志 feedback 回灌;③ 「不是」→ 卡片消失,重跑 Aing 同目标不再出现、换目标可再建议;④ 新面孔确认 → 搭子出新人,后续回灌日志可见;⑤ 关闭精修/agent provider → 无 identify 痕迹;⑥ MCP identify_speakers 带 stale 标志;⑦ 手动 identify_note 在未精修笔记上正常工作
- [ ] PR 描述注明:P2a 零自动写入;对 spec 的四处收窄(时机移到精修后/无 stage 字段/journal 内嵌/声学门简化)及理由;评测积累到 ≥20 场、high 档 ≥50 样本误认 ≤1% 才启动 P2b
