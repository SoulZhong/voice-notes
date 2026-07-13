//! 菜单栏常驻托盘（增值层）。设计姿态：托盘一切失败只 eprintln 降级，应用照常运行——
//! 建不出托盘、切不了图标都不许影响录制/转写这些核心功能。
//!
//! 图标语义：idle = 黑色圆环，作为 macOS 模板图（icon_as_template(true)），系统按亮/暗
//! 菜单栏自动反色；recording = 实心红点 #ff6161，必须显色，故此时关掉模板（模板会把颜色
//! 抹成单色，红点就没了）。图标由 scripts/gen_tray_icons.py 生成并提交入库，此处 include_bytes。

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

/// 托盘唯一 id：setup / set_recording / apply_enabled 都按它 tray_by_id 取用。
/// pub(crate)：关窗事件按 tray_by_id(TRAY_ID) 判托盘实存,决定是否拦截关闭并隐藏。
pub(crate) const TRAY_ID: &str = "main-tray";

const IDLE_ICON: &[u8] = include_bytes!("../icons/tray-idle.png");
const RECORDING_ICON: &[u8] = include_bytes!("../icons/tray-recording.png");

/// 读 settings.tray_enabled（读不到 app_data_dir → 回落默认 true，与 Settings::default 一致）。
fn tray_enabled(app: &AppHandle) -> bool {
    app.path()
        .app_data_dir()
        .map(|d| crate::settings::load(&d).tray_enabled)
        .unwrap_or(true)
}

/// 按录制态构建三项菜单：toggle 文案随 recording 切「停止录制」/「开始录制」，
/// show / quit 恒定。id 稳定（toggle/show/quit），on_menu_event 据此分发。
///
/// toggle 项按 recording_ready 禁用（spec：模型缺失时禁用开始录制）：录制中恒可停
/// （enabled = recording || ready）；未录且当前选型模型不完整则灰掉，避免点了必然失败。
/// 已知取舍:刷新时机只有 setup / set_recording(即 start/stop 前后),模型下载完成本身不触发
/// 菜单重建——故"模型刚下完到下一次 start/stop 之间"这段,菜单项仍是灰的(点不亮的窗口),
/// 要到下一次录制状态变化才刷新可用。可接受:下载完成是低频一次性事件。
fn build_menu(app: &AppHandle, recording: bool) -> tauri::Result<Menu<tauri::Wry>> {
    let toggle_label = if recording { "停止录制" } else { "开始录制" };
    let ready = crate::models::recording_ready(&crate::current_asr(app));
    let toggle = MenuItemBuilder::with_id("toggle", toggle_label)
        .enabled(recording || ready)
        .build(app)?;
    let show = MenuItem::with_id(app, "show", "打开主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    Menu::with_items(app, &[&toggle, &show, &quit])
}

/// 菜单事件分发。toggle → 切换录制；show → 显示并聚焦主窗；quit → 录制中先收尾再退。
fn on_menu_event(app: &AppHandle, id: &str) {
    match id {
        "toggle" => crate::toggle_recording(app),
        "show" => {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }
        "quit" => {
            // 录制中先收尾再退：经 actor 发 Cmd::Stop(P1 改道,委托 do_stop_recording,
            // 阻塞至 flush 尾段 + finalize 落盘完成),秒级延迟是 spec 已知取舍——绝不能
            // 为了退得快而丢掉正在写的笔记。Err 仅 actor 已退出时出现,仍继续退出。
            // running 锁 statement-scoped：读完即放，绝不与停录内部锁嵌套。
            let running = *app.state::<crate::AppState>().running.lock().unwrap();
            if running {
                if let Err(e) = app
                    .state::<crate::lifecycle::LifecycleHandle>()
                    .command(crate::lifecycle::Cmd::Stop)
                {
                    eprintln!("退出前停录失败(仍继续退出): {e}");
                }
            }
            app.exit(0);
        }
        _ => {}
    }
}

/// 建托盘：仅 tray_enabled 时建。任何一步失败都 eprintln 降级（应用照常）。
pub fn setup(app: &AppHandle) {
    if !tray_enabled(app) {
        return;
    }
    let icon = match Image::from_bytes(IDLE_ICON) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("托盘图标解码失败，跳过托盘（不影响应用）: {e}");
            return;
        }
    };
    // 读一次 running 作初始文案：录制中开托盘（设置里现开）时,菜单须建成「停止录制」而非
    // idle 的「开始录制」。running 锁 statement-scoped，读完即放，不与其它锁嵌套。
    let recording = *app.state::<crate::AppState>().running.lock().unwrap();
    let menu = match build_menu(app, recording) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("托盘菜单构建失败，跳过托盘（不影响应用）: {e}");
            return;
        }
    };
    let built = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .icon_as_template(true)
        .menu(&menu)
        .on_menu_event(|app, event| on_menu_event(app, event.id.as_ref()))
        .build(app);
    if let Err(e) = built {
        eprintln!("托盘创建失败，跳过托盘（不影响应用）: {e}");
    }
}

/// 录制态变化时刷新托盘：切图标（red/idle）+ 模板开关 + 重建菜单文案。
/// 托盘不存在（未启用/建失败）→ tray_by_id 为 None，静默跳过。失败只 eprintln。
///
/// P1 actor 改道后,本函数可能在 lifecycle-actor 线程上执行,而发起命令的主线程正阻塞
/// 等待 actor 回复;托盘/菜单 API(set_icon_with_as_template/set_menu/MenuItem 构建)
/// 内部是「派发到主线程并同步等结果」——此时同步等待即死锁(actor.rs 死锁注记③的前提)。
/// 故整段更新改为 fire-and-forget 派发:在主线程上调用时 run_on_main_thread 原地内联
/// 执行(与旧行为逐位一致);在其它线程上调用时入队主线程事件循环,主线程空闲后按入队
/// 序执行,托盘态与录制态最终一致(此前从后台加载线程调用本就等价于稍后生效)。
pub fn set_recording(app: &AppHandle, recording: bool) {
    let app2 = app.clone();
    if let Err(e) = app.run_on_main_thread(move || set_recording_on_main(&app2, recording)) {
        eprintln!("托盘更新派发失败（不影响录制）: {e}");
    }
}

fn set_recording_on_main(app: &AppHandle, recording: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let bytes = if recording { RECORDING_ICON } else { IDLE_ICON };
    match Image::from_bytes(bytes) {
        Ok(icon) => {
            // 原子设图标+模板位：避免 macOS 上先 set_icon 再 set_icon_as_template 的二次渲染闪烁。
            // recording 关模板（红点要显色），idle 开模板（黑环随菜单栏亮暗反色）。
            if let Err(e) = tray.set_icon_with_as_template(Some(icon), !recording) {
                eprintln!("托盘图标切换失败（不影响录制）: {e}");
            }
        }
        Err(e) => eprintln!("托盘图标解码失败（不影响录制）: {e}"),
    }
    match build_menu(app, recording) {
        Ok(menu) => {
            if let Err(e) = tray.set_menu(Some(menu)) {
                eprintln!("托盘菜单更新失败（不影响录制）: {e}");
            }
        }
        Err(e) => eprintln!("托盘菜单构建失败（不影响录制）: {e}"),
    }
}

/// 设置里 tray_enabled 开关变更时调：开→建（若尚无），关→拆（若存在）。幂等。
pub fn apply_enabled(app: &AppHandle) {
    let enabled = tray_enabled(app);
    let exists = app.tray_by_id(TRAY_ID).is_some();
    if enabled && !exists {
        setup(app);
    } else if !enabled && exists {
        app.remove_tray_by_id(TRAY_ID);
    }
}
