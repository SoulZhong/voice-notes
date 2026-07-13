# 生命周期 Actor 化 P0+P1 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** P0 落地跨进程文件锁（根治双实例重写竞态）；P1 落地 lifecycle actor 骨架（纯函数内核+hook 总线+信箱），五个录制命令改道信箱、内核影子对账，行为与现状完全等价。

**Architecture:** 绞杀者式：P1 的 actor 只做「串行化+影子状态机+hook 管道」，命令执行体仍是现有 `do_*` 函数原样委托；内核状态由 spawn_session 成功/失败与停录完成的通知消息驱动，与旧路径对账（不一致仅记日志）。权威状态翻转到内核是 P2 的事。

**Tech Stack:** Rust（crossbeam-channel 0.5 已有、libc 0.2 已有做 flock）；无新增依赖。

**Spec:** `docs/superpowers/specs/2026-07-13-voice-notes-lifecycle-actor-design.md`（附录 A 不变式清单是本计划的验收标准）

**后续计划**：P2（writer 入 actor）/P3（副作用 hook 化）/P4（清理冒烟）在 P1 合入后依真实落地形态另行制定——现在写出来的锚点必然过期。

## Global Constraints

- **行为完全等价**：397 个既有测试**零修改**全绿；任何测试必须改动=行为变更申请，停下来向控制器报告。
- 锁序纪律不变：`running → generation → session_slot`，绝不同时持两把全局锁（lib.rs:89-126 注释）。
- 新代码注释中文、写「为什么」；git 提交不带任何署名尾注。
- Rust 测试在 `src-tauri` 下 `cargo test`；`cargo build` 不得新增警告。
- 行号基于 commit f94ed4c，动手前以锚点文本确认。
- 分支：`lifecycle-actor`（从 master 切出）。

---

### Task 1: NoteLock 跨进程文件锁原语

**Files:**
- Create: `src-tauri/src/store/notelock.rs`
- Modify: `src-tauri/src/store/mod.rs`（`pub mod notelock;` 加入模块声明区）

**Interfaces:**
- Produces: `NoteLock::try_exclusive(dir: &Path) -> std::io::Result<Option<NoteLock>>`（None=他人持有）；`NoteLock` Drop 即释放；常量 `notelock::LOCK_FILE = ".note.lock"`。后续任务全部经此加锁。

- [ ] **Step 1: 写模块与失败测试**

创建 `src-tauri/src/store/notelock.rs`：

```rust
//! 笔记目录跨进程写锁(flock 独占)。
//!
//! 动机:2026-07-13 事故——第二个应用实例整表重写 segments.jsonl,录制实例的
//! 追加句柄从此指向被替换的孤儿 inode,35 分钟转写静默丢失。进程内锁
//! (EDIT_LOCK / writer Mutex)对跨进程无效,flock 是最小充分武器。
//!
//! 语义:flock 按 open file description 计——同进程再 open 也互斥,因此
//! 「本进程录制中」与「另一进程录制中」在编辑路径上得到同一种拒绝,无需区分。
//! 锁生命周期即值生命周期:Drop 关 fd 自动释放,崩溃时内核代为释放,无残留。

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

pub const LOCK_FILE: &str = ".note.lock";

pub struct NoteLock {
    _file: File,
}

impl NoteLock {
    /// 非阻塞尝试独占。Ok(None) = 已被其他持有者(进程或本进程另一句柄)占用。
    pub fn try_exclusive(dir: &Path) -> std::io::Result<Option<NoteLock>> {
        let f = OpenOptions::new().create(true).write(true).open(dir.join(LOCK_FILE))?;
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            Ok(Some(NoteLock { _file: f }))
        } else {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_blocks_second_holder_and_drop_releases() {
        let dir = tempfile::tempdir().unwrap();
        let l1 = NoteLock::try_exclusive(dir.path()).unwrap();
        assert!(l1.is_some(), "首个持有者应拿到锁");
        // 同进程第二个句柄也应被拒(flock 按 OFD 计)
        let l2 = NoteLock::try_exclusive(dir.path()).unwrap();
        assert!(l2.is_none(), "锁被持有时第二个句柄应拿不到");
        drop(l1);
        let l3 = NoteLock::try_exclusive(dir.path()).unwrap();
        assert!(l3.is_some(), "Drop 后应可重新获取");
    }

    #[test]
    fn lock_file_created_in_dir() {
        let dir = tempfile::tempdir().unwrap();
        let _l = NoteLock::try_exclusive(dir.path()).unwrap();
        assert!(dir.path().join(LOCK_FILE).exists());
    }
}
```

在 `src-tauri/src/store/mod.rs` 模块声明区（`pub mod audio;` 一带）加：

```rust
pub mod notelock;
```

- [ ] **Step 2: 跑测试确认通过**

Run: `cd src-tauri && cargo test notelock`
Expected: 2 个测试 PASS。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/store/notelock.rs src-tauri/src/store/mod.rs
git commit -m "P0 NoteLock:笔记目录 flock 独占锁原语,Drop 即释放"
```

---

### Task 2: 录制会话持锁 + 编辑路径拒绝

**Files:**
- Modify: `src-tauri/src/store/writer.rs`（struct 行 12-30、create 行 108-151、resume 行 153-205）
- Modify: `src-tauri/src/store/notes.rs`（edit_guard 行 15-19 附近新增辅助；七个编辑函数 rename:149 / delete:157 / rename_speaker:165 / assign_speaker_person:~185 / edit_segment_text:197 / delete_segment:216 / set_segment_speaker:228）

**Interfaces:**
- Consumes: Task 1 的 `NoteLock`。
- Produces: `NoteWriter` 全生命周期持有目录锁；`notes.rs` 新增私有 `fn write_lock(dir: &Path) -> anyhow::Result<store::notelock::NoteLock>`（拿不到 → 统一错误文案）。

- [ ] **Step 1: 写失败测试（writer 持锁 + 编辑被拒）**

`src-tauri/src/store/writer.rs` 底部测试 mod 追加：

```rust
#[test]
fn writer_holds_note_lock_for_whole_session() {
    let dir = tempfile::tempdir().unwrap();
    let w = NoteWriter::create(dir.path(), chrono::Local::now()).unwrap();
    let note_dir = w.dir().to_path_buf();
    // 会话期间:任何人拿不到锁
    assert!(crate::store::notelock::NoteLock::try_exclusive(&note_dir).unwrap().is_none());
    drop(w);
    // writer 落幕:锁释放
    assert!(crate::store::notelock::NoteLock::try_exclusive(&note_dir).unwrap().is_some());
}
```

`src-tauri/src/store/notes.rs` 底部测试 mod 追加：

```rust
#[test]
fn edit_rejected_while_note_locked() {
    let (dir, id) = fixture_note(); // 用本文件既有夹具惯例建一条笔记;若无现成夹具,仿 edit_segment_text 的既有测试搭建
    let note_dir = dir.path().join("notes").join(&id);
    let _lock = crate::store::notelock::NoteLock::try_exclusive(&note_dir).unwrap().unwrap();
    let store = NoteStore::new(dir.path().join("notes"));
    let err = store.edit_segment_text(&id, 0, "改").unwrap_err().to_string();
    assert!(err.contains("正在录制"), "锁占用时编辑应被明确拒绝,实际: {err}");
}
```

（夹具函数名以该文件既有测试为准——`edit_segment_text` 已有测试，抄它的搭建方式。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test writer_holds_note_lock edit_rejected_while`
Expected: 编译错误（NoteWriter 无锁字段）/断言失败。

- [ ] **Step 3: 实现**

`writer.rs`：struct 加字段（放 `created_this_session` 之后）：

```rust
    /// 笔记目录跨进程写锁:create/resume 时获取,随 NoteWriter 一起落幕。
    /// 拿不到锁 = 另一实例正在录制/编辑本笔记,开录失败并明确报错。
    _lock: super::notelock::NoteLock,
```

`create()` 内在确定 `dir` 之后、打开 segments 句柄之前插入：

```rust
        let lock = super::notelock::NoteLock::try_exclusive(&dir)
            .map_err(|e| anyhow::anyhow!("笔记目录锁不可用: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("该笔记正被另一实例录制或编辑,无法开始"))?;
```

（`resume()` 同样插入，错误文案同。）两处构造 `Self { ... }` 补 `_lock: lock,`。

`notes.rs`：`edit_guard()` 旁新增：

```rust
/// 编辑前的跨进程写锁:录制会话(本进程或另一实例)持锁期间,一切整表
/// 重写被明确拒绝——这是对「双实例重写丢转写」事故的直接防线。
/// 返回值须存活到写盘完成(锁生命周期即保护窗口)。
fn write_lock(dir: &Path) -> anyhow::Result<super::notelock::NoteLock> {
    super::notelock::NoteLock::try_exclusive(dir)
        .map_err(|e| anyhow::anyhow!("笔记目录锁不可用: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("该笔记正在录制中(可能来自另一个应用实例),请停止录制后再试"))
}
```

七个编辑函数：在各自 `let _guard = edit_guard();` 之后、任何读-改-写之前加一行（`dir` 为该函数已解析出的笔记目录）：

```rust
        let _flock = write_lock(&dir)?;
```

- [ ] **Step 4: 全量测试**

Run: `cd src-tauri && cargo test`
Expected: 新测试 PASS；**既有测试零修改全绿**（writer 测试都在 tempdir 建目录，锁可正常获取；若有测试在同目录建两个 writer 而失败——那是在复现真 bug，停下来报告而非改测试）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/store/writer.rs src-tauri/src/store/notes.rs
git commit -m "P0 会话持锁+编辑拒绝:录制期笔记目录 flock 独占,整表重写路径先取锁"
```

---

### Task 3: 启动回溯扫描与转码 worker 探锁

**Files:**
- Modify: `src-tauri/src/lib.rs:2783-2792`（setup 回溯扫描循环）
- Modify: `src-tauri/src/store/transcode.rs`（worker 循环里 `transcode_note_dir` 调用处；用 `grep -n "transcode_note_dir" src-tauri/src/store/transcode.rs` 定位）

**Interfaces:**
- Consumes: Task 1 `NoteLock`。

- [ ] **Step 1: 回溯扫描先探锁**

lib.rs setup 扫描循环改为：

```rust
        if let Ok(rd) = std::fs::read_dir(root.join("notes")) {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    // 先探跨进程锁:活动会话(含另一实例的)绝不误判为孤儿去修头/转码。
                    // 探完即释放——这里只是排除活会话,真正的持锁保护在转码 worker 内。
                    match store::notelock::NoteLock::try_exclusive(&e.path()) {
                        Ok(Some(_probe)) => {}
                        _ => continue,
                    }
                    store::audio::repair_stale_tracks(&e.path());
                    if should_enqueue_transcode(&e.path()) {
                        st.transcode.enqueue(e.path());
                    }
                }
            }
        }
```

- [ ] **Step 2: 转码 worker 持锁**

`store/transcode.rs` worker 循环中，对每个出队目录执行转码之前：

```rust
            // 转码会编码后删除源 WAV——必须独占持锁到转码结束,防止与
            // (本进程续录 cancel_and_wait 之外的)另一实例的活动会话相撞。
            let lock = match super::notelock::NoteLock::try_exclusive(&dir) {
                Ok(Some(l)) => l,
                _ => continue, // 拿不到 = 有活会话;续录停止后精修路径会重新入队,不丢
            };
```

转码调用结束后 `drop(lock);`（或让作用域自然结束）。锚点：worker 循环体内调用 `transcode_note_dir(&dir, ...)` 的那一段——按实际代码把持锁包住整个「转码+删 WAV」窗口。

- [ ] **Step 3: 全量测试 + 构建**

Run: `cd src-tauri && cargo test && cargo build 2>&1 | grep -c "^warning" `
Expected: 全绿；警告数与 master 基线一致（7）。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/store/transcode.rs
git commit -m "P0 收口:启动回溯扫描探锁排除活会话,转码 worker 持锁护住删 WAV 窗口"
```

---

### Task 4: lifecycle 纯函数内核（状态机表）

**Files:**
- Create: `src-tauri/src/lifecycle/mod.rs`（仅 `pub mod machine;` 与后续任务的挂点）
- Create: `src-tauri/src/lifecycle/machine.rs`
- Modify: `src-tauri/src/lib.rs`（mod 声明区加 `mod lifecycle;`）

**Interfaces:**
- Produces:
  - `SessionState { Idle, Starting { resume_id: Option<String> }, Recording { note_id: String, paused: bool }, Stopping { note_id: String } }`
  - `Msg { Cmd(Cmd), SessionStarted { note_id: String }, SessionFailed, SessionEnded { note_id: String } }`
  - `Cmd { Start { resume_id: Option<String> }, Stop, Pause, Unpause, QueryStatus }`
  - `Effect { Delegate(Cmd), ReplyErr(String), ShadowMismatch(String) }`
  - `fn handle(state: &SessionState, msg: &Msg) -> (SessionState, Vec<Effect>)`

- [ ] **Step 1: 写内核与全覆盖测试**

创建 `src-tauri/src/lifecycle/machine.rs`：

```rust
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
```

创建 `src-tauri/src/lifecycle/mod.rs`：

```rust
//! 笔记生命周期 actor(P1:骨架+影子内核)。设计文档:
//! docs/superpowers/specs/2026-07-13-voice-notes-lifecycle-actor-design.md
pub mod machine;
```

lib.rs mod 声明区加 `mod lifecycle;`。

- [ ] **Step 2: 跑测试**

Run: `cd src-tauri && cargo test lifecycle::machine`
Expected: 5 测试 PASS。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/lifecycle/ src-tauri/src/lib.rs
git commit -m "P1 lifecycle 内核:影子状态机纯函数+迁移表全覆盖测试"
```

---

### Task 5: hook 总线

**Files:**
- Create: `src-tauri/src/lifecycle/hooks.rs`
- Modify: `src-tauri/src/lifecycle/mod.rs`（`pub mod hooks;`）

**Interfaces:**
- Produces:
  - `struct TransitionCtx<'a> { pub note_id: Option<&'a str>, pub from: &'a machine::SessionState, pub to: &'a machine::SessionState }`
  - `trait LifecycleHook: Send + Sync { fn name(&self) -> &'static str; fn on_transition(&self, ctx: &TransitionCtx); }`
  - `struct HookBus`：`register(Box<dyn LifecycleHook>)`、`notify(&TransitionCtx)`（注册序、逐个 catch_unwind）。
  - 预留 `pub struct ExternalHookCfg { pub event: String, pub command: String }` + `HookBus::register_external(_cfg: ExternalHookCfg)`（本期 body 仅 `unimplemented` 注释说明下期实现——**不 panic**，记日志并忽略）。

- [ ] **Step 1: 写总线与测试**

创建 `src-tauri/src/lifecycle/hooks.rs`：

```rust
//! 迁移 hook 总线(通知类副作用的唯一挂点)。
//!
//! 契约:注册序执行;每个 hook 逐个 catch_unwind——任何 hook panic/失败只记
//! 日志,绝不影响主流程,也不影响后续 hook。保障类副作用(落盘/转码入队等
//! 语义契约)不走这里,走 Effect(见 machine.rs)。
//! 外部 hook(用户配置 shell/webhook)是未来在此注册的一个消费者,本期只留接口。

use super::machine::SessionState;

pub struct TransitionCtx<'a> {
    pub note_id: Option<&'a str>,
    pub from: &'a SessionState,
    pub to: &'a SessionState,
}

pub trait LifecycleHook: Send + Sync {
    fn name(&self) -> &'static str;
    fn on_transition(&self, ctx: &TransitionCtx);
}

#[derive(Default)]
pub struct HookBus {
    hooks: Vec<Box<dyn LifecycleHook>>,
}

/// 外部 hook 配置占位(下期实现执行体:shell 命令/webhook)。
#[allow(dead_code)]
pub struct ExternalHookCfg {
    pub event: String,
    pub command: String,
}

impl HookBus {
    pub fn register(&mut self, hook: Box<dyn LifecycleHook>) {
        self.hooks.push(hook);
    }

    /// 预留:外部 hook 注册入口。本期不实现执行体——记日志并忽略,不 panic。
    #[allow(dead_code)]
    pub fn register_external(&mut self, _cfg: ExternalHookCfg) {
        eprintln!("lifecycle: 外部 hook 尚未支持(接口预留),已忽略");
    }

    pub fn notify(&self, ctx: &TransitionCtx) {
        for h in &self.hooks {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                h.on_transition(ctx);
            }));
            if r.is_err() {
                eprintln!("lifecycle hook '{}' panic(已隔离,主流程不受影响)", h.name());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Recorder { order: Arc<std::sync::Mutex<Vec<&'static str>>>, tag: &'static str }
    impl LifecycleHook for Recorder {
        fn name(&self) -> &'static str { self.tag }
        fn on_transition(&self, _ctx: &TransitionCtx) {
            self.order.lock().unwrap().push(self.tag);
        }
    }

    struct Panicker;
    impl LifecycleHook for Panicker {
        fn name(&self) -> &'static str { "panicker" }
        fn on_transition(&self, _ctx: &TransitionCtx) { panic!("boom"); }
    }

    struct Counter(Arc<AtomicUsize>);
    impl LifecycleHook for Counter {
        fn name(&self) -> &'static str { "counter" }
        fn on_transition(&self, _ctx: &TransitionCtx) { self.0.fetch_add(1, Ordering::SeqCst); }
    }

    fn ctx_fixture<'a>(from: &'a SessionState, to: &'a SessionState) -> TransitionCtx<'a> {
        TransitionCtx { note_id: Some("n1"), from, to }
    }

    #[test]
    fn hooks_run_in_registration_order() {
        let order = Arc::new(std::sync::Mutex::new(vec![]));
        let mut bus = HookBus::default();
        bus.register(Box::new(Recorder { order: order.clone(), tag: "a" }));
        bus.register(Box::new(Recorder { order: order.clone(), tag: "b" }));
        let (f, t) = (SessionState::Idle, SessionState::Idle);
        bus.notify(&ctx_fixture(&f, &t));
        assert_eq!(*order.lock().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn panicking_hook_is_isolated_and_rest_still_run() {
        let n = Arc::new(AtomicUsize::new(0));
        let mut bus = HookBus::default();
        bus.register(Box::new(Panicker));
        bus.register(Box::new(Counter(n.clone())));
        let (f, t) = (SessionState::Idle, SessionState::Idle);
        bus.notify(&ctx_fixture(&f, &t));
        assert_eq!(n.load(Ordering::SeqCst), 1, "panic hook 之后的 hook 必须照常执行");
    }
}
```

`mod.rs` 加 `pub mod hooks;`。

- [ ] **Step 2: 跑测试**

Run: `cd src-tauri && cargo test lifecycle::hooks`
Expected: 2 测试 PASS（panic 测试的输出里允许出现 panic 打印，结果 PASS 即可）。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/lifecycle/
git commit -m "P1 hook 总线:注册序执行+catch_unwind 隔离,外部 hook 接口占位"
```

---

### Task 6: actor 线程与五命令改道（绞杀者）

**Files:**
- Create: `src-tauri/src/lifecycle/actor.rs`
- Modify: `src-tauri/src/lifecycle/mod.rs`
- Modify: `src-tauri/src/lib.rs`：
  - 命令壳 `start_recording:1170` / `stop_recording:1271` / `pause_recording:1334` / `unpause_recording:1362` / `resume_recording:1178` / `toggle_recording:1280` / `recording_status:1290`
  - `spawn_session` 成功点 :1118 与失败闭包 :640（发回报消息）
  - `do_stop_recording` 尾部 :1261 前（发 SessionEnded）
  - `run()` setup（起 actor 线程 + manage 句柄）
- Modify: `src-tauri/src/mcp/uds.rs`（AppBackend start/stop/pause/resume 4 处 `do_*` 调用改经 actor 句柄）
- Modify: `src-tauri/src/tray.rs:50,63`、`src-tauri/src/shortcuts.rs:13`（toggle/stop 改经句柄——`toggle_recording` 本身改道后这两处如仍调 `crate::toggle_recording` 则无需动，见 Step 3）

**Interfaces:**
- Consumes: Task 4 `machine::{handle, SessionState, Msg, Cmd, Effect}`；Task 5 `hooks::HookBus`。
- Produces:
  - `pub struct LifecycleHandle { tx: crossbeam_channel::Sender<Envelope> }`，`app.state::<LifecycleHandle>()` 全局可取；
  - `LifecycleHandle::command(&self, app: &AppHandle 不需要, cmd: Cmd) -> Result<(), String>`（阻塞等 reply）；
  - `LifecycleHandle::report(&self, msg: machine::Msg)`（工作线程回报，不等待）；
  - `pub fn spawn(app: AppHandle) -> LifecycleHandle`。

- [ ] **Step 1: 写 actor**

创建 `src-tauri/src/lifecycle/actor.rs`：

```rust
//! lifecycle actor:信箱 + 影子内核 + hook 总线 + 委托执行(P1 绞杀者)。
//!
//! P1 语义:命令消息在 actor 线程上被内核处理,产生 Delegate 效果后由 actor
//! 线程**同步调用既有 do_* 执行体**,其返回值即 reply——执行体、守卫、事件
//! 时序均与现状逐位一致,唯一变化是「所有命令经同一线程串行执行」(与今天
//! running/generation 锁串行等价或更强,可观测行为不变)。
//! 工作线程回报(SessionStarted/Failed/Ended)驱动影子内核演进;对账差异
//! 仅 eprintln,P2 翻转权威前它没有任何行为后果。

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
fn run_delegate(app: &AppHandle, cmd: &Cmd) -> Result<(), String> {
    match cmd {
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
                        Effect::Delegate(cmd) => result = run_delegate(&app, cmd),
                        Effect::ReplyErr(e) => result = Err(e.clone()),
                        Effect::ShadowMismatch(d) => {
                            eprintln!("lifecycle 影子对账: {d}");
                        }
                    }
                }
                if next != state {
                    let note_id = match &next {
                        SessionState::Recording { note_id, .. }
                        | SessionState::Stopping { note_id } => Some(note_id.as_str()),
                        _ => None,
                    };
                    bus.notify(&TransitionCtx { note_id, from: &state, to: &next });
                    state = next;
                }
                if let Some(r) = reply {
                    let _ = r.send(result);
                }
            }
        })
        .expect("lifecycle actor 线程创建失败");
    handle
}
```

`mod.rs` 变为：

```rust
//! 笔记生命周期 actor(P1:骨架+影子内核)。设计文档:
//! docs/superpowers/specs/2026-07-13-voice-notes-lifecycle-actor-design.md
pub mod actor;
pub mod hooks;
pub mod machine;

pub use actor::{spawn, LifecycleHandle};
pub use machine::Cmd;
```

- [ ] **Step 2: resume_recording 抽出 do_ 层**

`resume_recording`（lib.rs:1178-1210）现是命令直接实现。把函数体原样抽成 `fn do_resume_note_recording(app: &AppHandle, note_id: String) -> Result<(), String>`（**逐语句搬移，零改写**；原命令壳变薄壳调用它）——actor 的 `run_delegate` 需要这个入口。

- [ ] **Step 3: 命令改道 + 回报接线**

lib.rs `run()` 的 setup 里（`let handle = app.handle().clone();` 之后）：

```rust
            app.manage(lifecycle::spawn(handle.clone()));
```

五个命令壳改为经句柄（以 start 为例，其余同型）：

```rust
#[tauri::command]
fn start_recording(app: AppHandle) -> Result<(), String> {
    app.state::<lifecycle::LifecycleHandle>()
        .command(lifecycle::Cmd::Start { resume_id: None })
}
```

- `stop_recording` → `Cmd::Stop`；`pause_recording` → `Cmd::Pause`；`unpause_recording` → `Cmd::Unpause`；`resume_recording` → `Cmd::Start { resume_id: Some(note_id) }`。
- `toggle_recording`（lib.rs:1280-1287）：内部的 `do_start_recording`/`do_stop_recording` 调用改为经句柄发 `Cmd::Start{None}`/`Cmd::Stop`（快捷键/托盘调它，自动一起改道）。
- `recording_status`（:1290-1308）**保持直读 session slot 不动**——P1 内核非权威，经信箱回答只会引入无意义排队；spec 的 Query 改道在 P2 权威翻转时做（此偏离已在计划头声明，控制器知情）。
- `mcp/uds.rs` AppBackend 四处（:184 start / :214 stop / :219 pause / :224 resume）改为 `app.state::<crate::lifecycle::LifecycleHandle>().command(...)`，其余轮询/回读逻辑不动。
- `tray.rs:63` 的 `do_stop_recording` 改经句柄 `Cmd::Stop`；`tray.rs:50` 与 `shortcuts.rs:13` 调的是 `toggle_recording`，已随之改道，无需动。

回报接线三处：

```rust
// lib.rs spawn_session 成功点(:1118 emit "recording" 之后):
            app.state::<lifecycle::LifecycleHandle>()
                .report(lifecycle::machine::Msg::SessionStarted { note_id: note_id_for_report });
// lib.rs spawn_session 失败闭包(:640 emit 之后):
            app.state::<lifecycle::LifecycleHandle>().report(lifecycle::machine::Msg::SessionFailed);
// lib.rs do_stop_recording(:1261 emit "stopped" 之前、拿到 note_id 的分支内):
        app.state::<lifecycle::LifecycleHandle>()
            .report(lifecycle::machine::Msg::SessionEnded { note_id: note_id.clone() });
```

注意克隆变量所有权（`note_id_for_report` 在入槽块前克隆）。**死锁自检**：`do_stop_recording` 在 actor 线程上执行时发 `report`（send 到自己信箱，unbounded 不阻塞）——安全；`spawn_session` 的回报来自后台线程——安全。

- [ ] **Step 4: 全量测试 + 构建**

Run: `cd src-tauri && cargo test && cargo build 2>&1 | grep -c "^warning"`
Expected: **397+ 全绿零修改**；警告不增。若 uds/session 相关测试因函数搬移编译失败——检查搬移是否逐语句等价，禁止顺手改逻辑。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lifecycle/ src-tauri/src/lib.rs src-tauri/src/mcp/uds.rs src-tauri/src/tray.rs
git commit -m "P1 actor 改道:五命令+toggle+UDS 控制面经信箱串行,影子内核对账,行为委托不变"
```

---

### Task 7: P0+P1 对账收尾与真机冒烟

**Files:**
- Modify: `.superpowers/sdd/progress.md`（记账）
- 无代码改动预期；如对账发现问题回上游任务修

- [ ] **Step 1: 静态对账**

- `git diff master --stat`：确认改动面 = 计划列出的文件集合，无意外文件。
- `grep -rn "do_start_recording\|do_stop_recording\|do_pause_recording\|do_resume_recording" src-tauri/src/ | grep -v "lifecycle/actor.rs\|fn do_"`：除 actor 的映射表外不应再有直接调用点残留（toggle 改道后）。

- [ ] **Step 2: 真机冒烟（需 GUI，若用户正在录制则等待）**

`npm run tauri dev` 逐项验证并记录：
1. 开录 → 状态「录制中」、计时走；暂停/恢复；停止 → 笔记 complete、自动精修触发；
2. 续录一条旧笔记 → 时间轴接续；
3. 录制中在**另一终端**尝试 `notes` CLI 编辑该笔记（或直接第二实例）→ 明确拒绝文案；
4. MCP 控制面：`recording_status`→start→pause→resume→stop 全链路；
5. stderr.log 无「影子对账」输出（有则逐条分析：要么内核表错，要么回报接线漏/重）。

- [ ] **Step 3: 记账 Commit**

```bash
git add -A && git commit -m "P0+P1 收尾:对账与冒烟记录"
```
（如冒烟改动了代码，先回对应任务的流程走测试再提交。）
