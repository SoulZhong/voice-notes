# 说话人上下文推断 P3(macOS EventKit 日历)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 spec P3:录制停止后按时间窗匹配系统日历事件,标题+参会人**落盘即快照**进 `NoteMeta.calendar`;参会人作为 identify 的闭集先验候选;设置页开关(默认开)+ 授权前应用内说明卡;详情页展示/改选/清除;`Person.emails` 用于同名区分。Windows 留同形桩。

**Architecture:** 新模块 `src-tauri/src/calendar.rs`(macOS,objc2-event-kit)+ `calendar_stub.rs`(非 macOS 同形桩,`#[path]` 顶替——仿 `audio/aec` 形态)。EventKit 访问隔离在 `CalendarSource` trait 后面,时间窗匹配是纯函数(单测)。停止挂钩在 `actor.rs` DoFinalize 的 `spawn_refine` 旁起后台线程;授权流程绝不从后台线程发起——未授权时只发 `calendar_auth_needed` 事件,授权入口在设置页说明卡。identify 侧:`IdentifyContext` 增 `calendar` 字段,参会人成为候选第④路。

**Tech Stack:** `objc2-event-kit 0.3` + `objc2 0.6` + `objc2-foundation 0.3` + `block2 0.6`(全家已锁在 Cargo.lock 传递依赖,版本必须对齐避免双份 objc2);chrono。

**Spec:** `docs/superpowers/specs/2026-08-08-speaker-context-inference-design.md`(rev2「日历集成设计(P3)」节)
**基线:** 分支 `feat/speaker-context-p3` 叠在 `feat/speaker-context-p2a`(PR #80)上。

## Global Constraints

- 日历任何失败(无权限/EventKit 异常/解码)绝不影响录制与精修:后台线程 best-effort,只留日志。
- **授权只能由用户动作发起**(设置页说明卡「继续」按钮);停止挂钩发现未授权只 emit `calendar_auth_needed`,绝不拉系统弹窗。
- macOS 13 兼容:14+ 用 `requestFullAccessToEvents`,13 回退 `requestAccessToEntityType`;运行期用 `respondsToSelector` 分叉,不能只靠编译期。
- 落盘即快照:`NoteMeta.calendar` 存 title/attendees 副本,**不依赖 event_id 活性**;event_id 仅供改选时重新定位。
- 匹配边界(spec):排除全天事件;`ended_at` 空用最后一段时间戳兜底;重叠比例最高者胜;**并列平手不自动绑定**;时区取系统本地(EventKit 返回绝对时间)。
- 新 serde 字段一律 `#[serde(default, skip_serializing_if = ...)]`;`Settings` 默认 true 的 bool **必须** `#[serde(default = "default_true")]`(settings.rs:187 警示注释)且手写 `impl Default` 同步补行。
- 与 spec 的收窄:授权引导入口 = 设置页说明卡 + 详情页未授权提示行,**不做全局弹窗**(默认开语义 = 授权后自动匹配,授权前静默等待引导);`Person.emails` 的记录时机 = identify 建议确认时参会人名精确匹配(手动关联路径后续再接)。
- 前端文案全走 i18n(zh/en 双写);invoke 一律经 `$lib/notes`/`$lib/people` 薄封装。
- 不跑全量 `cargo fmt`;新增 IPC 留意 lib.rs 两处 generate_handler 源码解析测试;每任务测试绿后提交。

---

### Task 1: 依赖 + plist + entitlement + 校验脚本

**Files:** `src-tauri/Cargo.toml`(macOS 段 :97-101)、`src-tauri/Info.plist`、`src-tauri/Entitlements.plist`、`scripts/check_macos_entitlements.py`

- [ ] Cargo.toml macOS 段加(feature 名以 crate 0.3 文档为准,起点:`EKEventStore`/`EKEvent`/`EKParticipant`/`block2`):

```toml
objc2 = "0.6"
objc2-foundation = "0.3"
block2 = "0.6"
objc2-event-kit = { version = "0.3", features = ["EKEventStore", "EKEvent", "EKCalendarItem", "EKParticipant", "EKTypes", "block2"] }
```

  `cargo tree -p objc2 2>/dev/null | head -1` 确认仍是单版本 0.6.x。
- [ ] Info.plist 加两键(与麦克风键同级):`NSCalendarsUsageDescription` / `NSCalendarsFullAccessUsageDescription`,文案:「读取日程标题与参会人,用于把录音自动关联到会议并帮助认出说话人;日程数据只在本机使用。」
- [ ] Entitlements.plist 加 `com.apple.security.personal-information.calendars` = true。
- [ ] 校验脚本改表驱动:`REQUIRED_ENTITLEMENTS = {"com.apple.security.cs.disable-library-validation": True, "com.apple.security.device.audio-input": True, "com.apple.security.personal-information.calendars": True}` 循环校验;新增 Info.plist 校验(`NSMicrophoneUsageDescription` + 两个日历键存在且非空)。`python3 scripts/check_macos_entitlements.py` 通过。
- [ ] 提交 `build(calendar): EventKit 依赖与授权声明(objc2 单版本对齐)`。

---

### Task 2: CalendarSnapshot + NoteMeta.calendar

**Files:** `src-tauri/src/store/mod.rs`(:50-60 NoteMeta、:135 write_meta_atomic 旁)、字面量修复(`notes.rs:485`、`writer.rs:135`、`disk.rs:106`、`export.rs:361/418/439/460`,以编译错误为准)、`src/lib/notes.ts:15`(前端镜像)

```rust
/// 日历事件快照(P3):落盘即快照——title/attendees 是匹配时刻的副本,不依赖
/// event_id 活性;event_id 仅供改选时重新定位,事件被改/删后快照仍自洽。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarSnapshot {
    pub event_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<CalendarAttendee>,
    pub matched_at: String, // RFC3339;区分"自动匹配"与"手动改选"不落盘,回执由 matched_at 变化体现
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarAttendee {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
    #[serde(default)]
    pub is_me: bool,
}
```

- [ ] `NoteMeta` 加 `#[serde(default, skip_serializing_if = "Option::is_none")] pub calendar: Option<CalendarSnapshot>,`;`cargo check` 列字面量补 `calendar: None`。
- [ ] 单测(store/mod.rs 或 notes.rs tests):旧 meta.json(无 calendar 键)反序列化 → None;带 calendar 往返保真。
- [ ] `src/lib/notes.ts` `NoteMeta` 类型加 `calendar?: { event_id: string; title: string; attendees: { name: string; email: string; is_me: boolean }[]; matched_at: string } | null;`(导出 `CalendarSnapshot` 类型)。
- [ ] 提交 `feat(store): NoteMeta.calendar 快照字段(serde default 向后兼容)`。

---

### Task 3: settings 开关

**Files:** `src-tauri/src/settings.rs`(字段 + Default + 单测)

- [ ] `#[serde(default = "default_true")] pub calendar_match_enabled: bool,`;`impl Default` 补 `calendar_match_enabled: true,`;单测:旧 settings.json(无该键)→ true(仿现有 default_true 字段测试)。
- [ ] 提交 `feat(settings): calendar_match_enabled(默认开)`。

---

### Task 4: calendar 模块(EventKit 实现 + 桩 + 纯匹配逻辑)

**Files:**
- Create: `src-tauri/src/calendar.rs`(macOS)
- Create: `src-tauri/src/calendar_stub.rs`(非 macOS 同形桩:权限恒 "unavailable"、events 恒空、request 恒 false)
- Modify: `src-tauri/src/lib.rs` 模块声明区:

```rust
#[cfg(target_os = "macos")]
mod calendar;
#[cfg(not(target_os = "macos"))]
#[path = "calendar_stub.rs"]
mod calendar;
```

**共享接口(两侧同形;纯逻辑放 macOS 文件里但不依赖 EventKit 类型,stub 直接 `pub use` 或复制小函数——实现时取更省的)**:

```rust
/// 与 EventKit 解耦的事件视图:匹配逻辑只认它,可单测。
#[derive(Debug, Clone)]
pub struct EventInfo {
    pub event_id: String,
    pub title: String,
    pub start_ms: i64,   // unix ms
    pub end_ms: i64,
    pub all_day: bool,
    pub attendees: Vec<crate::store::CalendarAttendee>,
}

/// 授权态:"full" | "denied" | "not_determined" | "unavailable"(非 macOS 恒此值)。
pub fn permission_status() -> &'static str;
/// 发起系统授权(必须由用户动作触发;内部按 macOS 版本分叉 14+/13 API)。
/// 阻塞等 completion(调用方已在 spawn_blocking),返回是否授权。
pub fn request_permission() -> bool;
/// 时间窗内事件(已授权前提;EventKit predicate 查询,失败返回 Err 只记日志)。
pub fn events_between(start_ms: i64, end_ms: i64) -> anyhow::Result<Vec<EventInfo>>;

/// 纯匹配:排除全天;按与 [start,end) 的重叠时长取最大;并列平手(重叠差 <1s)
/// 返回 None(不自动绑定,候选留给用户改选)。零重叠返回 None。
pub fn best_match(events: &[EventInfo], start_ms: i64, end_ms: i64) -> Option<&EventInfo>;
```

- [ ] **Step 1: 纯函数先行(TDD)**:`best_match` 单测四条——全天排除、重叠最长胜、平手 None、零重叠 None。放在两侧共享的位置(macOS 文件 + stub `include!` 或直接双份小函数,取编译最省方案;测试跑在宿主平台即可)。
- [ ] **Step 2: EventKit 实现**(macOS)。要点:
  - `EKEventStore::new()` 进程内单例(`OnceLock`);
  - `permission_status`:`EKEventStore::authorizationStatusForEntityType(EKEntityType::Event)` 映射(14+ 的 `FullAccess` 与 13 的 `Authorized` 都算 "full");
  - `request_permission`:`respondsToSelector(sel!(requestFullAccessToEventsWithCompletion:))` 分叉 14+/13 API,`block2::RcBlock` + `std::sync::mpsc` 等 completion(60s 超时按 false);
  - `events_between`:`NSDate::dateWithTimeIntervalSince1970` 造窗 → `predicateForEventsWithStartDate_endDate_calendars(None)` → `eventsMatchingPredicate`;逐事件取 `eventIdentifier`/`title`/`startDate`/`endDate`/`isAllDay`/`attendees`(`EKParticipant`:`name`、`URL`(strip `mailto:` 得 email)、`isCurrentUser`→is_me);API 名以 objc2-event-kit 0.3 生成绑定为准,msg_send 兜底;
  - 全部 `unsafe` 收在本文件,对外安全接口。
- [ ] **Step 3**:`cargo check --lib`(macOS 宿主)+ 纯函数测试绿;提交 `feat(calendar): EventKit 访问层与纯匹配逻辑(非 macOS 同形桩)`。

---

### Task 5: 停止挂钩 + 匹配/改选/回填 IPC

**Files:** `src-tauri/src/lib.rs`(挂钩 + 5 条命令 + 注册)、`src-tauri/src/lifecycle/actor.rs:511` 旁(一行调用)

- [ ] **匹配落盘函数**(lib.rs,后台线程用;read-modify-write 走 `read_meta`+`write_meta_atomic`,在 `drop(o)` 之后运行,无锁冲突):

```rust
/// 停止后日历匹配(best-effort):已授权且开关开才查;未授权发 calendar_auth_needed
/// 事件(一次授权入口在设置页说明卡)。ended_at 空用最后一段时间戳兜底。
fn spawn_calendar_match(app: &AppHandle, note_id: String) {
    let app = app.clone();
    std::thread::spawn(move || {
        let run = || -> anyhow::Result<()> {
            let s = app.path().app_data_dir().map(|d| settings::load(&d)).map_err(|e| anyhow::anyhow!("{e}"))?;
            if !s.calendar_match_enabled { return Ok(()); }
            match calendar::permission_status() {
                "full" => {}
                "not_determined" => { let _ = app.emit("calendar_auth_needed", &note_id); return Ok(()); }
                _ => return Ok(()), // denied/unavailable:静默
            }
            match_and_store_calendar(&app, &note_id)
        };
        if let Err(e) = run() { eprintln!("calendar({note_id}): 匹配失败(不影响笔记): {e}"); }
    });
}

/// 读 meta(+段兜底)→ events_between → best_match → 写快照。已有 calendar 且非
/// force 时跳过(不覆盖手动改选)。返回是否写入。
fn match_and_store_calendar(app: &AppHandle, note_id: &str) -> anyhow::Result<()>;
```

  时间换算:`chrono::DateTime::parse_from_rfc3339(started_at).timestamp_millis()`;`ended_at` 空 → `started_at + note.segments 最末 end_ms`。
- [ ] **挂钩**:`actor.rs` DoFinalize 分支 `crate::do_stop_tail(&app, note_id)` 之后加 `crate::spawn_calendar_match(&app, finalize 成功的 o.note_id)`(与 spawn_refine 同款用槽内 note_id;finalize 失败不挂)。
- [ ] **IPC 五条**(注册进 generate_handler,过两处源码解析测试):

```rust
#[tauri::command] fn calendar_permission() -> String;                      // 函数体内薄转 calendar::permission_status
#[tauri::command] async fn request_calendar_permission(app: AppHandle) -> Result<bool, String>;
//   spawn_blocking 内 calendar::request_permission();授权成功后对无 calendar 的近期笔记
//   触发一次 backfill(最近 30 天,best-effort 后台)。
#[tauri::command] fn list_calendar_candidates(app: AppHandle, id: String) -> Result<Vec<ipc::CalendarCandidate>, String>;
//   {event_id,title,start_ms,end_ms,attendee_n}:笔记时间窗 ±2h 的非全天事件,按重叠降序。
#[tauri::command] fn set_note_calendar_event(app: AppHandle, id: String, event_id: Option<String>) -> Result<(), String>;
//   Some=按 event_id 从候选窗重取该事件快照写入(matched_at=now);None=清除字段。
//   守卫:validate_note_id + 录制中拒绝(meta 由 writer 独占)。
#[tauri::command] async fn backfill_calendar_matches(app: AppHandle) -> Result<u32, String>;
//   spawn_blocking:全部无 calendar 的完成态笔记逐一 match_and_store,返回写入数。
```

  `ipc::CalendarCandidate` 新类型(Serialize)。
- [ ] 纯逻辑单测:`note_window_ms(meta, segments) -> (i64, i64)`(ended_at 兜底)抽纯函数测两条。
- [ ] 提交 `feat(calendar): 停止挂钩、匹配落盘与改选/回填命令`。

---

### Task 6: identify 接入 + Person.emails

**Files:** `src-tauri/src/refine/identify.rs`、`src-tauri/src/store/voiceprints.rs`、`src-tauri/src/lib.rs`(两处 run_identify 调用点 + apply)

- [ ] `Person` 加 `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub emails: Vec<String>,`(构造点补字段;schema_version 数值不动——仓库该字段无读者,bump 纯声明,如实注释)。`VoiceprintStore::add_person_email(id, email)`(VP_LOCK 内去重追加)+ 单测。
- [ ] `IdentifyContext` 加 `#[serde(skip_serializing_if = "Option::is_none")] pub calendar: Option<CalendarContext>`:

```rust
#[derive(Debug, Serialize)]
pub struct CalendarContext {
    pub title: String,
    pub attendees: Vec<String>, // 名字列表;is_me 的标注成 "名字(我)"
}
```

  `build_context` 加参 `calendar: Option<&crate::store::CalendarSnapshot>`;候选召回加第④路:参会人 email 精确命中 `Person.emails` 的人、或参会人名精确等于库中人名的人(闭集先验,排履历最前);prompt 无需改(ctx 序列化自带),SYSTEM_PROMPT 补一句「calendar 为当场会议的日历事件(标题与参会人名单):参会人是强先验候选,但允许临时加入者/代参会,不是硬约束」。
- [ ] 两处 run_identify 调用点(spawn_refine 挂钩 + identify_note)读 `NoteMeta.calendar` 传入(`store::NoteStore::load` 的 meta 或 read_meta)。
- [ ] `apply_identify_suggestion` 成功路径补:目标人名精确等于某参会人名且 attendee.email 非空 → `add_person_email`(best-effort,失败只日志)——下一场同人 email 精确匹配,不再靠模糊猜。
- [ ] 单测:candidates 第④路(email 命中 + 名字命中 + is_me 标注);calendar 字段序列化进 ctx。
- [ ] 提交 `feat(identify): 日历参会人闭集先验与 Person.emails`。

---

### Task 7: 设置页开关 + 授权说明卡

**Files:** `src/routes/settings/+page.svelte`(sync :213 / saveSetting :376 / 模板 :811 附近)、`src/lib/models.ts` 或 `src/lib/notes.ts`(命令绑定,就近)、`src/lib/i18n/dict/settings.ts`

- [ ] 绑定:`calendarPermission()` / `requestCalendarPermission()`;本地镜像 `calendarMatch` + `calPerm`(挂载时查询)。
- [ ] 模板(录音区块内):开关行(仿 keep_audio)+ 权限态副行:
  - 开关开且 `calPerm === "not_determined"` → 显示**说明卡**(卡片内文案讲清为什么+只在本机,按钮「继续」→ `requestCalendarPermission()`;授权成功刷新 calPerm,失败提示去系统设置);
  - `calPerm === "denied"` → 提示行「日历权限被拒,去系统设置开启」;
  - 开关关闭 → 说明卡/提示都不显示。仿 `toggleShortcutEnabled` 形态:开关本身照常 saveSetting,授权副作用只由说明卡按钮触发(**开关切换绝不直接拉系统弹窗**)。
- [ ] layout 监听 `calendar_auth_needed` → 置一个轻量提示(复用现有 toast/banner 机制,若无则在设置入口加红点 state;实现取现有机制,不新造)。
- [ ] i18n zh/en:`settings.calendar.label/desc/cardTitle/cardBody/cardContinue/denied`。
- [ ] `npm run check` + vitest 绿;提交 `feat(ui): 日历匹配开关与授权说明卡`。

---

### Task 8: 详情页日历行

**Files:** `src/routes/notes/[id]/+page.svelte`(:1118 `.meta` 行后)、`src/lib/notes.ts`(3 个命令绑定 + 类型)、`src/lib/i18n/dict/notes.ts`

- [ ] `notes.ts`:`listCalendarCandidates(id)` / `setNoteCalendarEvent(id, eventId | null)`。
- [ ] `.header-main` 内 `.meta` 之后加日历行:
  - 有快照:📅 `{title}` + 参会人数;悬停/点击展开参会人名单(is_me 标「我」);「改选」→ 下拉列出 candidates(标题+时间+重叠),选中调 set;「清除」→ set(null);
  - 无快照且开关开且已授权:「关联日程…」按钮(打开同一候选下拉);
  - 未授权:灰字提示(链接去设置页)。
- [ ] i18n zh/en:`notes.calendar.*`。
- [ ] `npm run check` + vitest 绿;提交 `feat(ui): 详情页日历事件行(展示/改选/清除)`。

---

## 收尾核对

- [ ] `cargo test --lib --bins` + `cargo test --test mcp_stdio` + `npm run check` + `npm test` 全绿;`python3 scripts/check_macos_entitlements.py` 通过;`cargo tree -p objc2` 单版本
- [ ] 真机冒烟(PR 描述):① 设置页开关默认开,说明卡出现 → 「继续」拉起系统授权;② 录一段覆盖某日历事件的音频,停止后详情页出现事件行(标题+参会人);③ 改选/清除生效且重启保留;④ backfill 给历史笔记补快照;⑤ 拒权后录制/精修完全不受影响,详情页显示去设置提示;⑥ identify:参会人出现在候选、prompt 含 calendar 字段(看 ailog);⑦ 确认建议后 Person.emails 记录(voiceprints.json 可见)
- [ ] PR 描述注明:授权引导两处收窄(设置页说明卡+详情页提示,不做全局弹窗);Windows 桩恒 unavailable;EventKit 绑定 API 名如与 objc2-event-kit 0.3 生成名有出入以实际为准
