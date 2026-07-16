//! 菜单栏常驻托盘（增值层）。设计姿态：托盘一切失败只 eprintln 降级，应用照常运行——
//! 建不出托盘、切不了图标都不许影响录制/转写这些核心功能。
//!
//! 图标语义：菜单栏直接用 App Logo（戴眼镜的小姑娘拿笔记本）。空闲 = 静止 Logo；
//! 录制中 = 逐帧循环的「疯狂记笔记」抖动动画；停止录制即静止（Aing 在后台安静进行，
//! 不驱动图标——否则按了停止还在抖，像没停下）。图标是彩色 Logo，故全程非模板图
//! （icon_as_template(false)）——macOS 模板会把颜色抹成单色。
//!
//! 为何靠逐帧切图而非 GIF：macOS 菜单栏是静态 NSImage，不解析 GIF 帧；要「动」只能
//! 由运行时定时器逐帧 set_icon。帧 PNG 由 scripts/gen_tray_logo_frames.py 生成并提交
//! 入库，此处 include_bytes。活跃判定（是否录制）由 actor 提交点调 `update` 边沿驱动
//! （见本文件 update / start_anim）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

use crate::lifecycle::machine::{LifecycleState, SessionState};

/// 托盘唯一 id：setup / set_recording / apply_enabled 都按它 tray_by_id 取用。
/// pub(crate)：关窗事件按 tray_by_id(TRAY_ID) 判托盘实存,决定是否拦截关闭并隐藏。
pub(crate) const TRAY_ID: &str = "main-tray";

/// 空闲静止帧。彩色 App Logo，非模板图。
const IDLE_ICON: &[u8] = include_bytes!("../icons/tray-logo-idle.png");
/// 录制/Aing 抖动帧（循环播放）。与 IDLE 同源 Logo，逐帧轻微旋转+位移。
const REC_FRAMES: &[&[u8]] = &[
    include_bytes!("../icons/tray-logo-rec-0.png"),
    include_bytes!("../icons/tray-logo-rec-1.png"),
    include_bytes!("../icons/tray-logo-rec-2.png"),
    include_bytes!("../icons/tray-logo-rec-3.png"),
    include_bytes!("../icons/tray-logo-rec-4.png"),
    include_bytes!("../icons/tray-logo-rec-5.png"),
];
/// 逐帧间隔：约 9fps，忙碌但不抽搐；低频省电（菜单栏动画不追高帧率）。
const FRAME_MS: u64 = 110;

/// 动画代际计数。每次 start_anim 领取新一代并起一条动画线程按该代循环；代际一变
/// （再次 start 或 stop）旧线程下一 tick 自然退出——保证任一时刻至多一条动画线程在跑，
/// 无需 join。全局单托盘，单计数器即可。
static ANIM_GEN: AtomicU64 = AtomicU64::new(0);

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
        // 彩色 Logo 全程非模板（模板会抹成单色）。
        .icon_as_template(false)
        .menu(&menu)
        .on_menu_event(|app, event| on_menu_event(app, event.id.as_ref()))
        .build(app);
    if let Err(e) = built {
        eprintln!("托盘创建失败，跳过托盘（不影响应用）: {e}");
        return;
    }
    // 冷启动即在录制中（设置里现开托盘，或崩溃恢复）：立即进入抖动动画，
    // 否则要等到下一次状态迁移才动。Aing 态冷启动不可达，无需处理。
    if recording {
        start_anim(app);
    }
}

/// 会话录制态变化时刷新托盘**菜单**文案（开始/停止录制 + 模型就绪禁用位）。
/// 图标不在此处理——图标（静止/抖动）由 `update` 按「录制 OR Aing」活跃度独立驱动，
/// 避免会话钩子与 Aing 钩子争抢同一图标。托盘不存在则 tray_by_id 为 None，静默跳过。
///
/// P1 actor 改道后,本函数可能在 lifecycle-actor 线程上执行,而发起命令的主线程正阻塞
/// 等待 actor 回复;托盘/菜单 API 内部是「派发到主线程并同步等结果」——此时同步等待即
/// 死锁(actor.rs 死锁注记③的前提)。故改为 fire-and-forget 派发:在主线程上调用时
/// run_on_main_thread 原地内联执行;在其它线程上调用时入队主线程事件循环,最终一致。
pub fn set_recording(app: &AppHandle, recording: bool) {
    let app2 = app.clone();
    if let Err(e) = app.run_on_main_thread(move || set_menu_on_main(&app2, recording)) {
        eprintln!("托盘菜单派发失败（不影响录制）: {e}");
    }
}

fn set_menu_on_main(app: &AppHandle, recording: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match build_menu(app, recording) {
        Ok(menu) => {
            if let Err(e) = tray.set_menu(Some(menu)) {
                eprintln!("托盘菜单更新失败（不影响录制）: {e}");
            }
        }
        Err(e) => eprintln!("托盘菜单构建失败（不影响录制）: {e}"),
    }
}

// —— 图标动画：仅录制中逐帧循环抖动，停止即静止 —— //

/// 「活跃」= 会话正在录制。**只看录制**：停止录制通常紧接自动 Aing，若把 Aing 也算
/// 活跃，抖动会一路延续到 Aing 结束——用户按了停止却还在抖，读起来像「没停下」。
/// 故停录（会话离开 Recording）即停回静止 Logo，Aing 在后台安静进行、不再驱动图标。
fn is_active(s: &LifecycleState) -> bool {
    matches!(s.session, SessionState::Recording { .. })
}

/// 内核状态提交后由 actor 调用（见 actor.rs 提交点）：按活跃度**边沿**驱动图标动画。
/// 每条消息都会调用，故非活跃↔活跃无变化时零动作（不起线程、不派发）。
pub fn update(app: &AppHandle, before: &LifecycleState, after: &LifecycleState) {
    let (was, now) = (is_active(before), is_active(after));
    if was == now {
        return;
    }
    if now {
        start_anim(app);
    } else {
        stop_anim(app);
    }
}

/// 起动画：领取新一代 gen，起一条 tray-anim 线程按 gen 循环切帧；gen 变化即令旧线程退出。
fn start_anim(app: &AppHandle) {
    let generation = ANIM_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let app_thread = app.clone();
    let spawned = std::thread::Builder::new()
        .name("tray-anim".into())
        .spawn(move || {
            let mut i = 0usize;
            loop {
                if ANIM_GEN.load(Ordering::SeqCst) != generation {
                    return; // 被 stop 或新一轮 start 取代
                }
                dispatch_icon(&app_thread, REC_FRAMES[i % REC_FRAMES.len()]);
                i = i.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(FRAME_MS));
            }
        });
    if let Err(e) = spawned {
        // 线程起不来：降级为静止 Logo（下面 dispatch），绝不影响录制。
        eprintln!("托盘动画线程创建失败（降级静止，不影响录制）: {e}");
        dispatch_icon(app, IDLE_ICON);
    }
}

/// 停动画：作废当前代（令动画线程下一 tick 退出）并把图标切回静止 Logo。
fn stop_anim(app: &AppHandle) {
    ANIM_GEN.fetch_add(1, Ordering::SeqCst);
    dispatch_icon(app, IDLE_ICON);
}

/// 把某帧图标 fire-and-forget 派发到主线程设置（彩色 Logo，非模板）。
fn dispatch_icon(app: &AppHandle, bytes: &'static [u8]) {
    let app2 = app.clone();
    if let Err(e) = app.run_on_main_thread(move || set_icon_on_main(&app2, bytes)) {
        eprintln!("托盘图标派发失败（不影响录制）: {e}");
    }
}

fn set_icon_on_main(app: &AppHandle, bytes: &'static [u8]) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match Image::from_bytes(bytes) {
        // 原子设图标+模板位（false=彩色 Logo 显色）：避免二次渲染闪烁。
        Ok(icon) => {
            if let Err(e) = tray.set_icon_with_as_template(Some(icon), false) {
                eprintln!("托盘图标切换失败（不影响录制）: {e}");
            }
        }
        Err(e) => eprintln!("托盘图标解码失败（不影响录制）: {e}"),
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
