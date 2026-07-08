//! GUI 侧 Unix socket 服务:stdio MCP 进程的「活能力」后端。行式 JSON,一行请求
//! 一行响应。socket 固定在 app_data(不随 data_dir 迁移),权限 0600。
//! 控制类 op 受 settings.mcp_allow_control 门控——授权真值源在 GUI 侧,stdio 进程
//! 不可信(任何本机进程都能连 socket,但同 uid 本就有全部数据的文件权限,不新增面)。

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use tauri::Manager;

#[derive(Deserialize)]
struct Req {
    op: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    tail: Option<usize>,
}

#[derive(Serialize)]
struct Resp {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn ok(data: serde_json::Value) -> Resp {
    Resp { ok: true, data: Some(data), error: None }
}

fn err(msg: impl Into<String>) -> Resp {
    Resp { ok: false, data: None, error: Some(msg.into()) }
}

pub fn spawn_listener(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let Ok(app_data) = app.path().app_data_dir() else {
            eprintln!("mcp uds: app_data_dir 不可用,活能力不启动(查询类工具不受影响)");
            return;
        };
        let _ = std::fs::create_dir_all(&app_data);
        let sock = app_data.join("mcp.sock");
        let _ = std::fs::remove_file(&sock); // 上次异常退出的残留
        let listener = match UnixListener::bind(&sock) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("mcp uds: bind 失败(活能力不可用): {e}");
                return;
            }
        };
        // bind→chmod 间的 umask 窗口不可达:app_data 位于 ~/Library(700)之下,其它
        // uid 无法遍历到本目录(终审已验证,接受这个理论上存在但实际打不到的窗口)。
        let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
        for conn in listener.incoming().flatten() {
            let app = app.clone();
            // 每连接一线程:流量是"单 Agent 偶发调用"量级,线程成本可忽略。
            std::thread::spawn(move || handle_conn(&app, conn));
        }
    });
}

fn handle_conn(app: &tauri::AppHandle, conn: UnixStream) {
    let Ok(write_half) = conn.try_clone() else { return };
    let mut writer = std::io::BufWriter::new(write_half);
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Req>(&line) {
            Ok(req) => dispatch(app, &req),
            Err(e) => err(format!("请求解析失败: {e}")),
        };
        let Ok(json) = serde_json::to_string(&resp) else { break };
        if writeln!(writer, "{json}").and_then(|()| writer.flush()).is_err() {
            break;
        }
    }
}

/// 录制状态快照(与 recording_status 命令同源:session 槽)。
fn status_json(app: &tauri::AppHandle) -> serde_json::Value {
    let state = app.state::<crate::AppState>();
    let slot = state.session.lock().unwrap();
    match slot.as_ref() {
        Some(s) => serde_json::json!({
            "state": if s.paused_at.is_some() { "paused" } else { "recording" },
            "note_id": s.note_id, "elapsed_ms": s.elapsed_ms(),
            "system_audio": s.system_audio, "diarization": s.diarization,
        }),
        None => serde_json::json!({ "state": "idle", "note_id": "", "elapsed_ms": 0,
            "system_audio": "", "diarization": "" }),
    }
}

fn control_allowed(app: &tauri::AppHandle) -> bool {
    app.path().app_data_dir().map(|d| crate::settings::load(&d).mcp_allow_control).unwrap_or(false)
}

const CONTROL_DENIED: &str = "已被用户在设置中禁用:请在 voice-notes 的「设置 → AI 助手接入」开启「允许 AI 控制录制」";

fn dispatch(app: &tauri::AppHandle, req: &Req) -> Resp {
    match req.op.as_str() {
        "status" => ok(status_json(app)),
        "live" => {
            let note_id = {
                let state = app.state::<crate::AppState>();
                let slot = state.session.lock().unwrap();
                match slot.as_ref() {
                    Some(s) => s.note_id.clone(),
                    None => return err("没有正在进行的录制"),
                }
            };
            let tail = req.tail.unwrap_or(50).clamp(1, 500);
            let Ok(dir) = crate::notes_dir(app) else { return err("数据目录不可用") };
            match crate::store::NoteStore::new(dir).load(&note_id) {
                Ok(note) => {
                    let start = note.segments.len().saturating_sub(tail);
                    ok(serde_json::json!({
                        "note_id": note_id, "title": note.meta.title,
                        "segments": note.segments[start..].iter().map(|s| serde_json::json!({
                            "seq": s.seq, "source": s.source, "speaker": s.speaker,
                            "start_ms": s.start_ms, "text": s.text,
                        })).collect::<Vec<_>>(),
                    }))
                }
                Err(e) => err(e.to_string()),
            }
        }
        "start" => {
            if !control_allowed(app) {
                return err(CONTROL_DENIED);
            }
            if let Err(e) = crate::do_start_recording(app) {
                return err(e);
            }
            // spawn_session 异步加载模型后才入槽:轮询等 note_id(最多 20s,模型冷加载
            // 可能秒级);拿到后如带 title,经 writer 单写者路径改题(见 set_title 注释)。
            for _ in 0..200 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let state = app.state::<crate::AppState>();
                let slot = state.session.lock().unwrap();
                if let Some(s) = slot.as_ref() {
                    if let Some(title) = req.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                        if let Err(e) = s.writer.lock().unwrap().set_title(title) {
                            eprintln!("mcp start: 设标题失败(录制已开始,不回滚): {e}");
                        }
                    }
                    return ok(serde_json::json!({ "note_id": s.note_id }));
                }
                drop(slot);
                // 会话未入槽且 running 已被清(启动失败路径)→ 提前报错
                if !*state.running.lock().unwrap() {
                    return err("录制未能进入进行中状态(设备/模型异常,或已被手动停止;详见应用日志)");
                }
            }
            err("录制启动超时")
        }
        "stop" => {
            if !control_allowed(app) {
                return err(CONTROL_DENIED);
            }
            let note_id = status_json(app)["note_id"].as_str().unwrap_or_default().to_string();
            if note_id.is_empty() {
                return err("没有正在进行的录制");
            }
            crate::do_stop_recording(app); // 阻塞至排干,本线程等待无妨
            ok(serde_json::json!({ "note_id": note_id }))
        }
        "pause" => {
            if !control_allowed(app) {
                return err(CONTROL_DENIED);
            }
            match crate::do_pause_recording(app) {
                Ok(()) => ok(status_json(app)),
                Err(e) => err(e),
            }
        }
        "resume" => {
            if !control_allowed(app) {
                return err(CONTROL_DENIED);
            }
            match crate::do_resume_recording(app) {
                Ok(()) => ok(status_json(app)),
                Err(e) => err(e),
            }
        }
        other => err(format!("未知 op: {other}")),
    }
}
