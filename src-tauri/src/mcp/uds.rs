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
    #[serde(default)]
    note_id: Option<String>,
    #[serde(default)]
    input: Option<String>,
    /// 强制本次重转写使用的本地识别引擎(如 "firered");缺省按设置决策。
    #[serde(default)]
    engine: Option<String>,
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
    /// Aing 可观测(issue #173,只读不受控):在跑/心跳/盘上稿三视角合一。
    /// 默认实现报未支持,生产 AppBackend 覆写——mock 后端们不关心此 op。
    fn refine_status(&self, _note_id: &str) -> Result<serde_json::Value, String> {
        Err("refine_status 未实现".into())
    }
    /// 触发「重新 Aing」:Some(id)=单篇;None=全部未 Aing(entities 空)的 complete 笔记。
    fn reaing(&self, note_id: Option<&str>) -> Result<serde_json::Value, String>;
    /// 发起文件重转写(异步启动即返回;input 缺省 dual)。
    /// `engine`:强制用某个本地识别引擎跑这一次(如 "firered"),None = 按设置决策。
    /// 存在的理由与笔记页那个「用 FireRed 重转写本篇」同一个:换引擎救回识别失败的段,
    /// 但不动用户的默认选择(见 do_retranscribe 的注释)。
    fn retranscribe(
        &self,
        note_id: &str,
        input: &str,
        engine: Option<&str>,
    ) -> Result<serde_json::Value, String>;
    /// 当前重转写任务;空闲返回 null。
    fn retranscribe_status(&self) -> serde_json::Value;
}

/// 策略层:控制类 op 统一先过门控(集中一处,新增控制 op 不会漏挂门控),再路由到
/// backend;tail clamp 与 title trim 也在此,便于单测。未知 op 报错。
fn dispatch_with<B: UdsBackend>(b: &B, req: &Req) -> Resp {
    let op = req.op.as_str();
    if matches!(op, "start" | "stop" | "pause" | "resume" | "reaing" | "retranscribe") && !b.control_allowed() {
        return err(CONTROL_DENIED);
    }
    let result = match op {
        "status" => Ok(b.status()),
        "live" => b.live(req.tail.unwrap_or(50).clamp(1, 500)),
        "start" => b.start(req.title.as_deref().map(str::trim).filter(|t| !t.is_empty())),
        "stop" => b.stop(),
        "pause" => b.pause(),
        "resume" => b.resume(),
        "reaing" => b.reaing(req.note_id.as_deref().map(str::trim).filter(|s| !s.is_empty())),
        "retranscribe" => match req.note_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => b.retranscribe(
                id,
                req.input.as_deref().unwrap_or("dual"),
                req.engine.as_deref().map(str::trim).filter(|e| !e.is_empty()),
            ),
            None => Err("retranscribe 需要 note_id".into()),
        },
        "retranscribe_status" => Ok(b.retranscribe_status()),
        "refine_status" => match req.note_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => b.refine_status(id),
            None => Err("refine_status 需要 note_id".into()),
        },
        other => return err(format!("未知 op: {other}")),
    };
    match result {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

fn dispatch(app: &tauri::AppHandle, req: &Req) -> Resp {
    if let Some(op) = crate::telemetry::McpOp::parse(&req.op) {
        crate::telemetry::track(app, crate::telemetry::Event::McpToolUsed { op });
    }
    let resp = dispatch_with(&AppBackend(app), req);
    // 只上报**意外**的分发失败。控制面被禁(默认态)、空闲时调 stop、不支持的 op
    // 这些都是正常客户端行为,把它们记成异常会凭空造出 issue、把失败率看板搅浑
    // (codex review 第二轮发现)。校验/授权/状态类拒绝保持普通回执即可。
    if let Some(err) = resp.error.as_deref() {
        if is_unexpected_failure(err) {
            crate::telemetry::report_error(crate::telemetry::ErrorKind::McpDispatch, err);
        }
    }
    resp
}

/// 区分「意外失败」与「正常拒绝」。正常拒绝是协议的一部分,不是错误。
fn is_unexpected_failure(err: &str) -> bool {
    const EXPECTED: [&str; 6] = [
        "控制面未启用",
        "未在录制",
        "正在录制",
        "不支持的操作",
        "参数",
        "未找到",
    ];
    !EXPECTED.iter().any(|k| err.contains(k))
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
        // 开录前风险随返回值带出(Codex review P2):MCP 是无 UI 上下文的第三条开录
        // 入口,既走不了确认对话框,也享受不到录制页横幅兜底——它不会把用户导航到
        // 那一页。所以这里不拦(程序化调用拦不了),但必须把风险如实交给调用方,
        // 让 AI 助手能转达给人,而不是静默录一场残缺的会。
        let risks = crate::precheck::record_risks(
            crate::audio::mic_mode::active(),
            crate::audio::default_input_is_bluetooth(),
        );
        if !risks.is_empty() {
            eprintln!(
                "mcp start: 开录前检测到风险 {:?},已随返回值带出(MCP 路径不拦)",
                risks.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>()
            );
        }
        // P1 改道:经 lifecycle actor 信箱串行执行,执行体仍是 do_start_recording。
        app.state::<crate::lifecycle::LifecycleHandle>()
            .command(crate::lifecycle::Cmd::Start { resume_id: None })?;
        // spawn_session 异步加载模型后才入槽:轮询等 note_id(最多 20s,模型冷加载
        // 可能秒级);拿到后如带 title,经信箱走 writer 单写者路径改题(P2:writer 归
        // actor;录制中改题唯一安全路径,直写盘会被 finalize 的内存 meta 覆盖)。
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let state = app.state::<crate::AppState>();
            // statement-scoped 取 note_id 即放锁:request() 阻塞等 actor,而 actor 的
            // 执行体可能要取 session 锁,持锁等待会成环(见 actor.rs 死锁注记③)。
            let note_id = state.session.lock().unwrap().as_ref().map(|s| s.note_id.clone());
            if let Some(note_id) = note_id {
                if let Some(title) = title {
                    // 入槽晚于 AdoptWriter 入信箱(同一加载线程先采纳后入槽),故此刻
                    // 消息必落在采纳之后;失败(如恰逢停录)不回滚录制,与旧行为一致。
                    if let Err(e) = app.state::<crate::lifecycle::LifecycleHandle>().request(
                        crate::lifecycle::machine::Msg::SetTitle {
                            note_id: note_id.clone(),
                            title: title.into(),
                        },
                    ) {
                        eprintln!("mcp start: 设标题失败(录制已开始,不回滚): {e}");
                    }
                }
                return Ok(serde_json::json!({ "note_id": note_id, "risks": risks }));
            }
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
        // 经 actor 串行执行停录(P2:teardown+自投 Finalize)——阻塞至收尾完成,本线程等待无妨。
        app.state::<crate::lifecycle::LifecycleHandle>()
            .command(crate::lifecycle::Cmd::Stop)?;
        Ok(serde_json::json!({ "note_id": note_id }))
    }

    fn pause(&self) -> Result<serde_json::Value, String> {
        // P1 改道:经 lifecycle actor 信箱串行执行,执行体仍是 do_pause_recording。
        self.0
            .state::<crate::lifecycle::LifecycleHandle>()
            .command(crate::lifecycle::Cmd::Pause)?;
        Ok(status_json(self.0))
    }

    fn resume(&self) -> Result<serde_json::Value, String> {
        // P1 改道:经 lifecycle actor 信箱串行执行,执行体仍是 do_resume_recording。
        self.0
            .state::<crate::lifecycle::LifecycleHandle>()
            .command(crate::lifecycle::Cmd::Unpause)?;
        Ok(status_json(self.0))
    }

    fn refine_status(&self, note_id: &str) -> Result<serde_json::Value, String> {
        crate::store::validate_note_id(note_id).map_err(|e| e.to_string())?;
        let app = self.0;
        // 三视角(#173):内核在跑集合 / worker 心跳 / 盘上稿摘要。合起来能区分
        // 「在跑(refining=true 且心跳新鲜)/收工(有稿且 written_at 新)/真停摆
        // (refining=true 但心跳陈旧)」——2026-08-26 那晚要有这个,两小时误诊不会发生。
        let refining = app.state::<crate::lifecycle::LifecycleHandle>().is_refining(note_id);
        let beat = crate::refine_beat_of(note_id);
        let doc = crate::notes_dir(app)
            .ok()
            .map(|root| root.join(note_id))
            .and_then(|dir| {
                let d = crate::store::load_refined(&dir)?;
                if d.written_at.is_empty() {
                    // 旧稿刚被 load_refined 迁移成 aing.json 时,返回的还是未盖戳
                    // 的内存对象(戳只落在盘上)——重读一次拿真值(codex 三轮 P2)。
                    crate::store::load_refined(&dir).or(Some(d))
                } else {
                    Some(d)
                }
            })
            .map(|d| {
                serde_json::json!({
                    "stages": {
                        "filter": d.stages.filter, "recluster": d.stages.recluster,
                        "llm": d.stages.llm, "entities": d.stages.entities,
                        "relations": d.stages.relations,
                    },
                    "written_at": d.written_at, "writer_pid": d.writer_pid,
                    "generated_at": d.generated_at,
                    "llm_failed_paragraphs": d.llm_failed_paragraphs.len(),
                })
            });
        Ok(serde_json::json!({
            "note_id": note_id,
            "refining": refining,
            "beat": beat.map(|(stage, age_ms)| serde_json::json!({ "stage": stage, "age_ms": age_ms })),
            "doc": doc,
        }))
    }

    fn reaing(&self, note_id: Option<&str>) -> Result<serde_json::Value, String> {
        let app = self.0;
        // 与笔记页「重新 Aing」魔杖同路径:经 lifecycle actor 单写者投 RefineRequest,内核守卫
        // 只放行 complete、非活动会话;spawn 后即返回,重活受 AING_GATE 串行闸约束(逐篇跑不爆核)。
        let fire = |id: &str| -> Result<(), String> {
            crate::store::validate_note_id(id).map_err(|e| e.to_string())?;
            // 与 lib.rs::refine_note 命令逐字同款的重转写守卫——两处必须同步改。
            // 原因:reaing 是 MCP 侧直投 RefineRequest 的入口,不经过 refine_note 命令,
            // 若不在此复刻检查,MCP 客户端可在重转写运行期间绕过守卫触发 refine,
            // refine 用重转写覆盖前的旧 segments 跑完后再提交,把重转写刚写入的新
            // 结果盖掉(NoteLock 会让 refine 的提交失败,但那是跑完一整轮才失败,
            // 这里提前到「点下去就说清」)。
            if let Some((rid, _)) = app.state::<crate::AppState>().retranscribing.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                if rid == id {
                    return Err(crate::tr!("该笔记正在重转写中", "This note is being re-transcribed"));
                }
            }
            app.state::<crate::lifecycle::LifecycleHandle>()
                .request(crate::lifecycle::machine::Msg::RefineRequest { note_id: id.to_string() })
        };
        match note_id {
            Some(id) => {
                fire(id)?;
                Ok(serde_json::json!({ "queued": 1, "ids": [id] }))
            }
            None => {
                // --all:所有「未 Aing」(aing.json 无 entities)的 complete 笔记逐篇排队;
                // 活动会话由内核守卫挡下(fire 返 Err 即跳过),已 Aing 的直接跳过省钱。
                let root = crate::notes_dir(app).map_err(|e| e.to_string())?;
                let mut ids: Vec<String> = Vec::new();
                for n in crate::store::NoteStore::new(root.clone()).list() {
                    if n.state != "complete" {
                        continue;
                    }
                    let has_entities = crate::store::load_refined(&root.join(&n.id))
                        .map(|d| !d.entities.is_empty())
                        .unwrap_or(false);
                    if has_entities {
                        continue;
                    }
                    if fire(&n.id).is_ok() {
                        ids.push(n.id);
                    }
                }
                Ok(serde_json::json!({ "queued": ids.len(), "ids": ids }))
            }
        }
    }

    fn retranscribe(
        &self,
        note_id: &str,
        input: &str,
        engine: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        crate::do_retranscribe(self.0, note_id, input, engine.map(str::to_string))?;
        Ok(serde_json::json!({
            "started": true, "note_id": note_id, "input": input, "engine": engine,
        }))
    }

    fn retranscribe_status(&self) -> serde_json::Value {
        let state = self.0.state::<crate::AppState>();
        // poison 只可能因锁内 panic 产生,槽是纯数据,中毒后继续读最后写入值好过永久卡死。
        let slot = state.retranscribing.lock().unwrap_or_else(|e| e.into_inner());
        let mut v = match slot.as_ref() {
            Some((note_id, stage)) => serde_json::json!({ "running": true, "note_id": note_id, "stage": stage }),
            None => serde_json::json!({ "running": false }),
        };
        // additive:running/note_id/stage 字段不动(前端 command 契约不变),
        // 新增 last 只给 UDS/MCP 轮询方——批量驱动靠它区分"完成"与"放弃/失败"。
        let last = state.retranscribe_last.lock().unwrap_or_else(|e| e.into_inner());
        v["last"] = match last.as_ref() {
            Some(ev) => serde_json::to_value(ev).unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        };
        v
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
        fn reaing(&self, note_id: Option<&str>) -> Result<serde_json::Value, String> {
            self.log(format!("reaing:{note_id:?}"));
            Ok(serde_json::json!({ "queued": note_id.map(|_| 1).unwrap_or(0) }))
        }
        fn retranscribe(
            &self,
            note_id: &str,
            input: &str,
            engine: Option<&str>,
        ) -> Result<serde_json::Value, String> {
            match engine {
                Some(e) => self.log(format!("retranscribe:{note_id}:{input}:{e}")),
                None => self.log(format!("retranscribe:{note_id}:{input}")),
            }
            Ok(serde_json::json!({ "started": true, "note_id": note_id, "input": input }))
        }
        fn retranscribe_status(&self) -> serde_json::Value {
            self.log("retranscribe_status");
            serde_json::json!({ "running": false, "last": null })
        }
    }

    fn req(op: &str) -> Req {
        Req { op: op.into(), title: None, tail: None, note_id: None, input: None, engine: None }
    }

    #[test]
    fn control_ops_gated_when_disabled() {
        let b = MockBackend::new(false);
        for op in ["start", "stop", "pause", "resume", "reaing", "retranscribe"] {
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
        assert!(dispatch_with(&b, &Req { op: "live".into(), title: None, tail: None, note_id: None, input: None, engine: None }).ok, "live 不受门控");
        assert!(b.called("status") && b.called("live:50"));
    }

    #[test]
    fn control_ops_routed_when_enabled() {
        let b = MockBackend::new(true);
        for op in ["start", "stop", "pause", "resume", "reaing"] {
            assert!(dispatch_with(&b, &req(op)).ok, "{op} 门控开时应放行");
        }
        // retranscribe 需要 note_id,不能复用通用 req() helper。
        assert!(
            dispatch_with(&b, &Req {
                op: "retranscribe".into(), title: None, tail: None,
                note_id: Some("n1".into()), input: Some("dual".into()), engine: None,
            }).ok,
            "retranscribe 门控开时应放行"
        );
        assert!(b.called("start:None") && b.called("stop") && b.called("pause") && b.called("resume"));
        assert!(b.called("reaing:None"), "reaing 门控开时应路由到 backend");
        assert!(b.called("retranscribe:n1:dual"), "retranscribe 门控开时应路由到 backend");
    }

    #[test]
    fn live_tail_clamped_and_defaulted() {
        let b = MockBackend::new(true);
        dispatch_with(&b, &Req { op: "live".into(), title: None, tail: Some(1000), note_id: None, input: None, engine: None });
        dispatch_with(&b, &Req { op: "live".into(), title: None, tail: Some(0), note_id: None, input: None, engine: None });
        dispatch_with(&b, &Req { op: "live".into(), title: None, tail: None, note_id: None, input: None, engine: None });
        assert!(b.called("live:500"), "上限 500");
        assert!(b.called("live:1"), "下限 1");
        assert!(b.called("live:50"), "缺省 50");
    }

    #[test]
    fn start_title_trimmed() {
        let b = MockBackend::new(true);
        dispatch_with(&b, &Req { op: "start".into(), title: Some("  评审会  ".into()), tail: None, note_id: None, input: None, engine: None });
        dispatch_with(&b, &Req { op: "start".into(), title: Some("   ".into()), tail: None, note_id: None, input: None, engine: None });
        assert!(b.called("start:Some(\"评审会\")"), "两端空白应 trim: {:?}", b.calls.borrow());
        assert!(b.called("start:None"), "纯空白 title → None");
    }

    /// retranscribe 过 control 门;retranscribe_status 是只读查询,不过 control 门。
    /// engine 覆盖要如实传到 backend(空串按"未指定"处理,免得 `--engine ""` 被
    /// 当成引擎名传下去撞校验)。
    #[test]
    fn retranscribe_engine_override_is_passed_through() {
        let b = MockBackend::new(true);
        let r = dispatch_with(&b, &Req {
            op: "retranscribe".into(), title: None, tail: None,
            note_id: Some("n1".into()), input: Some("dual".into()),
            engine: Some("  firered  ".into()),
        });
        assert!(r.ok);
        assert!(b.called("retranscribe:n1:dual:firered"), "engine 应 trim 后传下去");

        let b2 = MockBackend::new(true);
        let r2 = dispatch_with(&b2, &Req {
            op: "retranscribe".into(), title: None, tail: None,
            note_id: Some("n1".into()), input: Some("dual".into()), engine: Some("   ".into()),
        });
        assert!(r2.ok);
        assert!(b2.called("retranscribe:n1:dual"), "空串等于未指定");
    }

    #[test]
    fn retranscribe_gated_but_status_is_not() {
        let denied = MockBackend::new(false);
        let r = dispatch_with(&denied, &Req {
            op: "retranscribe".into(), title: None, tail: None,
            note_id: Some("n1".into()), input: Some("dual".into()), engine: None,
        });
        assert!(!r.ok, "control 关闭时 retranscribe 必须被拒");
        let r = dispatch_with(&denied, &Req {
            op: "retranscribe_status".into(), title: None, tail: None, note_id: None,
            input: None, engine: None,
        });
        assert!(r.ok, "status 查询不受 control 门控");
        assert!(denied.called("retranscribe_status"));

        let allowed = MockBackend::new(true);
        let r = dispatch_with(&allowed, &Req {
            op: "retranscribe".into(), title: None, tail: None,
            note_id: Some("n1".into()), input: None, engine: None,
        });
        assert!(r.ok);
        assert!(allowed.called("retranscribe:n1:dual"), "input 缺省应补 dual");
    }

    #[test]
    fn unknown_op_errors() {
        let b = MockBackend::new(true);
        let r = dispatch_with(&b, &req("bogus"));
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("未知 op: bogus"));
    }
}
