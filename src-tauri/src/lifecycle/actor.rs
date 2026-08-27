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
use tauri::{AppHandle, Emitter, Manager};

use super::hooks::{HookBus, TransitionCtx};
use super::machine::{
    self, Cmd, Effect, EditOp, LifecycleState, Msg, PipelineOp, RefineState, SessionState,
};

pub enum Envelope {
    Cmd { cmd: Cmd, reply: Sender<Result<(), String>> },
    Report(Msg),
    /// 带回执的非命令消息(P2):SetTitle/RenameActiveSpeaker/自投 Finalize 等
    /// 需要同步结果的投递;处理完本条消息的全部效果后按 sticky-error 结果回复。
    Request { msg: Msg, reply: Sender<Result<(), String>> },
    /// Aing 态查询(P3):不经 machine::handle(查询不该在迁移表里制造伪迁移,
    /// 与 recording_status 直读同理),actor 直答自身内核态。供 rename/assign_
    /// refined_* 的「Aing 中拒绝」守卫读取(原 AppState.refining.contains)。
    QueryRefine { note_id: String, reply: Sender<bool> },
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

    /// 该笔记是否正在 Aing(P3,内核 Aing 态快照)。取代旧 AppState.refining 集合的
    /// contains 查询;经信箱串行化,读到的是与命令处理一致的快照。actor 已退出按
    /// 「未在 Aing」处理(仅进程退出路径)。死锁纪律同注记③:调用方不得持全局锁。
    pub fn is_refining(&self, note_id: &str) -> bool {
        let (rtx, rrx) = crossbeam_channel::bounded(1);
        if self
            .tx
            .send(Envelope::QueryRefine { note_id: note_id.to_string(), reply: rtx })
            .is_err()
        {
            return false;
        }
        rrx.recv().unwrap_or(false)
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
/// refine 参数(P3):内核 Aing 集的只读引用。续录执行体需要「该笔记是否正在 Aing」
/// 来做 F1 守卫,且必须在其自身的「迁移/下载中」检查之后原位判定(旧守卫顺序
/// 逐位还原,谁先判谁先报)——故不由内核抢答,而是把查询结果随 Delegate 传入。
/// 同一消息处理内读取,快照与内核裁决一致。
///
/// catch_unwind:do_* 若 panic(现实来源仅锁中毒),actor 线程绝不能死——
/// 否则控制面(按钮/托盘/快捷键/MCP)全部静默失联,比旧世界的显性崩溃更糟。
/// 捕获后转 Err 回给调用方并响亮记日志。
fn run_delegate(app: &AppHandle, cmd: &Cmd, refine: &RefineState) -> Result<(), String> {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match cmd {
        Cmd::Start { resume_id: None } => crate::do_start_recording(app),
        Cmd::Start { resume_id: Some(id) } => {
            crate::do_resume_note_recording(app, id.clone(), refine.is_running(id))
        }
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

/// 执行 DoEdit:非活动编辑七操作与 store/notes.rs 七个 NoteStore 方法一一对应。
/// 活动笔记拒绝等前置守卫已在命令壳原位判定完毕(见 lib.rs 各命令壳),这里只
/// 负责 NoteStore 调用本身——经本线程串行执行,命令壳生产路径不再需要额外
/// 互斥。store/notes.rs 的 EDIT_LOCK 本身未删:有并发测试直接绕过命令壳/actor
/// 多线程裸调 NoteStore(见 concurrent_speaker_edits_do_not_lose_updates),
/// 删锁会让它丢更新失败,故留原防线,详见 P3 Task 3 报告。
///
/// set_segment_speaker 返回新分配的说话人 id(旧命令壳直接透传给前端),此处
/// 丢弃、统一收敛成 Result<(),String>,与其余六个编辑操作同形状——actor 的
/// 请求面不为它单开回执类型。命令壳在 request 成功后重查 note 拿回该值(actor
/// 已把写入落盘完成才回执 Ok,重查读到的必是刚写入的最终值,不构成竞态)。
fn run_edit(app: &AppHandle, op: EditOp, refining_ids: &[String]) -> Result<(), String> {
    let dir = crate::notes_dir(app).map_err(|e| e.to_string())?;
    let store = crate::store::NoteStore::new(dir);
    match op {
        EditOp::Rename { id, title } => store.rename(&id, &title).map_err(|e| e.to_string()),
        EditOp::Delete { id } => store.delete(&id).map_err(|e| e.to_string()),
        EditOp::RenameSpeaker { id, speaker_id, name } => {
            store.rename_speaker(&id, &speaker_id, &name).map_err(|e| e.to_string())
        }
        EditOp::DeleteSpeaker { id, speaker_id } => {
            store.delete_speaker(&id, &speaker_id).map_err(|e| e.to_string())
        }
        EditOp::AssignPerson { id, speaker_id, person_id } => store
            .assign_speaker_person(&id, &speaker_id, &person_id)
            .map_err(|e| e.to_string()),
        EditOp::ClearPerson { id, speaker_id } => {
            store.clear_speaker_person(&id, &speaker_id).map_err(|e| e.to_string())
        }
        EditOp::SetMultiSpeaker { id, speaker_id } => {
            store.set_multi_speaker(&id, &speaker_id).map_err(|e| e.to_string())
        }
        EditOp::ReserveSpeakers { id, speaker_ids, op_id } => {
            store.reserve_speakers(&id, &speaker_ids, &op_id).map_err(|e| e.to_string())
        }
        EditOp::ReleaseReservedSpeakers { id, op_id } => store
            .release_reserved_speakers(&id, &op_id)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        EditOp::AssignPersonIf { id, speaker_id, person_id } => store
            .assign_speaker_person_if(&id, &speaker_id, &person_id)
            .map_err(|e| e.to_string()),
        EditOp::SplitReassign { id, moves, op_id } => {
            store.batch_set_segment_speaker(&id, &moves, &op_id).map_err(|e| e.to_string())
        }
        EditOp::EditText { id, seq, expected_text, new_text } => store
            .edit_segment_text(&id, seq, &expected_text, &new_text)
            .map_err(|e| e.to_string()),
        EditOp::DeleteSegment { id, seq, expected_text } => {
            store.delete_segment(&id, seq, &expected_text).map_err(|e| e.to_string())
        }
        EditOp::SetSegmentSpeaker { id, seq, expected_text, speaker_id } => store
            .set_segment_speaker(&id, seq, &expected_text, &speaker_id)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        EditOp::DeleteSegments { id, moves } => {
            store.delete_segments(&id, &moves).map_err(|e| e.to_string())
        }
        EditOp::RestoreSuppressed { id, seqs } => {
            store.restore_suppressed(&id, &seqs).map(|_| ()).map_err(|e| e.to_string())
        }
        EditOp::FoldSceneEcho { id } => {
            // 在 actor 串行流里判 Aing(codex:命令线程 check-then-act 有窗口;
            // 这里与 RefineProgress 的插入同信箱,判定即真值)。
            if refining_ids.iter().any(|r| r == &id) {
                return Err("该笔记正在 Aing,请稍后再试".to_string());
            }
            // 重转写同样拒(codex 三轮):提交会整表换 seq 并删抑制表——先折叠
            // 后被整替,成功回执成了谎话;worker 已持锁时这里也只会忙碌失败。
            {
                let st = app.state::<crate::AppState>();
                if crate::retranscribing_blocks_refine(&st.retranscribing, &id) {
                    return Err("该笔记正在重转写,请稍后再试".to_string());
                }
            }
            store.fold_dual_path_echo(&id).map(|_| ()).map_err(|e| e.to_string())
        }
        EditOp::SetSegmentsSpeaker { id, moves, speaker_id } => store
            .set_segments_speaker(&id, &moves, &speaker_id)
            .map(|_| ())
            .map_err(|e| e.to_string()),
    }
}

/// 会话未正常存续时的笔记收尾(原 lib.rs::abort_or_finalize 逐语句搬移,锁改所有权):
/// 有内容则 finalize 保全;无内容且是本会话新建的才删空目录;
/// 续录打开的既有笔记(即使零段)绝不删——留 recording 态(诚实显示「已中断」)。
fn abort_owned(mut o: Owned) {
    if o.writer.has_content() {
        if let Err(e) = o.writer.finalize(chrono::Local::now()) {
            eprintln!("abort_or_finalize: finalize 失败: {e}");
            // 落盘失败 = 用户的录音可能没保住,这是最该看见的一类失败。
            crate::telemetry::report_error(
                crate::telemetry::ErrorKind::NoteWrite,
                &format!("abort_or_finalize: finalize 失败: {e}"),
            );
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
            // append 前读 next_seq 即本段将被分配的 seq：actor 单线程串行，
            // 读取与 append 之间无并发写入，成功/降级两条路径下都成立。
            let seq = owned.writer.next_seq();
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
                    // 声纹缓存预热(issue #164):段落定稿即入队后台嵌入,与离线
                    // embed_all 同口径写 embeddings.json。纯旁路:队列满即丢,
                    // 失败只日志,绝不反压 actor/录制路径。
                    if let Ok(root) = crate::notes_dir(app) {
                        crate::pipeline::embed_prewarm::enqueue(
                            app,
                            crate::pipeline::embed_prewarm::Job {
                                note_dir: root.join(owned.writer.note_id()),
                                seq,
                                source: source.clone(),
                                start_ms,
                                end_ms,
                            },
                        );
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
                crate::ipc::FinalEvent { seq, source, text, start_ms, end_ms, speaker },
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
            crate::session::DiarEvent::SceneHint { scene } => {
                // 场景判定稳定切换(2026-08-23 一期):录制页非阻断提示,不动行为。
                let _ = app.emit(
                    "scene_hint",
                    serde_json::json!({ "note_id": owned.writer.note_id(), "scene": scene }),
                );
            }
            crate::session::DiarEvent::MatchLog(trace) => {
                // 匹配决策日志落盘(纯观测,同 scene.json 纪律)。
                let path = owned.writer.dir().join("match_log.json");
                match serde_json::to_vec_pretty(&trace) {
                    Ok(bytes) => {
                        if let Err(e) = std::fs::write(&path, bytes) {
                            eprintln!("match_log.json 写入失败(忽略): {e}");
                        }
                    }
                    Err(e) => eprintln!("match_log 序列化失败(忽略): {e}"),
                }
            }
            crate::session::DiarEvent::SceneReport(doc) => {
                // 整场场景时间线落盘(scene.json 为独立新文件,仅本线程写,无写者竞争)。
                if let Err(e) = crate::scene::save(owned.writer.dir(), &doc) {
                    eprintln!("scene.json 写入失败(忽略,纯观测数据): {e}");
                }
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
            crate::session::DiarEvent::SuppressedFinal {
                source,
                text,
                start_ms,
                end_ms,
                rms,
                reason,
            } => {
                let w = &mut owned.writer;
                let source = source.as_str();
                if let Err(e) = w
                    .append_final(source, &text, start_ms, end_ms, None, rms)
                    .and_then(|_| w.suppress_segment(source, start_ms, end_ms, &text, &reason))
                {
                    eprintln!("抑制段落盘失败({reason}, {start_ms}-{end_ms}): {e}");
                    let _ = app.emit("storage", crate::ipc::StorageEvent { state: "degraded".into() });
                }
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
            // P3:状态升维(会话+Aing 两维,见 machine::LifecycleState)。
            let mut state = LifecycleState::init();
            // P2:writer 所有权槽。AdoptWriter 装入,Abort/Finalize 取出;线程局部
            // 无锁——唯一触碰者是本线程(单写者)。
            let mut owned: Option<Owned> = None;
            let mut bus = HookBus::default();
            // P3 首批消费者:托盘图标由迁移驱动(取代 lib.rs 里 tray::set_recording
            // 的散点直调)。启动前注册完再进循环,符合 HookBus「注册序执行」契约。
            bus.register(Box::new(super::consumers::TrayHook));
            // Aing 集各条目「最后一次有进度」的时刻(相对 actor 启动的毫秒)。
            // 用于滞留自愈,见 REFINE_STALE_MS。单线程持有,无锁。
            let boot = std::time::Instant::now();
            let mut refine_clock: std::collections::BTreeMap<String, u64> = Default::default();
            loop {
                // 定时唤醒(codex 十三轮 P1):信箱空闲时也要做滞留体检——worker 吊死
                // 且此后再无 lifecycle 流量(无人开笔记页轮询/无头 MCP 场景)时,原
                // for-recv 会永远阻塞,一小时阈值形同虚设。60s 一跳:超时也走一遍
                // 体检再回来等信;通道关闭即退出(与 for-recv 的结束语义一致)。
                let env = match rx.recv_timeout(std::time::Duration::from_secs(60)) {
                    Ok(env) => Some(env),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                };
                // 每收一封(或每次定时醒来)先做一次滞留体检:worker 若永久阻塞,RAII 的 Drop 不会执行
                // (线程没结束),该 id 会永久占着 Aing 集把守卫钉死——这里兜住那条路径。
                //
                // 判据是「多久没有进度」而非「启动至今多久」。改口径的原因(codex review
                // 发现,2026-08-18):按启动计时会误杀正常的长会议 Aing——HTTP 精修按
                // CHUNK_CHARS=3000 分块串行跑,块数随会议长度无界增长,每块最坏
                // CHUNK_TIMEOUT_S×2=360s,十块就到一小时,而并发任务还要在 AING_GATE
                // 前排队。误杀的后果比不自愈更糟:移除标记并不会取消 worker,守卫就此
                // 放行,随后的编辑与仍在跑的 worker 抢写 aing.json。
                {
                    let now_ms = boot.elapsed().as_millis() as u64;
                    let running: Vec<String> =
                        state.refine.running_ids().map(str::to_string).collect();
                    // 心跳表并入停摆判据(codex 十五轮):有些阶段只碰 beat 不发
                    // RefineProgress(identify/标题/云端二遍),refine_clock 看不见它们
                    // 的活动。判死前问一嘴心跳,新鲜就当有进度回填时钟,免得定时
                    // 体检把还在干活的 worker 误杀(移除标记不等于取消线程,守卫一
                    // 放行编辑就会与它抢写 aing.json)。
                    let stale_ids: Vec<String> =
                        sync_and_take_stale(&mut refine_clock, &running, now_ms, REFINE_STALE_MS)
                            .into_iter()
                            .filter(|id| {
                                if let Some((stage, age_ms)) = crate::refine_beat_of(id) {
                                    if (age_ms as u128) < REFINE_STALE_MS as u128 {
                                        eprintln!(
                                            "lifecycle: {id} 事件流静默但心跳新鲜({stage}, {age_ms}ms 前),不判停摆"
                                        );
                                        // 按心跳真实年龄回填(codex 十七轮):填 now_ms 会把
                                        // 一个已经 59 分钟没跳的心跳当刚跳过,下次能判死
                                        // 要再等整整一个 TTL,自愈延迟翻倍。
                                        refine_clock
                                            .insert(id.clone(), now_ms.saturating_sub(age_ms));
                                        return false;
                                    }
                                }
                                true
                            })
                            .collect();
                    for id in stale_ids {
                        eprintln!(
                            "lifecycle: Aing 集条目 {id} 已 {}s 无进度,判定 worker 未收尾,自愈移除\
                             (该 id 的 is_refining 守卫此前会一直拒绝编辑类命令)",
                            REFINE_STALE_MS / 1000
                        );
                        // 自愈触发本身就是异常信号:兜住了不等于没发生。
                        // 它一响就说明有个 worker 卡死了,而这正是在别人机器上
                        // 永远看不到的那类事。
                        crate::telemetry::report_error(
                            crate::telemetry::ErrorKind::RefineStaleHeal,
                            "Aing 条目无进度超时,已自愈移除",
                        );
                        // 停摆监工升级(issue #173):自愈不只摘标记——
                        // ① 自采线程栈落 stderr:2026-08-26 的两小时误诊全因停摆现场
                        //   无尸检材料;下次直接带报告(macOS sample 自身 pid,best-effort)。
                        // ② 盘上补失败态:aing.json 缺失时写 llm=failed 的最小稿,UI 从
                        //   「这场没做 AI 整理」的幻觉变成「失败可重跑」;worker 若诈尸
                        //   跑完会整写覆盖,不冲突。均在独立线程做,不阻塞 actor 信箱。
                        {
                            let app2 = app.clone();
                            let id2 = id.clone();
                            std::thread::spawn(move || {
                                #[cfg(target_os = "macos")]
                                {
                                    let pid = std::process::id().to_string();
                                    match std::process::Command::new("sample")
                                        .args([pid.as_str(), "2"])
                                        .output()
                                    {
                                        Ok(o) => eprintln!(
                                            "lifecycle: 停摆尸检({id2})线程栈采样 {} 字节:
{}",
                                            o.stdout.len(),
                                            String::from_utf8_lossy(&o.stdout)
                                        ),
                                        Err(e) => eprintln!("lifecycle: 停摆尸检采样失败({id2}): {e}"),
                                    }
                                }
                                // 代次快查(codex 三轮 P1):sample 拖的两秒里新一轮可能
                                // 已接手(本信箱后续消息重新插回 Aing 集),先问一句再动手;
                                // 查后写前的窗口由 heal 内部的写盘戳比对兜底。
                                if app2
                                    .try_state::<crate::lifecycle::LifecycleHandle>()
                                    .map(|lc| lc.is_refining(&id2))
                                    .unwrap_or(false)
                                {
                                    eprintln!("lifecycle: 停摆自愈({id2})发现新一轮已接手,让路");
                                    return;
                                }
                                if let Ok(root) = crate::notes_dir(&app2) {
                                    let dir = root.join(&id2);
                                    // 查-判-写整体在一把 NoteLock 内(codex P1a/P1b):
                                    // 已有中间稿改标 failed;诈尸写完的稿原样保留。
                                    let still_stale = || {
                                        let lc_active = app2
                                            .try_state::<crate::lifecycle::LifecycleHandle>()
                                            .map(|lc| lc.is_refining(&id2))
                                            .unwrap_or(false);
                                        if lc_active {
                                            return false;
                                        }
                                        // 心跳新鲜同样算活(codex 三十轮):被摘的前任在
                                        // sample/锁重试窗口里诈尸继续跑时,只有心跳在动
                                        // (阶段级 report 不会把它重新插回 lifecycle 集),
                                        // 不能对着活人的中间稿写失败态。
                                        if let Some((_, age_ms)) = crate::refine_beat_of(&id2) {
                                            if age_ms < 10 * 60 * 1000 {
                                                return false;
                                            }
                                        }
                                        true
                                    };
                                    // 锁忙重试(codex 二十轮):吊死的 worker 可能正抱着
                                    // NoteLock,或普通编辑瞬时占锁。只试一次就放弃的话,
                                    // 停摆标记已摘、时钟已清,再没有下一次体检会回来补
                                    // 写失败态,盘上状态与 UI 就此永久失和。
                                    let mut healed =
                                        crate::store::heal_stale_refined(&dir, still_stale);
                                    for _ in 0..20 {
                                        match &healed {
                                            Ok(act) if act.contains("持锁") => {
                                                std::thread::sleep(
                                                    std::time::Duration::from_secs(15),
                                                );
                                                healed = crate::store::heal_stale_refined(
                                                    &dir,
                                                    still_stale,
                                                );
                                            }
                                            _ => break,
                                        }
                                    }
                                    if let Ok(act) = &healed {
                                        if act.contains("持锁") {
                                            eprintln!(
                                                "lifecycle: 停摆自愈({id2})五分钟内锁一直被占,放弃(盘上状态可能未收口)"
                                            );
                                        }
                                    }
                                    match healed {
                                        Ok(act) => {
                                            eprintln!("lifecycle: 停摆自愈({id2}):{act}");
                                            // 终态广播(codex P2b/九轮):笔记页在
                                            // note_refining 回 false 后已停轮询,凡是
                                            // 停摆标记被摘且盘上有稿的情形都补一发,
                                            // 页面按稿子实际状态收口。写了失败态报
                                            // failed;稿子本就终态(llm 收过尾、worker
                                            // 死在 identify/标题)报 done——稿子可用,
                                            // 但把心跳留证打进日志,不静默当成功。
                                            let state_s = if act.contains("failed")
                                                || act.contains("尾段停摆")
                                            {
                                                // 写了失败态,或 worker 从未有序退场
                                                // (收工戳缺失):都算停摆失败,附心跳留证
                                                if let Some((stage, age_ms)) =
                                                    crate::refine_beat_of(&id2)
                                                {
                                                    eprintln!(
                                                        "lifecycle: 停摆自愈({id2})心跳停在 {stage} 已 {age_ms}ms"
                                                    );
                                                }
                                                // 停摆定性落盘(codex 二十六轮):事件是
                                                // 一次性的,页面/应用重启后只剩盘上稿
                                                // (可能是 llm=done 的旧稿)——runs 日志
                                                // 补一条 stale 终局,refine_status 的
                                                // last_run 才不会指回更早的一轮。
                                                let _ = crate::append_refine_run_log(
                                                    &dir,
                                                    &id2,
                                                    &serde_json::json!({
                                                        "event": "finished",
                                                        "outcome": "stale_heal_failed",
                                                        "detail": act,
                                                        "at": chrono::Local::now().to_rfc3339(),
                                                    }),
                                                );
                                                Some("failed")
                                            } else if act.contains("已收工") {
                                                // 收工戳在 ≠ 成功(codex 三十六轮):诈尸
                                                // worker 可能以 failed/panic 收的工。事件
                                                // 状态按 runs 日志末条真实 outcome 选。
                                                // 事件要与盘上稿对上号(codex 三十七轮):
                                                // 末条可能是老 worker 迟到的 superseded
                                                // 退场记录,拿它给替补的成功稿定 failed
                                                // 就冤了。跳过 superseded;稿面有
                                                // writer_run 时优先找 run 匹配的记录。
                                                let doc_run = crate::store::load_refined(&dir)
                                                    .map(|d| d.writer_run)
                                                    .unwrap_or_default();
                                                let ok = std::fs::read_to_string(
                                                    dir.join("aing_runs.jsonl"),
                                                )
                                                .ok()
                                                .and_then(|raw| {
                                                    let evs: Vec<serde_json::Value> = raw
                                                        .lines()
                                                        .filter_map(|l| {
                                                            serde_json::from_str(l).ok()
                                                        })
                                                        .filter(|v: &serde_json::Value| {
                                                            v.get("event").is_some()
                                                                && v["superseded"]
                                                                    != serde_json::json!(true)
                                                        })
                                                        .collect();
                                                    evs.iter()
                                                        .rev()
                                                        .find(|v| {
                                                            !doc_run.is_empty()
                                                                && v["run"].as_str()
                                                                    == Some(doc_run.as_str())
                                                        })
                                                        .or_else(|| evs.last())
                                                        .cloned()
                                                })
                                                .map(|v| {
                                                    matches!(
                                                        v["outcome"].as_str(),
                                                        Some("done") | Some("retry_done")
                                                    )
                                                })
                                                // 无日志(旧世界收工稿):按成功处理
                                                .unwrap_or(true);
                                                Some(if ok { "done" } else { "failed" })
                                            } else {
                                                None // 让路/坏稿:没动盘,也不该发终态
                                            };
                                            if let Some(state_s) = state_s {
                                                // 发布前最后一验(codex 十七轮):替补可能在
                                                // heal 的 still_stale 之后、这一发之前入场,
                                                // 此时终态事件会把活跑的替补在页面上盖章
                                                // 收场。已有人在跑就撤销广播。
                                                let taken_over = app2
                                                    .try_state::<crate::lifecycle::LifecycleHandle>()
                                                    .map(|lc| lc.is_refining(&id2))
                                                    .unwrap_or(false);
                                                if taken_over {
                                                    eprintln!(
                                                        "lifecycle: 停摆自愈({id2})替补已接手,终态广播撤销"
                                                    );
                                                } else {
                                                    let _ = app2.emit(
                                                        "refine",
                                                        crate::ipc::RefineEvent {
                                                            note_id: id2.clone(),
                                                            stage: "all".into(),
                                                            state: state_s.into(),
                                                        },
                                                    );
                                                }
                                            }
                                        }
                                        Err(e) => eprintln!(
                                            "lifecycle: 停摆自愈({id2})落盘失败(仅日志): {e}"
                                        ),
                                    }
                                }
                            });
                        }
                        // RefineFinished 命中时只移除、零效果(见 machine.rs),可直接应用。
                        let (next, _fx) = machine::handle(&state, &Msg::RefineFinished { note_id: id.clone() });
                        state = next;
                        refine_clock.remove(&id);
                    }
                }
                let Some(env) = env else { continue }; // 定时醒来无信:体检完回去等
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
                    // 查询直答(不进迁移表):回执后处理下一封。
                    Envelope::QueryRefine { note_id, reply } => {
                        let _ = reply.send(state.refine.is_running(&note_id));
                        continue;
                    }
                };
                // 进度即心跳:worker 每报一次进度就把该条目的计时清零。没有这一步,
                // 判据退化回「启动至今」,长会议的正常 Aing 会被误杀(见上方说明)。
                // 放在 handle 之前:RefineProgress 的 all/running 分支会把 id 插进集合,
                // 先刷新则首次插入那一刻也有了准确起点。
                if let Msg::RefineProgress { note_id, .. } = &msg {
                    refine_clock.insert(note_id.clone(), boot.elapsed().as_millis() as u64);
                }
                let (next, effects) = machine::handle(&state, &msg);
                let is_cmd = matches!(msg, Msg::Cmd(_));
                // 效果不带 writer/管线载荷(见 machine.rs Effect 注释),载荷从本轮
                // 原始消息一次性取走——内核对每条这类消息恰发一个对应效果。
                let (mut adopt_payload, mut pipeline_payload, mut edit_payload) = match msg {
                    Msg::AdoptWriter { writer } => (Some(writer), None, None),
                    Msg::Pipeline { op, .. } => (None, Some(op), None),
                    Msg::EditNote { op } => (None, None, Some(op)),
                    _ => (None, None, None),
                };
                let mut result: Result<(), String> = Ok(());
                for fx in &effects {
                    match fx {
                        Effect::Delegate(cmd) => {
                            let r = run_delegate(&app, cmd, &state.refine);
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
                                // 同一笔记——旧世界里各会话独占 Arc,这类消息写进 S1
                                // 自己的孤儿 writer(其 abort_or_finalize 若有内容还会
                                // 保全成 S1 的笔记),从未影响 S2;新世界单槽下这些尾段
                                // 被丢弃——是「保 S2 不被污染」与「保 S1 尾段」二选一,
                                // 只影响已被用户放弃的 S1 极端窗口尾段,取前者。
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
                            // 会话被放弃 = 这次录制没能正常开起来/存下来。
                            // 用户视角就是「点了开录但没录成」,是最该远端可见的一类。
                            crate::telemetry::report_error(
                                crate::telemetry::ErrorKind::RecordingStart,
                                "会话被放弃(DoAbort)",
                            );
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
                                    // 漏斗 1 的"首次拿到转写"。必须在 finalize 之前取:
                                    // 之后 writer 要被搬空,拿不到了。
                                    let had_content = o.writer.has_content();
                                    let mut finalized_ok = false;
                                    let finalized = o.writer.finalize(chrono::Local::now());
                                    match finalized {
                                        Ok(()) => {
                                            // 录制中命名的兑现(codex 末轮 P1):live rename 只
                                            // 记名字不动库(录制中禁库写),停录 finalize 后把
                                            // 「有名无主」说话人补走命名入库通路。幂等后台任务,
                                            // 失败只日志。
                                            crate::spawn_enroll_named_speakers(
                                                &app,
                                                note_id.clone(),
                                            );
                                            // 一场录制到底有没有产出转写。空转写是这条链路最常见的
                                            // 失败形态(权限拿到了、录也录了,就是一个字都没出来),
                                            // 而它不报错、不崩溃,在别人机器上完全不可见。
                                            let asr_mode = app
                                                .path()
                                                .app_data_dir()
                                                .map(|d| crate::settings::load(&d).asr_mode)
                                                .unwrap_or_default();
                                            crate::telemetry::track(
                                                &app,
                                                crate::telemetry::Event::TranscriptReady {
                                                    engine: crate::telemetry::AsrEngine::classify(&asr_mode),
                                                    empty: !had_content,
                                                },
                                            );
                                            // 仅 finalize 成功（state=complete、meta 落盘）才发起 Aing。
                                            // 转码移交时机与失败兜底见 spawn_refine 文档注释。
                                            // 自动 Aing 是保障类直调,不经 RefineRequest 守卫(与旧
                                            // 世界停录钩子直调一致);内核 Aing 态由 spawn_refine 入口
                                            // 同步自投的 RefineProgress("all","running") 置 Running,
                                            // 该消息排在本条 Finalize 之后、停录 reply 之前入队——
                                            // 停录返回后到达的续录命令必然排在它后面,守卫不漏挡。
                                            // Aing 目标用 o.note_id(槽内笔记,真正被上面 finalize 的那条)
                                            // 而非消息携带的 note_id:错配分支(上方 eprintln)已表明二者
                                            // 可能不同——finalize 的 IO 只作用于槽内 writer,Aing 必须
                                            // 跟随真正落盘的那条笔记,否则会给一条根本没被收尾的笔记
                                            // 触发 Aing(内容还在 owned 槽或已被后续会话占用)。
                                            // Aing/折叠移到 drop(o) 之后(codex P1:writer 仍握
                                            // .note.lock,folding 在此必被「笔记被占用」拒掉),
                                            // 这里只立旗。
                                            finalized_ok = true;
                                        }
                                        Err(e) => {
                                            eprintln!("stop_recording: finalize 失败: {e}");
                                            crate::telemetry::report_error(
                                                crate::telemetry::ErrorKind::RecordingStop,
                                                &format!("stop_recording: finalize 失败: {e}"),
                                            );
                                            let _ = app.emit("storage", crate::ipc::StorageEvent { state: "degraded".into() });
                                        }
                                    }
                                    let done_id = o.note_id.clone();
                                    drop(o); // writer Drop 释放笔记目录 flock,此后转码/续录可拿锁
                                    if finalized_ok {
                                        // 场景二期·同源双路自动折叠(issue #162):锁已释放,
                                        // 且必须在 spawn_refine 之前同步做——Aing 经 NoteStore
                                        // 读段,折叠先落,精修稿天然不含回声段。文件操作毫秒级;
                                        // 失败只日志(折叠是增值,不挡 Aing)。
                                        let fold_on = app
                                            .path()
                                            .app_data_dir()
                                            .map(|d| crate::settings::load(&d).scene_auto_fold)
                                            .unwrap_or(true);
                                        if fold_on {
                                            match crate::notes_dir(&app)
                                                .map(crate::store::NoteStore::new)
                                                .and_then(|st| st.fold_dual_path_echo(&done_id))
                                            {
                                                Ok(0) => {}
                                                Ok(n) => eprintln!(
                                                    "scene: 同源双路自动折叠 {n} 段回声({done_id}),笔记页可展开恢复"
                                                ),
                                                Err(e) => eprintln!("scene: 自动折叠失败(跳过): {e}"),
                                            }
                                        }
                                        // 仅 finalize 成功才发起 Aing(原位置注释的语义不变:
                                        // 自动 Aing 保障类直调,RefineProgress 同步自投仍排在
                                        // 停录 reply 之前——do_stop_tail 在本行之后)。
                                        crate::spawn_refine(app.clone(), done_id.clone(), true);
                                    }
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
                        Effect::DoSpawnRefine { note_id, enqueue_transcode } => {
                            // 手动 Aing 路径(refine_note → RefineRequest 裁决通过):守卫
                            // 已在内核抢答,这里只负责发起。spawn_refine 入口会同步
                            // report RefineProgress("all","running")(自投,unbounded 不
                            // 阻塞——死锁注记①),对本路径是幂等重插(内核已插入 Aing 集)。
                            crate::spawn_refine(app.clone(), note_id.clone(), *enqueue_transcode);
                        }
                        Effect::DoEmitRefine { note_id, stage, state } => {
                            // 原 spawn_refine worker 的 emit("refine",..) 改道至此:同一
                            // worker 串行 report + 信箱 FIFO,事件种类/载荷/顺序逐位不变。
                            let _ = app.emit(
                                "refine",
                                crate::ipc::RefineEvent {
                                    note_id: note_id.clone(),
                                    stage: stage.clone(),
                                    state: state.clone(),
                                },
                            );
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
                        Effect::DoEditActiveSegment { note_id, seq, expected_text, new_text } => {
                            // 录制中改段文本:与 DoRenameActiveSpeaker 同骨架——先按
                            // note_id 与槽内 writer 对账(判定活动在命令线程、执行在此
                            // 线程,中间可能已停录/换会话),再走 writer 单写者路径重写
                            // segments.jsonl。与定稿追加同线程串行,天然无覆盖窗口。
                            let r = (|| {
                                let o = match owned.as_mut() {
                                    Some(o) if o.note_id == *note_id => o,
                                    // 竞态窗口(判定与执行之间恰逢停录):报错让调用方重试。
                                    // 重试在 writer 离槽(DoFinalize)后才走冷路径;停录排干
                                    // 窗口内冷路径会被笔记目录锁挡下,报"正被占用"——前端按
                                    // 文案分支处理,勿盲目自动重试。
                                    _ => return Err("录制会话已结束,请重试".to_string()),
                                };
                                o.writer
                                    .edit_segment_text(*seq, expected_text, new_text)
                                    .map_err(|e| format!("段落编辑失败: {e}"))?;
                                // 落盘成功才发事件:前端不做乐观更新,事件是唯一真值源。
                                let _ = app.emit(
                                    "segment_edited",
                                    crate::ipc::SegmentEditedEvent {
                                        note_id: note_id.clone(),
                                        seq: *seq,
                                        text: Some(new_text.clone()),
                                        speaker: None,
                                    },
                                );
                                Ok(())
                            })();
                            if result.is_ok() { result = r; }
                        }
                        Effect::DoSetActiveSegmentSpeaker {
                            note_id,
                            seq,
                            expected_text,
                            speaker_id,
                        } => {
                            // 同上骨架;writer 侧另有「目标须在本场说话人表内」的校验
                            // (录制中不开放 "new" 分配,命令壳已先拒)。
                            let r = (|| {
                                let o = match owned.as_mut() {
                                    Some(o) if o.note_id == *note_id => o,
                                    _ => return Err("录制会话已结束,请重试".to_string()),
                                };
                                o.writer
                                    .set_segment_speaker_live(*seq, expected_text, speaker_id)
                                    .map_err(|e| format!("段落改派说话人失败: {e}"))?;
                                let _ = app.emit(
                                    "segment_edited",
                                    crate::ipc::SegmentEditedEvent {
                                        note_id: note_id.clone(),
                                        seq: *seq,
                                        text: None,
                                        speaker: Some(speaker_id.clone()),
                                    },
                                );
                                Ok(())
                            })();
                            if result.is_ok() { result = r; }
                        }
                        Effect::DoEdit => {
                            if let Some(op) = edit_payload.take() {
                                let refining_ids: Vec<String> =
                                    state.refine.running_ids().map(str::to_string).collect();
                                let r = run_edit(&app, op, &refining_ids);
                                if result.is_ok() { result = r; }
                            } else {
                                eprintln!("lifecycle: DoEdit 无对应 EditNote 载荷(不应发生)");
                            }
                        }
                    }
                }
                // 委托失败 → 回退预演迁移:状态不动、不通知 hook。
                // 否则守卫拒绝的 Start 会留下幻影 Starting + 幻影迁移通知,
                // P3 挂上消费者后 hook 将收到从未真实发生的迁移。
                let commit = if is_cmd && result.is_err() { state.clone() } else { next };
                // hook 只关心会话主时间轴:TransitionCtx from/to 维持 SessionState,
                // Aing 维变化(置 Running/置回 Idle)不通知——托盘等消费者与 Aing 无关。
                if commit.session != state.session {
                    let note_id = match &commit.session {
                        SessionState::Recording { note_id, .. }
                        | SessionState::Stopping { note_id } => Some(note_id.as_str()),
                        _ => None,
                    };
                    bus.notify(&TransitionCtx {
                        note_id,
                        from: &state.session,
                        to: &commit.session,
                        app: Some(&app),
                    });
                }
                // 外部钩子(用户配置 shell/webhook):与 HookBus 不同,要看完整内核
                // 状态(session+refine 两维)——Aing 开始/完成也是白名单事件。
                // 映射是纯内存比较,无事件零开销;执行在 dispatch 内起线程,不占 actor。
                crate::hooks_external::dispatch(&app, &state, &commit);
                // 托盘图标动画:录制中抖动、停止即静止(Aing 在后台不驱动图标)。与 HookBus
                // (只驱动菜单文案)分工——图标由本调用按会话状态边沿驱动。
                crate::tray::update(&app, &state, &commit);
                state = commit;
                if let Some(r) = reply {
                    let _ = r.send(result);
                }
            }
        })
        .expect("lifecycle actor 线程创建失败");
    handle
}

/// Aing 集条目的滞留自愈:内核 Aing 集是「worker 还在跑」的**推断**,而推断的唯一
/// 撤销点原先是 worker 线程末尾那一条 RefineFinished。worker 一旦永久阻塞(卡在
/// AING_GATE 或某个 IO 上,线程不结束 ⇒ RAII 的 Drop 也不会执行),该 id 就永远留在
/// 集合里,`is_refining` 恒 true,delete_note_speaker 这类以它为第一道守卫的命令
/// **永久失败,只能重启应用**——2026-08-17 真实发生过一次。
///
/// 这里给每个在跑条目记一个起始时刻,超过 TTL 即判定 worker 未收尾并移除。
/// 时刻用「相对 actor 启动的毫秒数」而非 Instant:Instant 无法在测试里构造任意时刻。
///
/// 取 1 小时的理由:单场 Aing 的真实上界远低于此(HTTP 分块 CHUNK_TIMEOUT_S=180s
/// 且失败只重试一次,Agent 路径 REFINE_TIMEOUT_S=900s),并发多篇时 AING_GATE 排队
/// 也有界。宁可自愈得晚,也不能误杀正在跑的 Aing——误杀会让用户能再发起一次,
/// 两遍 Aing 互相覆盖 aing.json。
pub(crate) const REFINE_STALE_MS: u64 = 60 * 60 * 1000;

/// 同步「起始时刻」表并挑出滞留条目(纯函数,便于单测)。
/// - `running` 中的新 id 记为 `now_ms` 起算;
/// - 已不在 `running` 的条目清出表(worker 正常收尾);
/// - 返回已达 `ttl_ms` 的 id(调用方据此发 RefineFinished 自愈)。
pub(crate) fn sync_and_take_stale(
    clock: &mut std::collections::BTreeMap<String, u64>,
    running: &[String],
    now_ms: u64,
    ttl_ms: u64,
) -> Vec<String> {
    for id in running {
        clock.entry(id.clone()).or_insert(now_ms);
    }
    clock.retain(|id, _| running.iter().any(|r| r == id));
    clock
        .iter()
        .filter(|(_, &t0)| now_ms.saturating_sub(t0) >= ttl_ms)
        .map(|(id, _)| id.clone())
        .collect()
}

#[cfg(test)]
mod stale_tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn 新条目记起始时刻且未到期不判滞留() {
        let mut c = BTreeMap::new();
        let stale = sync_and_take_stale(&mut c, &["n1".into()], 1_000, 10_000);
        assert!(stale.is_empty());
        assert_eq!(c.get("n1"), Some(&1_000), "起始时刻按首次观察到的那一刻记");
        // 纯体检(无进度)不刷新计时,否则永远到不了期。刷新只由进度消息触发,
        // 那条路径在 actor 主循环里(见下面的「进度刷新计时」用例)。
        let stale = sync_and_take_stale(&mut c, &["n1".into()], 5_000, 10_000);
        assert!(stale.is_empty());
        assert_eq!(c.get("n1"), Some(&1_000), "体检本身不得刷新计时");
    }

    /// codex review(2026-08-18)发现的 P1 回归:判据必须是「多久没有进度」,
    /// 不能是「启动至今多久」。HTTP 精修按 3000 字分块串行跑,块数随会议长度无界,
    /// 每块最坏 360s,十块即到一小时——按启动计时会把正常的长会议 Aing 误判成卡死,
    /// 而移除标记并不取消 worker,守卫就此放行,随后的编辑与仍在跑的 worker 抢写。
    ///
    /// 这里模拟主循环的行为:每收到一次进度就 insert 当前时刻(即心跳),
    /// 验证只要还有进度,再久也不判滞留。
    #[test]
    fn 进度刷新计时使长任务不被误判() {
        let mut c = BTreeMap::new();
        let ttl = 10_000;
        sync_and_take_stale(&mut c, &["n1".into()], 0, ttl);
        // 每 9 秒一次心跳,连续 10 轮(累计 90 秒,远超 TTL),期间一次都不该被判滞留
        for round in 1..=10u64 {
            let now = round * 9_000;
            c.insert("n1".to_string(), now); // 主循环收到 RefineProgress 时做的事
            let stale = sync_and_take_stale(&mut c, &["n1".into()], now, ttl);
            assert!(stale.is_empty(), "第 {round} 轮:有心跳就不该判滞留");
        }
        // 心跳一停,超过 TTL 才判滞留
        let last = 90_000;
        assert!(sync_and_take_stale(&mut c, &["n1".into()], last + ttl - 1, ttl).is_empty());
        assert_eq!(
            sync_and_take_stale(&mut c, &["n1".into()], last + ttl, ttl),
            vec!["n1".to_string()],
            "心跳停止满 TTL 后才自愈"
        );
    }

    #[test]
    fn 到期即判滞留() {
        let mut c = BTreeMap::new();
        sync_and_take_stale(&mut c, &["n1".into()], 0, 10_000);
        assert_eq!(sync_and_take_stale(&mut c, &["n1".into()], 9_999, 10_000), Vec::<String>::new());
        assert_eq!(sync_and_take_stale(&mut c, &["n1".into()], 10_000, 10_000), vec!["n1".to_string()]);
    }

    #[test]
    fn 正常收尾的条目清出表不再判滞留() {
        let mut c = BTreeMap::new();
        sync_and_take_stale(&mut c, &["n1".into()], 0, 10_000);
        // worker 收尾 → 内核集合已无 n1
        let stale = sync_and_take_stale(&mut c, &[], 999_999, 10_000);
        assert!(stale.is_empty(), "已收尾的不该再被判滞留");
        assert!(c.is_empty(), "表要跟着内核集合收缩,否则无界增长");
    }

    #[test]
    fn 并发多篇各自计时互不波及() {
        let mut c = BTreeMap::new();
        sync_and_take_stale(&mut c, &["a".into()], 0, 10_000);
        sync_and_take_stale(&mut c, &["a".into(), "b".into()], 8_000, 10_000);
        let stale = sync_and_take_stale(&mut c, &["a".into(), "b".into()], 10_000, 10_000);
        assert_eq!(stale, vec!["a".to_string()], "只有 a 到期;b 起算晚,不受牵连");
    }

    /// 计时语义(上面四条)与 RefineFinished 的移除语义(machine.rs)各自都有测试,
    /// 但「卡死的条目经自愈后守卫真的放行」这条串起来的结论此前没人守——而 2026-08-17
    /// 出事的恰恰是它:worker 永久阻塞 ⇒ 永不发 RefineFinished ⇒ is_running 恒真 ⇒
    /// 以它为第一道守卫的命令永久失败。任何一侧单独正确都不足以保证这条成立
    /// (例如日后有人改 RefineFinished 的移除条件,两边的单测仍会全绿)。
    ///
    /// 真端到端(actor 主循环)要构造 AppHandle,仓库未接 tauri::test harness;这里
    /// 退一层,把主循环里那段自愈逻辑对着**真内核**跑一遍,覆盖到守卫这一步为止。
    /// 主循环自身的接线(每收一封先体检)不在覆盖内,见 #127 的真机项。
    #[test]
    fn 卡死条目经自愈后守卫放行且不波及并发笔记() {
        let ttl = 10_000;
        let mut clock = BTreeMap::new();

        // A 手动入集(其 worker 随后永久阻塞,永不回报收尾),B 稍后并发 Aing。
        let (st, _) = machine::handle(&LifecycleState::init(), &Msg::RefineRequest { note_id: "A".into() });
        let (st, _) = machine::handle(&st, &Msg::RefineRequest { note_id: "B".into() });
        let running = |s: &LifecycleState| s.refine.running_ids().map(str::to_string).collect::<Vec<_>>();
        sync_and_take_stale(&mut clock, &running(&st), 0, ttl);

        // 自愈前:A 的守卫生效,这正是用户看到的「点了没反应」。
        let (_, fx) = machine::handle(&st, &Msg::RefineRequest { note_id: "A".into() });
        assert_eq!(fx, vec![Effect::ReplyErr("该笔记正在 Aing 中".into())], "自愈前应照拒");

        // B 正常收尾(worker 活着),A 到期判滞留。
        let (st, _) = machine::handle(&st, &Msg::RefineFinished { note_id: "B".into() });
        let stale = sync_and_take_stale(&mut clock, &running(&st), ttl, ttl);
        assert_eq!(stale, vec!["A".to_string()], "只有卡死的 A 判滞留");

        // 自愈:主循环对每个滞留 id 发 RefineFinished。
        let (healed, fx) = machine::handle(&st, &Msg::RefineFinished { note_id: "A".into() });
        assert!(fx.is_empty(), "自愈命中集合内的 id,应零效果而非 ShadowMismatch 噪音:{fx:?}");

        // 守卫放行——本测试的落点:能重新发起 Aing,而不是只观察到集合变空。
        let (_, fx) = machine::handle(&healed, &Msg::RefineRequest { note_id: "A".into() });
        assert!(
            matches!(fx.as_slice(), [Effect::DoSpawnRefine { .. }]),
            "自愈后守卫必须放行,否则等于没修:{fx:?}"
        );

        // 自愈后表随内核收缩,不会每轮重复自愈同一个 id。
        clock.remove("A");
        assert!(sync_and_take_stale(&mut clock, &running(&healed), ttl * 9, ttl).is_empty());
        assert!(clock.is_empty(), "A 已移除、B 已收尾,表应为空");
    }
}
