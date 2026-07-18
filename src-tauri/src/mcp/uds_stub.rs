//! Windows 的 UDS 桥桩(GUI 侧)。std 不在 Windows 暴露 Unix domain socket
//! (系统本身 Win10 1803+ 支持 AF_UNIX,但 std::os::unix::net 仅 unix 编译),
//! 经 mcp/mod.rs 的 #[path] 顶替。首版:GUI 侧不起监听,MCP/CLI 的「活能力」
//! (录制控制/实时转写)在 Windows 降级为不可用,查询类工具(直读磁盘)不受影响。
//! 后续路线(计划文档已记):tokio net 已在依赖树,可评估命名管道或 uds crate。

/// 与 unix 版同形:Windows 首版不提供 GUI 桥,静默跳过(一行日志)。
pub fn spawn_listener(_app: tauri::AppHandle) {
    eprintln!("MCP GUI 桥(UDS)在 Windows 暂不可用:录制控制类工具降级,查询类不受影响");
}
