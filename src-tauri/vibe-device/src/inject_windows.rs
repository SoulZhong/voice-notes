//! Windows app control for Supported Apps: foreground switching, window
//! titles, clipboard paste with restore, and synthesized keys via SendInput.
//!
//! Apps are identified by executable name (see `config`). Windows refuses
//! `SetForegroundWindow` from a background process unless it "received the
//! last input"; we synthesize an Alt tap first (the usual workaround) and fall
//! back to attaching to the foreground thread. Input into a window of an
//! elevated (administrator) process is silently dropped by UIPI, so that case
//! is detected up front and reported as a permission problem.

use crate::config::app_ids;
use crate::session::{InjectError, Injector};
use std::thread::sleep;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HGLOBAL, HWND, LPARAM};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcess, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_MENU, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GW_OWNER, GetForegroundWindow, GetWindow,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::{BOOL, PCWSTR, PWSTR, w};

const VK_V: VIRTUAL_KEY = VIRTUAL_KEY(0x56);
/// How long the pasted text stays on the clipboard before restoring.
const RESTORE_DELAY: Duration = Duration::from_millis(400);
const ACTIVATE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Lower-case executable file name of a process.
fn exe_name(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        ok.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/'])
            .next()
            .map(|n| n.to_ascii_lowercase())
    }
}

/// All running processes as (pid, lower-case exe name).
fn processes() -> Vec<(u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snap, &mut e).is_ok();
        while more {
            let end = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.push((
                e.th32ProcessID,
                String::from_utf16_lossy(&e.szExeFile[..end]).to_ascii_lowercase(),
            ));
            more = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

fn matches(ids: &[String], exe: &str) -> bool {
    ids.iter().any(|i| i.eq_ignore_ascii_case(exe))
}

/// Whether an app with this id (or an alias) is running.
/// Not tracked on Windows: polling keeps its awake pace.
pub fn display_asleep() -> bool {
    false
}

pub fn app_running(id: &str) -> bool {
    let ids = app_ids(id);
    processes().iter().any(|(_, exe)| matches(&ids, exe))
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(hwnd);
        if n <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

/// Visible, unowned top-level windows with a title (what Alt+Tab shows).
fn app_windows() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let list = &mut *(lparam.0 as *mut Vec<HWND>);
            let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid());
            if IsWindowVisible(hwnd).as_bool() && !owned && GetWindowTextLengthW(hwnd) > 0 {
                list.push(hwnd);
            }
        }
        BOOL(1)
    }
    let mut list: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut Vec<HWND> as isize));
    }
    list
}

/// The app's main window: the foreground one if it belongs to the app,
/// else its first top-level window (EnumWindows lists in z-order).
fn main_window(id: &str) -> Option<HWND> {
    let ids = app_ids(id);
    let pids: Vec<u32> = processes()
        .into_iter()
        .filter(|(_, exe)| matches(&ids, exe))
        .map(|(pid, _)| pid)
        .collect();
    if pids.is_empty() {
        return None;
    }
    let fg = unsafe { GetForegroundWindow() };
    if !fg.is_invalid() && pids.contains(&window_pid(fg)) {
        return Some(fg);
    }
    app_windows()
        .into_iter()
        .find(|w| pids.contains(&window_pid(*w)))
}

fn token_elevated(process: HANDLE) -> Option<bool> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut e = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let r = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut e as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        r.ok()?;
        Some(e.TokenIsElevated != 0)
    }
}

/// Whether input into `pid`'s windows would be dropped by UIPI: the target
/// runs elevated and we do not. A token we may not even open is elevated.
fn blocked_by_uipi(pid: u32) -> bool {
    let me = token_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false);
    if me {
        return false;
    }
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let elevated = token_elevated(h).unwrap_or(true);
        let _ = CloseHandle(h);
        elevated
    }
}

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), InjectError> {
    let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if n as usize == inputs.len() {
        Ok(())
    } else {
        Err(InjectError::Failed(format!(
            "SendInput sent {n} of {} events",
            inputs.len()
        )))
    }
}

fn press(vk: VIRTUAL_KEY) -> Result<(), InjectError> {
    send(&[key_input(vk, false), key_input(vk, true)])
}

fn chord(modifier: VIRTUAL_KEY, vk: VIRTUAL_KEY) -> Result<(), InjectError> {
    send(&[
        key_input(modifier, false),
        key_input(vk, false),
        key_input(vk, true),
        key_input(modifier, true),
    ])
}

fn is_foreground(hwnd: HWND) -> bool {
    unsafe { GetForegroundWindow() == hwnd }
}

fn activate_window(hwnd: HWND) -> Result<(), InjectError> {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if is_foreground(hwnd) {
            return Ok(());
        }
        // An Alt tap makes this process the one that "received the last
        // input", which lets SetForegroundWindow through.
        let _ = press(VK_MENU);
        let _ = SetForegroundWindow(hwnd);
        let start = Instant::now();
        let mut attached = false;
        while start.elapsed() < ACTIVATE_TIMEOUT {
            sleep(Duration::from_millis(30));
            if is_foreground(hwnd) {
                // Let the app settle its focus before keys arrive.
                sleep(Duration::from_millis(80));
                return Ok(());
            }
            if !attached && start.elapsed() > Duration::from_millis(300) {
                attached = true;
                let fg = GetForegroundWindow();
                let fg_thread = GetWindowThreadProcessId(fg, None);
                let me = GetCurrentThreadId();
                let joined = fg_thread != 0 && fg_thread != me
                    && AttachThreadInput(me, fg_thread, true).as_bool();
                let _ = BringWindowToTop(hwnd);
                let _ = SetForegroundWindow(hwnd);
                if joined {
                    let _ = AttachThreadInput(me, fg_thread, false);
                }
            }
        }
    }
    Err(InjectError::Failed("window did not come to the front".into()))
}

/// Clipboard contents saved for restoring: (format, bytes) of every
/// memory-backed format. GDI-handle formats (bitmaps, metafiles) are skipped;
/// Windows re-synthesizes bitmaps from the saved DIB.
type SavedClipboard = Vec<(u32, Vec<u8>)>;

/// Formats whose data is not an HGLOBAL.
const HANDLE_FORMATS: [u32; 6] = [2, 3, 9, 14, 0x80, 0x82];

fn open_clipboard() -> bool {
    for _ in 0..10 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        sleep(Duration::from_millis(20));
    }
    false
}

fn save_clipboard() -> SavedClipboard {
    let mut saved = Vec::new();
    unsafe {
        let mut format = EnumClipboardFormats(0);
        while format != 0 {
            if !HANDLE_FORMATS.contains(&format)
                && let Ok(h) = GetClipboardData(format)
            {
                let g = HGLOBAL(h.0);
                let size = GlobalSize(g);
                let p = GlobalLock(g);
                if !p.is_null() && size > 0 {
                    saved.push((format, std::slice::from_raw_parts(p as *const u8, size).to_vec()));
                }
                let _ = GlobalUnlock(g);
            }
            format = EnumClipboardFormats(format);
        }
    }
    saved
}

fn put(format: u32, bytes: &[u8]) {
    unsafe {
        let Ok(g) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else {
            return;
        };
        let p = GlobalLock(g);
        if p.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
        let _ = GlobalUnlock(g);
        // On success the clipboard owns the memory.
        let _ = SetClipboardData(format, Some(HANDLE(g.0)));
    }
}

fn restore_clipboard(saved: SavedClipboard) {
    if !open_clipboard() {
        return;
    }
    unsafe {
        let _ = EmptyClipboard();
        for (format, bytes) in saved {
            put(format, &bytes);
        }
        let _ = CloseClipboard();
    }
}

fn register(name: PCWSTR) -> u32 {
    unsafe { RegisterClipboardFormatW(name) }
}

/// Put `text` on the clipboard, asking clipboard history and monitors to
/// skip it. Returns the saved previous contents and our sequence number.
fn set_clipboard_text(text: &str) -> Result<(SavedClipboard, u32), InjectError> {
    if !open_clipboard() {
        return Err(InjectError::Failed("clipboard is busy".into()));
    }
    let saved = save_clipboard();
    unsafe {
        let _ = EmptyClipboard();
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
        put(CF_UNICODETEXT.0 as u32, bytes);
        put(register(w!("ExcludeClipboardContentFromMonitorProcessing")), &[0]);
        put(register(w!("CanIncludeInClipboardHistory")), &0u32.to_le_bytes());
        let _ = CloseClipboard();
        Ok((saved, GetClipboardSequenceNumber()))
    }
}

/// Real Windows injector. Never launches apps.
#[derive(Default)]
pub struct WinInjector;

impl WinInjector {
    /// Bring the app to the front, refusing elevated targets.
    fn target(&self, id: &str) -> Result<(), InjectError> {
        let hwnd = main_window(id).ok_or(InjectError::NotRunning)?;
        if blocked_by_uipi(window_pid(hwnd)) {
            log::warn!("{id} runs as administrator; Windows drops input sent to it");
            return Err(InjectError::Permission);
        }
        activate_window(hwnd)
    }
}

impl Injector for WinInjector {
    /// Windows needs no Accessibility-style grant.
    fn accessibility_trusted(&mut self) -> bool {
        true
    }

    fn is_running(&mut self, id: &str) -> bool {
        app_running(id)
    }

    fn frontmost_bundle_id(&mut self) -> Option<String> {
        let fg = unsafe { GetForegroundWindow() };
        if fg.is_invalid() {
            return None;
        }
        exe_name(window_pid(fg))
    }

    fn window_title(&mut self, id: &str) -> Option<String> {
        let t = window_text(main_window(id)?);
        (!t.trim().is_empty()).then_some(t)
    }

    fn activate(&mut self, id: &str) -> Result<(), InjectError> {
        let hwnd = main_window(id).ok_or(InjectError::NotRunning)?;
        activate_window(hwnd)
    }

    fn insert(&mut self, id: &str, text: &str) -> Result<(), InjectError> {
        self.target(id)?;
        let (saved, ours) = set_clipboard_text(text)?;
        log::info!("paste: clipboard set (sequence {ours}), sending Ctrl+V");
        let result = chord(VK_CONTROL, VK_V);
        sleep(RESTORE_DELAY);
        if unsafe { GetClipboardSequenceNumber() } == ours {
            restore_clipboard(saved);
        } else {
            log::info!("clipboard changed meanwhile; not restoring");
        }
        result
    }

    fn submit(&mut self, id: &str) -> Result<(), InjectError> {
        self.target(id)?;
        press(VK_RETURN)
    }

    fn delete_back(&mut self, id: &str, count: usize) -> Result<(), InjectError> {
        self.target(id)?;
        for _ in 0..count {
            press(VK_BACK)?;
            sleep(Duration::from_millis(4));
        }
        Ok(())
    }
}
