# 说话人上下文推断 P3(macOS EventKit 日历)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

rev2:消化 Codex 审查(18 P1 + 14 P2)。骨架修正:① `EKEventStore` 是 `!Send+!Sync` → **专用串行 calendar worker 线程**持有 store,channel 收发纯 Rust 数据,所有 IPC/挂钩经它(天然并发门);② **日历匹配移进 `spawn_refine` 线程内、identify 之前同步执行**(消除与 identify 的竞速,identify 前重读 meta);③ meta 读改写收进 `NoteStore::update_calendar`(EDIT_LOCK+NoteLock,持锁条件复查);④ 清除加 tombstone(`calendar_cleared`),backfill/自动匹配不覆盖用户决定;⑤ 隐私文案如实(启用 AI 精修时标题/人名会发给所选 provider);⑥ 快照序列化不省略字段(TS 必填对齐);⑦ Person.emails 计入合并并集/journal 失效/空档判定;⑧ 授权态用 enum(含 macOS 14 write-only);删 CalendarSource trait 声明与 calendar_auth_needed 事件(权限态直接查询)。

**Goal:** 录制停止后按时间窗匹配日历事件,标题+参会人落盘即快照;参会人作 identify 闭集先验;设置页开关(默认开)+ 授权说明卡;详情页展示/改选/清除;`Person.emails` 同名区分。Windows 同形桩。

**Spec:** spec rev2「日历集成设计(P3)」节;**基线:** 分支 `feat/speaker-context-p3` 叠在 PR #80 上。

## Global Constraints

- 日历任何失败绝不影响录制/精修:best-effort,只留日志。授权只能由用户动作发起(设置页说明卡);自动路径未授权即静默返回。
- EventKit 规则:store 只活在 calendar worker 线程;worker 内 `objc2::rc::autoreleasepool` 包裹每次请求;所有可空返回(eventIdentifier/title/attendees/name)显式处理,无 identifier 的事件跳过;**按 objc2-event-kit 0.3.2 生成签名实现,禁止 msg_send 猜 selector**。
- 匹配规则:排除全天;**重叠时长(ms)最大**者胜;平手(差 <1000ms)不自动绑定;`ended_at` 空用最后一段 end_ms 兜底;attendees 过滤 declined 与 room/resource,快照上限 100 人。
- meta 写入唯一入口 `NoteStore::update_calendar`(锁内读-改-写-原子落盘;auto 路径持锁复查「仍无快照且未被清除」)。
- 隐私文案(plist/说明卡统一):「日程数据保存在本机;若启用 AI 精修,会议标题与参会人名会随转写一起发送给你选择的 AI 服务。」
- serde:`CalendarSnapshot`/`CalendarAttendee` **全字段固定序列化**(不 skip,前端必填类型成立);`NoteMeta.calendar`/`calendar_cleared` 用 default;Settings 默认 true 走 `default_true` + 手写 Default 补行。
- 前端 i18n 双语;新 IPC 过两处 generate_handler 源码解析测试;不跑全量 cargo fmt。

---

### Task 1: 依赖 + plist + entitlement + 校验脚本

- [ ] Cargo.toml macOS 段:`objc2 = "0.6"`、`objc2-foundation = "0.3"`、`block2 = "0.6"`、`objc2-event-kit = { version = "0.3", features = ["EKEventStore", "EKEvent", "EKCalendarItem", "EKCalendar", "EKObject", "EKParticipant", "EKTypes", "block2"] }`(features 以 0.3.2 文档为准,编译报缺再补);`cargo tree -i objc2 | head` 确认单版本。
- [ ] Info.plist 两键(文案用上方隐私口径);Entitlements.plist 加 `com.apple.security.personal-information.calendars`。
- [ ] `check_macos_entitlements.py` 改表驱动(entitlements 三键 + Info.plist 三键存在且非空);打包后校验超范围,PR 注明。
- [ ] 提交。

### Task 2: CalendarSnapshot + NoteMeta 字段 + tombstone

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarSnapshot {
    pub event_id: String,
    pub title: String,
    #[serde(default)]
    pub attendees: Vec<CalendarAttendee>,   // 固定序列化(空数组也写)
    pub matched_at: String,
    #[serde(default)]
    pub match_kind: String,                 // "auto" | "manual"
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarAttendee {
    #[serde(default)] pub name: String,
    #[serde(default)] pub email: String,    // 固定序列化;规范化:trim+小写,mailto 大小写不敏感剥离+percent-decode
    #[serde(default)] pub is_me: bool,
}
```

- [ ] `NoteMeta` 加两字段:`calendar: Option<CalendarSnapshot>`(default/skip_none)+ `#[serde(default)] pub calendar_cleared: bool`(用户明确清除的 tombstone:auto/backfill 永不再绑,手动改选可推翻并复位)。字面量修复(`notes.rs:485`、`writer.rs:135`、`disk.rs:106`、`export.rs` 四处,以编译错误为准)。
- [ ] `NoteStore::update_calendar(&self, id, f: impl FnOnce(&mut NoteMeta) -> bool) -> anyhow::Result<bool>`:EDIT_LOCK+NoteLock 内 read_meta→f→true 才 write_meta_atomic(仿 `rename` 的锁纪律);f 返回 false = 未修改。单测:旧 meta 兼容、往返保真、update_calendar 条件写、tmp 无残留。
- [ ] 前端 `src/lib/notes.ts`:`NoteMeta` 加 `calendar?: CalendarSnapshot | null; calendar_cleared?: boolean;`,导出 `CalendarSnapshot`/`CalendarAttendee` 类型(字段全必填)。
- [ ] 提交。

### Task 3: settings 开关

- [ ] `#[serde(default = "default_true")] pub calendar_match_enabled: bool` + Default 补行 + 旧文件缺键→true 单测。提交。

### Task 4: calendar worker(EventKit 实现 + 桩 + 纯匹配)

**Files:** `src-tauri/src/calendar.rs`(macOS)/ `calendar_stub.rs`(其它平台,`#[path]` 顶替);lib.rs 模块声明。

**对外接口(两侧同形,全部经 worker channel,调用方任意线程)**:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission { Full, WriteOnly, Denied, NotDetermined, Unavailable }
// WriteOnly(macOS 14 只写授权)读不了事件但≠用户拒读,前端文案单列"权限不足,去系统设置改为完全访问"。

#[derive(Debug, Clone)]
pub struct EventInfo { pub event_id: String, pub title: String, pub start_ms: i64, pub end_ms: i64, pub all_day: bool, pub attendees: Vec<crate::store::CalendarAttendee> }

pub fn permission_status() -> Permission;                       // 快速查询(worker 内取,同步等)
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthOutcome { Granted, Denied, Insufficient, Error, Timeout }
pub fn request_permission() -> AuthOutcome;                     // 用户动作触发;60s 超时;NSError→Error
pub fn events_between(start_ms: i64, end_ms: i64) -> anyhow::Result<Vec<EventInfo>>;

pub fn best_match(events: &[EventInfo], start_ms: i64, end_ms: i64) -> Option<&EventInfo>;  // 纯函数
```

- [ ] **Step 1(TDD)**:`best_match` 单测:全天排除/重叠时长最大胜/平手(<1s)None/零重叠 None。纯函数与 `EventInfo` 放平台无关位置(两文件顶部 `include!("calendar_common.rs")` 或第三个共享文件 `calendar_common.rs` 由两侧 `include!`——选共享文件,测试在 stub 平台也跑)。
- [ ] **Step 2:worker**(macOS):`std::mpsc::Sender<Req>` 静态 OnceLock;首次使用起 `calendar-worker` 线程,线程内建 `EKEventStore` 并循环处理 `Req::{Status, Request, Events{start,end}}`(reply oneshot channel);每单 `autoreleasepool`;版本分叉:`NSProcessInfo` `isOperatingSystemAtLeastVersion(14)` → `requestFullAccessToEventsWithCompletion`,否则 `requestAccessToEntityType_completion`;completion 经 `block2::RcBlock` + mpsc 回传 (granted, err_desc)。可空处理:`eventIdentifier()?` 无则跳过;attendees 过滤 `participantType` 为 room/resource 与 `participantStatus == declined`(feature 缺失时保守保留并注释);email 从 `URL().absoluteString()` 规范化。
- [ ] **Step 3**:stub 恒 `Unavailable`/`Error`/`Ok(vec![])`。`cargo check --lib` + 纯函数测试绿;提交。

### Task 5: 匹配落盘 + spawn_refine 前置挂钩 + IPC

- [ ] **落盘函数**(lib.rs):

```rust
/// 自动匹配(停止后/backfill 共用):开关+授权满足才查;持锁复查「无快照且未清除」
/// 才写(查询期间用户手动改选/清除则放弃)。返回是否写入。
fn match_and_store_calendar(app: &AppHandle, note_id: &str) -> anyhow::Result<bool> {
    let s = ...settings::load...;
    if !s.calendar_match_enabled { return Ok(false); }
    if calendar::permission_status() != calendar::Permission::Full { return Ok(false); }
    let root = notes_dir(app)?;
    let note = store::NoteStore::new(root.clone()).load(note_id)?;
    if note.meta.calendar.is_some() || note.meta.calendar_cleared { return Ok(false); }
    let (start_ms, end_ms) = note_window_ms(&note.meta, &note.segments);   // 纯函数,ended_at 兜底
    let events = calendar::events_between(start_ms - 60_000, end_ms + 60_000)?;
    let Some(ev) = calendar::best_match(&events, start_ms, end_ms) else { return Ok(false) };
    let snap = snapshot_of(ev, "auto");                                   // attendees 截 100
    store::NoteStore::new(root).update_calendar(note_id, |meta| {
        if meta.calendar.is_some() || meta.calendar_cleared { return false; } // 持锁复查
        meta.calendar = Some(snap.clone());
        true
    })
}
```

- [ ] **挂钩位置 = `spawn_refine` 线程内**(lib.rs,`run_local` 之后、identify 之前;该线程本就是停止后后台线程,EventKit 查询经 worker 不碰 actor):

```rust
// 日历匹配先于 identify:参会人闭集先验要进 ctx。失败不阻塞。
let calendar_snap = match match_and_store_calendar(&app, &note_id) {
    Ok(_) => store::NoteStore::new(notes_dir(&app)?.clone()).load(&note_id).ok().and_then(|n| n.meta.calendar),
    Err(e) => { eprintln!("calendar({note_id}): {e}"); None }
};
// …identify 挂钩处把 calendar_snap.as_ref() 传给 run_identify(Task 6 改签名)
```

  (identify_note 手动命令同样先读 meta.calendar 传入。)不再改 actor.rs;refine 完全关闭的用户,identify 也不会跑,日历匹配仍应发生 → `spawn_refine` 在 run_local 后必经此段,与 refine 开关无关,满足。
- [ ] **IPC**(全部 async + spawn_blocking,经 worker 不阻塞 IPC 线程):
  - `calendar_permission() -> String`(Permission serde 名);
  - `request_calendar_permission() -> String`(AuthOutcome;Granted 后后台 backfill 最近 30 天 best-effort);
  - `list_calendar_candidates(id) -> Vec<ipc::CalendarCandidate{event_id,title,start_ms,end_ms,attendee_n,overlap_ms}>`:窗口 = 笔记当天 00:00 前 2h 至次日 00:00 后 2h,非全天,按 overlap_ms 降序(0 也列出——延迟开录场景);
  - `set_note_calendar_event(id, event_id: Option<String>)`:守卫 validate+录制中拒绝;Some→候选窗重取快照 `match_kind="manual"`、复位 `calendar_cleared=false`;None→`calendar=None; calendar_cleared=true`;均经 `update_calendar`;
  - `backfill_calendar_matches() -> u32`:**一次**拉取(最早无快照笔记 start .. 最晚 end)事件,内存逐笔记 best_match,`update_calendar` 计数;静态 `CALENDAR_BACKFILL_GATE: Mutex<()>` 防并发重入。
- [ ] `note_window_ms` 单测两条;注册命令;提交。

### Task 6: identify 接入 + Person.emails

- [ ] `Person.emails: Vec<String>`(default/skip_empty 可以——后端自读自写,前端不消费);连锁:**合并路径 emails 并集**(`merge_journaled`/`do_merge_person` 的字段手工合并处,以 grep `winner_person`/合并实现为准)、`delete_person_if_empty` 计入 emails 非空即拒删、`add_person_email(id, email)`(VP_LOCK、规范化、去重、`journal_invalidate(该人, "此人档案有更新")`)。单测:并集、拒删、去重。
- [ ] `IdentifyContext.calendar: Option<CalendarContext{title, attendees: Vec<String>}>`(is_me 标注成 `名字(我)`;**上限 30 人**,超出截断并在字段里注明 `"(其余 N 人略)"` 附加项);`build_context`/`run_identify` 加参;候选第④路:attendee email 命中 `Person.emails` 或 attendee 名精确等于库中人名 → 候选(**插在最前**,超 cap 时后路被挤,顺序稳定:email 命中→名字命中→原三路);SYSTEM_PROMPT 补 calendar 释义句(闭集先验非硬约束)。
- [ ] `apply_identify_suggestion` 成功后:目标人名与某 attendee 名精确相等 **且该名在参会人中唯一 且该名在库中唯一** 且 email 非空 → `add_person_email`(best-effort;三重唯一性防同名污染,残余风险=确认本身指错人,注释注明)。
- [ ] 单测:第④路两种命中与排序、cap 截断、calendar 序列化进 ctx、apply 写 email 的唯一性防线。
- [ ] 提交。

### Task 7: 设置页开关 + 授权说明卡

- [ ] 绑定 `calendarPermission`/`requestCalendarPermission`(`$lib/notes.ts` 就近);设置页:开关行(仿 keep_audio)+ 状态副区:开且 `not_determined` → 说明卡(隐私口径文案 + 「继续」→ request → 刷新态;`denied`/`insufficient` → 去系统设置提示;`unavailable`(Windows)→ 整行隐藏)。开关切换绝不直接拉系统弹窗。
- [ ] i18n zh/en:`settings.calendar.{label,desc,cardTitle,cardBody,cardContinue,denied,insufficient}`。
- [ ] `npm run check` + vitest;提交。

### Task 8: 详情页日历行

- [ ] `notes.ts`:`listCalendarCandidates`/`setNoteCalendarEvent` 绑定。
- [ ] `.header-main` 的 `.meta` 后加行:有快照 → 📅 标题+人数,展开名单(is_me 标「我」),「改选」下拉(候选含时间与 overlap_ms 显示)/「清除」;无快照且开且 `full` → 「关联日程…」;`not_determined`/`denied` 且开 → 灰字去设置提示;`unavailable`/开关关 → 不渲染。
- [ ] i18n `notes.calendar.*`;`npm run check` + vitest;提交。

---

## 收尾核对

- [ ] `cargo test --lib --bins` + mcp_stdio + `npm run check` + `npm test` 全绿;entitlements 脚本过;`cargo tree -i objc2` 单版本
- [ ] 真机冒烟(PR 描述):① 说明卡→系统授权;② 覆盖日历事件的录音停止后详情页出现事件行且 identify prompt 含 calendar(ailog);③ 改选/清除持久且 backfill 不复活已清除;④ 拒权/只写权限文案正确且录制精修无恙;⑤ 确认建议后 voiceprints.json 出现 emails;⑥ Windows 构建(桩)通过
- [ ] PR 注明:打包后 plist 校验超范围;participantType/Status 过滤若 feature 不可用的降级;授权引导两处(设置页+详情页),无全局弹窗
