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
    /// P1 运行期不构造(仅被 actor/回报对账匹配),P2 停止异步化后由内核构造。
    #[allow(dead_code)]
    Stopping { note_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Start { resume_id: Option<String> },
    Stop,
    Pause,
    Unpause,
    /// P1 运行期不构造:recording_status 保持直读 session 槽(P1 内核非权威,
    /// 经信箱回答只会引入无意义排队);P2 权威翻转时命令壳改发此命令。
    #[allow(dead_code)]
    QueryStatus,
}

/// 管线事件载荷(ASR worker 线程原样转发,actor 持 writer 执行)。
/// 仅 Debug:内部 `DiarEvent` 无 PartialEq,载荷不参与内核判定(内核不读 writer),
/// 故无需可比较——与 Msg 整体不再 Clone/PartialEq 的理由一致(见下)。
/// P2 Task 2 只定义形状,尚无生产路径构造(仅矩阵测试);Task 3/4 由 ASR worker 接线后
/// 各分支转为可达,届时可去掉 dead_code allow。
#[derive(Debug)]
#[allow(dead_code)]
pub enum PipelineOp {
    Final {
        source: String,
        text: String,
        start_ms: u64,
        end_ms: u64,
        speaker: Option<String>,
        rms: Option<f32>,
    },
    Diar(crate::session::DiarEvent),
}

/// Msg 不再 derive Clone/PartialEq/Debug:`AdoptWriter` 携带 `Box<NoteWriter>`,
/// 而 `NoteWriter` 本身无 Clone/PartialEq/Debug(File 句柄语义上不可比较/打印)。
/// 这是新增 writer 语义变体的直接后果——runner 侧只消费一次(Box 转移所有权),
/// 内核也从不比较或打印 Msg 本身,故无需这些 trait。
pub enum Msg {
    Cmd(Cmd),
    SessionStarted { note_id: String },
    SessionFailed,
    /// 保留:P1 兼容,Task 4 停录改造后由 Finalize 取代发送。
    SessionEnded { note_id: String },
    // —— P2 新增 ——
    /// P2 Task 2 只定义形状,尚无生产路径构造(仅矩阵测试);
    /// Task 3 接线 actor 持有 writer 后可去掉 dead_code allow。
    #[allow(dead_code)]
    AdoptWriter { writer: Box<crate::store::writer::NoteWriter> },
    #[allow(dead_code)]
    Pipeline(PipelineOp),
    /// 停止中 teardown 完成后自投:runner 已执行完 finalize IO,此消息只做
    /// 内核状态收敛(与 SessionEnded 同规则),故不产生 DoFinalize 效果。
    #[allow(dead_code)]
    Finalize { note_id: String },
    /// 加载失败路径:abort_or_finalize 语义。
    #[allow(dead_code)]
    AbortSession,
    #[allow(dead_code)]
    SetTitle { note_id: String, title: String },
    #[allow(dead_code)]
    RenameActiveSpeaker { note_id: String, speaker_id: String, name: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// 委托既有 do_* 执行体(P1 绞杀者语义:执行结果即 reply)。
    Delegate(Cmd),
    /// 内核直接拒绝(P1 不启用拒绝路径,全部 Delegate 让旧守卫发挥;见表)。
    /// P1 运行期不构造(actor 已实现其执行分支),P2 内核抢答守卫后由迁移表产生。
    #[allow(dead_code)]
    ReplyErr(String),
    /// 影子对账不一致:仅记日志,绝不影响主流程。
    ShadowMismatch(String),
    // —— P2 新增(runner 持 writer 执行;内核只发指令不做 IO) ——
    /// P2 Task 3 消费后删(runner 落 AdoptWriter 携带的 writer)。
    #[allow(dead_code)]
    DoAdopt,
    /// P2 Task 3 消费后删(runner 用持有的 writer 执行 PipelineOp)。
    #[allow(dead_code)]
    DoPipeline,
    /// P2 Task 3/4 消费后由 runner 持 writer 执行真实收尾(finalize IO)。
    DoFinalize { note_id: String },
    /// P2 Task 4 消费后删(runner 执行 abort_or_finalize)。
    #[allow(dead_code)]
    DoAbort,
    /// P2 Task 3/4 消费后删(runner 落标题)。
    #[allow(dead_code)]
    DoSetTitle { note_id: String, title: String },
    /// P2 Task 3/4 消费后删(runner 落说话人改名)。
    #[allow(dead_code)]
    DoRenameActiveSpeaker { note_id: String, speaker_id: String, name: String },
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
            // 回报为准:重置 paused 是有意行为(真实世界刚启动的会话必然未暂停)
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
        // —— P2 新增:writer 语义消息 ——
        // AdoptWriter/Pipeline/SetTitle/RenameActiveSpeaker/AbortSession 均不改会话态
        // (writer 归属、说话人表、标题都是 runner 侧状态,内核只转发一个 Do* 指令),
        // 也从不产生 ShadowMismatch——它们与「录制中/停止中」这条主时间轴正交,
        // 任何状态下发生都不构成对账矛盾。
        Msg::AdoptWriter { .. } => (state.clone(), vec![DoAdopt]),
        Msg::Pipeline(_) => (state.clone(), vec![DoPipeline]),
        Msg::AbortSession => (state.clone(), vec![DoAbort]),
        Msg::SetTitle { note_id, title } => (
            state.clone(),
            vec![DoSetTitle { note_id: note_id.clone(), title: title.clone() }],
        ),
        Msg::RenameActiveSpeaker { note_id, speaker_id, name } => (
            state.clone(),
            vec![DoRenameActiveSpeaker {
                note_id: note_id.clone(),
                speaker_id: speaker_id.clone(),
                name: name.clone(),
            }],
        ),
        // Finalize:状态收敛与 SessionEnded 同规则(Recording{id}/Stopping{id} 顺流
        // Idle 零噪音,其余态 Idle+ShadowMismatch),但任何状态下都恒产出恰一个
        // DoFinalize——runner 持 writer 执行真实收尾。即使态不符也发效果:writer
        // 若在槽里就必须收尾,宁可对账噪音不可漏 finalize。
        Msg::Finalize { note_id } => {
            let mut effects = vec![DoFinalize { note_id: note_id.clone() }];
            match state {
                Recording { note_id: id, .. } | Stopping { note_id: id } if id == note_id => {}
                other => effects.push(ShadowMismatch(format!(
                    "Finalize({note_id}) 抵达时内核态为 {other:?}"
                ))),
            }
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

    /// 回报消息 4 状态 × 3 消息类型穷举矩阵。
    /// 验证:终态正确、对账噪音预期、paused 重置语义。
    #[test]
    fn every_report_in_every_state_reconciles() {
        // 测试状态
        let states = vec![
            ("Idle", Idle),
            ("Starting", Starting { resume_id: None }),
            ("Recording paused=false", rec("n1")),
            ("Stopping", Stopping { note_id: "n1".into() }),
        ];

        // 测试消息
        let started_msg = Msg::SessionStarted { note_id: "n1".into() };
        let failed_msg = Msg::SessionFailed;
        let ended_msg = Msg::SessionEnded { note_id: "n1".into() };

        for (state_name, state) in &states {
            // === SessionStarted 消息 ===
            let (next_state, effects) = handle(state, &started_msg);
            // 终态:始终进入 Recording{"n1", paused:false}
            assert_eq!(
                next_state,
                rec("n1"),
                "{state_name} 收 SessionStarted 应转入 Recording"
            );
            // 对账:只有 Starting 是顺流(无噪音),其余都有一个 ShadowMismatch
            let is_compatible = matches!(state, Starting { .. });
            if is_compatible {
                assert!(
                    effects.is_empty(),
                    "{state_name} + SessionStarted 是顺流组合,effects 应为空"
                );
            } else {
                assert_eq!(
                    effects.len(),
                    1,
                    "{state_name} + SessionStarted 不兼容,应有一个 ShadowMismatch"
                );
                assert!(matches!(effects[0], Effect::ShadowMismatch(_)));
            }

            // === SessionFailed 消息 ===
            let (next_state, effects) = handle(state, &failed_msg);
            // 终态:始终返回 Idle
            assert_eq!(
                next_state, Idle,
                "{state_name} 收 SessionFailed 应返回 Idle"
            );
            // 对账:只有 Starting 是顺流,其余都有一个 ShadowMismatch
            let is_compatible = matches!(state, Starting { .. });
            if is_compatible {
                assert!(
                    effects.is_empty(),
                    "{state_name} + SessionFailed 是顺流组合,effects 应为空"
                );
            } else {
                assert_eq!(
                    effects.len(),
                    1,
                    "{state_name} + SessionFailed 不兼容,应有一个 ShadowMismatch"
                );
                assert!(matches!(effects[0], Effect::ShadowMismatch(_)));
            }

            // === SessionEnded 消息 ===
            let (next_state, effects) = handle(state, &ended_msg);
            // 终态:始终返回 Idle
            assert_eq!(
                next_state, Idle,
                "{} 收 SessionEnded(n1) 应返回 Idle",
                state_name
            );
            // 对账:只有 Recording{"n1"} 和 Stopping{"n1"} 是顺流,其余都有一个 ShadowMismatch
            let is_compatible = matches!(
                state,
                Recording { note_id, .. } | Stopping { note_id }
                    if note_id == "n1"
            );
            if is_compatible {
                assert!(
                    effects.is_empty(),
                    "{} + SessionEnded(n1) 是顺流组合,effects 应为空",
                    state_name
                );
            } else {
                assert_eq!(
                    effects.len(),
                    1,
                    "{} + SessionEnded(n1) 不兼容,应有一个 ShadowMismatch",
                    state_name
                );
                assert!(matches!(effects[0], Effect::ShadowMismatch(_)));
            }
        }

        // 特殊验证:Recording 状态下 paused 被重置为 false
        // 先构造 paused=true 的 Recording 状态
        let recording_paused = Recording { note_id: "n1".into(), paused: true };
        let (next_state, effects) = handle(&recording_paused, &started_msg);
        // 验证 paused 被重置为 false
        assert_eq!(
            next_state,
            rec("n1"),
            "Recording(paused=true) 收 SessionStarted 应重置 paused 为 false"
        );
        // Recording + SessionStarted 产生一个 ShadowMismatch
        assert_eq!(effects.len(), 1, "不兼容的组合应有一个 ShadowMismatch");
        assert!(matches!(effects[0], Effect::ShadowMismatch(_)));
    }

    /// P2 前提:Cmd 产生的预演迁移必须可由 runner 回退——handle 本身无副作用,
    /// 同一状态重放同一 Cmd 结果恒定(幂等),runner 丢弃 next 即等于未发生。
    #[test]
    fn cmd_handling_is_pure_and_replayable() {
        let s = SessionState::Idle;
        let m = Msg::Cmd(Cmd::Start { resume_id: None });
        let a = handle(&s, &m);
        let b = handle(&s, &m);
        assert_eq!(a, b, "纯函数:同输入必同输出,runner 才能安全回退预演");
    }

    fn writer_box() -> Box<crate::store::writer::NoteWriter> {
        let tmp = tempfile::tempdir().unwrap();
        let w = crate::store::writer::NoteWriter::create(tmp.path(), chrono::Local::now())
            .expect("NoteWriter::create 不应失败");
        Box::new(w)
    }

    /// P2 迁移规则矩阵(5 类新消息 × 4 状态):AdoptWriter/Pipeline/SetTitle/
    /// RenameActiveSpeaker/AbortSession 与「录制中/停止中」主时间轴正交——writer
    /// 归属、说话人表、标题都是 runner 侧状态,内核只转发指令。任何状态下都应
    /// 状态不变 + 恰一个对应 Do* 效果 + 零 ShadowMismatch(载荷不进内核判定)。
    #[test]
    fn writer_semantic_msgs_are_state_orthogonal_in_every_state() {
        let states = [Idle, Starting { resume_id: None }, rec("n1"), Stopping { note_id: "n1".into() }];

        for s in &states {
            let (next, fx) = handle(s, &Msg::AdoptWriter { writer: writer_box() });
            assert_eq!(&next, s, "AdoptWriter 不应改变状态:state={s:?}");
            assert!(matches!(fx.as_slice(), [Effect::DoAdopt]), "state={s:?} fx={fx:?}");

            let op = PipelineOp::Final {
                source: "mic".into(),
                text: "hi".into(),
                start_ms: 0,
                end_ms: 1,
                speaker: None,
                rms: None,
            };
            let (next, fx) = handle(s, &Msg::Pipeline(op));
            assert_eq!(&next, s, "Pipeline 不应改变状态:state={s:?}");
            assert!(matches!(fx.as_slice(), [Effect::DoPipeline]), "state={s:?} fx={fx:?}");

            let (next, fx) = handle(s, &Msg::AbortSession);
            assert_eq!(&next, s, "AbortSession 不应改变状态:state={s:?}");
            assert!(matches!(fx.as_slice(), [Effect::DoAbort]), "state={s:?} fx={fx:?}");

            let (next, fx) =
                handle(s, &Msg::SetTitle { note_id: "n1".into(), title: "新标题".into() });
            assert_eq!(&next, s, "SetTitle 不应改变状态:state={s:?}");
            assert!(
                matches!(
                    fx.as_slice(),
                    [Effect::DoSetTitle { note_id, title }]
                        if note_id == "n1" && title == "新标题"
                ),
                "state={s:?} fx={fx:?}"
            );

            let (next, fx) = handle(
                s,
                &Msg::RenameActiveSpeaker {
                    note_id: "n1".into(),
                    speaker_id: "spk1".into(),
                    name: "张三".into(),
                },
            );
            assert_eq!(&next, s, "RenameActiveSpeaker 不应改变状态:state={s:?}");
            assert!(
                matches!(
                    fx.as_slice(),
                    [Effect::DoRenameActiveSpeaker { note_id, speaker_id, name }]
                        if note_id == "n1" && speaker_id == "spk1" && name == "张三"
                ),
                "state={s:?} fx={fx:?}"
            );
        }
    }

    /// Finalize:状态收敛与 SessionEnded 同规则(顺流零噪音/其余 ShadowMismatch),
    /// 但任何状态下都恒产出恰一个 DoFinalize{note_id}——即使态不符也不许漏收尾。
    #[test]
    fn finalize_follows_session_ended_rule_in_every_state() {
        let states = vec![
            ("Idle", Idle),
            ("Starting", Starting { resume_id: None }),
            ("Recording n1", rec("n1")),
            ("Stopping n1", Stopping { note_id: "n1".into() }),
        ];
        let finalize_msg = Msg::Finalize { note_id: "n1".into() };

        for (name, state) in &states {
            let (next_state, effects) = handle(state, &finalize_msg);
            assert_eq!(next_state, Idle, "{name} 收 Finalize(n1) 应归 Idle");
            // 任何状态下都恰有一个 DoFinalize,note_id 原样携带
            let do_finalize_count = effects
                .iter()
                .filter(|e| matches!(e, Effect::DoFinalize { note_id } if note_id == "n1"))
                .count();
            assert_eq!(do_finalize_count, 1, "{name} + Finalize(n1) 必须恰产出一个 DoFinalize(n1):{effects:?}");
            let is_compatible = matches!(
                state,
                Recording { note_id, .. } | Stopping { note_id }
                    if note_id == "n1"
            );
            if is_compatible {
                assert_eq!(effects.len(), 1, "{name} + Finalize(n1) 是顺流组合,除 DoFinalize 外应零噪音:{effects:?}");
            } else {
                assert_eq!(effects.len(), 2, "{name} + Finalize(n1) 不兼容,应为 DoFinalize + 一个 ShadowMismatch:{effects:?}");
                assert_eq!(
                    effects.iter().filter(|e| matches!(e, Effect::ShadowMismatch(_))).count(),
                    1,
                    "{name} + Finalize(n1) 不兼容,应恰有一个 ShadowMismatch:{effects:?}"
                );
            }
        }
    }
}
