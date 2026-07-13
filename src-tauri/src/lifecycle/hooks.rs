#![allow(dead_code)] // P1 Task 6 接线后删除

//! 迁移 hook 总线(通知类副作用的唯一挂点)。
//!
//! 契约:注册序执行;每个 hook 逐个 catch_unwind——任何 hook panic/失败只记
//! 日志,绝不影响主流程,也不影响后续 hook。保障类副作用(落盘/转码入队等
//! 语义契约)不走这里,走 Effect(见 machine.rs)。
//! 外部 hook(用户配置 shell/webhook)是未来在此注册的一个消费者,本期只留接口。

use super::machine::SessionState;

pub struct TransitionCtx<'a> {
    pub note_id: Option<&'a str>,
    pub from: &'a SessionState,
    pub to: &'a SessionState,
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

    fn ctx_fixture<'a>(from: &'a SessionState, to: &'a SessionState) -> TransitionCtx<'a> {
        TransitionCtx { note_id: Some("n1"), from, to }
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
