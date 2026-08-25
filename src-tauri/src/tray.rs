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

/// 按录制态构建菜单：toggle 文案随 recording 切「停止录制」/「开始录制」，
/// show / quit 恒定。id 稳定（toggle/stop_playback/show/quit），on_menu_event 据此分发。
///
/// toggle 项按录制就绪判定禁用（spec：模型缺失时禁用开始录制）：录制中恒可停
/// （enabled = recording || ready）；未录且当前选型模型不完整则灰掉，避免点了必然失败。
/// 已知取舍:刷新时机只有 setup / set_recording(即 start/stop 前后)/播放会话变化,
/// 模型下载完成本身不触发菜单重建——故"模型刚下完到下一次刷新之间"这段,菜单项仍是灰的
/// (点不亮的窗口),要到下一次状态变化才刷新可用。可接受:下载完成是低频一次性事件。
///
/// 菜单项 =(稳定 id, 标签),按当前界面语言取。抽成纯函数是为了可测:build_menu 要
/// AppHandle,单测里造不出来,而"语言切了托盘却还是旧文案"正是这里会漏的。
///
/// 「停止播放」只在**有播放会话**时插入(id: stop_playback):托盘常驻下关掉主窗口后
/// 音频继续播(与音乐 App 一致),托盘就是唯一能停它的地方;没在播时插一个点了什么也
/// 不发生的死项,比不插更糟。会话语义(装载不算播放,见 playback.svelte.ts)在前端,
/// 故这个开关由前端经 set_playback_active 告知,后端不猜。
/// `session` 为 None 表示"还没有会话槽"(开录途中 running 已置真、会话尚未入槽的窗口),
/// 此时不出暂停项——点了只会得到「没有正在进行的录制」(Codex P2)。
fn menu_items(recording: bool, session: Option<bool>, playback_active: bool) -> Vec<(&'static str, String)> {
    let mut items = vec![(
        "toggle",
        if recording {
            crate::tr!("停止录制", "Stop recording")
        } else {
            crate::tr!("开始录制", "Start recording")
        },
    )];
    // 暂停/恢复只在录制中出现:托盘存在的意义就是不开窗口也能控制,而此前只能开始/停止,
    // 唯独缺了最常用的暂停——接个电话就得把窗口翻出来。
    if let Some(paused) = session.filter(|_| recording) {
        items.push((
            "pause_toggle",
            if paused {
                crate::tr!("恢复录制", "Resume recording")
            } else {
                crate::tr!("暂停录制", "Pause recording")
            },
        ));
    }
    if playback_active {
        items.push(("stop_playback", crate::tr!("停止播放", "Stop playback")));
    }
    items.push(("show", crate::tr!("打开主窗口", "Open main window")));
    items.push(("quit", crate::tr!("退出", "Quit")));
    items
}

/// 会话槽快照:None = 没有会话(含"开录已置 running、会话还没入槽"的窗口),
/// Some(paused) = 有会话且是否暂停。菜单据此决定要不要出暂停项、文案取哪个。
/// 锁 statement-scoped,读完即放,不与其它锁嵌套(同本文件其它取值)。
fn session_paused(app: &AppHandle) -> Option<bool> {
    app.try_state::<crate::AppState>()
        .and_then(|s| s.session.lock().ok().map(|slot| slot.as_ref().map(|x| x.paused_at.is_some())))
        .flatten()
}

/// 当前是否有播放会话。try_state:托盘可能在 manage 之前建起来(setup 顺序变动时),
/// 取不到就当没在播——菜单少一项远好过 panic 掉整个托盘。
fn playback_active(app: &AppHandle) -> bool {
    app.try_state::<crate::AppState>()
        .map(|s| s.playback_active.load(Ordering::SeqCst))
        .unwrap_or(false)
}

fn build_menu(app: &AppHandle, recording: bool) -> tauri::Result<Menu<tauri::Wry>> {
    // 模式感知就绪判定,与设置页(models_status)/开录守卫同一份:云端模式下本机大模型
    // 不必需,只要 vad 在 + 凭证齐就该点得亮——否则云端用户面对一个永远灰着的菜单项。
    let ready = crate::current_models_status(app).recording_ready;
    let mut built: Vec<MenuItem<tauri::Wry>> = Vec::new();
    for (id, label) in menu_items(recording, session_paused(app), playback_active(app)) {
        // toggle 之外的项恒可点:show/quit 无前置条件,stop_playback 只在有会话时才存在。
        let enabled = id != "toggle" || recording || ready;
        built.push(MenuItemBuilder::with_id(id, label).enabled(enabled).build(app)?);
    }
    let refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        built.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>).collect();
    Menu::with_items(app, &refs)
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
        // 停播放不碰窗口:用户从托盘停,本意就是不想让窗口跳出来。前端(哪怕窗口是隐藏的,
        // webview 仍在跑)收 player_stopped 后自行清会话/复位播放器。
        "stop_playback" => crate::player::stop_from_tray(app),
        // 暂停/恢复:走与命令壳同一条 actor 信箱,不在菜单回调里直接改状态。
        // 丢阻塞线程池执行(同 toggle_recording 的理由):菜单回调跑在事件循环线程,
        // command() 会等 actor 回复,actor 正忙于停录收尾时同步等待会冻住整个 UI。
        "pause_toggle" => {
            let paused = session_paused(app).unwrap_or(false);
            let lc = app.state::<crate::lifecycle::LifecycleHandle>().inner().clone();
            tauri::async_runtime::spawn_blocking(move || {
                let cmd = if paused { crate::lifecycle::Cmd::Unpause } else { crate::lifecycle::Cmd::Pause };
                if let Err(e) = lc.command(cmd) {
                    eprintln!("托盘暂停/恢复失败(静默进日志): {e}");
                }
            });
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

/// 播放会话开关变化时重建菜单(「停止播放」项的出现/消失)。录制文案不是本次的
/// 变化点,但菜单是整份重建的,故现读一次 running 保持它正确——读完即放,
/// 与 set_recording 同一把 statement-scoped 锁,不与其它锁嵌套。
pub fn refresh_menu(app: &AppHandle) {
    let recording = *app.state::<crate::AppState>().running.lock().unwrap();
    set_recording(app, recording);
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

/// 「活跃」= 会话正在录制**且未暂停**。停录/暂停都静止:停止后继续抖读起来像
/// 「没停下」(Aing 不驱动图标),暂停后继续抖读起来像「没暂停成」——图标动画的
/// 语义就是"正在记",不在记就不该动。恢复录制经状态边沿重新起动画。
fn is_active(s: &LifecycleState) -> bool {
    matches!(s.session, SessionState::Recording { paused: false, .. })
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

/// 暂停/恢复的动画驱动。内核不追踪 paused 翻转(见 consumers::tray_flag:
/// (Recording, Recording) 转移一律 None),所以 `update` 的状态边沿在暂停路上
/// **不存在**——菜单文案早为此在 do_pause/do_resume 里显式刷过一次,图标动画
/// 当时漏了同样的补偿,暂停后图标继续抖,读起来像"没暂停成"(2026-08-24 真机
/// 实报)。与菜单同一choke point:仅 do_pause_recording / do_resume_recording
/// 在**实际发生翻转后**调用(幂等早退分支不会走到),恢复时会话必然存在且录制中,
/// 直接 start 是安全的。等价性测试见 is_active——那是判定,这里是被漏掉的接线。
pub fn set_anim_paused(app: &AppHandle, paused: bool) {
    if paused {
        stop_anim(app);
    } else {
        start_anim(app);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 动画活跃判定:录制中才动,暂停/空闲/停止都静止。暂停时若继续抖动,
    /// 读起来像「没暂停成」——与停录即静止同一条产品理由(2026-07-29 冒烟反馈)。
    #[test]
    fn animation_active_only_while_recording_unpaused() {
        let mut s = LifecycleState::init();
        assert!(!is_active(&s), "空闲不动");
        s.session = SessionState::Recording { note_id: "n".into(), paused: false };
        assert!(is_active(&s), "录制中要动");
        s.session = SessionState::Recording { note_id: "n".into(), paused: true };
        assert!(!is_active(&s), "暂停即静止");
    }

    fn labels(recording: bool, playing: bool) -> Vec<String> {
        menu_items(recording, Some(false), playing).into_iter().map(|(_, l)| l).collect()
    }
    fn ids(recording: bool, playing: bool) -> Vec<&'static str> {
        menu_items(recording, Some(false), playing).into_iter().map(|(id, _)| id).collect()
    }

    /// 托盘菜单标签随界面语言切换。语言是进程级全局,故先拿 test_lang_guard 与其它
    /// 改语言的用例互斥,末尾复位中文。
    #[test]
    fn menu_labels_follow_ui_language() {
        let _guard = crate::i18n::test_lang_guard();
        crate::i18n::set_lang("zh");
        assert_eq!(labels(false, false)[0], "开始录制");
        assert_eq!(labels(true, false)[0], "停止录制");
        assert_eq!(
            labels(false, false)[1..],
            ["打开主窗口".to_string(), "退出".to_string()]
        );
        assert_eq!(labels(false, true)[1], "停止播放");
        assert_eq!(labels(true, false)[1], "暂停录制");
        assert_eq!(menu_items(true, Some(true), false)[1].1, "恢复录制");

        crate::i18n::set_lang("en");
        assert_eq!(labels(false, false)[0], "Start recording");
        assert_eq!(labels(true, false)[0], "Stop recording");
        assert_eq!(
            labels(false, false)[1..],
            ["Open main window".to_string(), "Quit".to_string()]
        );
        assert_eq!(labels(false, true)[1], "Stop playback");
        assert_eq!(labels(true, false)[1], "Pause recording");
        assert_eq!(menu_items(true, Some(true), false)[1].1, "Resume recording");

        crate::i18n::set_lang("zh");
    }

    /// 「停止播放」只在有播放会话时出现,且不挤掉任何既有项——它是插入,不是替换。
    /// 死项(没在播还挂着一个点了什么都不发生的「停止播放」)是这条用例要挡的回退。
    #[test]
    fn stop_playback_item_only_while_a_session_exists() {
        assert_eq!(ids(false, false), ["toggle", "show", "quit"]);
        assert_eq!(ids(false, true), ["toggle", "stop_playback", "show", "quit"]);
        // 录制与播放互不排斥:边录边听旧笔记时两项都在。
        assert_eq!(
            ids(true, true),
            ["toggle", "pause_toggle", "stop_playback", "show", "quit"]
        );
    }

    /// 暂停/恢复只在录制中出现:空闲态给一个点了什么都不发生的「暂停录制」是死项。
    #[test]
    fn pause_item_only_while_recording() {
        let ids_of = |rec, paused: bool| {
            menu_items(rec, Some(paused), false).into_iter().map(|(id, _)| id).collect::<Vec<_>>()
        };
        assert_eq!(ids_of(false, false), ["toggle", "show", "quit"]);
        assert_eq!(ids_of(true, false), ["toggle", "pause_toggle", "show", "quit"]);
        assert_eq!(ids_of(true, true), ["toggle", "pause_toggle", "show", "quit"]);
        // 空闲态即便 paused 位为真(不该发生)也不出这一项
        assert_eq!(ids_of(false, true), ["toggle", "show", "quit"]);
        // 开录途中:running 已置真但会话还没入槽 → 不出暂停项(点了只会报"没有正在进行的录制")
        let no_session = menu_items(true, None, false).into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(no_session, ["toggle", "show", "quit"]);
    }

    /// 录制动画的可见性契约:6 帧必须两两不同,且都不等于静止帧。
    /// 为什么存在:PR#57 的图标流水线曾把 7 张帧 PNG 覆盖成 App 图标的近似副本,
    /// 动画线程照常循环但每帧长一样——用户看到"小姑娘不写字了"。字节级去重是
    /// 最便宜的哨兵:真动画必然逐帧有别,流水线再次压平资产时这里先红。
    #[test]
    fn rec_frames_are_pairwise_distinct_and_differ_from_idle() {
        for i in 0..REC_FRAMES.len() {
            assert_ne!(REC_FRAMES[i], IDLE_ICON, "第 {i} 帧不得等于静止帧(动画会隐形)");
            for j in (i + 1)..REC_FRAMES.len() {
                assert_ne!(REC_FRAMES[i], REC_FRAMES[j], "第 {i} 与第 {j} 帧重复(动画退化)");
            }
        }
    }
}
