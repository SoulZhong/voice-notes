//! lifecycle actor:信箱 + 内核 + hook 总线 + 委托执行(P1 绞杀者) + writer 单写者(P2)。
//!
//! P1 语义:命令消息在 actor 线程上被内核处理,产生 Delegate 效果后由 actor
//! 线程**同步调用既有 do_* 执行体**,其返回值即 reply——执行体、守卫、事件
//! 时序均与现状逐位一致,唯一变化是「所有命令经同一线程串行执行」。
//!
//! P2 语义:NoteWriter 所有权入 actor(Owned 槽,线程局部不上锁——唯一触碰者
//! 就是本线程,这正是 actor 化要达成的单写者形态)。管线回调(append/说话人
//! 事件)、停录 finalize、录制中改题/改名全部改发消息,由本线程串行执行,
//! `Arc<Mutex<NoteWriter>>` 与全部旁路锁删除。停录时序用自投消息解决排干
//! 问题:teardown 返回后向自己信箱投 Finalize——它排在 handle.stop() 排干
//! 期间入队的全部管线消息之后(同队列 FIFO + 跨线程 happens-before 传递
//! 入队序),「先全部落盘、再 finalize、再 emit stopped」由队列结构保证。
//!
//! 死锁注记(调用图上的边,无环):
//! ① do_stop teardown 后向自己信箱投 Finalize——unbounded send 不阻塞,安全;
//! ② spawn_session 的回报(含 AdoptWriter/Pipeline/AbortSession)来自后台
//!   加载线程与 ASR worker 线程,只投递不等待——安全;
//! ③ command()/request() 的调用方阻塞等 reply,actor 永不阻塞等调用方——无环。
//!   ③ 的前提有二:Delegate 执行体内不得有「派发到主线程并同步等结果」的调用
//!  (托盘/菜单 API 已改 fire-and-forget,见 tray.rs 注释);request() 调用方
//!   不得持有 running/generation/session 任一全局锁(执行体可能要取,持锁等
//!   reply 会成环——rename_speaker/uds set_title 均已 statement-scoped 取值)。
//! ④ 效果执行器内的 emit 是事件投递不等待;spawn_refine/preload_models 均
//!   spawn 后台线程不等待——不新增环。

use crossbeam_channel::{unbounded, Sender};
use tauri::{AppHandle, Emitter};

use super::hooks::{HookBus, TransitionCtx};
use super::machine::{self, Cmd, Effect, Msg, PipelineOp, SessionState};

pub enum Envelope {
    Cmd { cmd: Cmd, reply: Sender<Result<(), String>> },
    Report(Msg),
    /// 带回执的非命令消息(P2):SetTitle/RenameActiveSpeaker/自投 Finalize 等
    /// 需要同步结果的投递;处理完本条消息的全部效果后按 sticky-error 结果回复。
    Request { msg: Msg, reply: Sender<Result<(), String>> },
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

    /// 带回执的消息投递(P2):同 command() 的 bounded(1) 模式,供录制中改题/
    /// 改名等需要同步结果的调用方。死锁纪律见模块头注记③。
    pub fn request(&self, msg: Msg) -> Result<(), String> {
        let (rtx, rrx) = crossbeam_channel::bounded(1);
        self.tx
            .send(Envelope::Request { msg, reply: rtx })
            .map_err(|_| "lifecycle actor 已退出".to_string())?;
        rrx.recv().map_err(|_| "lifecycle actor 未回复".to_string())?
    }
}

/// runner 独占的会话落盘上下文:writer 所有权 + 存储降级标志。
/// note_id 冗余存一份作槽键(改题/改名/收尾按 id 对账,防串会话)。
struct Owned {
    note_id: String,
    writer: crate::store::writer::NoteWriter,
    /// on_final 落盘失败/恢复的一次性告警翻转位(原 lib.rs on_final 闭包局部变量)。
    degraded: bool,
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
            // P2:停录在信封层特化(teardown+自投 Finalize,见 spawn 主循环),
            // Delegate(Stop) 运行期不可达;防御性只记日志,不做半套拆除。
            eprintln!("lifecycle: Delegate(Stop) 不应到达(停录已在信封层特化)");
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

/// 会话未正常存续时的笔记收尾(原 lib.rs::abort_or_finalize 逐语句搬移,锁改所有权):
/// 有内容则 finalize 保全;无内容且是本会话新建的才删空目录;
/// 续录打开的既有笔记(即使零段)绝不删——留 recording 态(诚实显示「已中断」)。
fn abort_owned(mut o: Owned) {
    if o.writer.has_content() {
        if let Err(e) = o.writer.finalize(chrono::Local::now()) {
            eprintln!("abort_or_finalize: finalize 失败: {e}");
        }
    } else if o.writer.created_this_session() {
        let dir = o.writer.dir().to_path_buf();
        drop(o); // writer Drop 释放笔记目录 flock,删除才不被自己挡住
        let _ = std::fs::remove_dir_all(dir);
    }
    // 既有笔记零段:什么都不做,meta 留 recording,内容零损失。
}

/// 管线执行器:lib.rs on_final/on_diar 的 writer 触发块逐字搬入(P2),仅
/// `writer_x.lock().unwrap()` 换 `owned.writer`、闭包局部 `degraded` 换
/// `owned.degraded`、`app_f/app_d` 换 `app`。base_ms 已在回调侧加进消息
/// (Final 的 start/end 与 EchoRetract 的时间戳),此处一律不再加偏移。
fn run_pipeline(app: &AppHandle, owned: &mut Owned, op: PipelineOp) {
    match op {
        PipelineOp::Final { source, text, start_ms, end_ms, speaker, rms } => {
            // 不丢内容优先：先落盘（失败进待写队列），再通知 UI。
            match owned
                .writer
                .append_final(&source, &text, start_ms, end_ms, speaker.as_deref(), rms)
            {
                Ok(()) => {
                    if owned.degraded {
                        owned.degraded = false;
                        let _ = app.emit("storage", crate::ipc::StorageEvent { state: "ok".into() });
                    }
                }
                Err(e) => {
                    eprintln!("append_final 失败（段暂存内存待重试）: {e}");
                    if !owned.degraded {
                        owned.degraded = true;
                        let _ = app.emit("storage", crate::ipc::StorageEvent { state: "degraded".into() });
                    }
                }
            }
            let _ = app.emit(
                "final",
                crate::ipc::FinalEvent { source, text, start_ms, end_ms, speaker },
            );
        }
        PipelineOp::Diar(ev) => match ev {
            crate::session::DiarEvent::SpeakersChanged(infos) => {
                // sources 为空 ⇔ 未命中的库种子簇（assign 命中必 sources.insert）：
                // 这类簇只是种子注入时铺的库人物候选，本场从未真正出现过，不该
                // 泄漏进说话人表/chips/落盘（否则每场笔记都会囤上全库人物）。
                let infos: Vec<_> = infos.into_iter().filter(|s| !s.sources.is_empty()).collect();
                let pairs: Vec<(String, Vec<String>)> = infos
                    .iter()
                    .map(|s| (s.id.clone(), s.sources.iter().cloned().collect()))
                    .collect();
                let w = &mut owned.writer;
                if let Err(e) = w.sync_speakers(&pairs) {
                    eprintln!("speakers.json 写入失败: {e}");
                }
                // 种子命中显名：registry 里已关联库人物（seed 命中或续录带入）的簇，
                // 把 person_id 同步进本场 speakers 表；本地名为空时用库名兜底（本场
                // 手动改过名的一律保留，不被库名打回原形）。
                for s in &infos {
                    let Some(person) = &s.person else { continue };
                    w.set_speaker_person(&s.id, person);
                    let local_name_empty =
                        w.speakers().get(&s.id).map(|m| m.name.is_empty()).unwrap_or(true);
                    if local_name_empty {
                        if let Some(name) = s.name.as_deref().filter(|n| !n.is_empty()) {
                            w.set_speaker_name(&s.id, name);
                        }
                    }
                }
                let speakers = w
                    .speakers()
                    .iter()
                    .map(|(id, m)| crate::ipc::SpeakerEntry {
                        id: id.clone(),
                        name: m.name.clone(),
                        sources: m.sources.clone(),
                        person_id: m.person_id.clone(),
                    })
                    .collect();
                let _ = app.emit("speakers", crate::ipc::SpeakersEvent { speakers, merged: None });
            }
            crate::session::DiarEvent::Merged { loser, winner } => {
                let w = &mut owned.writer;
                // 落盘失败也照发 merged：内存/前端先统一（历史段徽章回写），
                // 磁盘落后由 storage degraded 告警，finalize 兜底再补。
                if let Err(e) = w.merge_speaker(&loser, &winner) {
                    eprintln!("说话人合并重写失败({loser}->{winner}): {e}");
                    let _ = app.emit("storage", crate::ipc::StorageEvent { state: "degraded".into() });
                }
                let speakers = w
                    .speakers()
                    .iter()
                    .map(|(id, m)| crate::ipc::SpeakerEntry {
                        id: id.clone(),
                        name: m.name.clone(),
                        sources: m.sources.clone(),
                        person_id: m.person_id.clone(),
                    })
                    .collect();
                let _ = app.emit(
                    "speakers",
                    crate::ipc::SpeakersEvent {
                        speakers,
                        merged: Some(crate::ipc::MergedPair { loser, winner }),
                    },
                );
            }
            crate::session::DiarEvent::EchoRetract { start_ms, end_ms, text } => {
                // 已放行的 mic 回声段被 system 定稿追认:磁盘删行 + 通知前端撤回显示。
                // 时间戳已在回调侧加续录偏移(与 on_final 同口径),此处不再加。落盘
                // 失败仍撤 UI(显示优先干净),磁盘差异走 storage 降级告警。
                let w = &mut owned.writer;
                if let Err(e) = w.retract_segment("mic", start_ms, end_ms, &text) {
                    eprintln!("回声撤回落盘失败({start_ms}-{end_ms}): {e}");
                    let _ = app.emit("storage", crate::ipc::StorageEvent { state: "degraded".into() });
                }
                let _ = app.emit(
                    "final_retract",
                    crate::ipc::RetractEvent { source: "mic".into(), start_ms, end_ms, text },
                );
            }
            crate::session::DiarEvent::Snapshot { snaps, samples: _ } => {
                // 声纹库回写/样本落盘不触 writer,已拆分留在回调线程原地执行
                // (见 lib.rs on_diar 闭包注释);新建的簇→人物关联已在回调侧注进
                // snaps[].person,store_centroids 落表时一并写 person_id——与原
                // 「store_centroids + set_speaker_person 循环」终态逐位等价
                // (store_centroids 对已有表项仅在 person=Some 时覆写 person_id,
                // 新建表项直接取 snap.person)。samples 在回调侧消费完,不随消息复运。
                owned.writer.store_centroids(&snaps);
            }
        },
    }
}

pub fn spawn(app: AppHandle) -> LifecycleHandle {
    let (tx, rx) = unbounded::<Envelope>();
    // actor 自持一份发送端用于自投 Finalize:这使 rx 循环不会因外部句柄全部
    // 掉落而退出——本 handle 常驻 app state 与进程同寿,行为与现状一致。
    let handle = LifecycleHandle { tx: tx.clone() };
    std::thread::Builder::new()
        .name("lifecycle-actor".into())
        .spawn(move || {
            let mut state = SessionState::Idle;
            // P2:writer 所有权槽。AdoptWriter 装入,Abort/Finalize 取出;线程局部
            // 无锁——唯一触碰者是本线程(单写者)。
            let mut owned: Option<Owned> = None;
            let bus = HookBus::default(); // P1 无注册消费者;P3 起接遥测/UI 等
            for env in rx {
                let (msg, reply) = match env {
                    // 停录特化(P2):teardown 同步执行(handle.stop 排干期间,管线
                    // 消息全部入队),随后把 stop 的 reply 转移进自投的 Finalize——
                    // 它排在那些管线消息之后(同队列 FIFO+跨线程 happens-before),
                    // 「先全部落盘、再 finalize、再 emit stopped」由队列结构保证,
                    // 停录命令的同步语义(返回=收尾完成)也随 reply 转移而保持。
                    // catch_unwind 与 run_delegate 同理:teardown panic 不许杀 actor。
                    // 极端窗口注记:teardown 排干期间(handle.stop 内部)若有 Resume
                    // 同一笔记的 Start 命令抢先入队并在此刻被处理,会因 w1(本次会话)
                    // 的 NoteWriter flock 尚未随 Owned 槽清空/drop 而释放,一次性误报
                    // 「笔记正被占用」。可达性极低(需精确落在 teardown 未完成、Finalize
                    // 未自投的窄窗内)且自愈(下次 Resume 重试即通过,不留脏状态)——
                    // 与本次 note_id 对账加固同源(P2 单信箱串行化带来的新窗口),
                    // 留痕说明,不做额外处理。
                    Envelope::Cmd { cmd: Cmd::Stop, reply } => {
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            crate::do_stop_teardown(&app)
                        }));
                        match r {
                            Ok(Some(note_id)) => {
                                // 自投进自己信箱:unbounded send 不阻塞(死锁注记①)。
                                let _ = tx.send(Envelope::Request { msg: Msg::Finalize { note_id }, reply });
                            }
                            Ok(None) => {
                                // 空停(无会话):与旧实现一致,仍发 stopped/复位托盘/补预载。
                                crate::do_stop_tail(&app, String::new());
                                let _ = reply.send(Ok(()));
                            }
                            Err(_) => {
                                eprintln!("lifecycle: 停录 teardown panic(已捕获,actor 存活)");
                                let _ = reply.send(Err("内部错误:停止录制失败".into()));
                            }
                        }
                        continue;
                    }
                    Envelope::Cmd { cmd, reply } => (Msg::Cmd(cmd), Some(reply)),
                    Envelope::Report(m) => (m, None),
                    Envelope::Request { msg, reply } => (msg, Some(reply)),
                };
                let (next, effects) = machine::handle(&state, &msg);
                let is_cmd = matches!(msg, Msg::Cmd(_));
                // 效果不带 writer/管线载荷(见 machine.rs Effect 注释),载荷从本轮
                // 原始消息一次性取走——内核对每条这类消息恰发一个对应效果。
                let (mut adopt_payload, mut pipeline_payload) = match msg {
                    Msg::AdoptWriter { writer } => (Some(writer), None),
                    Msg::Pipeline { op, .. } => (None, Some(op)),
                    _ => (None, None),
                };
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
                        Effect::DoAdopt => {
                            if let Some(w) = adopt_payload.take() {
                                let note_id = w.note_id().to_string();
                                if let Some(old) = owned.take() {
                                    // 不应发生(采纳恒在旧会话清槽之后):真到达也不能
                                    // 裸 drop 旧 writer——那会留下锁死的 recording 态
                                    // 孤儿笔记;按 abort_or_finalize 语义清理后再装新。
                                    eprintln!(
                                        "lifecycle 对账: AdoptWriter({note_id}) 抵达时槽已被 {} 占用,旧 writer 按 abort 语义清理",
                                        old.note_id
                                    );
                                    abort_owned(old);
                                }
                                owned = Some(Owned { note_id, writer: *w, degraded: false });
                            } else {
                                eprintln!("lifecycle: DoAdopt 无对应 AdoptWriter 载荷(不应发生)");
                            }
                        }
                        Effect::DoPipeline { note_id } => {
                            match (pipeline_payload.take(), owned.as_mut()) {
                                (Some(op), Some(o)) if &o.note_id == note_id => {
                                    run_pipeline(&app, o, op)
                                }
                                // 对账不过:双加载线程重叠窗口下(start→S1 卡住数秒→
                                // stop→start),S1 迟到的管线消息与槽内 S2 的会话不是
                                // 同一笔记——旧世界里各会话独占 Arc,这类消息只会写进
                                // S1 自己那个孤儿 writer 而被静默丢弃,从未真正影响
                                // S2;新世界单写者槽对齐同一后果:核对不上就丢弃,
                                // 绝不误写进新会话的 writer。
                                (Some(_op), Some(o)) => {
                                    eprintln!(
                                        "lifecycle: 迟到管线消息丢弃(会话已更替): {note_id}(槽内={})",
                                        o.note_id
                                    );
                                }
                                // 会话已放弃/收尾后的迟到管线事件:旧世界写进注定被
                                // abort 的 writer,新世界无处可写,丢弃并留痕(只记种类,
                                // 不整条 Debug——Snapshot 载荷含整组质心向量,拒绝刷屏)。
                                (Some(op), None) => {
                                    let kind = match &op {
                                        PipelineOp::Final { .. } => "Final",
                                        PipelineOp::Diar(_) => "Diar",
                                    };
                                    eprintln!("lifecycle: 管线事件({kind})抵达但槽内无 writer,丢弃");
                                }
                                (None, _) => eprintln!("lifecycle: DoPipeline 无载荷(不应发生)"),
                            }
                        }
                        Effect::DoAbort { note_id } => {
                            // 原 lib.rs abort_or_finalize 语义作用于槽内 writer + 清槽,
                            // 但先对账:note_id 与槽内 owned.note_id 不一致(同样是双
                            // 加载线程重叠窗口下 S1 迟到的 AbortSession)绝不能动槽——
                            // 那会误杀 S2 刚装入的新会话 writer,整场丢失。旧世界里
                            // S1 的 abort 只作用于自己独占的 Arc,天然不会波及 S2;
                            // 新世界靠这次对账补回等价保证。
                            match &owned {
                                Some(o) if &o.note_id == note_id => abort_owned(owned.take().unwrap()),
                                Some(o) => eprintln!(
                                    "lifecycle: 迟到放弃消息跳过(会话已更替): {note_id}(槽内={},不动新会话 writer)",
                                    o.note_id
                                ),
                                None => eprintln!("lifecycle: AbortSession 抵达但槽内无 writer(可能已被清理)"),
                            }
                        }
                        Effect::DoFinalize { note_id } => {
                            // 原 do_stop_recording 后半段逐语句搬移(writer 锁改槽所有权)。
                            // finalize 失败不置 result:旧世界 stop 无返回值,失败只
                            // eprintln+degraded 告警,停录命令仍然「成功」。
                            match owned.take() {
                                Some(mut o) => {
                                    if o.note_id != *note_id {
                                        eprintln!(
                                            "lifecycle 对账: Finalize({note_id}) 与槽内笔记({})不一致,仍收尾槽内 writer",
                                            o.note_id
                                        );
                                    }
                                    let finalized = o.writer.finalize(chrono::Local::now());
                                    match finalized {
                                        Ok(()) => {
                                            // 仅 finalize 成功（state=complete、meta 落盘）才发起精修。
                                            // 转码移交时机与失败兜底见 spawn_refine 文档注释。
                                            // 精修目标用 o.note_id(槽内笔记,真正被上面 finalize 的那条)
                                            // 而非消息携带的 note_id:错配分支(上方 eprintln)已表明二者
                                            // 可能不同——finalize 的 IO 只作用于槽内 writer,精修必须
                                            // 跟随真正落盘的那条笔记,否则会给一条根本没被收尾的笔记
                                            // 触发精修(内容还在 owned 槽或已被后续会话占用)。
                                            crate::spawn_refine(app.clone(), o.note_id.clone(), true);
                                        }
                                        Err(e) => {
                                            eprintln!("stop_recording: finalize 失败: {e}");
                                            let _ = app.emit("storage", crate::ipc::StorageEvent { state: "degraded".into() });
                                        }
                                    }
                                    drop(o); // writer Drop 释放笔记目录 flock,此后转码/续录可拿锁
                                    crate::do_stop_tail(&app, note_id.clone());
                                }
                                None => {
                                    eprintln!("lifecycle 对账: Finalize({note_id}) 抵达但槽内无 writer(不应发生)");
                                    crate::do_stop_tail(&app, note_id.clone());
                                }
                            }
                        }
                        Effect::DoSetTitle { note_id, title } => {
                            // 原 uds.rs set_title 块搬移:录制中改题唯一安全路径=writer
                            // 单写者(rename_note 拒绝活动笔记,直写盘会被 finalize 覆盖)。
                            let r = match owned.as_mut() {
                                Some(o) if o.note_id == *note_id => o
                                    .writer
                                    .set_title(title)
                                    .map_err(|e| format!("设标题失败: {e}")),
                                _ => Err("录制已结束或笔记不匹配,标题未设置".into()),
                            };
                            if result.is_ok() { result = r; }
                        }
                        Effect::DoRenameActiveSpeaker { note_id, speaker_id, name } => {
                            // 原 lib.rs rename_speaker 活动分支逐语句搬移(writer 锁改槽
                            // 所有权):单写者路径改内存表+persist_speakers 原子落盘+广播,
                            // 不与管线事件竞争(同线程串行,天然无覆盖窗口)。
                            let r = (|| {
                                let o = match owned.as_mut() {
                                    Some(o) if o.note_id == *note_id => o,
                                    // 判定(命令线程读 session 槽)与执行(此处)之间恰逢
                                    // 停录的竞态窗口:报错让调用方重试,重试会走非活动
                                    // 的 NoteStore 直写路径(此刻已合法)。
                                    _ => return Err("录制会话已结束,请重试".to_string()),
                                };
                                o.writer.set_speaker_name(speaker_id, name);
                                let persisted = o.writer.persist_speakers();
                                let speakers = o
                                    .writer
                                    .speakers()
                                    .iter()
                                    .map(|(id, m)| crate::ipc::SpeakerEntry {
                                        id: id.clone(),
                                        name: m.name.clone(),
                                        sources: m.sources.clone(),
                                        person_id: m.person_id.clone(),
                                    })
                                    .collect();
                                persisted.map_err(|e| format!("说话人改名落盘失败: {e}"))?;
                                let _ = app.emit("speakers", crate::ipc::SpeakersEvent { speakers, merged: None });
                                Ok(())
                            })();
                            if result.is_ok() { result = r; }
                        }
                    }
                }
                // 委托失败 → 回退预演迁移:状态不动、不通知 hook。
                // 否则守卫拒绝的 Start 会留下幻影 Starting + 幻影迁移通知,
                // P3 挂上消费者后 hook 将收到从未真实发生的迁移。
                let commit = if is_cmd && result.is_err() { state.clone() } else { next };
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
