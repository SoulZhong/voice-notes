//! Windows 的 UDS 客户端桩(stdio/CLI 侧),经 mcp/mod.rs 的 #[path] 顶替。
//! GUI 侧未起监听(见 uds_stub.rs),控制类调用统一返回人话指引——与 unix 侧
//! 「App 未运行」同一种降级形态,Agent 拿到的错误可直接转述给用户。

pub const NOT_RUNNING: &str =
    "voice-notes 应用未在运行。请先启动 voice-notes(查询类工具 list_notes/search_notes/get_note/list_speakers 无需应用运行)。";

/// 与 unix 版同形。Windows 首版无 GUI 桥:控制类操作一律不可用。
pub fn call(_op: &str, _extra: serde_json::Value) -> Result<serde_json::Value, String> {
    Err("Windows 版暂不支持连接运行中的 voice-notes(录制控制/实时转写不可用);查询类工具(list_notes/search_notes/get_note/list_speakers)不受影响。".to_string())
}
