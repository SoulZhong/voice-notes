//! 设备听写(AI Passport / Vibe Voice):把 vibe_device 运行时接进 Voice Notes。
//!
//! 设计见 docs/adr/0001 与 CONTEXT.md「设备听写」。要点:
//! - 默认开,但用户第一次点「连接设备」之前什么都不做:不扫蓝牙、不申请权限、
//!   不下载模型(settings.device_name 为 None = 未激活)。连过一次后每次启动自动去连。
//! - 识别按 settings.dictation_engine 选(见 recognizer.rs);录音控制进程内直调。
//! - 每次插入成功写一条听写记录,按目标会话归成听写笔记(dictation_store.rs)。
//! - Windows 在应用内配对:设备屏幕显示 6 位码,用户在 Voice Notes 弹框里输入
//!   (事件 device_pin_request → 命令 device_submit_pin)。

pub mod dictation_store;
mod notes_ctl;
pub mod recognizer;
pub mod sherpa_stream;

use dictation_store::{DictationNote, DictationStore, DictationSummary, NewRecord};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use vibe_device::ble::{self, FoundDevice, LinkEnd, LinkOptions, LinkState, PinProvider};
use vibe_device::runtime::{Host, Runtime, RuntimeConfig};
use vibe_device::session::DictationEvent;

/// 独立 VibeVoice 配套程序(已并入 Voice Notes,不再维护)的 bundle id:
/// 它在跑时两边会抢同一台设备,Voice Notes 让路并提示用户退出它。
#[cfg(target_os = "macos")]
const STANDALONE_BUNDLE_ID: &str = "cn.folotoy.vibevoice";

/// 设备运行时与界面状态。由 setup 注册为 tauri 托管状态。
#[derive(Default)]
pub struct DeviceState {
    runtime: Mutex<Option<Runtime>>,
    view: Mutex<LinkView>,
    /// 正在等用户输入的配对码(Windows):发 Some(码)/None(取消)。
    pin: Mutex<Option<mpsc::Sender<Option<String>>>>,
    /// 设备听写序号 → 刚写下的(听写笔记 id, 记录 id),供撤销标记。
    saved: Mutex<HashMap<u8, (String, String)>>,
    /// 听写笔记的读改写串行化(设备主循环写、前端删/读)。
    store_lock: Mutex<()>,
}

/// 链路在界面上的样子。
#[derive(Debug, Clone, Default, Serialize)]
pub struct LinkView {
    /// 最近一次链路状态;None = 运行时没在跑。
    pub state: Option<LinkState>,
    /// 状态的中文短句(与托盘/设置页共用)。
    pub text: String,
    /// 链路为什么停下、需要用户做什么:pairing_failed / bond_lost / standalone_running。
    pub ended: Option<&'static str>,
    /// 设备固件与 Voice Notes 的协议版本不一致:"device_older"(该升级固件)/
    /// "device_newer"(该更新 Voice Notes)。协议以固件仓库的 protocol.md 为准(ADR 0001)。
    pub mismatch: Option<&'static str>,
    /// 设备上报的固件版本串(HELLO)。
    pub firmware: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceStatus {
    pub enabled: bool,
    /// 连过的设备名;None = 还没激活。
    pub device_name: Option<String>,
    pub running: bool,
    pub link: LinkView,
    pub engine: String,
    /// 听写还缺的模型工件 id(前端据此调 download_models)。
    pub missing_models: Vec<&'static str>,
    /// macOS:辅助功能是否已授权(插入文字必需);其它平台恒 true。
    pub accessibility: bool,
    /// 端到端测试的模拟设备在跑(VN_DEVICE_SIM),界面据此标明「没连真设备」。
    pub sim: bool,
    /// macOS:语音识别授权状态("authorized" / "denied" / …);其它平台 "n/a"。
    pub speech_permission: String,
    /// 平台:"macos" / "windows" / 其它。决定配对与权限的界面文案。
    pub platform: &'static str,
}

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        std::env::consts::OS
    }
}

fn load_settings(app: &AppHandle) -> crate::settings::Settings {
    app.path()
        .app_data_dir()
        .map(|d| crate::settings::load(&d))
        .unwrap_or_default()
}

fn update_settings(app: &AppHandle, f: impl FnOnce(&mut crate::settings::Settings)) -> Result<(), String> {
    let d = app.path().app_data_dir().map_err(|e| e.to_string())?;
    crate::settings::update(&d, f).map(|_| ()).map_err(|e| e.to_string())
}

fn state(app: &AppHandle) -> tauri::State<'_, DeviceState> {
    app.state::<DeviceState>()
}

fn set_view(app: &AppHandle, f: impl FnOnce(&mut LinkView)) {
    let view = {
        let st = state(app);
        let mut v = st.view.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut v);
        v.clone()
    };
    let _ = app.emit("device_state", view);
}

fn dictation_store(app: &AppHandle) -> Result<DictationStore, String> {
    let root = crate::data_root(app).map_err(|e| e.to_string())?;
    Ok(DictationStore::new(&root))
}

/// 运行时回调 Voice Notes 的那一面。
struct VnHost {
    app: AppHandle,
}

impl Host for VnHost {
    fn link_state(&self, s: &LinkState) {
        let s = s.clone();
        set_view(&self.app, |v| {
            v.text = s.text();
            v.state = Some(s);
            v.ended = None;
        });
    }

    fn link_ended(&self, end: LinkEnd) {
        let why = match end {
            LinkEnd::PairingFailed => "pairing_failed",
            LinkEnd::BondLost => "bond_lost",
            LinkEnd::Stopped => return,
        };
        set_view(&self.app, |v| v.ended = Some(why));
        // 运行时的链路已停;核心循环还在,但没有设备可连,一并收掉,等用户重连。
        if let Some(mut rt) = state(&self.app).runtime.lock().unwrap_or_else(|e| e.into_inner()).take() {
            rt.stop();
        }
    }

    fn device_hello(&self, ver: u8, fw: &str) {
        let mismatch = protocol_mismatch(ver);
        if let Some(m) = mismatch {
            eprintln!(
                "设备: 协议版本不一致(设备 {ver},Voice Notes {}): {m}",
                vibe_device::protocol::PROTOCOL_VERSION
            );
        }
        let fw = fw.to_owned();
        set_view(&self.app, |v| {
            v.mismatch = mismatch;
            v.firmware = Some(fw);
        });
    }

    fn device_connected(&self, name: &str) {
        let s = load_settings(&self.app);
        if s.device_name.as_deref() != Some(name) {
            let name = name.to_owned();
            if let Err(e) = update_settings(&self.app, |s| s.device_name = Some(name)) {
                eprintln!("设备: 记住设备名失败: {e}");
            }
        }
    }

    fn dictation(&self, event: DictationEvent) {
        let s = load_settings(&self.app);
        let st = state(&self.app);
        match event {
            DictationEvent::Delivered { dict, text, target, pcm } => {
                if !s.dictation_save_text {
                    return;
                }
                let Ok(store) = dictation_store(&self.app) else { return };
                let _g = st.store_lock.lock().unwrap_or_else(|e| e.into_inner());
                let rec = NewRecord {
                    key: &target.key,
                    app: &target.app,
                    label: &target.label,
                    dict,
                    text: &text,
                    pcm: s.dictation_save_audio.then_some(pcm.as_slice()),
                };
                match store.append(rec, chrono::Local::now()) {
                    Ok(saved) => {
                        let _ = self.app.emit("dictation_notes_changed", &saved.id);
                        st.saved
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(dict, (saved.id, saved.rid));
                    }
                    Err(e) => eprintln!("设备: 听写记录写盘失败: {e:#}"),
                }
            }
            DictationEvent::Undone { dict } => {
                let Some((id, rid)) = st.saved.lock().unwrap_or_else(|e| e.into_inner()).remove(&dict) else {
                    return;
                };
                let Ok(store) = dictation_store(&self.app) else { return };
                let _g = st.store_lock.lock().unwrap_or_else(|e| e.into_inner());
                match store.mark_undone(&id, &rid) {
                    Ok(()) => {
                        let _ = self.app.emit("dictation_notes_changed", &id);
                    }
                    Err(e) => eprintln!("设备: 标记撤销失败: {e:#}"),
                }
            }
        }
    }
}

/// 设备协议版本与本端不一致时,哪一边该更新。
fn protocol_mismatch(device_ver: u8) -> Option<&'static str> {
    use std::cmp::Ordering;
    match device_ver.cmp(&vibe_device::protocol::PROTOCOL_VERSION) {
        Ordering::Less => Some("device_older"),
        Ordering::Greater => Some("device_newer"),
        Ordering::Equal => None,
    }
}

/// 配对码来自界面:发事件让前端弹框,阻塞等用户输入(最多 2 分钟)。
struct UiPins {
    app: AppHandle,
}

#[derive(Clone, Serialize)]
struct PinRequest {
    name: String,
}

impl PinProvider for UiPins {
    fn request_pin(&self, device_name: &str) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        *state(&self.app).pin.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        // 主窗可能藏在托盘里:拉到前台,用户才看得到输入框。
        if let Some(w) = self.app.get_webview_window("main") {
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
        let _ = self.app.emit("device_pin_request", PinRequest { name: device_name.to_owned() });
        let pin = rx.recv_timeout(Duration::from_secs(120)).ok().flatten();
        *state(&self.app).pin.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let _ = self.app.emit("device_pin_closed", ());
        pin
    }
}

/// 停掉运行时(没在跑则无事)。
fn stop_runtime(app: &AppHandle) {
    let rt = state(app).runtime.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut rt) = rt {
        rt.stop();
    }
    // 还挂着的配对框一并取消。
    if let Some(tx) = state(app).pin.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = tx.send(None);
    }
    set_view(app, |v| {
        v.state = None;
        v.text.clear();
        v.mismatch = None;
    });
}

/// 按当前设置(重新)启动运行时。未开启或未激活时只停。
fn restart_runtime(app: &AppHandle) {
    stop_runtime(app);
    let s = load_settings(app);
    if !s.device_enabled || s.device_name.is_none() {
        return;
    }
    #[cfg(target_os = "macos")]
    if vibe_device::platform::app_running(STANDALONE_BUNDLE_ID) {
        eprintln!("设备: 独立 VibeVoice 正在运行,让路(请先退出它)");
        set_view(app, |v| v.ended = Some("standalone_running"));
        return;
    }
    // 已激活的设备在启动时直接连上,不经过「连接设备」:语音识别还没问过就在这里问,
    // 否则第一次听写只会在设备上报「需要语音识别权限」而系统从不弹框。
    #[cfg(target_os = "macos")]
    if vibe_device::speech_apple::status_name(vibe_device::speech_apple::authorization_status()) == "not determined" {
        std::thread::spawn(|| {
            let s = vibe_device::speech_apple::request_authorization(Duration::from_secs(600));
            eprintln!("设备: 语音识别权限 {}", vibe_device::speech_apple::status_name(s));
        });
    }
    let Ok(app_data) = app.path().app_data_dir() else { return };
    let hotwords: Vec<String> = crate::qwen3_hotwords(app)
        .map(|h| h.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let factory = recognizer::factory(s.dictation_engine.clone(), hotwords, crate::models::root());
    let cfg = RuntimeConfig {
        target_store: app_data.join("device").join("target.json"),
        link: LinkOptions {
            wanted: s.device_name.clone(),
            pins: Some(Arc::new(UiPins { app: app.clone() }) as Arc<dyn PinProvider>),
        },
        sim: sim_dir(),
    };
    let host = Arc::new(VnHost { app: app.clone() });
    let notes = notes_ctl::InProcessNotes { app: app.clone() };
    set_view(app, |v| {
        v.ended = None;
        v.state = Some(LinkState::Searching);
        v.text = LinkState::Searching.text();
    });
    match Runtime::start(cfg, factory, notes, host) {
        Ok(rt) => *state(app).runtime.lock().unwrap_or_else(|e| e.into_inner()) = Some(rt),
        Err(e) => eprintln!("设备: 运行时启动失败: {e}"),
    }
}

/// macOS:激活时一次性申请辅助功能(插入文字)与语音识别权限。其它平台无事。
fn request_permissions() {
    #[cfg(target_os = "macos")]
    std::thread::spawn(|| {
        if !vibe_device::inject_macos::accessibility_trusted(true) {
            eprintln!("设备: 需要辅助功能权限(系统设置 › 隐私与安全性 › 辅助功能)");
        }
        let s = vibe_device::speech_apple::request_authorization(Duration::from_secs(600));
        eprintln!("设备: 语音识别权限 {}", vibe_device::speech_apple::status_name(s));
    });
}

/// vibe-device 走 `log` 打日志(连接、扫描、注入、识别的来龙去脉都在里面),而
/// Voice Notes 没有装 logger——不桥接这些日志会全部丢掉,Windows 上「靠日志定位」
/// 就成了空话。只放行 vibe_device 自己的 info 及以上,写进 stderr(即 logs/stderr.log)。
fn install_logger() {
    struct DeviceLog;
    impl log::Log for DeviceLog {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= log::Level::Info && m.target().starts_with("vibe_device")
        }
        fn log(&self, r: &log::Record) {
            if self.enabled(r.metadata()) {
                eprintln!(
                    "{} 设备[{}] {}",
                    chrono::Local::now().format("%H:%M:%S%.3f"),
                    r.level(),
                    r.args()
                );
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: DeviceLog = DeviceLog;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

/// 模拟设备目录(端到端测试):设了环境变量 VN_DEVICE_SIM 才有,平时恒为 None。
fn sim_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("VN_DEVICE_SIM").filter(|v| !v.is_empty()).map(Into::into)
}

/// 启动时调用:注册状态;已激活则自动连;顺手清理过期的听写音频。
pub fn init(app: &AppHandle) {
    install_logger();
    app.manage(DeviceState::default());
    let s = load_settings(app);
    if s.device_enabled && s.device_name.is_some() {
        restart_runtime(app);
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        if let Ok(store) = dictation_store(&app2) {
            let cutoff = chrono::Local::now() - chrono::Duration::days(dictation_store::AUDIO_RETENTION_DAYS);
            let n = store.purge_audio(cutoff);
            if n > 0 {
                eprintln!("听写音频保留期清理: 删除 {n} 段(>{} 天)", dictation_store::AUDIO_RETENTION_DAYS);
            }
        }
    });
}

/// 设置页改了总开关/引擎/热词:按新设置重启(或停掉)运行时。
pub fn on_settings_changed(app: &AppHandle) {
    let app = app.clone();
    // 停运行时要等线程收尾,别占着设置保存的 IPC 线程。
    std::thread::spawn(move || restart_runtime(&app));
}

fn current_status(app: &AppHandle) -> DeviceStatus {
    let s = load_settings(app);
    let running = state(app).runtime.lock().unwrap_or_else(|e| e.into_inner()).is_some();
    let link = state(app).view.lock().unwrap_or_else(|e| e.into_inner()).clone();
    #[cfg(target_os = "macos")]
    let (accessibility, speech_permission) = (
        vibe_device::inject_macos::accessibility_trusted(false),
        vibe_device::speech_apple::status_name(vibe_device::speech_apple::authorization_status()).to_owned(),
    );
    #[cfg(not(target_os = "macos"))]
    let (accessibility, speech_permission) = (true, "n/a".to_owned());
    DeviceStatus {
        enabled: s.device_enabled,
        device_name: s.device_name.clone(),
        running,
        link,
        missing_models: recognizer::missing_models(&s.dictation_engine),
        engine: s.dictation_engine,
        accessibility,
        speech_permission,
        platform: platform(),
        sim: sim_dir().is_some(),
    }
}

// ----- 命令 --------------------------------------------------------------

#[tauri::command]
pub fn device_status(app: AppHandle) -> DeviceStatus {
    current_status(&app)
}

/// 扫描附近的设备(「连接设备」对话框)。第一次调用会触发系统的蓝牙权限申请。
#[tauri::command]
pub async fn device_scan() -> Result<Vec<FoundDevice>, String> {
    if sim_dir().is_some() {
        return Ok(vec![FoundDevice { name: vibe_device::sim::device_name(), rssi: Some(-40), paired: None }]);
    }
    let t = std::time::Instant::now();
    // 蓝牙栈偶尔卡在取适配器上:给整个扫描一个上限,对话框不至于一直转圈。
    let r = match tokio::time::timeout(Duration::from_secs(15), ble::scan(Duration::from_secs(4))).await {
        Ok(r) => r,
        Err(_) => Err("timeout".to_owned()),
    };
    match &r {
        Ok(found) => eprintln!(
            "设备: 扫描 {:.1}s 找到 {:?}",
            t.elapsed().as_secs_f32(),
            found.iter().map(|d| d.name.as_str()).collect::<Vec<_>>()
        ),
        Err(e) => eprintln!("设备: 扫描 {:.1}s 失败: {e}", t.elapsed().as_secs_f32()),
    }
    r.map_err(|e| crate::tr!("扫描设备失败: {e}", "Scanning for devices failed: {e}"))
}

/// 连接(并记住)这台设备。返回连接后的状态;缺模型时前端据 missing_models 发起下载。
#[tauri::command]
pub async fn device_connect(app: AppHandle, name: String) -> Result<DeviceStatus, String> {
    let name = name.trim().to_owned();
    if !name.starts_with(vibe_device::protocol::NAME_PREFIX) {
        return Err(crate::tr!("不是 Vibe Voice 设备: {name}", "Not a Vibe Voice device: {name}"));
    }
    update_settings(&app, |s| {
        s.device_enabled = true;
        s.device_name = Some(name);
    })?;
    request_permissions();
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || restart_runtime(&app2))
        .await
        .map_err(|e| e.to_string())?;
    Ok(current_status(&app))
}

/// 配对失败/配对失效/独立程序退出后,用户点「重新连接」。
#[tauri::command]
pub async fn device_reconnect(app: AppHandle) -> Result<DeviceStatus, String> {
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || restart_runtime(&app2))
        .await
        .map_err(|e| e.to_string())?;
    Ok(current_status(&app))
}

/// 重新配对:删掉这台电脑上的旧配对(Windows 自动;macOS 需在系统设置里移除),再连。
#[tauri::command]
pub async fn device_repair(app: AppHandle) -> Result<DeviceStatus, String> {
    let s = load_settings(&app);
    let name = s
        .device_name
        .ok_or_else(|| crate::tr!("还没有连接过设备", "No device has been connected yet"))?;
    if !cfg!(windows) {
        // macOS 的配对记录归系统管,应用删不掉。
        return Err(crate::tr!(
            "请在「系统设置 › 蓝牙」里移除 {name},再点「重新连接」",
            "Remove {name} in System Settings › Bluetooth, then click Reconnect"
        ));
    }
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || stop_runtime(&app2))
        .await
        .map_err(|e| e.to_string())?;
    ble::forget(&name)
        .await
        .map_err(|e| crate::tr!("移除旧配对失败: {e}", "Removing the old pairing failed: {e}"))?;
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || restart_runtime(&app2))
        .await
        .map_err(|e| e.to_string())?;
    Ok(current_status(&app))
}

/// 忘记设备:断开并不再自动连(Windows 顺带删掉系统里的配对)。
#[tauri::command]
pub async fn device_forget(app: AppHandle) -> Result<DeviceStatus, String> {
    let name = load_settings(&app).device_name;
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || stop_runtime(&app2))
        .await
        .map_err(|e| e.to_string())?;
    update_settings(&app, |s| s.device_name = None)?;
    if cfg!(windows) {
        if let Some(name) = name {
            if let Err(e) = ble::forget(&name).await {
                eprintln!("设备: 移除系统配对失败(不影响忘记): {e}");
            }
        }
    }
    set_view(&app, |v| v.ended = None);
    Ok(current_status(&app))
}

/// 配对框的回答:Some(6 位码) 或 None(取消)。
#[tauri::command]
pub fn device_submit_pin(app: AppHandle, pin: Option<String>) -> Result<(), String> {
    let tx = state(&app).pin.lock().unwrap_or_else(|e| e.into_inner()).take();
    match tx {
        Some(tx) => tx
            .send(pin)
            .map_err(|_| crate::tr!("配对已超时,请重新连接", "Pairing timed out; reconnect to try again")),
        None => Err(crate::tr!("没有等待中的配对", "No pairing is waiting")),
    }
}

/// macOS 辅助功能授权引导:带 prompt 查一次授权,让系统把 Voice Notes 自动登记进
/// 「辅助功能」列表(用户只需拨开关,不必手动 + 或拖入),再直接打开那一页。
/// 返回当前是否已授权;界面随后轮询 device_status 等开关拨开。
#[tauri::command]
pub fn device_open_accessibility(app: AppHandle) -> Result<bool, String> {
    #[cfg(target_os = "macos")]
    {
        use tauri_plugin_opener::OpenerExt;
        if vibe_device::inject_macos::accessibility_trusted(true) {
            return Ok(true);
        }
        app.opener()
            .open_url(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
                None::<&str>,
            )
            .map_err(|e| crate::tr!("打开系统设置失败: {e}", "Failed to open System Settings: {e}"))?;
        Ok(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Ok(true)
    }
}

/// macOS 语音识别授权引导:还没问过就当场弹系统授权框;问过被拒则打开系统设置
/// 的「语音识别」页。返回授权后的状态名(与 DeviceStatus.speech_permission 同口径)。
#[tauri::command]
pub async fn device_grant_speech(app: AppHandle) -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        use tauri_plugin_opener::OpenerExt;
        use vibe_device::speech_apple as sp;
        let s = tauri::async_runtime::spawn_blocking(|| sp::request_authorization(Duration::from_secs(120)))
            .await
            .map_err(|e| e.to_string())?;
        let name = sp::status_name(s).to_owned();
        eprintln!("设备: 语音识别授权 {name}");
        if name == "denied" || name == "restricted" {
            app.opener()
                .open_url(
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_SpeechRecognition",
                    None::<&str>,
                )
                .map_err(|e| crate::tr!("打开系统设置失败: {e}", "Failed to open System Settings: {e}"))?;
        }
        Ok(name)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Ok("n/a".to_owned())
    }
}

#[tauri::command]
pub fn list_dictation_notes(app: AppHandle) -> Result<Vec<DictationSummary>, String> {
    Ok(dictation_store(&app)?.list())
}

#[tauri::command]
pub fn get_dictation_note(app: AppHandle, id: String) -> Result<DictationNote, String> {
    dictation_store(&app)?.load(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_dictation_note(app: AppHandle, id: String) -> Result<(), String> {
    let store = dictation_store(&app)?;
    let st = state(&app);
    let _g = st.store_lock.lock().unwrap_or_else(|e| e.into_inner());
    store.delete_note(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_dictation_record(app: AppHandle, id: String, rid: String) -> Result<(), String> {
    let store = dictation_store(&app)?;
    let st = state(&app);
    let _g = st.store_lock.lock().unwrap_or_else(|e| e.into_inner());
    store.delete_record(&id, &rid).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_mismatch_names_the_side_to_update() {
        let v = vibe_device::protocol::PROTOCOL_VERSION;
        assert_eq!(protocol_mismatch(v), None);
        assert_eq!(protocol_mismatch(v - 1), Some("device_older"));
        assert_eq!(protocol_mismatch(v + 1), Some("device_newer"));
    }
}
