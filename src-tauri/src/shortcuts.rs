//! 全局快捷键的注册与分发。为什么独立成模块:注册失败必须能作为 Result 上抛给设置页
//! (据此提示用户并回落关开关),而按键分发要在插件 handler 里静默切换录制——两件事都
//! 围绕 GlobalShortcut 插件,聚在一处便于对照。
//!
//! 两个键(会议备注加入后):
//! - **录制键**:按设置常驻注册,开停录。
//! - **备注键**:只在录音中注册(进入录制注册、回到空闲注销,见 [`MemoShortcutHook`]),
//!   按下即钉住当前录音时间并弹浮窗。停录即释放给其它应用。
//!
//! 插件 handler 对**每一个**已注册的键都会触发,所以 [`on_shortcut`] 必须按键分发——
//! 不分发的话按一下备注键会把录音停掉。两个键各自定向注册/注销,绝不 `unregister_all`
//! (录音中在设置页保存录制键,不能把备注键一起清掉)。

use std::sync::Mutex;
use tauri::Manager;
use tauri_plugin_global_shortcut::{Shortcut, ShortcutEvent, ShortcutState};

/// 当前注册着的录制键 / 备注键(定向注销用)。
static RECORD_KEY: Mutex<Option<Shortcut>> = Mutex::new(None);
static MEMO_KEY: Mutex<Option<Shortcut>> = Mutex::new(None);

#[derive(Debug, PartialEq, Eq)]
enum Action {
    ToggleRecording,
    MarkMemo,
    Ignore,
}

/// 按键分发(纯函数,便于测):只在 Pressed 沿动作——同一次按键会先后触发 Pressed/Released,
/// 两沿都动作会开录又立刻停录。
fn dispatch(sc: &Shortcut, state: ShortcutState, record: Option<&Shortcut>, memo: Option<&Shortcut>) -> Action {
    if state != ShortcutState::Pressed {
        return Action::Ignore;
    }
    if record == Some(sc) {
        Action::ToggleRecording
    } else if memo == Some(sc) {
        Action::MarkMemo
    } else {
        Action::Ignore
    }
}

/// 插件 handler。快捷键触发没有 UI 上下文,toggle_recording 内部的开录错误只进日志。
pub fn on_shortcut(app: &tauri::AppHandle, sc: &Shortcut, ev: ShortcutEvent) {
    let record = RECORD_KEY.lock().unwrap().clone();
    let memo = MEMO_KEY.lock().unwrap().clone();
    match dispatch(sc, ev.state, record.as_ref(), memo.as_ref()) {
        Action::ToggleRecording => crate::toggle_recording(app),
        Action::MarkMemo => crate::memo::mark_now(app, "hotkey"),
        Action::Ignore => {}
    }
}

fn load_settings(app: &tauri::AppHandle) -> crate::settings::Settings {
    // settings.json 是自举指针,永在 app_data_dir;读不到目录则退回默认设置。
    match app.path().app_data_dir() {
        Ok(d) => crate::settings::load(&d),
        Err(_) => crate::settings::Settings::default(),
    }
}

/// 依据当前设置(重)注册**录制键**。设置页每次保存都会调本函数:先定向注销上次注册的
/// 录制键再按需注册,保证幂等且不碰备注键。parse/register 失败一律上抛中文错误——
/// 这是设置页保存路径(apply_shortcut)的判据,用于提示并回落关闭开关;setup 启动路径
/// 对同一个 Err 只 eprintln 不挡启动。
///
/// 末尾顺带按当前录制状态重对一次备注键:设置页可能刚改了备注键或它的开关。
pub fn apply_from_settings(app: &tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let s = load_settings(app);
    if let Some(old) = RECORD_KEY.lock().unwrap().take() {
        let _ = app.global_shortcut().unregister(old);
    }
    let result = if s.shortcut_enabled {
        match s.shortcut.parse::<Shortcut>() {
            Err(e) => Err(crate::tr!("快捷键格式无效: {e}", "Invalid shortcut format: {e}")),
            Ok(sc) => match app.global_shortcut().register(sc) {
                Ok(()) => {
                    *RECORD_KEY.lock().unwrap() = Some(sc);
                    Ok(())
                }
                Err(e) => Err(crate::tr!(
                    "快捷键注册失败(可能与系统或其它应用冲突): {e}",
                    "Failed to register the shortcut (it may conflict with the system or another app): {e}"
                )),
            },
        }
    } else {
        Ok(())
    };
    let recording = app.state::<crate::AppState>().session.lock().unwrap().is_some();
    sync_memo_key(app, recording);
    result
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoShortcutStatus {
    pub ok: bool,
    /// 注册失败原因(被别的应用占用、格式错、与录制键相同);ok 时为空。
    pub reason: String,
    pub shortcut: String,
}

/// 按「是否录音中」注册或注销备注键。**必须在主线程上调**(插件 register/unregister
/// 在 macOS 上派到主线程同步等结果;从 actor 线程同步调会与等 actor 的主线程成环)——
/// 调用方一律经 `run_on_main_thread` 投递(见 [`MemoShortcutHook`])。
/// 注册失败只 emit `memo_shortcut_status`,不影响录音。
pub fn sync_memo_key(app: &tauri::AppHandle, recording: bool) {
    use tauri::Emitter;
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    if let Some(old) = MEMO_KEY.lock().unwrap().take() {
        let _ = app.global_shortcut().unregister(old);
    }
    let s = load_settings(app);
    if !recording || !s.memo_shortcut_enabled {
        return;
    }
    let status = |ok: bool, reason: String| MemoShortcutStatus { ok, reason, shortcut: s.memo_shortcut.clone() };
    let sc = match s.memo_shortcut.parse::<Shortcut>() {
        Ok(sc) => sc,
        Err(e) => {
            let _ = app.emit("memo_shortcut_status", status(false, e.to_string()));
            return;
        }
    };
    if RECORD_KEY.lock().unwrap().as_ref() == Some(&sc) {
        let _ = app.emit(
            "memo_shortcut_status",
            status(false, crate::tr!("与录制快捷键相同", "Same as the recording shortcut")),
        );
        return;
    }
    match app.global_shortcut().register(sc) {
        Ok(()) => {
            *MEMO_KEY.lock().unwrap() = Some(sc);
            let _ = app.emit("memo_shortcut_status", status(true, String::new()));
        }
        Err(e) => {
            eprintln!("备注快捷键注册失败: {e}");
            let _ = app.emit("memo_shortcut_status", status(false, e.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri_plugin_global_shortcut::{Code, Modifiers};

    fn sc(code: Code) -> Shortcut {
        Shortcut::new(Some(Modifiers::ALT | Modifiers::SUPER), code)
    }

    #[test]
    fn dispatch_routes_each_key_to_its_own_action() {
        let (r, m) = (sc(Code::KeyR), sc(Code::KeyM));
        assert_eq!(dispatch(&r, ShortcutState::Pressed, Some(&r), Some(&m)), Action::ToggleRecording);
        assert_eq!(dispatch(&m, ShortcutState::Pressed, Some(&r), Some(&m)), Action::MarkMemo);
        assert_eq!(dispatch(&m, ShortcutState::Released, Some(&r), Some(&m)), Action::Ignore);
        // 备注键没注册(未在录音)时,同一组合按下不能误触发开停录
        assert_eq!(dispatch(&m, ShortcutState::Pressed, Some(&r), None), Action::Ignore);
        assert_eq!(dispatch(&sc(Code::KeyK), ShortcutState::Pressed, Some(&r), Some(&m)), Action::Ignore);
    }

    #[test]
    fn default_memo_shortcut_parses_and_differs_from_record_key() {
        let m: Shortcut = crate::settings::default_memo_shortcut().parse().unwrap();
        let r: Shortcut = "Alt+CmdOrCtrl+R".parse().unwrap();
        assert_ne!(m, r);
    }
}
