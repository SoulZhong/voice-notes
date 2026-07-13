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

/// 精修维度(P3):取代 AppState.refining 集合。单槽只记「最近一次发起的精修」——
/// 旧集合可同时容纳多条笔记并发精修(手动精修 A 期间停录 B 触发自动精修),单槽
/// 下先启动的那条会被顶掉、其守卫(续录拦截/重复精修拒绝)在被顶掉期间失效。
/// 可达但罕见(需要精修 A 未完成时恰好又停录 B),且数据安全由 spawn_refine 的
/// is_resumed_by_active_session 入队检查兜底(见 lib.rs),故按设计取单槽。
#[derive(Debug, Clone, PartialEq)]
pub enum RefineState {
    Idle,
    Running { note_id: String },
}

/// 内核状态升维(P3):会话主时间轴 + 精修维度。两维正交——会话消息不动 refine,
/// Refine* 消息不动 session;唯一交叉点是两条守卫(RefineRequest 查 session、
/// Start{resume} 查 refine),都只读不写对方维度。
#[derive(Debug, Clone, PartialEq)]
pub struct LifecycleState {
    pub session: SessionState,
    pub refine: RefineState,
}

impl LifecycleState {
    /// 初始态:无会话、无精修。
    pub fn init() -> Self {
        LifecycleState { session: SessionState::Idle, refine: RefineState::Idle }
    }
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
/// 生产路径:lib.rs 的 on_final/on_diar 回调构造(时间戳已在回调侧加好续录偏移,
/// 消息里恒为落盘口径的绝对时间轴),actor 的 run_pipeline 消费。
#[derive(Debug)]
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
    /// 生产不再构造(停录已改自投 Finalize 收敛状态),分支保留兼容——迁移矩阵
    /// 测试仍全覆盖,P3 若确认无外部消费者再删。
    #[allow(dead_code)]
    SessionEnded { note_id: String },
    // —— P2 新增 ——
    /// 加载线程在 writer 创建、读完元信息后整体移交所有权(Box 入信箱),
    /// 此后该线程不得再持 writer 引用;runner 装入 Owned 槽。
    AdoptWriter { writer: Box<crate::store::writer::NoteWriter> },
    /// note_id 携带发送侧认定的归属会话:双加载线程重叠窗口下(start→卡住→
    /// stop→start),迟到的管线消息可能与槽内新会话不属同一笔记——actor 执行
    /// DoPipeline 时按 note_id 对账,不匹配则丢弃,绝不误写进新会话的 writer。
    Pipeline { note_id: String, op: PipelineOp },
    /// 停录 teardown(排干)完成后由 actor 自投:该消息排在排干期间入队的全部
    /// Pipeline 消息之后,「先落盘后 finalize」由信箱 FIFO 保证。
    Finalize { note_id: String },
    /// 加载失败路径:abort_or_finalize 语义(作用于 runner 槽内 writer)。note_id
    /// 携带发送侧认定的归属会话——理由同 Pipeline:迟到的 AbortSession 若不
    /// 对账,可能误杀槽内已入驻的新会话 writer(整场丢失),actor 侧不匹配则跳过。
    AbortSession { note_id: String },
    SetTitle { note_id: String, title: String },
    RenameActiveSpeaker { note_id: String, speaker_id: String, name: String },
    // —— P3 新增:精修维度 ——
    /// 手动精修(refine_note 命令壳经 request 带回执):守卫裁决入内核——录制中
    /// 拒绝、同 id 精修中拒绝,文案与旧命令壳逐字一致。
    RefineRequest { note_id: String },
    /// spawn_refine 的进度回报(原 worker 直发 emit("refine",..) 改道,由 actor
    /// 统一对外发事件)。"all/running" 兼作精修开始的置态信号:spawn_refine 在
    /// spawn 线程之前同步发出这一条(见 lib.rs),内核收到即置 Running——自动
    /// 精修路径(DoFinalize 保障类直调,不经 RefineRequest)靠它置态。
    RefineProgress { note_id: String, stage: String, state: String },
    /// spawn_refine worker 线程结束前的最后一条回报(原 refining.remove 的时机:
    /// 在收尾 emit 与兜底转码入队之后):按 id 对账,命中才把精修态置回 Idle。
    RefineFinished { note_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// 委托既有 do_* 执行体(P1 绞杀者语义:执行结果即 reply)。
    Delegate(Cmd),
    /// 内核直接拒绝。P3 起真实产生:RefineRequest 的两条守卫与「续录目标正在
    /// 精修」的 Start 抢答(此前 P1/P2 只有 actor 的执行分支,运行期不构造)。
    ReplyErr(String),
    /// 影子对账不一致:仅记日志,绝不影响主流程。
    ShadowMismatch(String),
    // —— P2 新增(runner 持 writer 执行;内核只发指令不做 IO) ——
    // Do* 效果刻意不带 writer/管线载荷:Box<NoteWriter> 不可克隆,管线文本克隆也
    // 无谓——runner 在效果执行时从本轮原始 Msg 一次性取走(每条消息恰一个对应效果)。
    // note_id 例外:String 廉价可克隆,随效果本身携带(与 DoFinalize/DoSetTitle
    // 同例),供 runner 与槽内 owned.note_id 对账,核对不上就丢弃/跳过——防双加载
    // 线程重叠窗口下迟到消息误写/误杀新会话的 writer(见下方 P2 对账加固注记)。
    /// runner 把 AdoptWriter 携带的 writer 装入 Owned 槽。
    DoAdopt,
    /// runner 用槽内 writer 执行 PipelineOp(append/说话人事件,含对应 emit)。
    /// note_id 与槽内 owned.note_id 不一致(迟到消息、会话已更替)则整条丢弃。
    DoPipeline { note_id: String },
    /// runner 持槽内 writer 执行真实收尾(finalize IO+精修/stopped 尾段)。
    DoFinalize { note_id: String },
    /// runner 对槽内 writer 执行 abort_or_finalize 语义并清槽。note_id 与槽内
    /// owned.note_id 不一致(迟到放弃、会话已更替)则跳过,绝不动新会话 writer。
    DoAbort { note_id: String },
    /// runner 落录制中标题(writer 单写者路径,UDS start --title 消费)。
    DoSetTitle { note_id: String, title: String },
    /// runner 落活动会话说话人改名(persist+speakers 快照 emit)。
    DoRenameActiveSpeaker { note_id: String, speaker_id: String, name: String },
    // —— P3 新增:精修维度 ——
    /// runner 调 spawn_refine 发起手动精修(守卫已在内核裁决通过,refine 已置
    /// Running)。enqueue_transcode 恒 false:手动重跑时 m4a 早已在盘上(首次精修
    /// 已移交过转码),与原 refine_note 调用一致;自动精修不经此效果(DoFinalize
    /// 内保障类直调 spawn_refine(.., true),不受守卫约束——与旧世界一致)。
    DoSpawnRefine { note_id: String, enqueue_transcode: bool },
    /// runner 对外发既有 "refine" 事件(字段与 ipc::RefineEvent 一一对应)。同一
    /// worker 串行 report + 信箱 FIFO ⇒ 事件种类/载荷/顺序与旧 worker 直发逐位一致。
    DoEmitRefine { note_id: String, stage: String, state: String },
}

/// 迁移表。P1 铁律:凡 Cmd 一律产生 Delegate(旧守卫是权威,内核不抢答),
/// 内核状态只由回报消息驱动;回报与当前态矛盾时记 ShadowMismatch 并
/// 以回报为准(回报来自真实世界)。
/// P3 唯一例外:Start{resume_id} 命中「同一笔记正在精修」由内核抢答拒绝
/// (原 lib.rs resume_blocked_by_refining 守卫上移,见 Cmd 分支注释)。
pub fn handle(state: &LifecycleState, msg: &Msg) -> (LifecycleState, Vec<Effect>) {
    use Effect::*;
    use SessionState::*;
    // 两维正交的机械保证:会话迁移原样带过精修维度,精修迁移原样带过会话维度。
    let with_session =
        |session: SessionState| LifecycleState { session, refine: state.refine.clone() };
    let with_refine =
        |refine: RefineState| LifecycleState { session: state.session.clone(), refine };
    match msg {
        Msg::Cmd(c) => {
            // P3 守卫上移(原 lib.rs resume_blocked_by_refining,文案逐字):精修完成
            // 后才 transcode.enqueue,而续录先 cancel_and_wait 再向 mic.wav 追加写;
            // 若放行,精修收尾时才入队的转码会把「活跃在追加」的 WAV 编码后删除,
            // 续录段音频永久丢失(F1)。故必须挡在 Delegate 之前。只挡命中的 id。
            if let Cmd::Start { resume_id: Some(id) } = c {
                if matches!(&state.refine, RefineState::Running { note_id } if note_id == id) {
                    return (state.clone(), vec![ReplyErr("该笔记正在精修,请稍后再试".into())]);
                }
            }
            let next = match (&state.session, c) {
                (Idle, Cmd::Start { resume_id }) => Starting { resume_id: resume_id.clone() },
                // 其余组合不预演状态——委托后旧守卫可能拒绝,状态由回报驱动
                _ => state.session.clone(),
            };
            (with_session(next), vec![Delegate(c.clone())])
        }
        Msg::SessionStarted { note_id } => {
            let effects = match &state.session {
                Starting { .. } => vec![],
                other => vec![ShadowMismatch(format!(
                    "SessionStarted 抵达时内核态为 {other:?}(预期 Starting)"
                ))],
            };
            // 回报为准:重置 paused 是有意行为(真实世界刚启动的会话必然未暂停)
            (with_session(Recording { note_id: note_id.clone(), paused: false }), effects)
        }
        Msg::SessionFailed => {
            let effects = match &state.session {
                Starting { .. } => vec![],
                other => vec![ShadowMismatch(format!(
                    "SessionFailed 抵达时内核态为 {other:?}(预期 Starting)"
                ))],
            };
            (with_session(Idle), effects)
        }
        Msg::SessionEnded { note_id } => {
            let effects = match &state.session {
                Recording { note_id: id, .. } | Stopping { note_id: id } if id == note_id => vec![],
                other => vec![ShadowMismatch(format!(
                    "SessionEnded({note_id}) 抵达时内核态为 {other:?}"
                ))],
            };
            (with_session(Idle), effects)
        }
        // —— P2 新增:writer 语义消息 ——
        // AdoptWriter/Pipeline/SetTitle/RenameActiveSpeaker/AbortSession 均不改会话态
        // (writer 归属、说话人表、标题都是 runner 侧状态,内核只转发一个 Do* 指令),
        // 也从不产生 ShadowMismatch——它们与「录制中/停止中」这条主时间轴正交,
        // 任何状态下发生都不构成对账矛盾。
        Msg::AdoptWriter { .. } => (state.clone(), vec![DoAdopt]),
        Msg::Pipeline { note_id, .. } => {
            (state.clone(), vec![DoPipeline { note_id: note_id.clone() }])
        }
        Msg::AbortSession { note_id } => {
            (state.clone(), vec![DoAbort { note_id: note_id.clone() }])
        }
        // —— P3 新增:精修维度(会话维度一律原样带过) ——
        Msg::RefineRequest { note_id } => {
            // 守卫序与原 refine_note 命令壳一致:先查录制、再查精修——两者同时
            // 命中时报「正在录制」,与旧文案选择一致。文案逐字搬自旧壳。
            if matches!(
                &state.session,
                Recording { note_id: id, .. } | Stopping { note_id: id } if id == note_id
            ) {
                return (state.clone(), vec![ReplyErr("该笔记正在录制，停止后才能精修".into())]);
            }
            // 按 id 判(旧世界 refining 是集合,refine_note 只查该 id):别的笔记在
            // 精修不挡本笔记。单槽顶替语义见 RefineState 文档注释。
            if matches!(&state.refine, RefineState::Running { note_id: r } if r == note_id) {
                return (state.clone(), vec![ReplyErr("该笔记正在精修中".into())]);
            }
            (
                with_refine(RefineState::Running { note_id: note_id.clone() }),
                vec![DoSpawnRefine { note_id: note_id.clone(), enqueue_transcode: false }],
            )
        }
        Msg::RefineProgress { note_id, stage, state: st } => {
            // "all/running" 是 spawn_refine 的第一条回报(spawn 线程前同步发出):
            // 手动路径 RefineRequest 已置 Running(此处幂等重置),自动路径靠它置态。
            // 其余进度只转发事件不动状态;置回 Idle 由 RefineFinished 负责——对齐
            // 旧世界 refining.remove 在收尾 emit 与兜底转码入队之后的时机。
            let next = if stage == "all" && st == "running" {
                with_refine(RefineState::Running { note_id: note_id.clone() })
            } else {
                state.clone()
            };
            (
                next,
                vec![DoEmitRefine {
                    note_id: note_id.clone(),
                    stage: stage.clone(),
                    state: st.clone(),
                }],
            )
        }
        Msg::RefineFinished { note_id } => match &state.refine {
            RefineState::Running { note_id: r } if r == note_id => {
                (with_refine(RefineState::Idle), vec![])
            }
            // 不命中(单槽被更晚的精修顶替后,先前 worker 的迟到收尾)或已 Idle:
            // 不动状态,只记对账噪音——绝不能把别的笔记的 Running 误清掉。
            other => (
                state.clone(),
                vec![ShadowMismatch(format!(
                    "RefineFinished({note_id}) 抵达时精修态为 {other:?}"
                ))],
            ),
        },
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
            match &state.session {
                Recording { note_id: id, .. } | Stopping { note_id: id } if id == note_id => {}
                other => effects.push(ShadowMismatch(format!(
                    "Finalize({note_id}) 抵达时内核态为 {other:?}"
                ))),
            }
            (with_session(Idle), effects)
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

    /// 会话态包装成完整内核态(精修维 Idle):既有会话矩阵测试的机械适配入口。
    fn ls(session: SessionState) -> LifecycleState {
        LifecycleState { session, refine: RefineState::Idle }
    }

    /// P1 铁律:任何状态收任何 Cmd 都且仅产生一个 Delegate(旧守卫是权威)。
    /// P3 起唯一例外——Start{resume} 命中「同一笔记正在精修」由内核抢答拒绝,
    /// 由 resume_rejected_only_while_same_note_refining 单独锁死;本表覆盖的
    /// refine=Idle 维度下铁律不变。
    #[test]
    fn every_cmd_in_every_state_delegates() {
        let states = [Idle, Starting { resume_id: None }, rec("n1"), Stopping { note_id: "n1".into() }]
            .map(ls);
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
        let (s1, _) = handle(&ls(Idle), &Msg::Cmd(Cmd::Start { resume_id: None }));
        assert_eq!(s1, ls(Starting { resume_id: None }));
        let (s2, fx) = handle(&s1, &Msg::SessionStarted { note_id: "n1".into() });
        assert_eq!(s2, ls(rec("n1")));
        assert!(fx.is_empty(), "顺流迁移不应有对账噪音");
    }

    #[test]
    fn failed_returns_idle() {
        let (s, fx) = handle(&ls(Starting { resume_id: None }), &Msg::SessionFailed);
        assert_eq!(s, ls(Idle));
        assert!(fx.is_empty());
    }

    #[test]
    fn ended_from_recording_returns_idle_quietly() {
        let (s, fx) = handle(&ls(rec("n1")), &Msg::SessionEnded { note_id: "n1".into() });
        assert_eq!(s, ls(Idle));
        assert!(fx.is_empty());
    }

    /// 回报与内核态矛盾:以回报为准 + 记对账差异。
    #[test]
    fn out_of_order_reports_reconcile_with_mismatch_logged() {
        let (s, fx) = handle(&ls(Idle), &Msg::SessionStarted { note_id: "n1".into() });
        assert_eq!(s, ls(rec("n1")), "回报来自真实世界,必须采纳");
        assert!(matches!(fx.as_slice(), [Effect::ShadowMismatch(_)]));

        let (s, fx) = handle(&ls(rec("n1")), &Msg::SessionEnded { note_id: "n2".into() });
        assert_eq!(s, ls(Idle));
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
            let state = ls(state.clone());
            let state = &state;
            // === SessionStarted 消息 ===
            let (next_state, effects) = handle(state, &started_msg);
            // 终态:始终进入 Recording{"n1", paused:false}
            assert_eq!(
                next_state,
                ls(rec("n1")),
                "{state_name} 收 SessionStarted 应转入 Recording"
            );
            // 对账:只有 Starting 是顺流(无噪音),其余都有一个 ShadowMismatch
            let is_compatible = matches!(state.session, Starting { .. });
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
                next_state,
                ls(Idle),
                "{state_name} 收 SessionFailed 应返回 Idle"
            );
            // 对账:只有 Starting 是顺流,其余都有一个 ShadowMismatch
            let is_compatible = matches!(state.session, Starting { .. });
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
                next_state,
                ls(Idle),
                "{} 收 SessionEnded(n1) 应返回 Idle",
                state_name
            );
            // 对账:只有 Recording{"n1"} 和 Stopping{"n1"} 是顺流,其余都有一个 ShadowMismatch
            let is_compatible = matches!(
                &state.session,
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
        let recording_paused = ls(Recording { note_id: "n1".into(), paused: true });
        let (next_state, effects) = handle(&recording_paused, &started_msg);
        // 验证 paused 被重置为 false
        assert_eq!(
            next_state,
            ls(rec("n1")),
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
        let s = ls(SessionState::Idle);
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
        let states = [Idle, Starting { resume_id: None }, rec("n1"), Stopping { note_id: "n1".into() }]
            .map(ls);

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
            let (next, fx) = handle(s, &Msg::Pipeline { note_id: "n1".into(), op });
            assert_eq!(&next, s, "Pipeline 不应改变状态:state={s:?}");
            assert!(
                matches!(fx.as_slice(), [Effect::DoPipeline { note_id }] if note_id == "n1"),
                "state={s:?} fx={fx:?}"
            );

            let (next, fx) = handle(s, &Msg::AbortSession { note_id: "n1".into() });
            assert_eq!(&next, s, "AbortSession 不应改变状态:state={s:?}");
            assert!(
                matches!(fx.as_slice(), [Effect::DoAbort { note_id }] if note_id == "n1"),
                "state={s:?} fx={fx:?}"
            );

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
            let state = ls(state.clone());
            let (next_state, effects) = handle(&state, &finalize_msg);
            assert_eq!(next_state, ls(Idle), "{name} 收 Finalize(n1) 应归 Idle");
            // 任何状态下都恰有一个 DoFinalize,note_id 原样携带
            let do_finalize_count = effects
                .iter()
                .filter(|e| matches!(e, Effect::DoFinalize { note_id } if note_id == "n1"))
                .count();
            assert_eq!(do_finalize_count, 1, "{name} + Finalize(n1) 必须恰产出一个 DoFinalize(n1):{effects:?}");
            let is_compatible = matches!(
                &state.session,
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

    // ======== P3:精修维度 ========

    /// RefineRequest 裁决表:4 会话态 × 3 精修态全组合(请求 id 恒为 n1)。
    /// 守卫序:录制中(该 id 的 Recording/Stopping)最先拒;其次同 id 精修中拒;
    /// 其余放行——置 Running{n1} + 恰一个 DoSpawnRefine(手动路径 enqueue=false)。
    /// 拒绝路径状态两维都不许动;放行路径会话维不许动。文案逐字锁死。
    #[test]
    fn refine_request_decision_matrix() {
        let sessions = [
            ("Idle", Idle),
            ("Starting", Starting { resume_id: None }),
            ("Recording n1", rec("n1")),
            ("Stopping n1", Stopping { note_id: "n1".into() }),
        ];
        let refines = [
            ("refine=Idle", RefineState::Idle),
            ("refine=Running n1", RefineState::Running { note_id: "n1".into() }),
            // 单槽语义:别的笔记在精修不挡本笔记(旧集合按 id 查),放行并顶替。
            ("refine=Running n2", RefineState::Running { note_id: "n2".into() }),
        ];
        let msg = Msg::RefineRequest { note_id: "n1".into() };
        for (sn, sess) in &sessions {
            for (rn, rf) in &refines {
                let st = LifecycleState { session: sess.clone(), refine: rf.clone() };
                let (next, fx) = handle(&st, &msg);
                let recording_same = matches!(
                    sess,
                    Recording { note_id, .. } | Stopping { note_id } if note_id == "n1"
                );
                let refining_same =
                    matches!(rf, RefineState::Running { note_id } if note_id == "n1");
                if recording_same {
                    assert_eq!(next, st, "{sn}/{rn}: 录制中拒绝不得改状态");
                    assert_eq!(
                        fx,
                        vec![Effect::ReplyErr("该笔记正在录制，停止后才能精修".into())],
                        "{sn}/{rn}: 录制守卫优先,文案逐字"
                    );
                } else if refining_same {
                    assert_eq!(next, st, "{sn}/{rn}: 精修中拒绝不得改状态");
                    assert_eq!(
                        fx,
                        vec![Effect::ReplyErr("该笔记正在精修中".into())],
                        "{sn}/{rn}: 精修守卫文案逐字"
                    );
                } else {
                    assert_eq!(next.session, *sess, "{sn}/{rn}: 放行不动会话维");
                    assert_eq!(
                        next.refine,
                        RefineState::Running { note_id: "n1".into() },
                        "{sn}/{rn}: 放行即置 Running"
                    );
                    assert_eq!(
                        fx,
                        vec![Effect::DoSpawnRefine { note_id: "n1".into(), enqueue_transcode: false }],
                        "{sn}/{rn}: 恰一个 DoSpawnRefine,手动路径不再入队转码"
                    );
                }
            }
        }
    }

    /// 续录被精修阻塞的守卫(原 lib.rs resume_blocked_by_refining 上移内核):
    /// 只挡「命中 id 的续录」,不误伤其它笔记/其它命令;文案逐字。
    /// (原 lib.rs resume_blocked_by_refining_matches_refining_set 的语义由本测试接管。)
    #[test]
    fn resume_rejected_only_while_same_note_refining() {
        let refining_n1 = LifecycleState {
            session: Idle,
            refine: RefineState::Running { note_id: "n1".into() },
        };
        // 命中:拒绝、不 Delegate、状态两维都不动
        let (next, fx) = handle(&refining_n1, &Msg::Cmd(Cmd::Start { resume_id: Some("n1".into()) }));
        assert_eq!(next, refining_n1, "拒绝不得改状态(不预演 Starting)");
        assert_eq!(fx, vec![Effect::ReplyErr("该笔记正在精修,请稍后再试".into())], "文案逐字");

        // 不命中的续录/新开录/其余命令:照常 Delegate(精修不影响)
        let others = [
            Cmd::Start { resume_id: Some("n2".into()) },
            Cmd::Start { resume_id: None },
            Cmd::Stop,
            Cmd::Pause,
            Cmd::Unpause,
        ];
        for c in &others {
            let (_, fx) = handle(&refining_n1, &Msg::Cmd(c.clone()));
            assert_eq!(fx, vec![Effect::Delegate(c.clone())], "cmd={c:?} 不该被精修守卫拦截");
        }

        // 精修空闲:同 id 续录照常 Delegate
        let (_, fx) = handle(&ls(Idle), &Msg::Cmd(Cmd::Start { resume_id: Some("n1".into()) }));
        assert_eq!(fx, vec![Effect::Delegate(Cmd::Start { resume_id: Some("n1".into()) })]);
    }

    /// RefineProgress:任何会话态下恒转发恰一个 DoEmitRefine(载荷原样);
    /// 仅 "all/running" 置 Running(精修开始信号),其余进度不动精修态。
    #[test]
    fn refine_progress_forwards_and_only_all_running_sets_state() {
        let sessions =
            [Idle, Starting { resume_id: None }, rec("n1"), Stopping { note_id: "n1".into() }];
        for sess in &sessions {
            // 开始信号:置 Running,会话维不动
            let st = LifecycleState { session: sess.clone(), refine: RefineState::Idle };
            let (next, fx) = handle(
                &st,
                &Msg::RefineProgress { note_id: "n1".into(), stage: "all".into(), state: "running".into() },
            );
            assert_eq!(next.session, *sess, "session={sess:?}: 会话维不动");
            assert_eq!(next.refine, RefineState::Running { note_id: "n1".into() });
            assert_eq!(
                fx,
                vec![Effect::DoEmitRefine { note_id: "n1".into(), stage: "all".into(), state: "running".into() }],
                "载荷原样转发"
            );

            // 中途进度与收尾事件:只转发,不动状态(置回 Idle 是 RefineFinished 的事,
            // 对齐旧 refining.remove 在收尾 emit 之后的时机)
            let running = LifecycleState {
                session: sess.clone(),
                refine: RefineState::Running { note_id: "n1".into() },
            };
            for (stage, st_val) in [("filter", "done"), ("llm", "running"), ("all", "done"), ("all", "failed")] {
                let (next, fx) = handle(
                    &running,
                    &Msg::RefineProgress { note_id: "n1".into(), stage: stage.into(), state: st_val.into() },
                );
                assert_eq!(next, running, "session={sess:?} {stage}/{st_val}: 状态不动");
                assert_eq!(
                    fx,
                    vec![Effect::DoEmitRefine { note_id: "n1".into(), stage: stage.into(), state: st_val.into() }],
                    "session={sess:?} {stage}/{st_val}"
                );
            }
        }
    }

    /// RefineFinished 按 id 对账:命中置回 Idle 零噪音;不命中(单槽被顶替后的迟到
    /// 收尾)/已 Idle 不动状态、恰一个 ShadowMismatch——绝不误清别的笔记的 Running。
    #[test]
    fn refine_finished_resets_idle_only_on_matching_id() {
        let running_n1 = LifecycleState {
            session: Idle,
            refine: RefineState::Running { note_id: "n1".into() },
        };
        let (next, fx) = handle(&running_n1, &Msg::RefineFinished { note_id: "n1".into() });
        assert_eq!(next, ls(Idle), "命中:精修态归 Idle");
        assert!(fx.is_empty(), "顺流收尾零噪音:{fx:?}");

        let (next, fx) = handle(&running_n1, &Msg::RefineFinished { note_id: "n2".into() });
        assert_eq!(next, running_n1, "不命中:不动 n1 的 Running");
        assert!(matches!(fx.as_slice(), [Effect::ShadowMismatch(_)]), "{fx:?}");

        let (next, fx) = handle(&ls(Idle), &Msg::RefineFinished { note_id: "n1".into() });
        assert_eq!(next, ls(Idle), "已 Idle:状态不动");
        assert!(matches!(fx.as_slice(), [Effect::ShadowMismatch(_)]), "{fx:?}");
    }

    /// 两维正交(反方向):会话回报/收尾消息穿过内核时,精修维必须原样保留——
    /// 停录 Finalize 绝不能把并行精修的 Running 冲掉(旧世界二者本就无关)。
    #[test]
    fn session_reports_preserve_refine_dimension() {
        let base = LifecycleState {
            session: Starting { resume_id: None },
            refine: RefineState::Running { note_id: "nX".into() },
        };
        let (next, _) = handle(&base, &Msg::SessionStarted { note_id: "n1".into() });
        assert_eq!(next.refine, base.refine, "SessionStarted 不动精修维");
        let (next, _) = handle(&base, &Msg::SessionFailed);
        assert_eq!(next.refine, base.refine, "SessionFailed 不动精修维");
        let rec_state = LifecycleState { session: rec("n1"), refine: base.refine.clone() };
        let (next, _) = handle(&rec_state, &Msg::Finalize { note_id: "n1".into() });
        assert_eq!(next.refine, base.refine, "Finalize 不动精修维");
        let (next, _) = handle(&rec_state, &Msg::SessionEnded { note_id: "n1".into() });
        assert_eq!(next.refine, base.refine, "SessionEnded 不动精修维");
    }
}
