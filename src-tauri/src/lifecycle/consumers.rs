//! hook 总线的首批真实消费者(P3 Task 1):托盘图标由迁移驱动,而非命令壳散点直调。
//!
//! 时序差异(与旧世界比):托盘翻转从「emit 状态事件后紧邻」变为「actor 处理完
//! 对应消息、commit 状态、bus.notify 之后」——同为毫秒级异步投递(tray::set_recording
//! 本身就是 fire-and-forget,排到主线程事件循环执行),用户不可感知。

use super::hooks::{LifecycleHook, TransitionCtx};
use super::machine::SessionState;

/// 迁移 → 托盘目标态的纯判定:无 IO、无 AppHandle 依赖,可直接单测四象限。
///
/// - 从非 Recording 进入 Recording(开录/续录成功)→ Some(true)。
/// - 从 Recording/Stopping 退到 Idle(停录收尾/空停)→ Some(false)。
/// - 其余一律 None,不触发托盘调用——尤其是 Recording{paused} 内部翻转
///   (暂停/恢复):from/to 都是 Recording{..}(仅 paused 字段不同),必须落在
///   第一条臂(Recording→Recording)才不会被下面「进入 Recording」的宽匹配
///   误判成"重新开始录制"。
pub fn tray_flag(from: &SessionState, to: &SessionState) -> Option<bool> {
    use SessionState::*;
    match (from, to) {
        (Recording { .. }, Recording { .. }) => None,
        (_, Recording { .. }) => Some(true),
        (Recording { .. } | Stopping { .. }, Idle) => Some(false),
        _ => None,
    }
}

/// 托盘消费者:决策逻辑全在 `tray_flag`(纯函数,单测覆盖);本结构只负责取值后
/// 转发给 `tray::set_recording`——后者已是 fire-and-forget(派发主线程执行,
/// 不阻塞、不持共享锁),满足 hook 契约(见 hooks.rs 模块文档的总线契约)。
pub struct TrayHook;

impl LifecycleHook for TrayHook {
    fn name(&self) -> &'static str {
        "tray"
    }

    fn on_transition(&self, ctx: &TransitionCtx) {
        let Some(recording) = tray_flag(ctx.from, ctx.to) else { return };
        // ctx.app 仅测试态为 None(见 TransitionCtx.app 字段注释);生产路径
        // (actor.rs::spawn)恒传 Some,这里静默跳过不影响真实运行时行为。
        let Some(app) = ctx.app else { return };
        crate::tray::set_recording(app, recording);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionState::*;

    fn rec(paused: bool) -> SessionState {
        Recording { note_id: "n1".into(), paused }
    }

    #[test]
    fn entering_recording_from_starting_is_true() {
        assert_eq!(tray_flag(&Starting { resume_id: None }, &rec(false)), Some(true));
    }

    #[test]
    fn entering_recording_from_idle_is_true() {
        // 理论上不经 Starting 直接 Idle->Recording 不会发生,但判定应仍按「进入
        // Recording」的通用规则给 Some(true),不依赖 from 的具体取值。
        assert_eq!(tray_flag(&Idle, &rec(false)), Some(true));
    }

    #[test]
    fn recording_to_idle_is_false() {
        assert_eq!(tray_flag(&rec(false), &Idle), Some(false));
    }

    #[test]
    fn stopping_to_idle_is_false() {
        assert_eq!(
            tray_flag(&Stopping { note_id: "n1".into() }, &Idle),
            Some(false)
        );
    }

    #[test]
    fn pause_toggle_within_recording_is_none() {
        assert_eq!(tray_flag(&rec(false), &rec(true)), None);
        assert_eq!(tray_flag(&rec(true), &rec(false)), None);
    }

    #[test]
    fn unrelated_transition_is_none() {
        assert_eq!(tray_flag(&Idle, &Starting { resume_id: None }), None);
    }
}
