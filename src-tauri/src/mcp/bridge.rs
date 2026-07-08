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

/// UDS 桥集成测试:起一个假"GUI"监听端,按 uds.rs 的行协议(`{"ok":..,"data"/"error":..}`)
/// 应答,断言 stdio 侧 `call` 的成功/拒绝两条路径都正确往返;并覆盖 socket 不存在时的
/// "未运行"错误路径(设计文档 §七「UDS 桥集成」测试项)。
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    #[test]
    fn call_round_trips_success_and_denied_over_uds() {
        let _guard = super::super::ENV_VAR_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VN_APP_DATA", tmp.path());
        let listener = UnixListener::bind(tmp.path().join("mcp.sock")).unwrap();

        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (conn, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut writer = conn;
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let req: serde_json::Value = serde_json::from_str(&line).unwrap();
                let resp = if req["op"] == "status" {
                    serde_json::json!({ "ok": true, "data": { "state": "recording", "note_id": "n1", "elapsed_ms": 1234 } })
                } else {
                    serde_json::json!({ "ok": false, "error": "denied" })
                };
                writeln!(writer, "{resp}").unwrap();
            }
        });

        let data = call("status", serde_json::json!({})).expect("status 应成功往返");
        assert_eq!(data["note_id"], "n1");
        assert_eq!(data["elapsed_ms"], 1234);

        let err = call("start", serde_json::json!({ "title": "x" })).expect_err("拒绝态应回 Err");
        assert_eq!(err, "denied");

        server.join().unwrap();
        std::env::remove_var("VN_APP_DATA");
    }

    #[test]
    fn call_reports_not_running_when_socket_absent() {
        let _guard = super::super::ENV_VAR_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VN_APP_DATA", tmp.path());
        let err = call("status", serde_json::json!({})).expect_err("无 socket 应报未运行");
        assert_eq!(err, NOT_RUNNING);
        std::env::remove_var("VN_APP_DATA");
    }
}
