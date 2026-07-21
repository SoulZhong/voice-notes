//! rmcp stdio MCP 服务。查询工具直读数据文件;UDS 工具(状态/实时/控制)经 bridge 连 GUI。

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Deserializer};
use std::borrow::Cow;

use super::tools;

const AGENT_TOOL_NAMES: &[&str] = &[
    "get_note",
    "apply_refined_texts",
    "get_aing_context",
    "apply_aing_graph",
];

#[derive(Clone, Default)]
pub struct VnMcp {
    agent_tools_only: bool,
}

impl VnMcp {
    fn from_environment() -> Self {
        Self {
            agent_tools_only: std::env::var_os("VN_MCP_AGENT_MODE").as_deref()
                == Some(std::ffi::OsStr::new("1")),
        }
    }

    fn agent_tool_router() -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        let mut router = Self::tool_router();
        let disabled = router
            .list_all()
            .into_iter()
            .filter(|tool| !AGENT_TOOL_NAMES.contains(&tool.name.as_ref()))
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        for name in disabled {
            router.disable_route(name);
        }
        router
    }

    fn active_tool_router(&self) -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        if self.agent_tools_only {
            Self::agent_tool_router()
        } else {
            Self::tool_router()
        }
    }
}

fn ok_json(v: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(v.to_string())])
}

fn err_text(msg: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg)])
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ListNotesParams {
    /// 返回条数,默认 20,最大 100
    pub limit: Option<usize>,
    /// 跳过条数(翻页),默认 0
    pub offset: Option<usize>,
    /// 起始时间过滤,RFC3339 前缀,如 "2026-07-01"
    pub from: Option<String>,
    /// 截止时间过滤,RFC3339 前缀
    pub to: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// 检索词(在所有笔记的逐句文本里做大小写不敏感子串匹配)
    pub query: String,
    /// 最多返回命中数,默认 20,最大 100
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetNoteParams {
    /// 笔记 id(来自 list_notes / search_notes)
    pub note_id: String,
    /// "segments"(默认,逐句结构化) | "markdown" | "text"
    pub format: Option<String>,
    /// 有修订稿时优先返回修订稿,默认 true
    pub prefer_refined: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetAingContextParams {
    /// 笔记 id；返回的是文本提交后的当前 Aing 图谱上下文。
    pub note_id: String,
}

#[derive(Debug, Clone)]
pub struct ApplyAingGraphParams {
    pub note_id: String,
    pub entities: Vec<crate::store::Entity>,
    pub relations: Vec<crate::store::RelationFact>,
    pub contract_version: u32,
    pub model: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ApplyAingGraphWire {
    note_id: String,
    entities: Vec<AingEntityWire>,
    relations: Vec<AingRelationWire>,
    contract_version: u32,
    model: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AingEntityWire {
    id: String,
    kind: String,
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AingPredicateWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AingEvidenceWire {
    #[serde(default)]
    id: String,
    paragraph_index: usize,
    start: usize,
    end: usize,
    quote: String,
    source_seqs: Vec<u64>,
    source_hash: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AingRelationWire {
    #[serde(default)]
    id: String,
    subject: String,
    predicate: AingPredicateWire,
    object: String,
    #[serde(default)]
    subject_mentions: Vec<String>,
    #[serde(default)]
    object_mentions: Vec<String>,
    confidence: f64,
    #[serde(default)]
    valid_from: Option<String>,
    #[serde(default)]
    valid_to: Option<String>,
    evidence: Vec<AingEvidenceWire>,
}

impl<'de> Deserialize<'de> for ApplyAingGraphParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ApplyAingGraphWire::deserialize(deserializer)?;
        Ok(Self {
            note_id: wire.note_id,
            entities: wire
                .entities
                .into_iter()
                .map(|entity| crate::store::Entity {
                    id: entity.id,
                    kind: entity.kind,
                    name: entity.name,
                    aliases: entity.aliases,
                })
                .collect(),
            relations: wire
                .relations
                .into_iter()
                .map(|relation| crate::store::RelationFact {
                    id: relation.id,
                    subject: relation.subject,
                    predicate: crate::store::RelationPredicate {
                        kind: relation.predicate.kind,
                        label: relation.predicate.label,
                    },
                    object: relation.object,
                    subject_mentions: relation.subject_mentions,
                    object_mentions: relation.object_mentions,
                    confidence: relation.confidence,
                    valid_from: relation.valid_from,
                    valid_to: relation.valid_to,
                    evidence: relation
                        .evidence
                        .into_iter()
                        .map(|evidence| crate::store::RelationEvidence {
                            id: evidence.id,
                            paragraph_index: evidence.paragraph_index,
                            start: evidence.start,
                            end: evidence.end,
                            quote: evidence.quote,
                            source_seqs: evidence.source_seqs,
                            source_hash: evidence.source_hash,
                        })
                        .collect(),
                })
                .collect(),
            contract_version: wire.contract_version,
            model: wire.model,
        })
    }
}

impl schemars::JsonSchema for ApplyAingGraphParams {
    fn schema_name() -> Cow<'static, str> {
        "ApplyAingGraphParams".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::ApplyAingGraphParams").into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        ApplyAingGraphWire::json_schema(generator)
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RefinedTextUpdate {
    /// 段落下标(get_note segments 格式返回的 paragraphs 数组下标,0 起)
    pub index: usize,
    /// 该段修订后的完整文本(整段替换,不是 diff)
    pub text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ApplyRefinedParams {
    /// 笔记 id(须已有修订稿,即 get_note 返回 refined=true)
    pub note_id: String,
    /// 有改动的段落集合;确认全文无需修订时传空数组(同样标记 Aing 完成)
    pub updates: Vec<RefinedTextUpdate>,
    /// 执行 Aing 的模型名(记入修订稿元数据),如 "claude-sonnet-4-5"
    pub model: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct LiveParams {
    /// 返回最近几句,默认 50,最大 500
    pub tail: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct StartParams {
    /// 可选:本场录制的标题
    pub title: Option<String>,
}

/// UDS 桥的阻塞 IO 包一层 spawn_blocking,避免占用 tokio 工作线程。
async fn bridge_call(op: &'static str, extra: serde_json::Value) -> CallToolResult {
    match tokio::task::spawn_blocking(move || super::bridge::call(op, extra)).await {
        Ok(Ok(data)) => ok_json(data),
        Ok(Err(msg)) => err_text(msg),
        Err(e) => err_text(format!("内部错误: {e}")),
    }
}

#[tool_router]
impl VnMcp {
    #[tool(
        description = "列出会议笔记(倒序分页;from/to 可按时间过滤)。返回 id/标题/开始时间/时长/状态。"
    )]
    async fn list_notes(
        &self,
        Parameters(p): Parameters<ListNotesParams>,
    ) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        Ok(ok_json(tools::list_notes(
            &roots,
            p.limit.unwrap_or(20),
            p.offset.unwrap_or(0),
            p.from.as_deref(),
            p.to.as_deref(),
        )))
    }

    #[tool(
        description = "全文检索所有会议笔记的转写内容,返回命中句与上下文各一句、说话人与时间戳。"
    )]
    async fn search_notes(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        Ok(ok_json(tools::search_notes(
            &roots,
            &p.query,
            p.limit.unwrap_or(20),
        )))
    }

    #[tool(
        description = "读取一场会议笔记全文。segments 给逐句结构化(含说话人/时间戳),markdown/text 给渲染稿;有 AI 修订稿时默认优先修订稿。"
    )]
    async fn get_note(
        &self,
        Parameters(p): Parameters<GetNoteParams>,
    ) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        match tools::get_note(
            &roots,
            &p.note_id,
            p.format.as_deref().unwrap_or("segments"),
            p.prefer_refined.unwrap_or(true),
        ) {
            Ok(v) => Ok(ok_json(v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    #[tool(
        description = "把 Aing 修订写回笔记的修订稿:按段落下标整段替换文本(只能改文本,说话人/时间戳/段落数不可变)。\
                          流程:先 get_note(format=segments) 拿 paragraphs,修订后只提交有改动的段落;全文无需修订则传空 updates。"
    )]
    async fn apply_refined_texts(
        &self,
        Parameters(p): Parameters<ApplyRefinedParams>,
    ) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        let updates: Vec<(usize, String)> =
            p.updates.into_iter().map(|u| (u.index, u.text)).collect();
        let model = p
            .model
            .as_deref()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or("agent");
        match tools::apply_refined_texts(&roots, &p.note_id, &updates, model) {
            Ok(v) => Ok(ok_json(v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    #[tool(
        description = "读取文本提交后的当前 Aing 图谱上下文。返回 final paragraphs/source_seqs、局部实体、live mentions、核心 predicate、契约版本与服务端当前 source hash；只读。"
    )]
    async fn get_aing_context(
        &self,
        Parameters(p): Parameters<GetAingContextParams>,
    ) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        match tools::get_aing_context(&roots, &p.note_id) {
            Ok(value) => Ok(ok_json(value)),
            Err(error) => Ok(err_text(error.to_string())),
        }
    }

    #[tool(
        description = "提交当前笔记的模型实体与证据关系。必须先写回文本、再 get_aing_context；服务端在笔记锁内重载并重算所有实体/mention/evidence/relation ID 与 source hash。不能提交人工裁决、registry 或 operation。"
    )]
    async fn apply_aing_graph(
        &self,
        Parameters(p): Parameters<ApplyAingGraphParams>,
    ) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        match tools::apply_aing_graph(&roots, p) {
            Ok(value) => Ok(ok_json(value)),
            Err(error) => Ok(err_text(error.to_string())),
        }
    }

    #[tool(
        description = "列出全局声纹库中的说话人(跨会议一致的人物编号/名字/累计说话时长/出现的笔记数)。"
    )]
    async fn list_speakers(&self) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        Ok(ok_json(tools::list_speakers(&roots)))
    }

    #[tool(
        description = "查询录制状态(idle/recording/paused)、当前笔记 id 与已录时长。需要 voice-notes 应用正在运行。"
    )]
    async fn recording_status(&self) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("status", serde_json::json!({})).await)
    }

    #[tool(
        description = "获取正在录制会话的实时转写(最近 N 句,含说话人与时间戳)。需要应用正在运行且正在录制。"
    )]
    async fn get_live_transcript(
        &self,
        Parameters(p): Parameters<LiveParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("live", serde_json::json!({ "tail": p.tail })).await)
    }

    #[tool(
        description = "开始录制一场会议(可选标题)。需要应用正在运行,且用户已在左侧 AI 页开启「允许 AI 控制录制」。"
    )]
    async fn start_recording(
        &self,
        Parameters(p): Parameters<StartParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("start", serde_json::json!({ "title": p.title })).await)
    }

    #[tool(
        description = "停止当前录制并返回笔记 id。需要应用运行 + 用户开启「允许 AI 控制录制」。"
    )]
    async fn stop_recording(&self) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("stop", serde_json::json!({})).await)
    }

    #[tool(description = "暂停当前录制。需要应用运行 + 用户开启「允许 AI 控制录制」。")]
    async fn pause_recording(&self) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("pause", serde_json::json!({})).await)
    }

    #[tool(description = "恢复已暂停的录制。需要应用运行 + 用户开启「允许 AI 控制录制」。")]
    async fn resume_recording(&self) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("resume", serde_json::json!({})).await)
    }
}

#[tool_handler(router = self.active_tool_router())]
impl ServerHandler for VnMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "voice-notes 本地会议笔记。查询类工具(list/search/get/speakers)随时可用;\
             录制状态与控制类工具需要 voice-notes 应用正在运行。所有数据均在本机。",
        )
    }
}

/// `/ai` 页展示用的静态能力清单:MCP 十三工具 + CLI 命令一行用法。与上方 `#[tool]`
/// 定义相邻放置,便于人工同步;`catalog_matches_tool_router` 测试做防漂移守卫。
/// gate:`none` 随时可用,`app` 需 App 运行,`control` 还需用户开启「允许 AI 控制录制」。
pub fn catalog() -> serde_json::Value {
    let tools: &[(&str, &str, &str)] = &[
        ("list_notes", "列出会议笔记(倒序分页;from/to 可按时间过滤)。返回 id/标题/开始时间/时长/状态。", "none"),
        ("search_notes", "全文检索所有会议笔记的转写内容,返回命中句与上下文各一句、说话人与时间戳。", "none"),
        (
            "get_note",
            "读取一场会议笔记全文。segments 给逐句结构化(含说话人/时间戳),markdown/text 给渲染稿;有 AI 修订稿时默认优先修订稿。",
            "none",
        ),
        (
            "apply_refined_texts",
            "把 Aing 修订写回笔记的修订稿:按段落下标整段替换文本(只能改文本,说话人/时间戳/段落数不可变)。",
            "none",
        ),
        (
            "get_aing_context",
            "读取文本提交后的当前 Aing 图谱上下文(final paragraphs、live mentions、核心 predicates、契约版本与 source hash)。",
            "none",
        ),
        (
            "apply_aing_graph",
            "提交当前笔记的模型实体与证据关系；服务端在笔记锁内重载、校验并重算全部稳定 ID。",
            "none",
        ),
        ("list_speakers", "列出全局声纹库中的说话人(跨会议一致的人物编号/名字/累计说话时长/出现的笔记数)。", "none"),
        ("recording_status", "查询录制状态(idle/recording/paused)、当前笔记 id 与已录时长。", "app"),
        ("get_live_transcript", "获取正在录制会话的实时转写(最近 N 句,含说话人与时间戳)。", "app"),
        ("start_recording", "开始录制一场会议(可选标题)。", "control"),
        ("stop_recording", "停止当前录制并返回笔记 id。", "control"),
        ("pause_recording", "暂停当前录制。", "control"),
        ("resume_recording", "恢复已暂停的录制。", "control"),
    ];
    let cli: &[(&str, &str)] = &[
        ("voice-notes notes list [--limit N] [--offset N] [--from 2026-07-01] [--to 2026-07-08] [--json]", "列出会议笔记"),
        ("voice-notes notes search \"关键词\" [--limit N] [--json]", "全文检索会议笔记"),
        (
            "voice-notes notes get <note-id> [--format md|txt|json] [--json] [--raw]",
            "读取一场会议笔记全文(默认 md;--json 是 --format json 的别名;--raw 取原始逐字稿)",
        ),
        ("voice-notes notes retitle [--dry-run] [--agent claude]", "AI 为默认标题的会议批量生成主题标题(手动命名不动)"),
        ("voice-notes speakers list [--json]", "列出全局声纹库中的说话人"),
        ("voice-notes record status", "查询录制状态(需应用运行)"),
        ("voice-notes record start [--title \"评审会\"]", "开始录制(需应用运行 + 允许 AI 控制)"),
        ("voice-notes record stop", "停止录制并返回笔记 id(需应用运行 + 允许 AI 控制)"),
        ("voice-notes record pause", "暂停录制(需应用运行 + 允许 AI 控制)"),
        ("voice-notes record resume", "恢复已暂停的录制(需应用运行 + 允许 AI 控制)"),
        ("voice-notes record live [--tail N]", "获取实时转写(需应用运行)"),
        ("voice-notes ailog list [--limit N] [--kind K] [--note ID] [--json]", "查询 AI 调用日志(Aing/标题的请求与响应全量留痕)"),
        ("voice-notes ailog export [--out PATH]", "AI 调用日志全量导出为 JSONL"),
    ];
    serde_json::json!({
        "tools": tools.iter().map(|(name, desc, gate)| serde_json::json!({ "name": name, "desc": desc, "gate": gate })).collect::<Vec<_>>(),
        "cli": cli.iter().map(|(cmd, desc)| serde_json::json!({ "cmd": cmd, "desc": desc })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    /// 防漂移守卫:catalog 的工具名集合必须与 rmcp `#[tool_router]` 生成的
    /// 路由(`VnMcp::tool_router().list_all()`)完全一致——新增/改名/删除工具
    /// 时若忘了同步 catalog,这个测试会先炸。
    #[test]
    fn catalog_matches_tool_router() {
        let router_names: std::collections::BTreeSet<String> = VnMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        let cat = catalog();
        let cat_tools = cat["tools"].as_array().expect("tools 是数组");
        let cat_names: std::collections::BTreeSet<String> = cat_tools
            .iter()
            .map(|t| t["name"].as_str().expect("name 是字符串").to_string())
            .collect();
        assert_eq!(cat_names.len(), cat_tools.len(), "catalog 不应有重复工具名");
        assert_eq!(
            cat_names, router_names,
            "catalog.tools 必须与 tool_router 注册的工具名完全一致"
        );
        assert_eq!(
            cat_names.len(),
            13,
            "11 个既有工具 + get_aing_context/apply_aing_graph"
        );
        assert!(cat_names.contains("get_aing_context"));
        assert!(cat_names.contains("apply_aing_graph"));
    }

    #[test]
    fn agent_router_exposes_exactly_the_four_refine_tools() {
        let names: std::collections::BTreeSet<String> = VnMcp::agent_tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(
            names,
            [
                "apply_aing_graph",
                "apply_refined_texts",
                "get_aing_context",
                "get_note",
            ]
            .into_iter()
            .map(str::to_string)
            .collect()
        );
    }

    #[test]
    fn graph_tool_payload_rejects_unknown_decision_fields_at_every_level() {
        let base = serde_json::json!({
            "note_id": "note-1",
            "contract_version": crate::store::aing_graph::GRAPH_CONTRACT_VERSION,
            "model": "agent-model",
            "entities": [{
                "id": "incoming-person",
                "kind": "person",
                "name": "张三",
                "aliases": []
            }],
            "relations": []
        });
        assert!(serde_json::from_value::<ApplyAingGraphParams>(base.clone()).is_ok());

        for field in [
            "decision",
            "registry",
            "operation",
            "human_decision",
            "overrides",
        ] {
            let mut attempted = base.clone();
            attempted[field] = serde_json::json!({ "action": "confirm" });
            assert!(
                serde_json::from_value::<ApplyAingGraphParams>(attempted).is_err(),
                "顶层人工裁决字段 {field} 必须拒绝"
            );
        }

        let mut nested = base;
        nested["entities"][0]["registry"] = serde_json::json!({ "global_id": "P1" });
        assert!(
            serde_json::from_value::<ApplyAingGraphParams>(nested).is_err(),
            "嵌套 registry 字段同样必须拒绝"
        );

        let nested_relation = serde_json::json!({
            "note_id": "note-1",
            "contract_version": crate::store::aing_graph::GRAPH_CONTRACT_VERSION,
            "model": "agent-model",
            "entities": [
                { "id": "s", "kind": "person", "name": "张三" },
                { "id": "o", "kind": "tool", "name": "Rust" }
            ],
            "relations": [{
                "subject": "s",
                "predicate": { "type": "uses" },
                "object": "o",
                "confidence": 0.9,
                "evidence": [{
                    "paragraph_index": 0,
                    "start": 0,
                    "end": 8,
                    "quote": "张三使用Rust",
                    "source_seqs": [7, 8],
                    "source_hash": "hash",
                    "human_decision": "confirmed"
                }]
            }]
        });
        assert!(
            serde_json::from_value::<ApplyAingGraphParams>(nested_relation).is_err(),
            "relation/evidence 内的人工裁决字段也必须拒绝"
        );

        let tool = VnMcp::tool_router()
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "apply_aing_graph")
            .unwrap();
        let schema = serde_json::to_value(tool.input_schema).unwrap();
        assert_eq!(
            schema["additionalProperties"], false,
            "顶层 MCP schema 必须显式封闭"
        );
    }

    #[test]
    fn catalog_gates_are_known_values() {
        let cat = catalog();
        for t in cat["tools"].as_array().unwrap() {
            let gate = t["gate"].as_str().unwrap();
            assert!(
                matches!(gate, "none" | "app" | "control"),
                "未知 gate: {gate}"
            );
        }
    }

    #[test]
    fn catalog_cli_nonempty_and_well_formed() {
        let cat = catalog();
        let cli = cat["cli"].as_array().expect("cli 是数组");
        assert_eq!(cli.len(), 13, "notes 4 + speakers 1 + record 6 + ailog 2");
        for c in cli {
            assert!(c["cmd"].as_str().unwrap().starts_with("voice-notes "));
            assert!(!c["desc"].as_str().unwrap().is_empty());
        }
    }
}

/// stdio 服务主循环:客户端关 stdin 即退出。仅此分支创建 tokio runtime,GUI 路径零影响。
pub fn serve_stdio() -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tokio runtime 创建失败: {e}");
            return 1;
        }
    };
    let result: anyhow::Result<()> = rt.block_on(async {
        let service = VnMcp::from_environment().serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    });
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("mcp serve 退出: {e}");
            1
        }
    }
}
