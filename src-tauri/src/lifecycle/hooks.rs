//! 迁移 hook 总线(通知类副作用的唯一挂点)。
//!
//! 契约:注册序执行;每个 hook 逐个 catch_unwind——任何 hook panic/失败只记
//! 日志,绝不影响主流程,也不影响后续 hook。保障类副作用(落盘/转码入队等
//! 语义契约)不走这里,走 Effect(见 machine.rs)。
//! 外部 hook(用户配置 shell/webhook)是未来在此注册的一个消费者,本期只留接口。

use super::machine::SessionState;

pub struct TransitionCtx<'a> {
    // note_id 目前尚无消费者读取(留给未来按笔记过滤的 hook,如外部 webhook 只关心
    // 某条笔记);单独标注 allow,不因暂无读者产生 dead_code 警告。
    #[allow(dead_code)]
    pub note_id: Option<&'a str>,
    pub from: &'a SessionState,
    pub to: &'a SessionState,
    /// 消费者发系统调用需要的句柄(如 TrayHook 调 tray::set_recording)。
    /// Option 而非裸引用:构造一个可用于单测的 `tauri::AppHandle` 离不开 tauri
    /// 的 `test` feature(mock runtime),会牵出 Cargo.toml 改动——本任务提交范围
    /// 只动 lifecycle/ 与 lib.rs,故留 None 供 hooks.rs 自身的总线机制测试(顺序/
    /// 隔离)构造 ctx 而不依赖真实 AppHandle。生产路径(actor.rs)恒传 Some;
    /// 消费者对 None 静默跳过即可(该分支只在测试出现)。
    pub app: Option<&'a tauri::AppHandle>,
}

pub trait LifecycleHook: Send + Sync {
    fn name(&self) -> &'static str;
    fn on_transition(&self, ctx: &TransitionCtx);
}

#[derive(Default)]
pub struct HookBus {
    hooks: Vec<Box<dyn LifecycleHook>>,
}

/// 外部 hook 配置占位(下期实现执行体:shell 命令/webhook)。
#[allow(dead_code)]
pub struct ExternalHookCfg {
    pub event: String,
    pub command: String,
}

impl HookBus {
    /// P3 起在 actor spawn 处注册消费者(见 actor.rs::spawn 首个 TrayHook)。
    pub fn register(&mut self, hook: Box<dyn LifecycleHook>) {
        self.hooks.push(hook);
    }

    /// 预留:外部 hook 注册入口。本期不实现执行体——记日志并忽略,不 panic。
    #[allow(dead_code)]
    pub fn register_external(&mut self, _cfg: ExternalHookCfg) {
        eprintln!("lifecycle: 外部 hook 尚未支持(接口预留),已忽略");
    }

    pub fn notify(&self, ctx: &TransitionCtx) {
        for h in &self.hooks {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                h.on_transition(ctx);
            }));
            if r.is_err() {
                eprintln!("lifecycle hook '{}' panic(已隔离,主流程不受影响)", h.name());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Recorder { order: Arc<std::sync::Mutex<Vec<&'static str>>>, tag: &'static str }
    impl LifecycleHook for Recorder {
        fn name(&self) -> &'static str { self.tag }
        fn on_transition(&self, _ctx: &TransitionCtx) {
            self.order.lock().unwrap().push(self.tag);
        }
    }

    struct Panicker;
    impl LifecycleHook for Panicker {
        fn name(&self) -> &'static str { "panicker" }
        fn on_transition(&self, _ctx: &TransitionCtx) { panic!("boom"); }
    }

    struct Counter(Arc<AtomicUsize>);
    impl LifecycleHook for Counter {
        fn name(&self) -> &'static str { "counter" }
        fn on_transition(&self, _ctx: &TransitionCtx) { self.0.fetch_add(1, Ordering::SeqCst); }
    }

    // app: None —— 本文件测的是总线机制(注册序/panic 隔离),与消费者要发的系统调用
    // 无关,构造真实 AppHandle 无必要也不可行(见 TransitionCtx.app 字段注释)。
    fn ctx_fixture<'a>(from: &'a SessionState, to: &'a SessionState) -> TransitionCtx<'a> {
        TransitionCtx { note_id: Some("n1"), from, to, app: None }
    }

    #[test]
    fn hooks_run_in_registration_order() {
        let order = Arc::new(std::sync::Mutex::new(vec![]));
        let mut bus = HookBus::default();
        bus.register(Box::new(Recorder { order: order.clone(), tag: "a" }));
        bus.register(Box::new(Recorder { order: order.clone(), tag: "b" }));
        let (f, t) = (SessionState::Idle, SessionState::Idle);
        bus.notify(&ctx_fixture(&f, &t));
        assert_eq!(*order.lock().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn panicking_hook_is_isolated_and_rest_still_run() {
        let n = Arc::new(AtomicUsize::new(0));
        let mut bus = HookBus::default();
        bus.register(Box::new(Panicker));
        bus.register(Box::new(Counter(n.clone())));
        let (f, t) = (SessionState::Idle, SessionState::Idle);
        bus.notify(&ctx_fixture(&f, &t));
        assert_eq!(n.load(Ordering::SeqCst), 1, "panic hook 之后的 hook 必须照常执行");
    }
}
