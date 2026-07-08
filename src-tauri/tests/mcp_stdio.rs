//! MCP stdio e2e:spawn 真二进制,走 initialize → tools/list → tools/call 全链路。
//! 数据经 VN_APP_DATA 注入 tempdir(settings.json 缺省 → data_root=app_data)。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl Mcp {
    fn spawn(app_data: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_voice-notes"))
            .args(["mcp", "serve"])
            .env("VN_APP_DATA", app_data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn voice-notes mcp serve");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout, next_id: 0 }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.stdin,
            "{}",
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        )
        .unwrap();
        // 服务端可能穿插通知,读到匹配 id 的响应为止
        loop {
            let mut line = String::new();
            assert!(self.stdout.read_line(&mut line).unwrap() > 0, "服务端过早关闭");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v.get("id") == Some(&serde_json::json!(id)) {
                assert!(v.get("error").is_none(), "RPC 错误: {v}");
                return v["result"].clone();
            }
        }
    }

    fn notify(&mut self, method: &str) {
        writeln!(self.stdin, "{}", serde_json::json!({ "jsonrpc": "2.0", "method": method })).unwrap();
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 工具结果的首块文本解析为 JSON。
fn tool_json(result: &serde_json::Value) -> serde_json::Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn stdio_initialize_list_and_call_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("notes/20260101-100000");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"schema_version":1,"id":"20260101-100000","title":"评审会","started_at":"2026-01-01T10:00:00+08:00","ended_at":"2026-01-01T11:00:00+08:00","state":"complete"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("segments.jsonl"),
        r#"{"seq":0,"source":"mic","text":"交付日期定在 Q3","start_ms":0,"end_ms":1000,"speaker":"S1"}"#.to_string() + "\n",
    )
    .unwrap();

    let mut mcp = Mcp::spawn(tmp.path());
    let init = mcp.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "e2e", "version": "0" }
        }),
    );
    assert!(init["capabilities"]["tools"].is_object(), "{init}");
    mcp.notify("notifications/initialized");

    let tools = mcp.request("tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expect in [
        "list_notes",
        "search_notes",
        "get_note",
        "list_speakers",
        "recording_status",
        "get_live_transcript",
        "start_recording",
        "stop_recording",
        "pause_recording",
        "resume_recording",
    ] {
        assert!(names.contains(&expect), "缺工具 {expect}: {names:?}");
    }

    // 漂移守卫:实际注册的每个工具都必须出现在 README 工具表里(反引号包裹)。
    // README/CLI/MCP 三处描述同一套能力,这条让"改了工具却漏更新 README"在 CI 就红,
    // 不必靠人眼在终审时逐字段核对(真值源纪律的自动化兜底)。README 双语,以中文为准。
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../README.md"))
        .expect("读取 README.md");
    for name in &names {
        assert!(
            readme.contains(&format!("`{name}`")),
            "工具 {name} 已注册但 README.md 工具表未列出——三面描述漂移了",
        );
    }

    let r = mcp.request("tools/call", serde_json::json!({ "name": "list_notes", "arguments": {} }));
    let v = tool_json(&r);
    assert_eq!(v["total"], 1);
    assert_eq!(v["notes"][0]["title"], "评审会");

    let r = mcp.request("tools/call", serde_json::json!({ "name": "search_notes", "arguments": { "query": "交付日期" } }));
    assert_eq!(tool_json(&r)["hits"][0]["note_id"], "20260101-100000");

    let r = mcp.request(
        "tools/call",
        serde_json::json!({ "name": "get_note", "arguments": { "note_id": "20260101-100000", "format": "markdown" } }),
    );
    assert!(tool_json(&r)["content"].as_str().unwrap().contains("交付日期定在 Q3"));

    // 错误路径:不存在的笔记 → isError 工具级错误(而非 RPC 协议错误)
    let r = mcp.request(
        "tools/call",
        serde_json::json!({ "name": "get_note", "arguments": { "note_id": "no-such" } }),
    );
    assert_eq!(r["isError"], true, "{r}");

    // App 未运行(VN_APP_DATA 指向 tempdir,必无 mcp.sock):UDS 工具给指引性错误而非崩溃
    let r = mcp.request("tools/call", serde_json::json!({ "name": "recording_status", "arguments": {} }));
    assert_eq!(r["isError"], true, "{r}");
    assert!(r["content"][0]["text"].as_str().unwrap().contains("未在运行"), "{r}");
}
