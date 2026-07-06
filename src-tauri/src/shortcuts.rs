//! 全局快捷键的注册与分发。为什么独立成模块:注册失败必须能作为 Result 上抛给设置页
//! (据此提示用户并回落关开关),而按键分发要在插件 handler 里静默切换录制——两件事都
//! 围绕 GlobalShortcut 插件,聚在一处便于对照。

use tauri::Manager;
use tauri_plugin_global_shortcut::{Shortcut, ShortcutEvent, ShortcutState};

/// 插件 handler:同一次按键会先后触发 Pressed/Released 两个事件,只在 Pressed 沿切换
/// 一次录制——否则一次按键会开录又立刻停录(或反向),等于什么都没发生。快捷键触发
/// 没有 UI 上下文,toggle_recording 内部的开录错误只进日志(见其实现)。
pub fn on_shortcut(app: &tauri::AppHandle, _sc: &Shortcut, ev: ShortcutEvent) {
    if ev.state == ShortcutState::Pressed {
        crate::toggle_recording(app);
    }
}

/// 依据当前设置(重)注册全局快捷键。先 unregister_all 再按需 register:设置页每次保存
/// 都会调本函数,先清空旧绑定保证重注册幂等,不会把同一/历史快捷键越堆越多。
/// parse/register 失败一律上抛中文错误——这是设置页保存路径(apply_shortcut)的判据,
/// 用于提示用户并回落关闭开关;而 setup 启动路径对同一个 Err 只 eprintln 不挡启动。
pub fn apply_from_settings(app: &tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    // 先清旧:保证本函数可反复调用而不残留、不重复注册(幂等前提)。
    app.global_shortcut()
        .unregister_all()
        .map_err(|e| format!("清除旧快捷键失败: {e}"))?;
    // settings.json 是自举指针,永在 app_data_dir;读不到目录则退回默认设置
    //(shortcut_enabled=false),即本次不注册任何快捷键。
    let s = match app.path().app_data_dir() {
        Ok(d) => crate::settings::load(&d),
        Err(_) => crate::settings::Settings::default(),
    };
    if s.shortcut_enabled {
        let sc = s
            .shortcut
            .parse::<Shortcut>()
            .map_err(|e| format!("快捷键格式无效: {e}"))?;
        app.global_shortcut()
            .register(sc)
            .map_err(|e| format!("快捷键注册失败(可能与系统或其它应用冲突): {e}"))?;
    }
    Ok(())
}
