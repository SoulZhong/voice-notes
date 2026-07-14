//! 外部钩子(用户配置 shell/webhook):配置持久化 + 业务事件映射 + 执行体。
//!
//! 配置存 app_data_dir/hooks.json(原子写,模式同 settings.rs;独立文件,
//! 不与设置页抢 settings.json 的读-改-写窗口)。后端每次事件读快照,无内存
//! 状态同步。执行契约与 lifecycle::hooks::HookBus 一致:任何失败只记日志,
//! 绝不影响录制/精修主流程。

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use crate::lifecycle::machine::{LifecycleState, SessionState};

/// 一条钩子配置。event/kind 存字符串而非枚举:未知值只让该条失配,不让整个
/// hooks.json 反序列化失败(枚举会连带炸掉全表,老文件升级即中招)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookCfg {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// 事件白名单值,见 HookEvent::as_str。
    #[serde(default)]
    pub event: String,
    /// "shell" | "webhook"。
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksFile {
    #[serde(default)]
    pub hooks: Vec<HookCfg>,
}

fn default_kind() -> String {
    "shell".into()
}

fn default_true() -> bool {
    true
}

/// 缺失/损坏 → 空表(容忍,不报错;与 settings::load 同策略)。
pub fn load(app_data: &Path) -> HooksFile {
    std::fs::read_to_string(app_data.join("hooks.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(app_data: &Path, f: &HooksFile) -> anyhow::Result<()> {
    std::fs::create_dir_all(app_data)?;
    let tmp = app_data.join("hooks.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(f)?)?;
    std::fs::rename(&tmp, app_data.join("hooks.json"))?;
    Ok(())
}

/// 业务事件白名单:对用户暴露的稳定契约。内部状态机重构不改这些值,
/// 否则用户的 hooks.json 配置静默失效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    RecordingStarted,
    RecordingStopped,
    RecordingPaused,
    RecordingResumed,
    RefineStarted,
    RefineFinished,
}

impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookEvent::RecordingStarted => "recording_started",
            HookEvent::RecordingStopped => "recording_stopped",
            HookEvent::RecordingPaused => "recording_paused",
            HookEvent::RecordingResumed => "recording_resumed",
            HookEvent::RefineStarted => "refine_started",
            HookEvent::RefineFinished => "refine_finished",
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct HookFire {
    pub event: HookEvent,
    pub note_id: String,
}

/// 提交前后完整内核状态 → 业务事件列表(纯函数)。一次提交可能产出多个事件
/// (停录+自动精修同帧);顺序固定 session 先、refine 后,断言与日志都稳定。
pub fn hook_events(before: &LifecycleState, after: &LifecycleState) -> Vec<HookFire> {
    let mut out = Vec::new();
    match (&before.session, &after.session) {
        (SessionState::Recording { note_id, paused: false }, SessionState::Recording { note_id: id2, paused: true })
            if note_id == id2 =>
        {
            out.push(HookFire { event: HookEvent::RecordingPaused, note_id: note_id.clone() });
        }
        (SessionState::Recording { note_id, paused: true }, SessionState::Recording { note_id: id2, paused: false })
            if note_id == id2 =>
        {
            out.push(HookFire { event: HookEvent::RecordingResumed, note_id: note_id.clone() });
        }
        (from, SessionState::Recording { note_id, .. }) if !matches!(from, SessionState::Recording { .. }) => {
            out.push(HookFire { event: HookEvent::RecordingStarted, note_id: note_id.clone() });
        }
        (SessionState::Recording { note_id, .. } | SessionState::Stopping { note_id }, SessionState::Idle) => {
            out.push(HookFire { event: HookEvent::RecordingStopped, note_id: note_id.clone() });
        }
        _ => {}
    }
    let (added, removed) = before.refine.diff(&after.refine);
    out.extend(added.into_iter().map(|id| HookFire { event: HookEvent::RefineStarted, note_id: id }));
    out.extend(removed.into_iter().map(|id| HookFire { event: HookEvent::RefineFinished, note_id: id }));
    out
}

/// 构造钩子执行的环境变量向量。
pub fn shell_envs(event: &str, note_id: &str, title: &str) -> Vec<(String, String)> {
    vec![
        ("VN_EVENT".into(), event.into()),
        ("VN_NOTE_ID".into(), note_id.into()),
        ("VN_NOTE_TITLE".into(), title.into()),
    ]
}

/// 构造 webhook 载荷 JSON。
pub fn payload(event: &str, note_id: &str, title: &str, occurred_at: &str) -> serde_json::Value {
    serde_json::json!({
        "event": event,
        "note_id": note_id,
        "note_title": title,
        "occurred_at": occurred_at,
    })
}

/// 轮询 try_wait 实现超时:std 没有 wait_timeout,200ms 步进对 30s 上限的
/// 精度足够;超时 kill+wait 收尸,不留僵尸。
fn wait_timeout(child: &mut std::process::Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            return Some(st);
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// /bin/sh -c 执行;stdio 全接 null——钩子输出不是产品数据,要日志请命令自己重定向。
pub fn run_shell(command: &str, envs: &[(String, String)], limit: Duration) -> Result<i32, String> {
    let mut c = std::process::Command::new("/bin/sh");
    c.arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (k, v) in envs {
        c.env(k, v);
    }
    let mut child = c.spawn().map_err(|e| format!("启动失败: {e}"))?;
    match wait_timeout(&mut child, limit) {
        Some(st) => Ok(st.code().unwrap_or(-1)),
        None => Err(format!("超时({}s),已终止", limit.as_secs())),
    }
}

/// 发送 webhook POST 请求,返回 HTTP 状态码;非 2xx 为错误。
pub fn run_webhook(url: &str, payload: &serde_json::Value, limit: Duration) -> Result<u16, String> {
    match ureq::post(url)
        .timeout(limit)
        .set("content-type", "application/json")
        .send_string(&payload.to_string())
    {
        Ok(resp) => Ok(resp.status()),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(format!("请求失败: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::machine::{LifecycleState, SessionState};
    use std::time::Duration;

    fn st(session: SessionState) -> LifecycleState {
        LifecycleState { session, refine: Default::default() }
    }

    fn fire(event: HookEvent, id: &str) -> HookFire {
        HookFire { event, note_id: id.into() }
    }

    #[test]
    fn load_missing_or_corrupt_falls_back_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load(tmp.path()).hooks.is_empty(), "缺文件 → 空表");
        std::fs::write(tmp.path().join("hooks.json"), "not json").unwrap();
        assert!(load(tmp.path()).hooks.is_empty(), "损坏 → 空表");
    }

    #[test]
    fn save_then_load_roundtrip_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let f = HooksFile {
            hooks: vec![HookCfg {
                id: "h_1".into(),
                name: "停录归档".into(),
                event: "recording_stopped".into(),
                kind: "shell".into(),
                command: "echo done".into(),
                url: String::new(),
                enabled: true,
            }],
        };
        save(tmp.path(), &f).unwrap();
        let got = load(tmp.path());
        assert_eq!(got.hooks.len(), 1);
        assert_eq!(got.hooks[0].event, "recording_stopped");
        assert!(!tmp.path().join("hooks.json.tmp").exists(), "原子写不留 tmp");
    }

    #[test]
    fn missing_fields_take_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("hooks.json"),
            r#"{"hooks":[{"id":"h_2","event":"recording_started","command":"true"}]}"#,
        )
        .unwrap();
        let got = load(tmp.path());
        assert_eq!(got.hooks[0].kind, "shell", "kind 缺省 shell");
        assert!(got.hooks[0].enabled, "enabled 缺省 true");
    }

    #[test]
    fn session_transitions_map_to_events() {
        let rec = |paused| SessionState::Recording { note_id: "n1".into(), paused };
        // 开始(经 Starting,含续录同路径)
        assert_eq!(
            hook_events(&st(SessionState::Starting { resume_id: None }), &st(rec(false))),
            vec![fire(HookEvent::RecordingStarted, "n1")]
        );
        // 停止(Recording→Idle 与 Stopping→Idle 同义)
        assert_eq!(
            hook_events(&st(rec(false)), &st(SessionState::Idle)),
            vec![fire(HookEvent::RecordingStopped, "n1")]
        );
        assert_eq!(
            hook_events(&st(SessionState::Stopping { note_id: "n1".into() }), &st(SessionState::Idle)),
            vec![fire(HookEvent::RecordingStopped, "n1")]
        );
        // 暂停/恢复(同 id 的 paused 翻转)
        assert_eq!(
            hook_events(&st(rec(false)), &st(rec(true))),
            vec![fire(HookEvent::RecordingPaused, "n1")]
        );
        assert_eq!(
            hook_events(&st(rec(true)), &st(rec(false))),
            vec![fire(HookEvent::RecordingResumed, "n1")]
        );
        // 非迁移:Idle→Starting、原地不动,都不产事件
        assert!(hook_events(&st(SessionState::Idle), &st(SessionState::Starting { resume_id: None })).is_empty());
        assert!(hook_events(&st(SessionState::Idle), &st(SessionState::Idle)).is_empty());
    }

    #[test]
    fn refine_diff_maps_to_events_and_composes_with_session() {
        // 停录 + 同帧自动精修启动:一次提交两个事件,顺序 = session 事件在前
        let before = LifecycleState {
            session: SessionState::Recording { note_id: "n1".into(), paused: false },
            refine: Default::default(),
        };
        let mut after = LifecycleState { session: SessionState::Idle, refine: Default::default() };
        after.refine = before.refine.diff_test_insert("n1"); // 见 Step 3:测试辅助
        let got = hook_events(&before, &after);
        assert_eq!(
            got,
            vec![fire(HookEvent::RecordingStopped, "n1"), fire(HookEvent::RefineStarted, "n1")]
        );
        // 精修完成
        let done = LifecycleState { session: SessionState::Idle, refine: Default::default() };
        assert_eq!(hook_events(&after, &done), vec![fire(HookEvent::RefineFinished, "n1")]);
    }

    #[test]
    fn shell_envs_and_payload_shape() {
        let envs = shell_envs("recording_stopped", "n1", "周会");
        assert!(envs.contains(&("VN_EVENT".into(), "recording_stopped".into())));
        assert!(envs.contains(&("VN_NOTE_ID".into(), "n1".into())));
        assert!(envs.contains(&("VN_NOTE_TITLE".into(), "周会".into())));

        let p = payload("refine_finished", "n1", "周会", "2026-07-14T10:00:00+08:00");
        assert_eq!(p["event"], "refine_finished");
        assert_eq!(p["note_id"], "n1");
        assert_eq!(p["note_title"], "周会");
        assert_eq!(p["occurred_at"], "2026-07-14T10:00:00+08:00");
    }

    #[test]
    fn run_shell_exit_code_env_and_timeout() {
        let t = Duration::from_secs(5);
        assert_eq!(run_shell("exit 0", &[], t).unwrap(), 0);
        assert_eq!(run_shell("exit 3", &[], t).unwrap(), 3);
        // 环境变量注入:变量对得上才退 0
        let envs = shell_envs("recording_started", "n1", "t");
        assert_eq!(run_shell(r#"[ "$VN_EVENT" = recording_started ]"#, &envs, t).unwrap(), 0);
        // 超时:1s 限制跑 sleep 10 → Err 且不悬挂
        let start = std::time::Instant::now();
        assert!(run_shell("sleep 10", &[], Duration::from_secs(1)).is_err());
        assert!(start.elapsed() < Duration::from_secs(5), "超时后必须立刻返回");
    }
}
