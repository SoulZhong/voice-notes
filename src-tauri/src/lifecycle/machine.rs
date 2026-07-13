//! lifecycle 纯函数内核:状态机迁移表的唯一载体。
//!
//! P1 阶段内核是「影子」:命令一律 Delegate 给既有 do_* 执行体,内核只按
//! 工作线程回报(SessionStarted/Failed/Ended)演进自己的状态并与旧世界对账
//! (不一致仅产生 ShadowMismatch 效果=记日志)。权威翻转是 P2 的事——
//! 这保证 P1 行为与现状逐位等价,而迁移表已可被全覆盖单测锁死。
//! 无 IO、无锁、无 AppHandle:所有副作用以 Effect 值返回,runner 负责执行。

#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    Idle,
    /// 会话装配中(模型加载/音频源构建在后台线程)。resume_id=Some 为续录。
    Starting { resume_id: Option<String> },
    Recording { note_id: String, paused: bool },
    /// 停止中(handle.stop+finalize 在工作线程)。P1 阶段停止仍同步委托,
    /// 此态在 P1 只在 Delegate 前后瞬间存在,为 P2 预留。
    Stopping { note_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Start { resume_id: Option<String> },
    Stop,
    Pause,
    Unpause,
    QueryStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Cmd(Cmd),
    SessionStarted { note_id: String },
    SessionFailed,
    SessionEnded { note_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// 委托既有 do_* 执行体(P1 绞杀者语义:执行结果即 reply)。
    Delegate(Cmd),
    /// 内核直接拒绝(P1 不启用拒绝路径,全部 Delegate 让旧守卫发挥;见表)。
    ReplyErr(String),
    /// 影子对账不一致:仅记日志,绝不影响主流程。
    ShadowMismatch(String),
}

/// 迁移表。P1 铁律:凡 Cmd 一律产生 Delegate(旧守卫是权威,内核不抢答),
/// 内核状态只由回报消息驱动;回报与当前态矛盾时记 ShadowMismatch 并
/// 以回报为准(回报来自真实世界)。
pub fn handle(state: &SessionState, msg: &Msg) -> (SessionState, Vec<Effect>) {
    use Effect::*;
    use SessionState::*;
    match msg {
        Msg::Cmd(c) => {
            let next = match (state, c) {
                (Idle, Cmd::Start { resume_id }) => Starting { resume_id: resume_id.clone() },
                // 其余组合不预演状态——委托后旧守卫可能拒绝,状态由回报驱动
                _ => state.clone(),
            };
            (next, vec![Delegate(c.clone())])
        }
        Msg::SessionStarted { note_id } => {
            let effects = match state {
                Starting { .. } => vec![],
                other => vec![ShadowMismatch(format!(
                    "SessionStarted 抵达时内核态为 {other:?}(预期 Starting)"
                ))],
            };
            (Recording { note_id: note_id.clone(), paused: false }, effects)
        }
        Msg::SessionFailed => {
            let effects = match state {
                Starting { .. } => vec![],
                other => vec![ShadowMismatch(format!(
                    "SessionFailed 抵达时内核态为 {other:?}(预期 Starting)"
                ))],
            };
            (Idle, effects)
        }
        Msg::SessionEnded { note_id } => {
            let effects = match state {
                Recording { note_id: id, .. } | Stopping { note_id: id } if id == note_id => vec![],
                other => vec![ShadowMismatch(format!(
                    "SessionEnded({note_id}) 抵达时内核态为 {other:?}"
                ))],
            };
            (Idle, effects)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionState::*;

    fn rec(id: &str) -> SessionState {
        Recording { note_id: id.into(), paused: false }
    }

    /// P1 铁律:任何状态收任何 Cmd 都且仅产生一个 Delegate(旧守卫是权威)。
    #[test]
    fn every_cmd_in_every_state_delegates() {
        let states = [Idle, Starting { resume_id: None }, rec("n1"), Stopping { note_id: "n1".into() }];
        let cmds = [
            Cmd::Start { resume_id: None },
            Cmd::Start { resume_id: Some("n1".into()) },
            Cmd::Stop, Cmd::Pause, Cmd::Unpause, Cmd::QueryStatus,
        ];
        for s in &states {
            for c in &cmds {
                let (_, fx) = handle(s, &Msg::Cmd(c.clone()));
                assert_eq!(fx, vec![Effect::Delegate(c.clone())], "state={s:?} cmd={c:?}");
            }
        }
    }

    #[test]
    fn idle_start_enters_starting_and_started_enters_recording() {
        let (s1, _) = handle(&Idle, &Msg::Cmd(Cmd::Start { resume_id: None }));
        assert_eq!(s1, Starting { resume_id: None });
        let (s2, fx) = handle(&s1, &Msg::SessionStarted { note_id: "n1".into() });
        assert_eq!(s2, rec("n1"));
        assert!(fx.is_empty(), "顺流迁移不应有对账噪音");
    }

    #[test]
    fn failed_returns_idle() {
        let (s, fx) = handle(&Starting { resume_id: None }, &Msg::SessionFailed);
        assert_eq!(s, Idle);
        assert!(fx.is_empty());
    }

    #[test]
    fn ended_from_recording_returns_idle_quietly() {
        let (s, fx) = handle(&rec("n1"), &Msg::SessionEnded { note_id: "n1".into() });
        assert_eq!(s, Idle);
        assert!(fx.is_empty());
    }

    /// 回报与内核态矛盾:以回报为准 + 记对账差异。
    #[test]
    fn out_of_order_reports_reconcile_with_mismatch_logged() {
        let (s, fx) = handle(&Idle, &Msg::SessionStarted { note_id: "n1".into() });
        assert_eq!(s, rec("n1"), "回报来自真实世界,必须采纳");
        assert!(matches!(fx.as_slice(), [Effect::ShadowMismatch(_)]));

        let (s, fx) = handle(&rec("n1"), &Msg::SessionEnded { note_id: "n2".into() });
        assert_eq!(s, Idle);
        assert!(matches!(fx.as_slice(), [Effect::ShadowMismatch(_)]));
    }
}
