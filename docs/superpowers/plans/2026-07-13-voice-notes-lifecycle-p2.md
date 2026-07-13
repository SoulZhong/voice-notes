# 生命周期 Actor 化 P2 实施计划（writer 所有权入 actor）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** NoteWriter 所有权移入 lifecycle actor：管线回调（append/说话人事件）、停录 finalize、活动改名、开录改标题全部经信箱串行；删除 `Arc<Mutex<NoteWriter>>` 与全部旁路锁；附带终审点名的两项必修（run_delegate catch_unwind、委托失败回退预演态）。

**Architecture:** 继续绞杀者：会话装配/音频/ASR 线程结构不动，回调从「抓 writer 锁」改为「发消息」；actor 独占 writer。停录时序用**自投消息**解决排干问题：teardown 委托返回后 actor 向自己信箱投 `Finalize`——它排在 handle.stop() 排干期间入队的全部管线消息之后（同队列 FIFO + happens-before），「先全部落盘、再 finalize、再 emit stopped」的既有顺序由队列结构保证而非锁。

**Tech Stack:** 既有 crossbeam-channel；无新依赖。

**Spec:** `docs/superpowers/specs/2026-07-13-voice-notes-lifecycle-actor-design.md`（附录 A 仍是硬验收）
**前置事实（调研确认）**：writer 生产触点=管线回调 9 处（单一 ASR worker 线程串行产生）、do_stop finalize、加载线程创建/读元信息、rename_speaker 活动旁路（命令线程）、uds set_title 旁路（UDS 线程）、abort_or_finalize×3（加载线程失败路径）；LLM 改名不触活动 writer；crossbeam 队列可线性化——同生产者 FIFO + 跨线程 happens-before 传递入队序。

## Global Constraints

- **行为完全等价**：409 测试零修改全绿（必须改动=行为变更申请，停下报控制器）；UI 事件种类与相对顺序不变（final 在 append 之后 emit、stopped 在 finalize 之后、storage ok/degraded 翻转条件不变）。
- 锁序纪律不变；actor 死锁三条边约束不变（尤其:Delegate 执行体内不得主线程同步派发）。
- 管线事件顺序契约不变：同一 ASR worker 线程产生 Final/Diar → 同生产者 FIFO 保序；Merged 先于 SpeakersChanged 的既有契约随之保持。
- 注释中文写为什么；提交不带署名尾注；**提交只 add 点名文件，严禁 add -A**（工作区有用户未提交的 README 改动）。
- `cd src-tauri && cargo test --lib` 全绿；`cargo build` 警告 ≤7 基线。mcp_stdio README 漂移失败是 master 既有，忽略。
- 行号基于 5694e9c，以锚点文本为准。

---

### Task 1: 必修两项——run_delegate 防 panic + 委托失败回退预演态

**Files:**
- Modify: `src-tauri/src/lifecycle/actor.rs`（run_delegate :52-65、spawn 主循环 :67-107）
- Modify: `src-tauri/src/lifecycle/machine.rs`（仅测试区，补断言）

**Interfaces:**
- Produces: run_delegate 语义变为「永不 panic：捕获转 `Err("命令执行体 panic: …")` 并 eprintln」；主循环语义变为「Cmd 消息的预演迁移在任一 Delegate 效果失败时整体回退（状态不变、不通知 hook）」。

- [ ] **Step 1: 写失败测试**

machine.rs 测试区补（纯内核层面锁住「预演可回退」的前提——迁移只由 handle 计算、由 runner 决定是否提交）：

```rust
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
```

actor.rs 无法脱离 AppHandle 单测，防 panic 与回退逻辑靠 Step 3 实现 + Task 5 冒烟验证；本步先跑通内核测试。

- [ ] **Step 2: 跑测试**

Run: `cd src-tauri && cargo test lifecycle::machine`
Expected: 新测试 PASS（handle 本就是纯函数）。

- [ ] **Step 3: 实现**

`run_delegate` 整体包裹：

```rust
/// 执行 Delegate。catch_unwind:do_* 若 panic(现实来源仅锁中毒),actor 线程
/// 绝不能死——否则控制面(按钮/托盘/快捷键/MCP)全部静默失联,比旧世界的
/// 显性崩溃更糟。捕获后转 Err 回给调用方并响亮记日志。
fn run_delegate(app: &AppHandle, cmd: &Cmd) -> Result<(), String> {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match cmd {
        // …… 既有映射表原样 ……
    }));
    match r {
        Ok(inner) => inner,
        Err(_) => {
            eprintln!("lifecycle: 命令执行体 panic(已捕获,actor 存活): {cmd:?}");
            Err("内部错误:命令执行失败".into())
        }
    }
}
```

主循环回退（替换现有「先执行效果、后无条件提交 next」段）：

```rust
                let (next, effects) = machine::handle(&state, &msg);
                let mut result: Result<(), String> = Ok(());
                for fx in &effects { /* …… 既有执行 …… */ }
                // 委托失败 → 回退预演迁移:状态不动、不通知 hook。
                // 否则守卫拒绝的 Start 会留下幻影 Starting + 幻影迁移通知,
                // P3 挂上消费者后 hook 将收到从未真实发生的迁移。
                let commit = if matches!(msg, Msg::Cmd(_)) && result.is_err() { state.clone() } else { next };
                if commit != state {
                    /* …… 既有 notify …… */
                    state = commit;
                }
```

- [ ] **Step 4: 全量测试 + 构建**

Run: `cd src-tauri && cargo test --lib && cargo build 2>&1 | grep -c "^warning"`
Expected: 全绿；警告 ≤7。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lifecycle/actor.rs src-tauri/src/lifecycle/machine.rs
git commit -m "P2 必修:run_delegate 捕获 panic 保活 actor,委托失败回退预演迁移消灭幻影 Starting"
```

---

### Task 2: 内核扩展——writer 语义消息与效果

**Files:**
- Modify: `src-tauri/src/lifecycle/machine.rs`

**Interfaces:**
- Produces（Task 3/4 依赖的确切形状）:

```rust
/// 管线事件载荷(ASR worker 线程原样转发,actor 持 writer 执行)。
#[derive(Debug)]
pub enum PipelineOp {
    Final { source: String, text: String, start_ms: u64, end_ms: u64,
            speaker: Option<String>, rms: Option<f32> },
    Diar(crate::session::DiarEvent),
}
pub enum Msg {
    Cmd(Cmd),
    SessionStarted { note_id: String },
    SessionFailed,
    SessionEnded { note_id: String },          // 保留:P1 兼容,Task 4 停录改造后由 Finalize 取代发送
    // —— P2 新增 ——
    AdoptWriter { writer: Box<crate::store::writer::NoteWriter> },
    Pipeline(PipelineOp),
    Finalize { note_id: String },              // 停录 teardown 完成后自投
    AbortSession,                              // 加载失败路径:abort_or_finalize 语义
    SetTitle { note_id: String, title: String },
    RenameActiveSpeaker { note_id: String, speaker_id: String, name: String },
}
pub enum Effect {
    Delegate(Cmd), ReplyErr(String), ShadowMismatch(String),
    // —— P2 新增(runner 持 writer 执行;内核只发指令不做 IO) ——
    DoAdopt, DoPipeline, DoFinalize { note_id: String }, DoAbort,
    DoSetTitle { note_id: String, title: String },
    DoRenameActiveSpeaker { note_id: String, speaker_id: String, name: String },
}
```

迁移规则（写成表测试）：`AdoptWriter/Pipeline/SetTitle/RenameActiveSpeaker/AbortSession` 不改内核会话态（writer 归属是 runner 状态）；`Finalize{id}`：从 `Recording{id}`/`Stopping{id}` → `Idle` 顺流零噪音，其余态 → `Idle` + ShadowMismatch（与 SessionEnded 同规则）。载荷不进内核判定（内核不读 writer）。

- [ ] **Step 1: 写失败测试**（新增迁移规则矩阵：5 类新消息 × 4 状态，断言状态不变/Finalize 规则与效果种类一一对应；照 every_report_in_every_state_reconciles 的既有写法扩展）

- [ ] **Step 2: 跑测试确认失败 → Step 3: 实现（enum + handle 分支）→ Step 4: `cargo test lifecycle` 全绿、警告 ≤7（新 Effect 未消费用单项 allow(dead_code)+「Task 3 消费后删」注释）**

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lifecycle/machine.rs
git commit -m "P2 内核扩展:writer 语义消息(Adopt/Pipeline/Finalize/Abort/SetTitle/RenameActive)+迁移矩阵测试"
```

---

### Task 3: runner 持有 writer——采纳/管线/放弃三条路径接线

**Files:**
- Modify: `src-tauri/src/lifecycle/actor.rs`（runner 增 writer 槽与效果执行器）
- Modify: `src-tauri/src/lib.rs`（spawn_session：writer 创建后改发 AdoptWriter；on_final/on_diar 回调改发 Pipeline 消息；三处失败路径改发 AbortSession；ActiveSession 删 writer 字段）
- Modify: `src-tauri/src/mcp/uds.rs`（set_title 改 `command(Cmd::…)`——注意 Cmd 无此变体，经 `LifecycleHandle` 新增 `request(msg, 需要回执)` 通道，见 Interfaces）

**Interfaces:**
- Produces:
  - runner 内部状态 `struct Owned { note_id: String, writer: store::writer::NoteWriter, degraded: bool }`，`Option<Owned>` 槽；
  - `LifecycleHandle::request(&self, msg: machine::Msg) -> Result<(), String>`（带回执的非 Cmd 消息投递，供 SetTitle/RenameActiveSpeaker/Finalize 等需要同步结果的调用方）；`report()` 语义不变（不等待）；
  - 效果执行器语义（**逐字对齐旧代码**，把 lib.rs 对应块原样搬进 runner，仅把 `writer.lock().unwrap()` 换成 `&mut owned.writer`）：
    - `DoAdopt`：装槽；若槽已占，eprintln 对账异常并覆盖（不应发生）。
    - `DoPipeline(Final)`：搬 lib.rs:897-923 的 on_final 块——append_final + degraded 翻转 + emit storage + emit final，顺序逐字保持。
    - `DoPipeline(Diar(ev))`：搬 lib.rs:938-1030 的 on_diar 四分支块（SpeakersChanged/Merged/EchoRetract/Snapshot），含 speakers 快照 emit 与 merge/retract 失败的一次性 degraded 告警。
    - `DoAbort`：搬 abort_or_finalize 语义作用于槽内 writer（has_content→finalize 失败仅日志；空新建→drop writer 后删目录），清槽。
    - `DoFinalize/DoSetTitle/DoRenameActiveSpeaker`：Task 4 消费，本任务先落执行器空位（`unreachable!` 不可用——用 eprintln+忽略占位并注明 Task 4 填充）。
- Consumes: Task 2 全部新消息/效果。

- [ ] **Step 1: lib.rs 回调与失败路径改造**（核心 diff 形状）

```rust
// spawn_session 加载线程,writer 创建后(原 lib.rs:762-776 之后):
//   note_id/dir/base_ms/registry_snapshot 先在本线程读完(writer 尚未移交,直接方法调用),
//   然后立即移交所有权——此后本线程不再持有 writer,一切写经信箱。
    let lc = app.state::<lifecycle::LifecycleHandle>().inner().clone();
    lc.report(lifecycle::machine::Msg::AdoptWriter { writer: Box::new(w) });
// on_final 闭包(原 897-923 整块替换为):
    move |src, text, start_ms, end_ms, spk, rms| {
        let start_ms = start_ms + base_ms;
        let end_ms = end_ms + base_ms;
        lc_f.report(lifecycle::machine::Msg::Pipeline(lifecycle::machine::PipelineOp::Final {
            source: src.as_str().into(), text, start_ms, end_ms, speaker: spk, rms,
        }));
    }
// on_diar 闭包(原 938-1030 整块替换为):
    move |ev| lc_d.report(lifecycle::machine::Msg::Pipeline(lifecycle::machine::PipelineOp::Diar(ev)))
// 三处失败路径 abort_or_finalize(&writer) → lc.report(Msg::AbortSession)
// ActiveSession 结构体删 writer 字段;入槽处同步删。
```

注意点写给实现者：①emit 所需的 `app` 在 runner 侧已有（actor 持 AppHandle），搬块时闭包捕获的 `app_f`/`vp_store_d`/`enroller` 等上下文要一并评估——`vp_store_d`（声纹库回写）与 `enroller` 若只在 on_diar 块内用，随块搬进 runner 需要跨线程 Send：确认类型；若不可 Send 则该子块保留在回调线程、只把「触 writer 的部分」拆出经消息（把你的拆分决策写进报告）。②`degraded` 局部变量移为 Owned.degraded。③base_ms 已在闭包捕获，保持。

- [ ] **Step 2: runner 效果执行器实现**（按 Interfaces 逐字搬块；`Envelope` 增 `Request { msg, reply }` 变体承载带回执消息；`request()` 实现同 command() 的 bounded(1) 模式）

- [ ] **Step 3: 全量测试 + 构建**：`cargo test --lib` 全绿零修改（管线相关测试都在 session.rs/store 层，不触 lib.rs 闭包——若有编译错误多为搬块漏捕获）；警告 ≤7。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lifecycle/ src-tauri/src/lib.rs
git commit -m "P2 writer 入 actor(上):AdoptWriter/Pipeline/Abort 三径接线,管线回调只发消息,删 ActiveSession.writer"
```

---

### Task 4: 停录时序重构 + 两条旁路收编

**Files:**
- Modify: `src-tauri/src/lib.rs`（do_stop_recording 拆分；rename_speaker 活动分支）
- Modify: `src-tauri/src/lifecycle/actor.rs`（Cmd::Stop 处理改为 teardown+自投 Finalize；DoFinalize/DoSetTitle/DoRenameActiveSpeaker 执行器填充）
- Modify: `src-tauri/src/mcp/uds.rs`（set_title 改 request）

**Interfaces:**
- Consumes: Task 3 的 Owned 槽、request()。
- Produces: `do_stop_teardown(app) -> Option<String>`（原 do_stop 的 running/generation/take/telemetry/handle.stop/stash/joins 段，逐语句搬移，返回 note_id；无会话返回 None）。

- [ ] **Step 1: 停录重构**

actor 的 `Delegate(Cmd::Stop)` 特化：

```rust
// Cmd::Stop:teardown 同步做(排干 finals——期间管线消息全部入队),
// 然后自投 Finalize:它排在那些消息**之后**(同队列,teardown 返回 happens-before
// 自投),actor 先消化全部落盘再收尾——「先落盘后 finalize」由队列结构保证。
// reply 延迟到 Finalize 处理完(停录命令的同步语义=收尾完成,与旧世界一致)。
```

实现：Envelope::Cmd{Stop} 分支不走通用路径——调 `do_stop_teardown`；None→立即 reply Ok（旧世界空停静默）；Some(note_id)→`tx.send(Envelope::Request{ msg: Finalize{note_id}, reply })`（把 stop 的 reply 转移进去），本轮不 reply。`DoFinalize` 执行器 = 原 do_stop 后半段逐字搬移：owned.writer.finalize → Ok→spawn_refine(true) / Err→eprintln+emit degraded → 清槽（drop writer 释放 flock）→ emit "stopped" → tray false → preload_models。内核 `(Recording|Stopping, Finalize)` → Idle 已由 Task 2 落好，SessionEnded report 删除（被 Finalize 取代——machine 的 SessionEnded 分支保留兼容但 lib.rs 不再发送）。

- [ ] **Step 2: rename_speaker 活动分支 → `request(Msg::RenameActiveSpeaker{...})`**；`DoRenameActiveSpeaker` 执行器=原 lib.rs:1683-1698 块逐字搬（set_speaker_name/persist_speakers/speakers 快照 emit）；活动判定改为「runner 槽 note_id 匹配」（旧判定读 session slot——语义等价：槽与会话同生共死，把等价性论证写报告）。非活动分支不动。

- [ ] **Step 3: uds set_title → `request(Msg::SetTitle{...})`**；`DoSetTitle`=owned.writer.set_title；note_id 不匹配/无槽→Err（旧世界锁上直接调,新世界会话已停则报「录制已结束」——行为差异点:旧代码在会话槽消失后本就拿不到 s,同样走不到 set_title,等价）。

- [ ] **Step 4: 全量测试 + 构建 + 死锁自检写报告**（新自检点：Cmd::Stop 处理中自投 Request 到自己信箱=unbounded 不阻塞 ✓；rename/set_title 从命令/UDS 线程 request 阻塞等 actor——actor 不等它们 ✓；DoFinalize 里 spawn_refine/preload 均 spawn 不等待 ✓）

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lifecycle/actor.rs src-tauri/src/lib.rs src-tauri/src/mcp/uds.rs
git commit -m "P2 writer 入 actor(下):停录自投 Finalize 保证先落盘后收尾,活动改名/开录标题收编信箱,删全部 writer 旁路"
```

---

### Task 5: P2 对账收尾与真机冒烟

- [ ] **Step 1: 静态对账**：`grep -rn "writer.lock()\|Arc<Mutex<store::writer\|Arc<Mutex<NoteWriter" src-tauri/src/ | grep -v "#\[cfg(test)\]" | grep -v tests` 应为空（生产代码零残留）；`abort_or_finalize` 函数体应已迁入 runner（lib.rs 原函数删除或仅剩 runner 调用）；Task 2/3 的占位 allow(dead_code) 全部删除，警告 ≤7。
- [ ] **Step 2: 真机冒烟**（dev 实例 + CLI 控制面，复用 P1 Task 7 流程）：录→说话产生转写段（检查 segments.jsonl 实时追加）→暂停/恢复→停录→meta complete + segments 完整 + m4a 转码 + 自动精修触发；续录再停;录制中 CLI `record status` 不被阻塞（管线消息高峰下查询即回）；影子对账 0 条；开录带 --title 标题生效；无 panic。
- [ ] **Step 3: 记账 Commit**（`git add` 点名文件）
