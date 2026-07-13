# 生命周期 Actor 化 P3 实施计划（副作用 hook 化 + 精修态入内核 + 编辑入信箱）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ①hook 总线接入首批真实消费者（托盘图标随迁移驱动），注册机制成为新功能的标准接入面；②精修状态入内核（删 `refining` HashSet，精修守卫与进度事件经 actor）；③七个笔记编辑命令入信箱（actor 串行取代 EDIT_LOCK，删除之）。

**Architecture:** 延续绞杀者。hook 只收编「通知类」副作用（托盘）；遥测两事件**保持原调用点不迁**（迁移会把「尝试计数」变「成功计数」，违反等价硬约束——此为控制器裁决，写入不变式说明）；标题生成/转码入队是保障类，留在精修 worker（spec hook 语义节:保障类不走 hook）。UI 事件的种类与顺序照旧，仅精修进度事件的 emit 点从 worker 线程移到 actor（同一 worker 串行 report，顺序不变）。

**Spec:** `docs/superpowers/specs/2026-07-13-voice-notes-lifecycle-actor-design.md`

## Global Constraints

- 行为完全等价：412 测试零修改全绿；UI 事件（refine/status/…）种类、载荷、相对顺序不变；精修守卫拒绝文案逐字不变（「该笔记正在录制，停止后才能精修」「该笔记正在精修中」及续录被精修阻塞的既有文案）。
- 遥测触发点不迁（裁决见上）；telemetry mcp_tool_used 等其他事件不动。
- 提交只 add 点名文件严禁 add -A（工作区有用户未提交 README 改动）；无署名尾注；注释中文写为什么；警告 ≤7 基线；行号以锚点文本为准。

---

### Task 1: hook 首批消费者——托盘图标迁移驱动

**Files:**
- Create: `src-tauri/src/lifecycle/consumers.rs`（首个 hook 消费者模块）
- Modify: `src-tauri/src/lifecycle/hooks.rs`（TransitionCtx 补 `app: &'a AppHandle` 字段——消费者需要句柄发系统调用）
- Modify: `src-tauri/src/lifecycle/actor.rs`（spawn 时注册消费者；notify 传 app）
- Modify: `src-tauri/src/lifecycle/mod.rs`、`src-tauri/src/lib.rs`（删除两处 `tray::set_recording` 直调：spawn_session 成功点与 do_stop_tail）

**要点：**
- `TrayHook`：`on_transition` 里 `to` 进入 `Recording{..}` → `tray::set_recording(app, true)`；`to` 为 `Idle` 且 `from` 是 `Recording/Stopping` → `set_recording(app, false)`。set_recording 已是 fire-and-forget，hook 契约（不持共享锁、不阻塞）满足。
- 时序差异声明：托盘图标翻转从「emit 后紧邻」变为「actor 处理完对应消息后」——同为毫秒级异步投递，用户不可感知，写进报告即可。
- 注册在 `spawn()` 建 bus 处：`bus.register(Box::new(consumers::TrayHook));`（启动前注册完再进循环，符合 HookBus 契约）；顺带删除 hooks.rs 因「无消费者」加的 allow(dead_code)。
- 测试：hooks 层已有顺序/隔离测试；TrayHook 逻辑给纯函数测试（抽 `fn tray_flag(from,to)->Option<bool>` 单测四象限）。
- 验证：`cargo test --lib` 全绿；`grep -rn "tray::set_recording" src-tauri/src/lib.rs` 仅剩注释/零命中（tray.rs 定义与 consumers.rs 消费除外）。
- Commit: `git add src-tauri/src/lifecycle/ src-tauri/src/lib.rs` → 「P3 hook 首批消费者:托盘图标由迁移驱动,扩展接入面实弹化」

### Task 2: 精修态入内核

**Files:**
- Modify: `src-tauri/src/lifecycle/machine.rs`（内核增 `refine: RefineState { Idle, Running{note_id} }` 维度——LifecycleState 变 struct{session, refine} 或并列字段，矩阵测试同步；新 Msg：`RefineRequest{note_id}`(经 request 带回执)、`RefineProgress{note_id, stage, state}`、`RefineFinished{note_id}`；新 Effect：`DoSpawnRefine{note_id, enqueue_transcode}`、`DoEmitRefine{..}`）
- Modify: `src-tauri/src/lifecycle/actor.rs`（执行器；RefineRequest 的守卫判定在内核：session 态为该 id 的 Recording/Stopping → ReplyErr「该笔记正在录制，停止后才能精修」；refine Running → ReplyErr「该笔记正在精修中」——文案逐字）
- Modify: `src-tauri/src/lib.rs`（`refine_note` 命令壳改 request(RefineRequest)；`spawn_refine` 的 refining 集合插入/移除改为 report RefineProgress("all","running") 由内核置 Running、RefineFinished 置 Idle；worker 内全部 `emit("refine",…)` 改 report RefineProgress，由 actor 统一 emit——同一 worker 串行 report,事件顺序不变；`resume_blocked_by_refining` 改读内核态——resume 守卫在 do_resume_note_recording（actor 线程上执行），把内核 refine 态经 runner 传入或改为 actor 拦截 `Cmd::Start{resume_id:Some}` 时先查内核（推荐后者:守卫文案逐字搬进 ReplyErr）；`AppState.refining` 字段删除；spawn_refine 的 `is_resumed_by_active_session` 闭包保留——它查 session 槽,不查 refining）
- **停录自动精修**：DoFinalize 执行器里 `spawn_refine(app, o.note_id, true)` 的调用改为 Effect 语义不变（仍在 DoFinalize 内直接调用——它是保障类,不经 RefineRequest 守卫,与旧世界一致:停录精修不受「正在精修」拒绝影响,因 refining 在停录时必然不含该 id——把这个等价论证写报告）。自动路径同样要置内核 Running：spawn_refine 入口统一 report RefineProgress("all","running")。
- 测试：内核矩阵补 refine 维度（RefineRequest 在四种 session 态×refine 两态的裁决表）；`cargo test --lib` 全绿。
- Commit: 「P3 精修态入内核:守卫/进度/收尾经 actor,删 refining 集合」

### Task 3: 编辑入信箱，删 EDIT_LOCK

**Files:**
- Modify: `src-tauri/src/lifecycle/machine.rs`（新 Msg `EditNote { op: EditOp }` + Effect `DoEdit`；`pub enum EditOp { Rename{id,title}, Delete{id}, RenameSpeaker{id,speaker,name}, AssignPerson{id,speaker,person}, EditText{id,seq,text}, DeleteSegment{id,seq}, SetSegmentSpeaker{id,seq,speaker} }`——与 notes.rs 七函数一一对应）
- Modify: `src-tauri/src/lifecycle/actor.rs`（DoEdit 执行器：match op 调 NoteStore 对应方法——**逐字保留每个命令壳现有的前置守卫与返回类型**;有返回值的（如 delete 需要刷新列表?）核对各命令签名后原样透传——需要回传数据的经 `request_with::<T>` 泛型回执或维持 Result<(),String>+命令壳自行重查,以现状为准零改变）
- Modify: `src-tauri/src/lib.rs`（七个编辑命令壳改 request(EditNote{op})；**活动笔记守卫**（如 delete/rename 对录制中笔记的既有拒绝）逐字保留在原判定层级）
- Modify: `src-tauri/src/store/notes.rs`（删 `EDIT_LOCK`/`edit_guard()` 与七处 `let _guard`——actor 串行是其完全替代;flock（跨进程）保留不动;文件头注释同步改写「进程内互斥由 lifecycle actor 单线程保证」）
- 注意：notes.rs 的单测直接调用 NoteStore 方法（无 actor）——EDIT_LOCK 删除后测试仍串行执行（cargo test 单测内无并发编辑），零修改可过；若有测试专测 edit_guard 行为则停下报告。
- 验证：`grep -rn "EDIT_LOCK\|edit_guard" src-tauri/src/` 零命中；412 全绿。
- Commit: 「P3 编辑入信箱:七命令经 actor 串行,EDIT_LOCK 退役(flock 跨进程职责保留)」

### Task 4: P3 对账收尾与真机冒烟

- 静态对账：refining/EDIT_LOCK 零残留；hook 注册点唯一；警告 ≤7；`git diff master --stat` 无意外文件。
- 真机冒烟（dev+CLI+say）：录→停（托盘图标经 hook 翻转——日志/行为观察）→自动精修事件序列（前端 refine 事件时序：running→filter/recluster/llm→all done）→精修中触发 refine_note 拒绝文案逐字→精修中续录拒绝文案逐字→编辑一条非活动笔记（改名+改段文本）成功→录制中编辑同笔记被 flock 拒绝→影子对账 0→panic 0。
- DESIGN.md 不涉及（无 UI 变化）；spec 附录 A 补充「遥测触发点不迁」裁决记录。
- 记账 Commit。
