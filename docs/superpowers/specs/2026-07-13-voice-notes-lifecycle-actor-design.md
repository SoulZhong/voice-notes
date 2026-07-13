# voice-notes 生命周期 Actor 化重构设计

日期：2026-07-13
状态：已定稿（用户确认；方案对比后拍板 C：Actor 化）

## 背景与目标

笔记生命周期的状态今天散落四处（磁盘 meta.json、`AppState.session`、`NoteWriter` 内存、`refining` 集合），迁移靠隐式约定，副作用（转码入队/自动精修/标题/遥测/UI 事件）硬编码在各迁移点。目标：

1. **显式状态机**：全部状态迁移收敛到单一 lifecycle actor，非法迁移被机器拒绝而非靠约定。
2. **迁移 hook**：到达状态/发生迁移时触发可注册的 hook，新功能以新增 hook 的方式接入；为未来「用户可配置外部 hook（shell/webhook）」预留接口（本期不实现执行体）。
3. **行为完全等价**（硬约束）：重构前后可观测行为一致，验收标准见附录 A 不变式清单。
4. 附带：**跨进程文件锁**，根治 2026-07-13 「第二实例重写 segments.jsonl 导致 35 分钟转写丢失」一类事故。

方案对比结论：A typestate 在 IO 边界退化且改动半径最大；B 表驱动最稳但不解决进程内竞态；用户拍板 **C Actor 化**（接受 ~2.5x 工作量），进程内时序竞态类 bug 整体消失，B 的迁移表思想以「函数式内核」形式保留在 C 里。

## 架构：函数式内核 + 命令式外壳

### lifecycle actor（新模块 `src-tauri/src/lifecycle/`）

- **纯函数内核** `machine.rs`：`fn handle(state: &LifecycleState, msg: &Msg) -> (LifecycleState, Vec<Effect>)`。无 IO、无锁，状态机迁移表的唯一载体，单测按「状态×消息→效果序列」全覆盖。非法迁移返回 `Effect::ReplyErr`/记日志效果，绝不 panic。
- **效果执行器** `runner.rs`（actor 线程）：顺序执行 `Effect`：
  `WriteMeta` / `AppendSegment` / `RewriteSegments` / `PersistSpeakers` / `EmitUi(event)` / `SpawnSessionWorker` / `SpawnRefineWorker` / `EnqueueTranscode` / `RemoveNoteDir` / `Reply(ok|err)` / `FireHooks(transition)`。
  保障类效果失败沿用现有语义（如实降级：finalize 刷不出去→留 recording；删除失败→日志）。
- **hook 总线** `hooks.rs`：`trait LifecycleHook { fn on_transition(&self, ctx: &TransitionCtx); }`；`TransitionCtx { note_id, from, to, msg 摘要, app }`。
  - **保障类**副作用不走 hook，走 Effect（顺序与失败语义是契约的一部分）。
  - **通知类** hook（遥测、UI 附加事件、日志、未来的外部 hook）：注册序执行、逐个 `catch_unwind`，失败/panic 只记日志，绝不影响主流程。
  - 预留 `register_external(cfg: ExternalHookCfg)`（trait + 配置结构占位，本期不实现执行体）。
- **信箱**：`crossbeam_channel::unbounded::<Msg>()`（项目已有 crossbeam 依赖）。FIFO 保证「先 append 后 emit」等既有顺序。

### 状态（含显式过渡态）

```rust
enum SessionState {           // 全局唯一活动会话
    Idle,
    Starting { note_id, target },            // 模型加载/会话装配中(工作线程)
    Recording { note_id, paused: bool },
    Stopping { note_id },                    // handle.stop()+finalize 中(工作线程)
}
enum NoteDiskState { Recording, Complete }   // meta.json 权威值不变,仅两值
// 「已中断」= Recording 且非活动,由 StartupScan/SessionEnded(失败) 显式产生与消费
enum RefineState { Idle, Running { note_id, stage } }  // refining 集合被吸收
```

前端 `recording.pending` 从猜测变为 `Starting/Stopping` 的权威映射（IPC 状态事件增补字段，兼容旧值）。

### 消息协议（首版全集）

```rust
enum Msg {
  // —— 命令面(带 reply oneshot) ——
  Start { target: NoteTarget, reply },      // 新建或续录
  Stop { reply }, Pause { reply }, Unpause { reply },
  RefineRequest { note_id, reply },
  EditNote { note_id, op: EditOp, reply },  // 改名/说话人编辑/删段:统一入信箱,天然与录制互斥
  DeleteNote { note_id, reply },
  Query { kind: Status | ActiveId, reply },
  // —— 工作线程回报 ——
  SessionStarted { note_id, active_sources },
  SessionFailed { note_id, err },
  SessionEnded { note_id, outcome: HasContent | EmptyNew | EmptyResumed | FlushFailed },
  FinalSegment { note_id, src, text, start_ms, end_ms, speaker, rms },
  SpeakerMerge { note_id, loser, winner },
  SpeakerSync { note_id, infos },
  RefineProgress { note_id, stage, state },
  RefineFinished { note_id, outcome },
  TranscodeDone { note_id },
  // —— 系统 ——
  StartupScan,                               // 启动回溯:孤儿 recording → Interrupted 语义
}
```

### 同步命令桥接（行为等价关键）

每条命令 reply 的时机**逐条对照现状**写进实施计划并测试：

| 命令 | 今天返回时机 | Actor 化后 reply 时机 |
|---|---|---|
| start/resume | spawn_session 派发后（成功启动经事件通知） | `Starting` 受理即回，失败经既有 status 事件（不变） |
| stop | finalize 完成后 | `SessionEnded` 效果执行完 |
| pause/unpause | 立即 | 内核判定后立即 |
| refine_note | 立即（校验后 spawn） | 校验判定后立即 |
| status / list 的 active 派生 | 读锁即回 | `Query` 消息即回（毫秒级排队，一致性更强，已获用户知情认可） |
| get_note / list 内容读取 | 直读磁盘 | 不变，不经 actor（纯读无状态） |

重活永不占用 actor 线程：模型加载、会话装配、handle.stop()+finalize、LLM 调用、转码全部在工作线程，完成后回报消息。

### 音频热路径

final 段按语速产生（秒级频率），`FinalSegment` 走信箱无性能问题；音频采集/ASR 线程结构不动，仅把「拿 writer 锁」换成「发消息」。写失败 → actor 发 storage degraded 事件（语义不变，待写队列逻辑随 writer 整体移入 actor 独占）。

## 跨进程文件锁（P0，独立于 Actor）

- 会话启动时在笔记目录创建并 `flock` 独占 `.note.lock`，会话结束释放；锁文件不入 mirror/导出。
- 所有整表重写路径（编辑/合并/精修写回/启动回溯）先尝试非阻塞取锁：取不到 → 明确报错「该笔记正被另一实例录制」。
- 启动回溯扫描先探锁，活会话绝不误判为孤儿。
- 单元测试：双进程（或双句柄）交错场景。

## 分阶段落地（一条分支，阶段各自成 PR 或按序提交）

| 阶段 | 交付 | 等价性验证 |
|---|---|---|
| P0 | 跨进程文件锁 | 新增锁测试；既有测试全绿 |
| P1 | lifecycle 模块骨架：纯内核+效果执行器+hook 总线+信箱；start/stop/pause/unpause/status 五命令改道（效果层内部委托现有实现，绞杀者式） | 397 既有测试零修改全绿；内核迁移表全覆盖单测 |
| P2 | writer 所有权入 actor：FinalSegment/SpeakerMerge/SpeakerSync 走信箱；删 writer Mutex | 顺序性单测（append→emit）；录制/暂停/续录/中断恢复真机冒烟 |
| P3 | 精修/转码/标题/遥测/UI 事件迁为消息+hook；删 refining 集合与旧调用点；EditNote/DeleteNote 入信箱后移除 EDIT_LOCK | 精修全链路冒烟；hook 顺序与 panic 隔离测试 |
| P4 | 清理死代码、DESIGN.md 与本文档同步、附录 A 清单逐项真机勾销 | 全量冒烟 + 终审 |

## 测试策略

- **内核**：纯函数，全部迁移边 + 全部非法组合的表测试（对照 2026-07-12 状态机全景图逐边）。
- **效果层**：效果序列断言（mock 执行器记录调用）；顺序契约（append 先于 emit、reply 时机表）单测。
- **既有 397 测试零修改全绿**：任何必须改动的测试逐条列出原因，视为行为变更申请，需用户批准。
- **真机冒烟清单**（P4 附录 A 逐项勾）：录/停/暂停恢复/续录/中断恢复/零段删除/精修(成功/失败/partial)/标题生成与不覆盖/导出/MCP 控制面/遥测事件/双实例锁拒绝。

## 附录 A：行为不变式清单（验收硬标准）

1. 磁盘产物逐项等价：meta.json 两值与写入时机、segments.jsonl 追加与合并重写、speakers.json、refined.json stages 如实记档、原子写(tmp+rename)一律保留。
2. UI 事件（status/final/speakers/refine/storage/transcode_done/note_renamed）种类、载荷、相对顺序不变；新增 pending 权威化字段向后兼容。
3. 既有保证原样：转码入队在精修路径返回前至少一次（幂等）；finalize 刷盘失败留 recording；零段新建删目录、零段续录保留；续录 base_ms 时间轴接续；精修失败不碰原始数据；标题只在默认名时覆盖；遥测属性只出枚举与桶。
4. 命令同步语义按上表逐条一致。
5. MCP/CLI 接入面行为不变（stdio 子进程只读路径不经 actor，不受影响；UDS 控制面命令改发消息，语义同命令面）。

## 不做的事

- 不实现外部 hook 执行体（只留 trait+配置占位）。
- 不做多会话并行录制（actor 天然支持扩展，但本期单会话语义不变）。
- 不改前端交互与视觉。
- stdio MCP 查询子进程不引入 actor（无共享可变状态，直读磁盘）。

## 实施偏差记录（P0-P3 落地）

以下是实施过程中相对本文档正文的偏离，供 PR 审阅者与未来维护者核对；均已确认行为等价，不影响附录 A 的不变式清单。

1. **遥测两录制事件触发点未迁入 hook**：`RecordingStarted`/`RecordingStopped` 仍是 `do_start_recording`/`do_resume_note_recording`/`do_stop_recording`（`lib.rs`）里的直接 `telemetry::track` 调用，未改经迁移 hook 总线触发。原因：遥测语义是「尝试计数」而非「迁移计数」（例如启动失败也应记一次尝试），与 hook 契约的「到达状态才触发」不等价；裁决为保持原直调，等价性由计数语义而非迁移次数保证。
2. **EDIT_LOCK 未删，降级为 store 层第二道防线**：正文「P3：…EditNote/DeleteNote 入信箱后移除 EDIT_LOCK」（第 106 行）未执行——七个非活动编辑操作已经命令壳统一走 actor 单线程串行（`Msg::EditNote`），但 `store/notes.rs` 的 `EDIT_LOCK` 本身保留。原因：`concurrent_speaker_edits_do_not_lose_updates` 等既有测试会绕过命令壳/actor、多线程裸调 `NoteStore`，删锁会让这类直调路径丢更新；保留是对「入信箱后移除」条款的保守覆盖，不改变生产路径已单线程串行的事实。
3. **标题生成/转码入队仍留在精修 worker，未迁为独立 hook**：`spawn_refine` 内部（`lib.rs`）继续同步完成「精修完成后生成标题」与「转码入队」，未拆成迁移 hook 消费者。原因：这两者是保障类副作用（标题覆盖时机影响 UI 可见状态、转码入队幂等性是附录 A 第 3 条的硬约束），按架构原则「保障类副作用不走 hook，走 Effect/直调」，本就不该迁——正文表述「精修/转码/标题…迁为消息+hook」在落地时按此原则收窄为只迁进度事件转发（`RefineProgress`/`DoEmitRefine`），执行体本身不动。
4. **recording_status/status 查询仍直读 session 槽，未改发 `QueryStatus`**：`Cmd::QueryStatus` 变体已定义（`machine.rs`）但生产路径从未构造它，命令壳查询仍直读旧 session 槽。这是 P1 声明的偏离（原计划「P2 权威翻转时命令壳改发此命令」）延续到 P3 收尾未兑现的结果：内核在 P3 仍非唯一权威源，此时改道只会给读路径引入无意义的信箱排队,收益为负;留待内核真正翻权成唯一权威源时再改。
5. **`tray::set_recording` 改为 fire-and-forget**：托盘迁为 hook 消费者（`consumers.rs::TrayHook`）后，`tray::set_recording` 从同步调用改为派发主线程执行、不等待完成。原因：hook 在 actor 线程内同步调用消费者，若消费者内部又同步等主线程完成会与「request() 调用方持锁等 reply」路径成环（见 `actor.rs` 模块文档「死锁注记」③），改 fire-and-forget 是消除该环的必要修复，非可选项。
6. **`RefineState` 用 `BTreeSet<String>` 而非正文草案的 `enum { Idle, Running { note_id, stage } }`**：正文第 42 行草案假设精修全局单态，实测旧世界 `AppState.refining` 是 `HashSet`，支持多笔记并发精修（手动精修 A 期间停录 B 触发自动精修 B）；为与旧集合语义逐位对齐（等价性是硬约束），内核态改用按 id 查询的集合而非单态 enum，`BTreeSet` 只是取其 `PartialEq`/确定序 Debug 输出，语义等同 `HashSet`。
7. **`Msg::SessionEnded` 保留但生产不再发送**：正文消息协议列出 `SessionEnded { note_id, outcome }`（第 61 行）为工作线程回报之一，P2/P3 落地后停录收敛统一改为工作线程 teardown 后自投 `Msg::Finalize`（见 `actor.rs` 模块文档「P2 语义」），`SessionEnded` 未被生产路径构造。分支连同其 `ShadowMismatch` 对账语义保留在 `machine.rs`，只为不削掉迁移矩阵测试对该历史回报路径的覆盖，不代表仍在使用。

状态图 artifact 反映设计时点，Starting/Stopping 过渡态与精修集合语义以本节为准。
