//! stdio 进程 → GUI 的 UDS 客户端。同步阻塞(单请求毫秒级;stop 会等排干,给宽超时)。
//! 连不上 = App 未运行,统一转成给 Agent 看的指引文案。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const NOT_RUNNING: &str =
    "voice-notes 应用未在运行。请先启动 voice-notes(查询类工具 list_notes/search_notes/get_note/list_speakers 无需应用运行)。";

/// op + 额外字段(对象)合并成一行请求,读一行响应。Err 是给 Agent 的人话。
pub fn call(op: &str, extra: serde_json::Value) -> Result<serde_json::Value, String> {
    let sock = super::app_data_dir().join("mcp.sock");
    let mut stream = UnixStream::connect(&sock).map_err(|_| NOT_RUNNING.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok(); // stop 等排干,宽限
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let mut req = serde_json::json!({ "op": op });
    if let (Some(obj), Some(ext)) = (req.as_object_mut(), extra.as_object()) {
        for (k, v) in ext {
            obj.insert(k.clone(), v.clone());
        }
    }
    writeln!(stream, "{req}").map_err(|e| format!("请求发送失败: {e}"))?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(|e| format!("响应读取失败: {e}"))?;
    let resp: serde_json::Value = serde_json::from_str(&line).map_err(|e| format!("响应解析失败: {e}"))?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(resp.get("data").cloned().unwrap_or(serde_json::json!({})))
    } else {
        Err(resp["error"].as_str().unwrap_or("未知错误").to_string())
    }
}
