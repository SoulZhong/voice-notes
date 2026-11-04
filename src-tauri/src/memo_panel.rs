//! 备注浮窗:⌥⌘M / 托盘「加备注…」钉住时间后弹出的小输入窗(设计「定死的选择 7」,路线 A)。
//!
//! 普通 WebviewWindow,label `memo`,路由 `/memo-panel`;首次创建后隐藏复用。弹出前记下
//! 当前前台应用,关闭(回车保存 / Esc / 失焦)时隐藏浮窗并把焦点还给它——用户在会议软件里
//! 按快捷键,记完一句就回到会议软件,不必切回 Voice Notes。
//!
//! 浮窗首次创建时页面还没加载,事件会丢;所以「当前要编辑哪一条」放在 [`PENDING`] 里,
//! 页面挂载时用 `memo_panel_state` 主动取,之后每次弹出再 emit `memo_panel_open`。
//!
//! 已知风险(真机冒烟):macOS 上激活本应用时主窗口可能被一起带到前台;若实测挡住会议
//! 软件,改走路线 B(非激活式 NSPanel)。Windows 只做基本实现,未经真机验证。

use crate::store::memos::Memo;
use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub const LABEL: &str = "memo";

#[derive(Debug, Clone, Serialize)]
pub struct PanelOpen {
    pub note_id: String,
    pub memo: Memo,
}

static PENDING: Mutex<Option<PanelOpen>> = Mutex::new(None);

/// 弹出前的前台应用(macOS: pid;Windows: HWND)。还焦点用,用完即清。
static PREV_FRONT: Mutex<Option<isize>> = Mutex::new(None);

/// 显示浮窗并让它编辑 `memo`。可从任意线程调用:窗口操作投递到主线程,立即返回
/// (fire-and-forget,不在调用线程上同步等主线程——见快捷键死锁注记)。
pub fn show(app: &AppHandle, note_id: &str, memo: &Memo) {
    let payload = PanelOpen { note_id: note_id.to_string(), memo: memo.clone() };
    *PENDING.lock().unwrap() = Some(payload.clone());
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        let already_visible = app2.get_webview_window(LABEL).and_then(|w| w.is_visible().ok()).unwrap_or(false);
        if !already_visible {
            *PREV_FRONT.lock().unwrap() = platform::frontmost_other();
        }
        let w = match app2.get_webview_window(LABEL) {
            Some(w) => w,
            None => match build(&app2) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("浮窗创建失败: {e}");
                    return;
                }
            },
        };
        let _ = w.show();
        let _ = w.set_focus();
        let _ = app2.emit_to(LABEL, "memo_panel_open", payload);
    });
}

fn build(app: &AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    let mut b = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("memo-panel".into()))
        .title("Voice Notes")
        .inner_size(440.0, 150.0)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .focused(true)
        .visible(false);
    // 放在屏幕上方居中:不挡会议画面中央,也不贴菜单栏。
    if let Some(m) = app.primary_monitor().ok().flatten() {
        let scale = m.scale_factor();
        let size = m.size().to_logical::<f64>(scale);
        b = b.position(((size.width - 440.0) / 2.0).max(0.0), (size.height * 0.18).max(40.0));
    }
    b.build()
}

/// 页面挂载时取当前要编辑的那条(首次创建时事件可能早于页面加载)。
#[tauri::command]
pub fn memo_panel_state() -> Option<PanelOpen> {
    PENDING.lock().unwrap().clone()
}

/// 隐藏浮窗,把焦点还给弹出前的前台应用。保存与否由页面先调 `edit_memo_line` 决定。
#[tauri::command]
pub fn memo_panel_hide(app: AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window(LABEL) {
            let _ = w.hide();
        }
        *PENDING.lock().unwrap() = None;
        if let Some(prev) = PREV_FRONT.lock().unwrap().take() {
            platform::activate(prev);
        }
    });
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};

    /// 当前前台应用的 pid;前台就是本应用时返回 None(没有可还的焦点)。
    pub fn frontmost_other() -> Option<isize> {
        let ws = NSWorkspace::sharedWorkspace();
        let front = ws.frontmostApplication()?;
        let pid = front.processIdentifier();
        if pid == std::process::id() as i32 {
            return None;
        }
        Some(pid as isize)
    }

    pub fn activate(pid: isize) {
        if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32) {
            #[allow(deprecated)]
            app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow};

    pub fn frontmost_other() -> Option<isize> {
        // SAFETY: 纯查询 API,无入参指针以外的前置条件;pid 出参指向栈上变量。
        unsafe {
            let h = GetForegroundWindow();
            if h.is_null() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, &mut pid);
            if pid == std::process::id() {
                return None;
            }
            Some(h as isize)
        }
    }

    pub fn activate(hwnd: isize) {
        // SAFETY: HWND 可能已失效,SetForegroundWindow 对无效句柄只返回失败。
        unsafe {
            SetForegroundWindow(hwnd as _);
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    pub fn frontmost_other() -> Option<isize> {
        None
    }
    pub fn activate(_: isize) {}
}
