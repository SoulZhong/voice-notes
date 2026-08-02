//! 后端用户可见文案的语言开关(spec 2026-08-02-i18n-design)。与前端 src/lib/i18n 同一
//! 份 settings.ui_lang 驱动:启动时 set_lang 一次,set_settings 检测变更再 set_lang。
//! 只管"用户可见"(托盘菜单、invoke 返回给前端展示的错误);日志/注释保持中文不经此。

use std::sync::atomic::{AtomicBool, Ordering};

/// 当前是否英文界面。false = 中文——默认值即历史行为,init 前的极短窗口不会闪英文。
static EN: AtomicBool = AtomicBool::new(false);

pub fn is_en() -> bool {
    EN.load(Ordering::Relaxed)
}

/// 按 settings.ui_lang 解析并应用:"zh"/"en" 直取;"system"(及未知值)按系统
/// locale,zh 开头(zh/zh-CN/zh-Hans…)即中文;取不到系统 locale 回落中文(与默认一致)。
pub fn set_lang(ui_lang: &str) {
    let en = match ui_lang {
        "zh" => false,
        "en" => true,
        _ => !sys_locale::get_locale()
            .map(|l| l.to_lowercase().starts_with("zh"))
            .unwrap_or(true),
    };
    EN.store(en, Ordering::Relaxed);
}

/// 按当前语言二选一的格式化:`tr!("未知模型: {id}", "Unknown model: {id}")`。
/// 两个格式串必须使用同名参数;返回 String。仅供本 crate 用户可见文案使用。
#[macro_export]
macro_rules! tr {
    ($zh:literal, $en:literal $(, $($arg:tt)+)?) => {
        if $crate::i18n::is_en() {
            format!($en $(, $($arg)+)?)
        } else {
            format!($zh $(, $($arg)+)?)
        }
    };
}

/// 语言是进程级全局,而 cargo test 并行跑用例:任何**改动**当前语言的测试都必须先拿到
/// 这把锁,否则会把正在断言中文的另一个用例掀翻(典型的偶发红)。中毒锁降级取值继续——
/// 一个 panic 的用例不该让后续全部卡死。
#[cfg(test)]
pub(crate) fn test_lang_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 本模块用例串行收敛在一个 #[test] 里,测完恢复默认中文;跨模块的并发由 test_lang_guard 挡。
    #[test]
    fn set_lang_and_tr_roundtrip() {
        let _guard = test_lang_guard();
        set_lang("en");
        assert!(is_en());
        assert_eq!(tr!("未知模型: {id}", "Unknown model: {id}", id = "x"), "Unknown model: x");
        set_lang("zh");
        assert!(!is_en());
        assert_eq!(tr!("未知模型: {id}", "Unknown model: {id}", id = "x"), "未知模型: x");
        // 未知值/system:不 panic,回落系统判定(结果依宿主环境,不断言具体值)。
        set_lang("system");
        set_lang("zh"); // 恢复默认,避免污染同进程其它用例
    }
}
