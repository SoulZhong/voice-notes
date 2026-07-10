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

const CONTROL_DENIED: &str = "已被用户禁用:请在 voice-notes 左侧「AI」页开启「允许 AI 控制录制」";

/// dispatch 依赖的能力抽象:把「读授权开关、取状态、执行录制操作」从 AppHandle 解耦,
/// 使门控判定与 op 路由这层策略可脱离 GUI 单测(控制面最该锁住的不变量是"某个控制
/// op 别漏了门控")。生产实现是 AppBackend;测试用 mock 覆盖门控矩阵与路由。
trait UdsBackend {
    fn control_allowed(&self) -> bool;
    fn status(&self) -> serde_json::Value;
    fn live(&self, tail: usize) -> Result<serde_json::Value, String>;
    fn start(&self, title: Option<&str>) -> Result<serde_json::Value, String>;
    fn stop(&self) -> Result<serde_json::Value, String>;
    fn pause(&self) -> Result<serde_json::Value, String>;
    fn resume(&self) -> Result<serde_json::Value, String>;
}

/// 策略层:控制类 op 统一先过门控(集中一处,新增控制 op 不会漏挂门控),再路由到
/// backend;tail clamp 与 title trim 也在此,便于单测。未知 op 报错。
fn dispatch_with<B: UdsBackend>(b: &B, req: &Req) -> Resp {
    let op = req.op.as_str();
    if matches!(op, "start" | "stop" | "pause" | "resume") && !b.control_allowed() {
        return err(CONTROL_DENIED);
    }
    let result = match op {
        "status" => Ok(b.status()),
        "live" => b.live(req.tail.unwrap_or(50).clamp(1, 500)),
        "start" => b.start(req.title.as_deref().map(str::trim).filter(|t| !t.is_empty())),
        "stop" => b.stop(),
        "pause" => b.pause(),
        "resume" => b.resume(),
        other => return err(format!("未知 op: {other}")),
    };
    match result {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

fn dispatch(app: &tauri::AppHandle, req: &Req) -> Resp {
    dispatch_with(&AppBackend(app), req)
}

/// 生产实现:各能力逐块搬自原 dispatch 分支(仅错误从 `return err(..)` 改 `Err(..)`,
/// 门控上移到 dispatch_with),行为等价。
struct AppBackend<'a>(&'a tauri::AppHandle);

impl UdsBackend for AppBackend<'_> {
    fn control_allowed(&self) -> bool {
        control_allowed(self.0)
    }

    fn status(&self) -> serde_json::Value {
        status_json(self.0)
    }

    fn live(&self, tail: usize) -> Result<serde_json::Value, String> {
        let app = self.0;
        let note_id = {
            let state = app.state::<crate::AppState>();
            let slot = state.session.lock().unwrap();
            match slot.as_ref() {
                Some(s) => s.note_id.clone(),
                None => return Err("没有正在进行的录制".into()),
            }
        };
        let dir = crate::notes_dir(app).map_err(|_| "数据目录不可用".to_string())?;
        let note = crate::store::NoteStore::new(dir).load(&note_id).map_err(|e| e.to_string())?;
        let start = note.segments.len().saturating_sub(tail);
        Ok(serde_json::json!({
            "note_id": note_id, "title": note.meta.title,
            "segments": note.segments[start..].iter().map(|s| serde_json::json!({
                "seq": s.seq, "source": s.source, "speaker": s.speaker,
                "start_ms": s.start_ms, "text": s.text,
            })).collect::<Vec<_>>(),
        }))
    }

    fn start(&self, title: Option<&str>) -> Result<serde_json::Value, String> {
        let app = self.0;
        crate::do_start_recording(app)?;
        // spawn_session 异步加载模型后才入槽:轮询等 note_id(最多 20s,模型冷加载
        // 可能秒级);拿到后如带 title,经 writer 单写者路径改题(见 set_title 注释)。
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let state = app.state::<crate::AppState>();
            let slot = state.session.lock().unwrap();
            if let Some(s) = slot.as_ref() {
                if let Some(title) = title {
                    if let Err(e) = s.writer.lock().unwrap().set_title(title) {
                        eprintln!("mcp start: 设标题失败(录制已开始,不回滚): {e}");
                    }
                }
                return Ok(serde_json::json!({ "note_id": s.note_id }));
            }
            drop(slot);
            // 会话未入槽且 running 已被清(启动失败路径)→ 提前报错
            if !*state.running.lock().unwrap() {
                return Err("录制未能进入进行中状态(设备/模型异常,或已被手动停止;详见应用日志)".into());
            }
        }
        Err("录制启动超时".into())
    }

    fn stop(&self) -> Result<serde_json::Value, String> {
        let app = self.0;
        let note_id = status_json(app)["note_id"].as_str().unwrap_or_default().to_string();
        if note_id.is_empty() {
            return Err("没有正在进行的录制".into());
        }
        crate::do_stop_recording(app); // 阻塞至排干,本线程等待无妨
        Ok(serde_json::json!({ "note_id": note_id }))
    }

    fn pause(&self) -> Result<serde_json::Value, String> {
        crate::do_pause_recording(self.0)?;
        Ok(status_json(self.0))
    }

    fn resume(&self) -> Result<serde_json::Value, String> {
        crate::do_resume_recording(self.0)?;
        Ok(status_json(self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 记录被调方法 + 可配置 control_allowed 的假后端。
    struct MockBackend {
        control: bool,
        calls: RefCell<Vec<String>>,
    }
    impl MockBackend {
        fn new(control: bool) -> Self {
            Self { control, calls: RefCell::new(Vec::new()) }
        }
        fn log(&self, s: impl Into<String>) {
            self.calls.borrow_mut().push(s.into());
        }
        fn called(&self, s: &str) -> bool {
            self.calls.borrow().iter().any(|c| c == s)
        }
    }
    impl UdsBackend for MockBackend {
        fn control_allowed(&self) -> bool {
            self.control
        }
        fn status(&self) -> serde_json::Value {
            self.log("status");
            serde_json::json!({ "state": "idle" })
        }
        fn live(&self, tail: usize) -> Result<serde_json::Value, String> {
            self.log(format!("live:{tail}"));
            Ok(serde_json::json!({ "tail": tail }))
        }
        fn start(&self, title: Option<&str>) -> Result<serde_json::Value, String> {
            self.log(format!("start:{title:?}"));
            Ok(serde_json::json!({ "note_id": "N1" }))
        }
        fn stop(&self) -> Result<serde_json::Value, String> {
            self.log("stop");
            Ok(serde_json::json!({ "note_id": "N1" }))
        }
        fn pause(&self) -> Result<serde_json::Value, String> {
            self.log("pause");
            Ok(serde_json::json!({ "state": "paused" }))
        }
        fn resume(&self) -> Result<serde_json::Value, String> {
            self.log("resume");
            Ok(serde_json::json!({ "state": "recording" }))
        }
    }

    fn req(op: &str) -> Req {
        Req { op: op.into(), title: None, tail: None }
    }

    #[test]
    fn control_ops_gated_when_disabled() {
        let b = MockBackend::new(false);
        for op in ["start", "stop", "pause", "resume"] {
            let r = dispatch_with(&b, &req(op));
            assert!(!r.ok, "{op} 应被门控拒绝");
            assert_eq!(r.error.as_deref(), Some(CONTROL_DENIED));
        }
        // 门控在 backend 调用之前:被拒的 op 绝不触达真实操作。
        assert!(b.calls.borrow().is_empty(), "门控关时不得调用任何控制方法: {:?}", b.calls.borrow());
    }

    #[test]
    fn query_ops_not_gated() {
        let b = MockBackend::new(false); // 即便控制关
        assert!(dispatch_with(&b, &req("status")).ok, "status 不受门控");
        assert!(dispatch_with(&b, &Req { op: "live".into(), title: None, tail: None }).ok, "live 不受门控");
        assert!(b.called("status") && b.called("live:50"));
    }

    #[test]
    fn control_ops_routed_when_enabled() {
        let b = MockBackend::new(true);
        for op in ["start", "stop", "pause", "resume"] {
            assert!(dispatch_with(&b, &req(op)).ok, "{op} 门控开时应放行");
        }
        assert!(b.called("start:None") && b.called("stop") && b.called("pause") && b.called("resume"));
    }

    #[test]
    fn live_tail_clamped_and_defaulted() {
        let b = MockBackend::new(true);
        dispatch_with(&b, &Req { op: "live".into(), title: None, tail: Some(1000) });
        dispatch_with(&b, &Req { op: "live".into(), title: None, tail: Some(0) });
        dispatch_with(&b, &Req { op: "live".into(), title: None, tail: None });
        assert!(b.called("live:500"), "上限 500");
        assert!(b.called("live:1"), "下限 1");
        assert!(b.called("live:50"), "缺省 50");
    }

    #[test]
    fn start_title_trimmed() {
        let b = MockBackend::new(true);
        dispatch_with(&b, &Req { op: "start".into(), title: Some("  评审会  ".into()), tail: None });
        dispatch_with(&b, &Req { op: "start".into(), title: Some("   ".into()), tail: None });
        assert!(b.called("start:Some(\"评审会\")"), "两端空白应 trim: {:?}", b.calls.borrow());
        assert!(b.called("start:None"), "纯空白 title → None");
    }

    #[test]
    fn unknown_op_errors() {
        let b = MockBackend::new(true);
        let r = dispatch_with(&b, &req("bogus"));
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("未知 op: bogus"));
    }
}
