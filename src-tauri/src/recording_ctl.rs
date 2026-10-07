//! 程序化录制控制:查状态、开录(带开录前风险)、停录。
//!
//! 两个调用方:MCP 控制面(mcp/uds.rs,仅 Unix)与设备听写(device,双击 OK 开始/
//! 停止录音,两个平台都有)。原先这些逻辑长在 uds.rs 的 AppBackend 里,Windows 不编
//! uds.rs,设备搬进来后需要一份跨平台的,于是挪到这里,两边共用同一份行为。
//! 授权门控不在这里:MCP 的门控仍在 uds.rs 的 dispatch_with;设备是用户手里的
//! 实体按键,本就是用户本人的操作,不过「允许 AI 控制录制」那道门。

use tauri::Manager;

/// 录制状态快照(与 recording_status 命令同源:session 槽)。
pub fn status(app: &tauri::AppHandle) -> serde_json::Value {
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

/// 开录并等到会话入槽,返回 `{note_id, risks}`。
pub fn start(app: &tauri::AppHandle, title: Option<&str>) -> Result<serde_json::Value, String> {
    // 开录前风险随返回值带出(Codex review P2):MCP 是无 UI 上下文的第三条开录
    // 入口,既走不了确认对话框,也享受不到录制页横幅兜底——它不会把用户导航到
    // 那一页。所以这里不拦(程序化调用拦不了),但必须把风险如实交给调用方,
    // 让 AI 助手能转达给人,而不是静默录一场残缺的会。设备那边把风险显示在设备屏上。
    let risks = crate::precheck::record_risks(
        crate::audio::mic_mode::active(),
        crate::audio::default_input_is_bluetooth(),
    );
    if !risks.is_empty() {
        eprintln!(
            "程序化开录: 开录前检测到风险 {:?},已随返回值带出(不拦)",
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
                    eprintln!("程序化开录: 设标题失败(录制已开始,不回滚): {e}");
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

/// 停录(阻塞至收尾完成),返回 `{note_id}`;没有录制时报错。
pub fn stop(app: &tauri::AppHandle) -> Result<serde_json::Value, String> {
    let note_id = status(app)["note_id"].as_str().unwrap_or_default().to_string();
    if note_id.is_empty() {
        return Err("没有正在进行的录制".into());
    }
    // 经 actor 串行执行停录(P2:teardown+自投 Finalize)——阻塞至收尾完成,本线程等待无妨。
    app.state::<crate::lifecycle::LifecycleHandle>()
        .command(crate::lifecycle::Cmd::Stop)?;
    Ok(serde_json::json!({ "note_id": note_id }))
}
