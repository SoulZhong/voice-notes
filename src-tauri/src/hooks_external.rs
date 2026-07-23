//! 外部钩子(用户配置 shell/webhook):配置持久化 + 业务事件映射 + 执行体。
//!
//! 配置存 app_data_dir/hooks.json(原子写,模式同 settings.rs;独立文件,
//! 不与设置页抢 settings.json 的读-改-写窗口)。后端每次事件读快照,无内存
//! 状态同步。执行契约与 lifecycle::hooks::HookBus 一致:任何失败只记日志,
//! 绝不影响录制/Aing 主流程。

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use crate::lifecycle::machine::{LifecycleState, SessionState};
use tauri::Manager;

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
    /// 附带笔记内容:开启时执行注入笔记详情与全文(修订稿优先)。默认关,
    /// serde default 兼容老 hooks.json。
    #[serde(default)]
    pub include_note: bool,
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
///
/// 这是「派发容错」读取口:执行钩子的后台线程(run_fires)靠它取配置,任何原因
/// 读不到都不能让事件派发链路挂掉,所以损坏时静默退化为空表——但静默不等于
/// 无迹可查,存在且解析失败时打一条日志,方便用户/开发者事后翻日志定位。
/// 若要在 UI 层如实报错(不掩盖损坏、不让保存流程覆盖用户数据),用下面的
/// load_checked。
pub fn load(app_data: &Path) -> HooksFile {
    let path = app_data.join("hooks.json");
    match std::fs::read_to_string(&path) {
        Err(_) => HooksFile::default(), // 文件不存在(或不可读):首次使用的正常状态,不是错误
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("hooks: hooks.json 存在但解析失败,派发本轮按空表处理: {e}");
            HooksFile::default()
        }),
    }
}

/// 这是「UI 如实报错」读取口:`list_hooks` 命令用它把配置文件的真实状态如实
/// 回传前端。文件缺失是正常的空表初始状态,但文件存在且解析失败必须 Err——
/// 否则编辑页的 保存流程(listHooks → 改 → saveHooks)会在 listHooks 静默拿到
/// 空表后,把这份"假空表"整表写回,永久覆盖用户手编但仅仅是格式有误的原配置。
pub fn load_checked(app_data: &Path) -> Result<HooksFile, String> {
    let path = app_data.join("hooks.json");
    match std::fs::read_to_string(&path) {
        Err(_) => Ok(HooksFile::default()), // 文件缺失:未配置过,不是错误
        Ok(s) => serde_json::from_str(&s).map_err(|e| {
            format!("hooks.json 已损坏,无法解析: {e}。请手动修复该文件或删除后重新配置。")
        }),
    }
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
/// (停录+自动 Aing 同帧);顺序固定 session 先、refine 后,断言与日志都稳定。
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

/// actor 提交点唯一入口:同步映射(纯内存,不拖累 actor),有事件才起线程。
/// 契约与 HookBus 相同:执行层任何失败只记日志,绝不回传 actor。
pub fn dispatch(app: &tauri::AppHandle, before: &LifecycleState, after: &LifecycleState) {
    let fires = hook_events(before, after);
    if fires.is_empty() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || run_fires(&app, fires));
}

const SHELL_LIMIT: Duration = Duration::from_secs(30);
const WEBHOOK_LIMIT: Duration = Duration::from_secs(10);

/// 内嵌全文的字节上限:macOS execve 的 env+argv 总预算约 1MB,超限 spawn 直接
/// E2BIG 失败——截断是"都内嵌文本"方案的硬约束,不是优化。
pub const NOTE_TEXT_MAX: usize = 200_000;

/// 钩子附带的笔记内容(详情+全文)。text 为 markdown,可能被截断。
pub struct NoteContent {
    pub started_at: String,
    /// 空串 = 未结束。
    pub ended_at: String,
    pub duration_secs: u64,
    /// 显示名:名字 > 「说话人 N」。
    pub speakers: Vec<String>,
    pub text: String,
    pub truncated: bool,
}

/// 按 UTF-8 字符边界安全截断:上限落在多字节字符中间时回退,绝不产生半个字符。
pub fn truncate_utf8(s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// 说话人显示名兜底,对齐 Aing 正文标签(store/export.rs render_refined)的
/// 名字 > 关联人物全局编号(P 号)兜底语义——未命名但已关联库人物时,正文里
/// 印的是 P 号,这里若仍退回原始 speakers.json 的 S/P id 会与正文编号对不上。
/// person_id 有值时优先用它(去 P 前缀),否则退回 id 自身(去 P/S 前缀)。
pub fn speaker_display(id: &str, name: &str, person_id: Option<&str>) -> String {
    if !name.is_empty() {
        return name.to_string();
    }
    if let Some(pid) = person_id {
        return format!("说话人 {}", pid.trim_start_matches('P'));
    }
    format!("说话人 {}", id.trim_start_matches(['P', 'S']))
}

/// include_note 追加的环境变量(在 shell_envs 三件之上)。截断标记只在截断时
/// 注入——存在即真,脚本用 [ -n "$VN_NOTE_TEXT_TRUNCATED" ] 判断。
pub fn note_envs(c: &NoteContent) -> Vec<(String, String)> {
    let mut v = vec![
        ("VN_NOTE_TEXT".into(), c.text.clone()),
        ("VN_NOTE_STARTED_AT".into(), c.started_at.clone()),
        ("VN_NOTE_ENDED_AT".into(), c.ended_at.clone()),
        ("VN_NOTE_DURATION_SECS".into(), c.duration_secs.to_string()),
        ("VN_NOTE_SPEAKERS".into(), c.speakers.join("、")),
    ];
    if c.truncated {
        v.push(("VN_NOTE_TEXT_TRUNCATED".into(), "1".into()));
    }
    v
}

/// 笔记内容构建核心(可测):notes_dir 定笔记,data_root 有值时修订稿做声纹库
/// 现名 join(与 export_note 同款只读语义)。任何读盘失败回 None——内容是增值
/// 信息,由调用方决定跳过附带照常执行。
fn note_content_from_dirs(
    notes_dir: &std::path::Path,
    data_root: Option<&std::path::Path>,
    note_id: &str,
) -> Option<NoteContent> {
    let store = crate::store::NoteStore::new(notes_dir.to_path_buf());
    let note = store.load(note_id).ok()?;
    let text = match crate::store::load_refined(&notes_dir.join(note_id)) {
        Some(mut doc) => {
            if doc.paragraphs.iter().any(|p| p.person_id.is_some()) {
                if let Some(root) = data_root {
                    let vp = crate::store::VoiceprintStore::new(root.to_path_buf()).load();
                    crate::store::join_library_names(&mut doc, &vp);
                }
            }
            crate::store::render_refined(&note.meta.title, &doc, true)
        }
        None => store.render_loaded(&note, "md").ok()?,
    };
    let (text, truncated) = truncate_utf8(text, NOTE_TEXT_MAX);
    let duration_secs = note.segments.iter().map(|s| s.end_ms).max().unwrap_or(0) / 1000;
    Some(NoteContent {
        started_at: note.meta.started_at.clone(),
        ended_at: note.meta.ended_at.clone().unwrap_or_default(),
        duration_secs,
        speakers: note
            .speakers
            .iter()
            .map(|(id, m)| speaker_display(id, &m.name, m.person_id.as_deref()))
            .collect(),
        text,
        truncated,
    })
}

/// AppHandle 薄壳:解析两个根目录后进核心。
fn note_content(app: &tauri::AppHandle, note_id: &str) -> Option<NoteContent> {
    let notes = crate::notes_dir(app).ok()?;
    let root = crate::data_root(app).ok();
    note_content_from_dirs(&notes, root.as_deref(), note_id)
}

fn run_fires(app: &tauri::AppHandle, fires: Vec<HookFire>) {
    let Ok(app_data) = app.path().app_data_dir() else {
        eprintln!("hooks: app_data_dir 不可用,本批事件放弃");
        return;
    };
    let cfgs = load(&app_data).hooks;
    // 批内内容缓存:停录+自动 Aing 同帧多事件共享同一 note_id,只构建一次。
    // Option 也缓存——构建失败(笔记刚删)同批不再重试,只记一次日志。
    let mut contents: std::collections::HashMap<String, Option<NoteContent>> =
        std::collections::HashMap::new();
    for f in &fires {
        let event = f.event.as_str();
        let matched: Vec<&HookCfg> =
            cfgs.iter().filter(|c| c.enabled && c.event == event).collect();
        if matched.is_empty() {
            continue;
        }
        // 标题尽力而为:拿不到(笔记刚建/已删)用空串,不因标题挡执行。
        let title = crate::notes_dir(app)
            .ok()
            .and_then(|d| crate::store::NoteStore::new(d).title(&f.note_id))
            .unwrap_or_default();
        let occurred_at = chrono::Local::now().to_rfc3339();
        let need_note = matched.iter().any(|c| c.include_note);
        let content = if need_note {
            contents
                .entry(f.note_id.clone())
                .or_insert_with(|| {
                    let c = note_content(app, &f.note_id);
                    if c.is_none() {
                        eprintln!("hooks: 笔记内容构建失败({}),照常触发但不附带", f.note_id);
                    }
                    c
                })
                .as_ref()
        } else {
            None
        };
        for cfg in matched {
            let note = if cfg.include_note { content } else { None };
            let r = match cfg.kind.as_str() {
                "webhook" => run_webhook(&cfg.url, &payload(event, &f.note_id, &title, &occurred_at, note), WEBHOOK_LIMIT)
                    .map(|s| format!("HTTP {s}")),
                // 非 0 退出码算失败,与 test_run 语义一致——否则"完成"日志会把真实的
                // 命令失败盖过去,排查通道形同虚设。
                _ => {
                    let mut envs = shell_envs(event, &f.note_id, &title);
                    if let Some(c) = note {
                        envs.extend(note_envs(c));
                    }
                    match run_shell(&cfg.command, &envs, SHELL_LIMIT) {
                        Ok(0) => Ok("退出码 0".into()),
                        Ok(c) => Err(format!("退出码 {c}")),
                        Err(e) => Err(e),
                    }
                }
            };
            match r {
                Ok(msg) => eprintln!("hooks: '{}' [{}] 完成({msg})", cfg.name, event),
                Err(e) => eprintln!("hooks: '{}' [{}] 失败: {e}(已忽略,不影响主流程)", cfg.name, event),
            }
        }
    }
}

/// 配置页「测试」按钮:以假载荷立即执行一次,结果如实回传 UI。
/// 超时收紧到 10s——测试是交互动作,30s 转圈没人等得起。
pub fn test_run(cfg: &HookCfg) -> Result<String, String> {
    let event = if cfg.event.is_empty() { "recording_stopped" } else { cfg.event.as_str() };
    let occurred_at = chrono::Local::now().to_rfc3339();
    // 假内容:测试不读库,注入固定占位——用户看得出变量有值即可。
    let fake = cfg.include_note.then(|| NoteContent {
        started_at: "2026-07-14T10:00:00+08:00".into(),
        ended_at: "2026-07-14T11:00:00+08:00".into(),
        duration_secs: 3600,
        speakers: vec!["测试说话人".into()],
        text: "测试正文".into(),
        truncated: false,
    });
    match cfg.kind.as_str() {
        "webhook" => run_webhook(&cfg.url, &payload(event, "note-test", "测试笔记", &occurred_at, fake.as_ref()), WEBHOOK_LIMIT)
            .map(|s| format!("HTTP {s}")),
        _ => {
            let mut envs = shell_envs(event, "note-test", "测试笔记");
            if let Some(c) = &fake {
                envs.extend(note_envs(c));
            }
            match run_shell(&cfg.command, &envs, Duration::from_secs(10)) {
                Ok(0) => Ok("退出码 0".into()),
                Ok(c) => Err(format!("退出码 {c}")),
                Err(e) => Err(e),
            }
        }
    }
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
pub fn payload(
    event: &str,
    note_id: &str,
    title: &str,
    occurred_at: &str,
    note: Option<&NoteContent>,
) -> serde_json::Value {
    let mut p = serde_json::json!({
        "event": event,
        "note_id": note_id,
        "note_title": title,
        "occurred_at": occurred_at,
    });
    if let Some(c) = note {
        p["note"] = serde_json::json!({
            "started_at": c.started_at,
            "ended_at": c.ended_at,
            "duration_secs": c.duration_secs,
            "speakers": c.speakers,
            "text": c.text,
            "text_truncated": c.truncated,
        });
    }
    p
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

/// 使用系统原生 shell 执行（Windows 为 cmd.exe，Unix 为 /bin/sh）；
/// stdio 全接 null——钩子输出不是产品数据，要日志请命令自己重定向。
pub fn run_shell(command: &str, envs: &[(String, String)], limit: Duration) -> Result<i32, String> {
    #[cfg(windows)]
    let mut c = {
        let mut c = std::process::Command::new("cmd.exe");
        c.args(["/S", "/C"]);
        c
    };
    #[cfg(not(windows))]
    let mut c = {
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c");
        c
    };
    c.arg(command)
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
        assert!(load(tmp.path()).hooks.is_empty(), "损坏 → 空表(派发路径容错语义不变)");
    }

    #[test]
    fn load_checked_errs_on_corrupt_but_oks_on_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // 缺失文件:未配置过的正常初始状态,不是错误
        assert!(load_checked(tmp.path()).unwrap().hooks.is_empty(), "缺文件 → Ok(空表)");
        // 存在且解析失败:必须如实报错,UI 才能点亮横幅、编辑页 save 流程才会中止而非覆盖
        std::fs::write(tmp.path().join("hooks.json"), "not json").unwrap();
        let err = load_checked(tmp.path()).expect_err("损坏文件必须 Err");
        assert!(err.contains("损坏"), "错误信息应提示文件已损坏: {err}");
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
                include_note: false,
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
        // 停录 + 同帧自动 Aing 启动:一次提交两个事件,顺序 = session 事件在前
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
        // Aing 完成
        let done = LifecycleState { session: SessionState::Idle, refine: Default::default() };
        assert_eq!(hook_events(&after, &done), vec![fire(HookEvent::RefineFinished, "n1")]);
    }

    #[test]
    fn shell_envs_and_payload_shape() {
        let envs = shell_envs("recording_stopped", "n1", "周会");
        assert!(envs.contains(&("VN_EVENT".into(), "recording_stopped".into())));
        assert!(envs.contains(&("VN_NOTE_ID".into(), "n1".into())));
        assert!(envs.contains(&("VN_NOTE_TITLE".into(), "周会".into())));

        let p = payload("refine_finished", "n1", "周会", "2026-07-14T10:00:00+08:00", None);
        assert_eq!(p["event"], "refine_finished");
        assert_eq!(p["note_id"], "n1");
        assert_eq!(p["note_title"], "周会");
        assert_eq!(p["occurred_at"], "2026-07-14T10:00:00+08:00");
    }

    #[test]
    fn run_shell_exit_code_env_and_timeout() {
        let t = Duration::from_secs(5);
        assert_eq!(run_shell("exit 0", &[], t).unwrap(), 0);
        #[cfg(not(windows))]
        assert_eq!(run_shell("exit 3", &[], t).unwrap(), 3);
        #[cfg(windows)]
        assert_eq!(run_shell("exit /b 3", &[], t).unwrap(), 3);
        // 环境变量注入:变量对得上才退 0
        let envs = shell_envs("recording_started", "n1", "t");
        #[cfg(not(windows))]
        assert_eq!(run_shell(r#"[ "$VN_EVENT" = recording_started ]"#, &envs, t).unwrap(), 0);
        #[cfg(windows)]
        assert_eq!(
            run_shell(
                r#"if "%VN_EVENT%"=="recording_started" (exit /b 0) else (exit /b 1)"#,
                &envs,
                t,
            )
            .unwrap(),
            0
        );
        // 超时:1s 限制跑 sleep 10 → Err 且不悬挂
        let start = std::time::Instant::now();
        #[cfg(not(windows))]
        assert!(run_shell("sleep 10", &[], Duration::from_secs(1)).is_err());
        #[cfg(windows)]
        assert!(
            run_shell(
                "ping 127.0.0.1 -n 10 >nul",
                &[],
                Duration::from_secs(1),
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(5), "超时后必须立刻返回");
    }

    #[test]
    fn include_note_defaults_false_on_old_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("hooks.json"),
            r#"{"hooks":[{"id":"h_old","event":"recording_stopped","command":"true"}]}"#,
        )
        .unwrap();
        assert!(!load(tmp.path()).hooks[0].include_note, "老配置缺字段 → false");
    }

    #[test]
    fn truncate_utf8_respects_char_boundary() {
        // 「会」UTF-8 三字节:上限落在字符中间必须回退到边界,不产生半个字符
        let (out, cut) = truncate_utf8("会议纪要".to_string(), 4);
        assert_eq!(out, "会");
        assert!(cut);
        let (out, cut) = truncate_utf8("会议".to_string(), 6);
        assert_eq!(out, "会议");
        assert!(!cut, "恰好等长不算截断");
        let (out, cut) = truncate_utf8(String::new(), 10);
        assert_eq!(out, "");
        assert!(!cut);
    }

    #[test]
    fn speaker_display_prefers_name_falls_back_to_number() {
        // 有名字:忽略 person_id,直接用名字
        assert_eq!(speaker_display("P3", "张三", Some("P9")), "张三");
        assert_eq!(speaker_display("P3", "张三", None), "张三");
        // 无名字但已关联库人物(person_id 有值):对齐 Aing 正文标签的 P 号,不用原始 id
        assert_eq!(speaker_display("S1", "", Some("P9")), "说话人 9");
        assert_eq!(speaker_display("P3", "", Some("P9")), "说话人 9");
        // 无名字且未关联(person_id 为 None):退回原始 speakers.json id,去 P/S 前缀
        assert_eq!(speaker_display("P3", "", None), "说话人 3");
        assert_eq!(speaker_display("S1", "", None), "说话人 1");
    }

    fn content_fixture() -> NoteContent {
        NoteContent {
            started_at: "2026-07-14T10:00:00+08:00".into(),
            ended_at: "2026-07-14T11:00:00+08:00".into(),
            duration_secs: 3600,
            speakers: vec!["张三".into(), "说话人 2".into()],
            text: "# 占位正文".into(),
            truncated: false,
        }
    }

    #[test]
    fn note_envs_and_payload_shapes() {
        let c = content_fixture();
        let envs = note_envs(&c);
        assert!(envs.contains(&("VN_NOTE_TEXT".into(), "# 占位正文".into())));
        assert!(envs.contains(&("VN_NOTE_STARTED_AT".into(), "2026-07-14T10:00:00+08:00".into())));
        assert!(envs.contains(&("VN_NOTE_ENDED_AT".into(), "2026-07-14T11:00:00+08:00".into())));
        assert!(envs.contains(&("VN_NOTE_DURATION_SECS".into(), "3600".into())));
        assert!(envs.contains(&("VN_NOTE_SPEAKERS".into(), "张三、说话人 2".into())));
        assert!(!envs.iter().any(|(k, _)| k == "VN_NOTE_TEXT_TRUNCATED"), "未截断不注入标记");

        let mut cut = content_fixture();
        cut.truncated = true;
        assert!(note_envs(&cut).contains(&("VN_NOTE_TEXT_TRUNCATED".into(), "1".into())));

        let p = payload("refine_finished", "n1", "周会", "2026-07-14T11:00:01+08:00", Some(&c));
        assert_eq!(p["note"]["duration_secs"], 3600);
        assert_eq!(p["note"]["speakers"][0], "张三");
        assert_eq!(p["note"]["text"], "# 占位正文");
        assert_eq!(p["note"]["text_truncated"], false);
        // 未附带时与现状逐字一致(回归防漂移):没有 note 键
        let p0 = payload("refine_finished", "n1", "周会", "2026-07-14T11:00:01+08:00", None);
        assert!(p0.get("note").is_none());
        assert_eq!(p0["event"], "refine_finished");
    }

    fn write_note_fixture(notes_dir: &std::path::Path, id: &str) {
        let dir = notes_dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"schema_version":1,"id":"n1","title":"占位标题","started_at":"2026-07-14T10:00:00+08:00","ended_at":"2026-07-14T11:00:00+08:00","state":"complete"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("segments.jsonl"),
            r#"{"seq":1,"source":"mic","text":"占位甲","start_ms":0,"end_ms":2000,"speaker":"S1"}
{"seq":2,"source":"mic","text":"占位乙","start_ms":2000,"end_ms":5000,"speaker":"S2"}
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("speakers.json"),
            r#"{"S1":{"name":"张三","sources":["mic"]},"S2":{"name":"","sources":["mic"]}}"#,
        )
        .unwrap();
    }

    #[test]
    fn note_content_raw_fallback_and_details() {
        let tmp = tempfile::tempdir().unwrap();
        write_note_fixture(tmp.path(), "n1");
        let c = note_content_from_dirs(tmp.path(), None, "n1").unwrap();
        assert_eq!(c.started_at, "2026-07-14T10:00:00+08:00");
        assert_eq!(c.ended_at, "2026-07-14T11:00:00+08:00");
        assert_eq!(c.duration_secs, 5, "时长=段落最大 end_ms(5000)/1000");
        assert_eq!(c.speakers, vec!["张三".to_string(), "说话人 2".to_string()]);
        assert!(c.text.contains("占位甲"), "无修订稿回落原始稿渲染");
        assert!(!c.truncated);
    }

    #[test]
    fn note_content_prefers_refined() {
        let tmp = tempfile::tempdir().unwrap();
        write_note_fixture(tmp.path(), "n1");
        // 字段形状以 store/refined.rs 的 RefinedDoc/RefinedParagraph 为准:
        // generated_at 无 serde default 必须给,段落显示名字段是 name 不是 label。
        std::fs::write(
            tmp.path().join("n1").join("refined.json"),
            r#"{"schema_version":1,"generated_at":"2026-07-14T11:00:00+08:00","stages":{"filter":"done","recluster":"done","llm":"done"},"paragraphs":[{"speaker":"R1","name":"张三","start_ms":0,"end_ms":5000,"text":"Aing 占位正文","source_seqs":[1,2]}]}"#,
        )
        .unwrap();
        let c = note_content_from_dirs(tmp.path(), None, "n1").unwrap();
        assert!(c.text.contains("Aing 占位正文"), "修订稿在盘时优先");
        assert!(!c.text.contains("占位甲"), "不再是原始稿");
    }

    #[test]
    fn note_content_missing_note_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(note_content_from_dirs(tmp.path(), None, "nope").is_none());
    }
}
