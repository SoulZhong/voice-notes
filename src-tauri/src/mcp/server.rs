//! rmcp stdio MCP 服务。查询工具直读数据文件;UDS 工具(状态/实时/控制)经 bridge 连 GUI。

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::Deserialize;

use super::tools;

#[derive(Clone, Default)]
pub struct VnMcp;

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
    /// 有精修稿时优先返回精修稿,默认 true
    pub prefer_refined: Option<bool>,
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
    #[tool(description = "列出会议笔记(倒序分页;from/to 可按时间过滤)。返回 id/标题/开始时间/时长/状态。")]
    async fn list_notes(&self, Parameters(p): Parameters<ListNotesParams>) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        Ok(ok_json(tools::list_notes(&roots, p.limit.unwrap_or(20), p.offset.unwrap_or(0), p.from.as_deref(), p.to.as_deref())))
    }

    #[tool(description = "全文检索所有会议笔记的转写内容,返回命中句与上下文各一句、说话人与时间戳。")]
    async fn search_notes(&self, Parameters(p): Parameters<SearchParams>) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        Ok(ok_json(tools::search_notes(&roots, &p.query, p.limit.unwrap_or(20))))
    }

    #[tool(description = "读取一场会议笔记全文。segments 给逐句结构化(含说话人/时间戳),markdown/text 给渲染稿;有 AI 精修稿时默认优先精修稿。")]
    async fn get_note(&self, Parameters(p): Parameters<GetNoteParams>) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        match tools::get_note(&roots, &p.note_id, p.format.as_deref().unwrap_or("segments"), p.prefer_refined.unwrap_or(true)) {
            Ok(v) => Ok(ok_json(v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    #[tool(description = "列出全局声纹库中的说话人(跨会议一致的人物编号/名字/累计说话时长/出现的笔记数)。")]
    async fn list_speakers(&self) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        Ok(ok_json(tools::list_speakers(&roots)))
    }

    #[tool(description = "查询录制状态(idle/recording/paused)、当前笔记 id 与已录时长。需要 voice-notes 应用正在运行。")]
    async fn recording_status(&self) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("status", serde_json::json!({})).await)
    }

    #[tool(description = "获取正在录制会话的实时转写(最近 N 句,含说话人与时间戳)。需要应用正在运行且正在录制。")]
    async fn get_live_transcript(&self, Parameters(p): Parameters<LiveParams>) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("live", serde_json::json!({ "tail": p.tail })).await)
    }

    #[tool(description = "开始录制一场会议(可选标题)。需要应用正在运行,且用户已在左侧 AI 页开启「允许 AI 控制录制」。")]
    async fn start_recording(&self, Parameters(p): Parameters<StartParams>) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("start", serde_json::json!({ "title": p.title })).await)
    }

    #[tool(description = "停止当前录制并返回笔记 id。需要应用运行 + 用户开启「允许 AI 控制录制」。")]
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

#[tool_handler]
impl ServerHandler for VnMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "voice-notes 本地会议笔记。查询类工具(list/search/get/speakers)随时可用;\
             录制状态与控制类工具需要 voice-notes 应用正在运行。所有数据均在本机。",
        )
    }
}

/// `/ai` 页展示用的静态能力清单:MCP 十工具 + CLI 命令一行用法。与上方 `#[tool]`
/// 定义相邻放置,便于人工同步;`catalog_matches_tool_router` 测试做防漂移守卫。
/// gate:`none` 随时可用,`app` 需 App 运行,`control` 还需用户开启「允许 AI 控制录制」。
pub fn catalog() -> serde_json::Value {
    let tools: &[(&str, &str, &str)] = &[
        ("list_notes", "列出会议笔记(倒序分页;from/to 可按时间过滤)。返回 id/标题/开始时间/时长/状态。", "none"),
        ("search_notes", "全文检索所有会议笔记的转写内容,返回命中句与上下文各一句、说话人与时间戳。", "none"),
        (
            "get_note",
            "读取一场会议笔记全文。segments 给逐句结构化(含说话人/时间戳),markdown/text 给渲染稿;有 AI 精修稿时默认优先精修稿。",
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
        ("voice-notes speakers list [--json]", "列出全局声纹库中的说话人"),
        ("voice-notes record status", "查询录制状态(需应用运行)"),
        ("voice-notes record start [--title \"评审会\"]", "开始录制(需应用运行 + 允许 AI 控制)"),
        ("voice-notes record stop", "停止录制并返回笔记 id(需应用运行 + 允许 AI 控制)"),
        ("voice-notes record pause", "暂停录制(需应用运行 + 允许 AI 控制)"),
        ("voice-notes record resume", "恢复已暂停的录制(需应用运行 + 允许 AI 控制)"),
        ("voice-notes record live [--tail N]", "获取实时转写(需应用运行)"),
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
        let router_names: std::collections::BTreeSet<String> =
            VnMcp::tool_router().list_all().into_iter().map(|t| t.name.to_string()).collect();
        let cat = catalog();
        let cat_tools = cat["tools"].as_array().expect("tools 是数组");
        let cat_names: std::collections::BTreeSet<String> =
            cat_tools.iter().map(|t| t["name"].as_str().expect("name 是字符串").to_string()).collect();
        assert_eq!(cat_names.len(), cat_tools.len(), "catalog 不应有重复工具名");
        assert_eq!(cat_names, router_names, "catalog.tools 必须与 tool_router 注册的工具名完全一致");
    }

    #[test]
    fn catalog_gates_are_known_values() {
        let cat = catalog();
        for t in cat["tools"].as_array().unwrap() {
            let gate = t["gate"].as_str().unwrap();
            assert!(matches!(gate, "none" | "app" | "control"), "未知 gate: {gate}");
        }
    }

    #[test]
    fn catalog_cli_nonempty_and_well_formed() {
        let cat = catalog();
        let cli = cat["cli"].as_array().expect("cli 是数组");
        assert_eq!(cli.len(), 10, "notes 3 + speakers 1 + record 6");
        for c in cli {
            assert!(c["cmd"].as_str().unwrap().starts_with("voice-notes "));
            assert!(!c["desc"].as_str().unwrap().is_empty());
        }
    }
}

/// stdio 服务主循环:客户端关 stdin 即退出。仅此分支创建 tokio runtime,GUI 路径零影响。
pub fn serve_stdio() -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tokio runtime 创建失败: {e}");
            return 1;
        }
    };
    let result: anyhow::Result<()> = rt.block_on(async {
        let service = VnMcp::default().serve(stdio()).await?;
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
