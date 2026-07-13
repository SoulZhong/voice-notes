//! lifecycle actor:信箱 + 影子内核 + hook 总线 + 委托执行(P1 绞杀者)。
//!
//! P1 语义:命令消息在 actor 线程上被内核处理,产生 Delegate 效果后由 actor
//! 线程**同步调用既有 do_* 执行体**,其返回值即 reply——执行体、守卫、事件
//! 时序均与现状逐位一致,唯一变化是「所有命令经同一线程串行执行」(与今天
//! running/generation 锁串行等价或更强,可观测行为不变)。
//! 工作线程回报(SessionStarted/Failed/Ended)驱动影子内核演进;对账差异
//! 仅 eprintln,P2 翻转权威前它没有任何行为后果。
//!
//! 死锁注记(调用图上仅三条边,无环):
//! ① do_stop_recording 在 actor 线程上执行时经 report() 向自己信箱发
//!   SessionEnded——unbounded send 不阻塞,安全;
//! ② spawn_session 的回报来自后台加载线程,只投递不等待——安全;
//! ③ command() 的调用方阻塞等 reply,actor 永不阻塞等调用方——无环。
//!   ③ 的前提是 Delegate 执行体内不得有「派发到主线程并同步等结果」的调用
//!  (主线程可能正是阻塞等 reply 的命令调用方):托盘/菜单 API 正是这类调用,
//!   故 tray::set_recording 已改为 fire-and-forget 派发(见 tray.rs 注释)。

use crossbeam_channel::{unbounded, Sender};
use tauri::AppHandle;

use super::hooks::{HookBus, TransitionCtx};
use super::machine::{self, Cmd, Effect, Msg, SessionState};

pub enum Envelope {
    Cmd { cmd: Cmd, reply: Sender<Result<(), String>> },
    Report(Msg),
}

#[derive(Clone)]
pub struct LifecycleHandle {
    tx: Sender<Envelope>,
}

impl LifecycleHandle {
    /// 命令面:阻塞等待执行结果(与今天命令直接调 do_* 的同步语义一致)。
    pub fn command(&self, cmd: Cmd) -> Result<(), String> {
        let (rtx, rrx) = crossbeam_channel::bounded(1);
        self.tx
            .send(Envelope::Cmd { cmd, reply: rtx })
            .map_err(|_| "lifecycle actor 已退出".to_string())?;
        rrx.recv().map_err(|_| "lifecycle actor 未回复".to_string())?
    }

    /// 工作线程回报:只投递不等待。actor 落幕后的投递静默丢弃(进程退出路径)。
    pub fn report(&self, msg: Msg) {
        let _ = self.tx.send(Envelope::Report(msg));
    }
}

/// 执行 Delegate:P1 的旧世界执行体映射表。返回值即 reply。
///
/// catch_unwind:do_* 若 panic(现实来源仅锁中毒),actor 线程绝不能死——
/// 否则控制面(按钮/托盘/快捷键/MCP)全部静默失联,比旧世界的显性崩溃更糟。
/// 捕获后转 Err 回给调用方并响亮记日志。
fn run_delegate(app: &AppHandle, cmd: &Cmd) -> Result<(), String> {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match cmd {
        Cmd::Start { resume_id: None } => crate::do_start_recording(app),
        Cmd::Start { resume_id: Some(id) } => crate::do_resume_note_recording(app, id.clone()),
        Cmd::Stop => {
            crate::do_stop_recording(app);
            Ok(())
        }
        Cmd::Pause => crate::do_pause_recording(app),
        Cmd::Unpause => crate::do_resume_recording(app),
        // 状态查询在命令壳直接读旧路径(P1 不经内核回答,见计划 Task 6 Step 3)
        Cmd::QueryStatus => Ok(()),
    }));
    match r {
        Ok(inner) => inner,
        Err(_) => {
            eprintln!("lifecycle: 命令执行体 panic(已捕获,actor 存活): {cmd:?}");
            Err("内部错误:命令执行失败".into())
        }
    }
}

pub fn spawn(app: AppHandle) -> LifecycleHandle {
    let (tx, rx) = unbounded::<Envelope>();
    let handle = LifecycleHandle { tx };
    std::thread::Builder::new()
        .name("lifecycle-actor".into())
        .spawn(move || {
            let mut state = SessionState::Idle;
            let bus = HookBus::default(); // P1 无注册消费者;P3 起接遥测/UI 等
            for env in rx {
                let (msg, reply) = match env {
                    Envelope::Cmd { cmd, reply } => (Msg::Cmd(cmd), Some(reply)),
                    Envelope::Report(m) => (m, None),
                };
                let (next, effects) = machine::handle(&state, &msg);
                let mut result: Result<(), String> = Ok(());
                for fx in &effects {
                    match fx {
                        Effect::Delegate(cmd) => {
                            let r = run_delegate(&app, cmd);
                            // sticky-error: 首个失败即定局,后续效果不得漂白结果。
                            // Delegate 即使 result 已 Err 仍执行(保持现状语义:效果序列全部跑完,
                            // 只是 result 不被覆盖)。
                            if result.is_ok() { result = r; }
                        }
                        Effect::ReplyErr(e) => {
                            if result.is_ok() { result = Err(e.clone()); }
                        }
                        Effect::ShadowMismatch(d) => {
                            eprintln!("lifecycle 影子对账: {d}");
                        }
                        // P2 Task 2 只扩内核形状,writer 语义效果尚未接线(runner 不持 writer)。
                        // Task 3/4 逐个落地执行体后,把对应分支从这里迁出、删除本通配。
                        #[allow(unused_variables)]
                        Effect::DoAdopt
                        | Effect::DoPipeline
                        | Effect::DoFinalize { .. }
                        | Effect::DoAbort
                        | Effect::DoSetTitle { .. }
                        | Effect::DoRenameActiveSpeaker { .. } => {}
                    }
                }
                // 委托失败 → 回退预演迁移:状态不动、不通知 hook。
                // 否则守卫拒绝的 Start 会留下幻影 Starting + 幻影迁移通知,
                // P3 挂上消费者后 hook 将收到从未真实发生的迁移。
                let commit = if matches!(msg, Msg::Cmd(_)) && result.is_err() { state.clone() } else { next };
                if commit != state {
                    let note_id = match &commit {
                        SessionState::Recording { note_id, .. }
                        | SessionState::Stopping { note_id } => Some(note_id.as_str()),
                        _ => None,
                    };
                    bus.notify(&TransitionCtx { note_id, from: &state, to: &commit });
                    state = commit;
                }
                if let Some(r) = reply {
                    let _ = r.send(result);
                }
            }
        })
        .expect("lifecycle actor 线程创建失败");
    handle
}
