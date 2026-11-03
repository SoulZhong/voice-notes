# 会议备注(memo):会中/会后在录音时间轴上记一句话

日期:2026-11-03 · 状态:**待评审** · 跟踪:AxiomOS #729 · 原型:`docs/prototypes/2026-11-03-meeting-memo.prototype.html`(方案 D)

## 要解决什么

开会时听到要紧的话,想随手记一句「定了:下周二上线」「张总去跟客户说」,事后能顺着这句话回到那一刻
的原文和录音。现在 voice-notes 里没有任何一块是用户自己写的文字:原始稿、修订稿都是转写或 AI 的产物,
用户只能改它们,不能在旁边记自己的东西。

## 一句话设计

**备注 = 笔记目录里一个独立的 `memos.json`:每条是「纯文本 + 录音时间轴上的一个毫秒点」。**
它不进原始稿、不进修订稿,谁都不改写它;各个界面把它按时间点插进原文流里显示。

词汇见 `CONTEXT.md`「备注」「时间标记」「书签 / 备注栏」三条。

## 已定的产品决策(设计讨论 Q1–Q25 摘要)

| # | 决定 |
|---|---|
| 名字 | 叫「备注」,不叫「笔记」(「笔记」指整场会议)。没有文字的备注叫「时间标记」,是备注的一种。 |
| 形态 | 一条备注 = 纯文本(可多行,无格式)+ 一个时间点。 |
| 时间点 | 会中打字:**开始打字**那一刻;快捷键/托盘:**按下**那一刻;暂停中:暂停位置;会后:当前播放位置。事后可调;录制中只能往前调。 |
| 入口 | 录制页底部固定输入框;全局快捷键 ⌥⌘M(只在录音中生效,按下即钉住时间并弹出小输入框);托盘「加备注…」(录音中才出现);笔记页播放条「加备注」按钮 + 前台按 `M`。 |
| 显示 | 原文流左侧时间轴上的**书签**(全文展示,不折叠);页面右侧**备注栏**列出全部备注,点一条跳到书签;窗口约 900px 以上默认展开,窄了自动收起,记住用户选择;无「只看备注」过滤。 |
| 录制中跳转 | 点备注栏 → 跳过去并暂停自动跟随,出现「回到最新」。 |
| 编辑 | 文字、删除随时可改(含录制中);删除无确认、给「撤销」。 |
| 不变性 | 备注永远属于用户:Aing、重新转文字、音频删减都不改它。落在删减时段里的照样显示,标「该时段已删减」。 |
| 对外 | 导出按时间插入;MCP `get_note` 返回;搜索能搜到。 |
| AI | Aing 精修与拟题把备注当只读提示;不产生新输出(无总结/待办)。 |
| 不做 | 设备(AI Passport)入口;备注关联说话人/人物;听写笔记没有备注。 |

## 总体结构

```
                       ┌──────────── memos.json(笔记目录内,独立文件,自己的锁)────────────┐
写入方(全部经 store::memos):                                                              │
  录制页输入框 ──invoke add_memo──┐                                                        │
  快捷键 ⌥⌘M / 托盘 ── memo::mark_now(Rust 侧直接写一条时间标记)──┤                        │
  浮窗回车 ──invoke edit_memo─────┤── store::memos::{add,edit,delete} ── tmp+rename ──────┘
  笔记页 加/改/删/调时间 ─────────┘            │
                                               └─ emit "memos_changed" {note_id, memos}
读取方(只读):
  recording.svelte.ts / 笔记页 ← 事件 + note_memos 命令
  导出 render_* · MCP get_note / search_notes · Aing 精修(HTTP 分块 / Agent 经 get_note)· 拟题
```

**不经 lifecycle actor,不经录制 writer。** 理由见「定死的选择 1」。

---

## 定死的选择

### 1. 独立文件 + 独立锁,照 `edits.json`(音频删减)的先例

事实(调查报告 2026-11-03):

- 录制 writer 整场持有 `.note.lock`;重新转文字、补生成成品轨的后台线程也全程持有它。
  凡是走 `.note.lock` 的写入(`edit_speakers` 那一套),**录制中和重转写中一律写不进去**。
- `edits.json`(音频删减,PR#199)是现成先例:独立文件、进程内 `static WRITE_LOCK`、tmp+rename、
  不碰 `.note.lock`、命令壳直调 store、不经 actor(`store/edits.rs:15-64`,`lib.rs:6942-6957`)。
- 没有任何操作会整目录替换笔记或按白名单清理未知文件(重转写只换 segments/speakers 等固定几个文件;
  数据目录迁移是整树复制)。`memos.json` 只会随删除整篇笔记而消失——这正是想要的。

所以:

- 新模块 `src-tauri/src/store/memos.rs`,结构照抄 `edits.rs`。
- 并发:进程内 `static MEMO_LOCK: Mutex<()>` + 跨进程文件锁 `.memos.lock`(`NoteLock::acquire_opened`,
  与 `.meta.lock` 同法)。跨进程锁是为 MCP 进程(独立 stdio 进程,应用没开时也能跑)——一期 MCP 只读,
  但留着锁,将来开放 MCP 写备注时不用返工。
- **不放 `aing.json`**:整篇重新 Aing 会用原始稿重建 `aing.json`(`refine/mod.rs:294`)。
- **不放 `meta.json`**:Aing 会写 meta(日历、标题),虽是锁内改字段,但「谁能碰备注」的边界会变模糊。
- **不放 `segments.jsonl`**:重转写会整个替换它。

### 2. 不经 actor、不设占用准入

备注与录制 writer 不共享任何文件,没有需要串行的写者;Aing/重转写/补生成期间加备注也没有冲突
(Aing 在开头读一次备注,之后的改动本轮看不到,下一轮才看到——可接受)。因此:

- 命令壳**不调 `admit_note`**,录制中、Aing 中、重转写中都能加/改/删。
- 唯一会失败的情况:笔记已被删除(目录不在)→ 报「笔记不存在」。
- 活动笔记与非活动笔记走**同一条**路径——不像段编辑那样分叉。

### 3. 时间点就是「笔记时间轴」上的毫秒,与段落 `start_ms`、播放器游标同一条轴

事实:

- 段落 `start_ms` 来自音频样本计数 + 续录偏移 `base_ms`;播放器游标、删减区间与段时间同口径(`edits.rs:6`)。
- 录制中前端显示的时间来自后端 `ActiveSession::elapsed_ms()`(墙钟,扣掉暂停,含续录 `base_ms`)。
  暂停期间墙钟表与段时间轴都冻结,两者一致。两条来源之间只有时钟漂移(约 1ppm,即每小时毫秒级),
  对「定位到哪一句附近」没有影响。

规则:

- **会中**:时间点**一律由后端取** `ActiveSession::elapsed_ms()`,不信任前端走表(前端 `nowTick` 只有 1 秒精度)。
  - 录制页输入框:第一次按键时调 `memo_anchor_now(note_id)` 拿到当时的 `at_ms` 存进草稿,回车时随 `add_memo` 一起提交。
    草稿被清空(删光文字或 Esc)就丢掉这个锚点,下一次按键重新取。
  - 快捷键/托盘:Rust 侧在按下的那一刻直接取 `elapsed_ms()` 写入,不经前端。
- **会后**:取播放器当前游标 `playerMs`(已是段时间轴口径)。
- **不做 align 投影**:align.json 只纠正 mic 段的时基,备注不属于任何一条轨,用的就是会话时间轴本身。
  (需冒烟验证:双轨笔记里备注落点与听感一致。)

### 4. 续录时把「超出上一场末尾」的备注钳回末尾

事实:续录时 `NoteWriter::resume` 取上一场最后一段的 `end_ms` 当 `base_ms`,并把音频轨截到这里
(`writer.rs:247-255`,`recording_sink.rs:99-104`)——上一场最后一句之后的静音被切掉了。

问题:上一场 9:30 说完最后一句,你 10:00 记了一条备注然后停录。续录后第二场的内容从 9:30 开始排,
这条 10:00 的备注就会落进第二场开头 30 秒的内容里——指错了地方。

规则:续录建会话时(`spawn_session` 的 Resume 分支,writer resume 成功之后),调用
`memos::clamp_after(dir, base_ms)`,把 `at_ms > base_ms` 的备注改成 `base_ms`。这是唯一一处
「系统改备注」,只动时间不动文字,且只在续录这一刻发生一次。

「中断笔记就此结束」(`finalize_interrupted`)不需要钳:那篇笔记不会再有后续内容,
超出末尾的备注在界面上按「落在末尾之后 → 显示在最后一段之后」处理即可。

### 5. 笔记页的书签用 ProseMirror 装饰(decoration),不进编辑器文档

事实:

- 原始稿编辑器有骨架锁:非段落节点混入就判「骨架已破坏」,`filterTransaction` 只放行带
  `external-load` 标记的事务(`segmentSchema.ts:55,65-67`)。
- 修订稿编辑器会把任何顶层块收进保存载荷(`MarkdownEditor.svelte:297-307`)——书签会被当正文保存。
- 全库还没用过 ProseMirror 的 `Decoration`。

规则:书签做成 **`Decoration.widget`**,经 `EditorView` 的 `decorations` prop 提供,备注变化时
`view.setProps(...)` 刷新——**不发事务**,所以不触发骨架锁,也不进 `collectBlocks` 的保存载荷。

- widget 放在「最后一个 `start_ms <= at_ms` 的块」**之后**(即第一个 `start_ms > at_ms` 的块之前);
  比所有块都晚的,放在文末。修订稿用展示时基(投影后的 `start_ms`,与播放器同口径)。
- widget 选项:`side: -1`、`ignoreSelection: true`、`stopEvent: () => true`(书签上的按钮能点、
  不会把光标放进去)。widget 的 DOM 用 Svelte 之外的纯 DOM 构建(NodeView 同法),文案用 `tStatic`。
- 书签占行、把正文往下推(全文展示,不是悬浮层)。
- **风险**:装饰在编辑器重建(`{#key canEdit}`)、`setSegments` 全量重载后要重新挂;
  实现第一步先做一个只挂一个 widget 的 spike,确认骨架锁、保存载荷、光标行为都不受影响。

录制页不受这条约束:实时原文是纯 Svelte 的 `{#each recording.finals}`,书签直接按时间归并进列表渲染。

### 6. 全局快捷键按键分发,只在录音中注册,注册注销都不阻塞

事实(全是坑):

- 插件的全局 handler 对**每一个**已注册的快捷键都会触发,而现在 `shortcuts::on_shortcut` 不看是哪个键,
  一律 `toggle_recording`——直接加 ⌥⌘M,按一下会把录音停掉。
- `apply_from_settings` 先 `unregister_all()` 再注册录制键——录音中在设置页保存快捷键,会把备注键一起清掉。
- 插件 register/unregister 走「派到主线程再同步等结果」;录制状态变化在 lifecycle actor 线程上提交,
  而主线程可能正同步等 actor——在 hook 里同步注册会死锁(`tray.rs:249-252` 记过同一类死锁)。

规则:

- `on_shortcut` 按 `sc` 分发:等于录制键 → `toggle_recording`;等于备注键 → `memo::mark_now`;其它忽略。
- `apply_from_settings` 改成**定向注销**旧录制键(记住上次注册的那个),不再 `unregister_all`;
  回落(注册失败把 `shortcut_enabled` 写回 false)只针对录制键。
- 新增 lifecycle 消费者 `MemoShortcutHook`(照 `consumers.rs` 的 `TrayHook`):进入 Recording 注册备注键,
  回到 Idle 注销。注册/注销一律 `app.run_on_main_thread(...)` 投递后立即返回(fire-and-forget)。
- 应用启动时若恢复到录制中(冷启动接管),setup 末尾补注册一次。
- 注册失败(被别的应用占用)→ emit `memo_shortcut_status {ok:false, reason}`,录制页在输入框上方提示一行
  「⌥⌘M 被其它应用占用,可在设置里换一个」;不影响录音。
- 设置:`memo_shortcut_enabled`(默认 **true**)、`memo_shortcut`(默认 `"Alt+CmdOrCtrl+M"`)。设置页
  「快捷键」一节加一行;保存时校验不得与录制键相同。

### 7. 浮窗:独立小窗口复用 + 关闭后把焦点还给原应用(先 spike 定技术路线)

事实:项目里只有一个窗口(`main`),没有 NSPanel、没有「把焦点还给上一个应用」的现成做法;
`capabilities/default.json` 只授权 `main`。

规则(两条路,P0 spike 选一条):

- **路线 A(先试)**:普通 `WebviewWindow`,label `memo`,无边框、`always_on_top`、`visible_on_all_workspaces`、
  不进任务栏、不可缩放,首次创建后隐藏复用。弹出前用 `NSWorkspace.frontmostApplication` 记下当前前台应用,
  关闭时隐藏浮窗并 `activate` 那个应用。风险:激活本应用时主窗口可能被一起带到前台、挡住会议软件——spike 要实测。
- **路线 B(A 不行再上)**:非激活式 `NSPanel`(引入 `tauri-nspanel` 或用 objc2-app-kit 自己包),浮窗能收键盘输入
  但不激活整个应用,主窗口完全不动,也不需要还焦点。代价是新依赖与平台代码。
- Windows:普通置顶窗口 + 关闭后 `SetForegroundWindow` 还给原窗口(Windows 真机未验,列为已知风险)。

浮窗是 SvelteKit 的一个独立路由 `/memo-panel`,挂载时自己初始化 i18n(从 settings 读语言)。
capability 增加 `memo` 窗口(只给 `edit_memo`/`delete_memo` 和事件监听所需的权限)。

浮窗流程:

1. 按 ⌥⌘M / 点托盘 → Rust `memo::mark_now`:不在录音中则忽略;否则取 `elapsed_ms()`,
   **立即**写入一条空文字备注(时间标记,`origin: "hotkey" | "tray"`),emit `memos_changed`。
2. 显示浮窗,传 `{note_id, memo_id, at_ms}`;浮窗显示「已钉住 12:34」+ 输入框。
3. 回车 → `edit_memo(text)` → 隐藏浮窗并还焦点。Esc / 失焦 → 直接隐藏(保留时间标记)。
4. 浮窗开着时又按 ⌥⌘M:当前输入有文字先保存,再钉一条新的(连按 = 连续钉多个时刻)。
5. 浮窗开着时停录:浮窗照常可保存(备注命令不依赖录制状态)。

### 8. Aing 与拟题:只读提示,只进输入不进输出

事实:

- HTTP 精修的 user 消息现在只有「术语表 + 编号段落」,**没有任何附加上下文的位置**(与会人/日历只进
  身份推断 identify)。有护栏测试钉死 prompt 只能写在 `prompts.rs`、user 消息形状固定。
- Agent 精修只能调 4 个 MCP 工具、没有文件/shell 权限,经 `get_note` 读稿。
- 拟题只看修订稿开头 1500 字。

规则:

- **HTTP 精修**:`prompts::refine_user` 增加可选的备注块;**备注为空时 prompt 与现在逐字节一致**(现有测试不改)。
  分块时每块只带时间落在 `[块首段 start_ms − 30s, 块末段 end_ms + 30s]` 内的备注,每块备注合计 ≤ 800 字,
  且**不计入** `CHUNK_CHARS`(不改变现有分块边界)。备注行格式 `t=mm:ss 备注:…`(换行与冒号先清洗,
  同 `speaker_label` 的做法),**不放进编号段落里**——否则段落条数校验会失败。
- **`REFINE_SYSTEM` 加一条规则**:备注是用户亲手写的只读提示,用来判断错字、统一人名与术语写法、理解主题;
  不得复制进 texts,不得改写;与正文冲突时以正文为准。
- **Agent 精修**:`get_note` 的 segments 输出新增顶层 `memos` 字段(见下「MCP」);`agent_refine` 第 1 步
  补一句怎么用备注。**不把备注拼进 argv 里的 prompt**(prompt 走命令行参数,有长度上限)。
- **拟题**:两种执行体的 `gen_title` 都增加 `memos` 参数,放在正文**之前**,合计 ≤ 500 字(备注覆盖全场,
  正是现在「只看开头 1500 字」的补位)。三个调用点:主管线两处 + CLI 批量补标题。
- **关系抽取与身份推断(identify)一期不带备注**:关系要求逐字引用段落原文当证据,备注会被误当证据。
- **护栏测试**:新增源码自审测试(仿 `prompts.rs:175` 的 `include_str!` 写法),断言 `refine/*.rs`
  不出现 `memos::` 的写函数。
- **ai_logs**:HTTP 与拟题路径的请求体里已含备注,自动入日志。Agent 精修路径例外(备注是 Agent
  经 MCP 读的,读不写日志)→ `agent_refine` 的日志 request 里补一个备注快照(条数 + 内容)。

---

## 数据结构

`notes/<id>/memos.json`:

```json
{
  "schema_version": 1,
  "memos": [
    {
      "id": "m_20261103T153412_7f3a",
      "at_ms": 754000,
      "text": "定了:下周二上线,周一封版",
      "origin": "composer",
      "created_at": "2026-11-03T15:34:12+08:00",
      "updated_at": "2026-11-03T15:34:20+08:00"
    }
  ]
}
```

- `id`:稳定 id(时间戳 + 随机后缀),编辑/删除/撤销按它定位,不用下标。
- `at_ms`:笔记时间轴毫秒。表内按 `at_ms` 升序,相同时按 `created_at`。
- `text`:纯文本,允许换行;空串 = 时间标记。上限 2000 字(超出前端拦、后端拒)。
- `origin`:`composer`(录制页输入框)/`hotkey`/`tray`/`note`(笔记页)。只用于诊断与将来统计,不影响行为。
- 坏档(解析失败)按空表读,**但不覆盖**:写入前若读到坏档,先把它改名为 `memos.json.corrupt-<时间>` 再写新表。
  (`edits.json` 是坏档按空表直接覆盖;备注是用户亲手写的内容,丢了无法再生,所以多这一步。)
- NoteMeta 的 `SCHEMA_VERSION` 不动(全仓没有读它的代码;附属文件各自带版本,同 edits.json)。
- **不并入 `Note` 结构、不进 `NoteStore::load`**:`load` 有按固定文件签名的缓存(`LoadSig`),并进去就得
  一起改签名。备注单独用 `note_memos` 命令读,像 `note_edits` 一样。

## 后端接口

### 命令(Tauri)

| 命令 | 入参 | 说明 |
|---|---|---|
| `note_memos` | `note_id` | 读全部备注(升序)。 |
| `memo_anchor_now` | `note_id` | 录制中返回当前 `elapsed_ms`;笔记不在录制中 → 报错(前端不会在非录制时调)。 |
| `add_memo` | `note_id, at_ms, text, origin, id?` | 新增。`id` 只在「撤销删除」时由前端带回原 id;已存在同 id 则报错。返回新备注。 |
| `edit_memo` | `note_id, id, expected_updated_at, text?, at_ms?` | 改文字和/或时间点。`expected_updated_at` 做乐观校验,不一致报「备注已被修改,请刷新」。 |
| `delete_memo` | `note_id, id` | 删除,返回被删的那条(供撤销)。 |

时间点的统一校验(`add_memo` / `edit_memo` 都过):

- 笔记在录制中:`at_ms ≤ 当前 elapsed_ms`(只能往前,Q10)。
- 笔记不在录制中:`at_ms ≤ 笔记时长`(最后一段 `end_ms`,与音频时长取大)。
- 下限 0。超出范围**钳位**而不是报错(拖时间轴拖过头是常态)。

每次成功写入后 emit `memos_changed { note_id, memos }`(整表,备注量小,省去增量合并的复杂度)。
前端一律以事件为准,不做乐观更新(与 `segment_edited` 同纪律)。

### Rust 内部入口

- `memo::mark_now(app, origin)`:快捷键与托盘共用。不在录音中 → 什么都不做;否则写入时间标记、emit、弹浮窗。
- `memos::clamp_after(dir, base_ms)`:续录专用,见「定死的选择 4」。

### MCP

- `get_note`:
  - `format=segments`:顶层新增 `memos: [{id, at_ms, text}]`(有修订稿与无修订稿两种输出都加)。
  - `format=markdown|text`:内容里按时间插入(复用导出渲染,见下)。
- `search_notes`:逐篇扫描时多扫一层 `memos.json`,命中返回 `{kind:"memo", note_id, title, memo_id, at_ms, text}`。
- 一期**不开放 MCP 写备注**(备注属于用户;Agent 精修模式的 4 个工具白名单也不变)。

### 导出

`render_markdown` / `render_text` / `render_refined` 增加 `memos: &[Memo]` 参数,在逐段循环里按 `at_ms`
归并插入(规则同书签:插在最后一个 `start_ms <= at_ms` 的段之后)。

- Markdown:`> 📝 **备注 12:34** 定了:下周二上线`(多行备注每行都带 `> `);时间标记写 `> 🔖 **时间标记 12:34**`。
- 纯文本:`[备注 00:12:34] 定了:下周二上线`。
- 圈选范围导出:只带范围内的备注(点判定 `start ≤ t < end`)。
- 落在删减时段里的备注:**照样导出**,行尾标「(该时段已删减)」——与界面一致(2026-11-03 已确认)。

## 前端

### 状态与事件

- `src/lib/recording.svelte.ts`:新增 `memos` 状态;订阅 `memos_changed`(按 `note_id` 过滤);
  新会话清空,续录与冷启动 `hydrateFromDisk` 时用 `note_memos` 回灌。
- `src/lib/memos.ts`:命令封装 + 纯函数(按时间归并段落与备注、删减点判定、时间钳位、窄屏断点)。纯函数配单测。
- 笔记页:`note_memos` 初读 + 订阅 `memos_changed`。

### 录制页(`src/routes/record/+page.svelte`)

- **底部输入框**:`position: sticky; bottom: 0` 的零高锚点 + 输入卡,只在录制中出现;与现有「回到最新」药丸
  (`.jump-anchor`)错开高度。回车保存、Shift+回车换行、Esc 清空。第一次按键调 `memo_anchor_now`,
  输入卡下方显示「📌 钉在 12:34」。输入框获焦时**不拦截**任何现有快捷键以外的按键。
- **书签**:在 `{#each recording.finals}` 里按时间归并备注,渲染成兄弟节点 `<div class="memo-bookmark">`;
  左侧时间轴用 `.transcript-wrap` 的左内边距 + 一条竖线画出,与右缘已有的迷你时间轴对称;
  迷你时间轴上同时画出备注刻度。
- **备注栏**:页面主区改为两栏(`1fr` + 260px),栏 `position: sticky`。点一条 → `follow = false`、
  滚到对应书签并闪一下;已有「回到最新」接管恢复跟随。
- 刚保存的备注在书签和备注栏同时高亮 2 秒(只是视觉确认,不折叠任何东西)。
- 快捷键注册失败的提示条(见「定死的选择 6」)。

### 笔记页(`src/routes/notes/[id]/+page.svelte`)

- **布局**:`.container` 改为两栏 grid,备注栏 `position: sticky; top: <吸顶操作栏高度>`;滚动仍归 layout 的 `.main`。
- **书签**:`MarkdownEditor` 新增 prop `memos` 与回调 `onMemoAction`,内部用装饰渲染(见「定死的选择 5」)。
  书签内容:旗标图标(16px 线性 SVG——DESIGN.md 禁用 emoji,原型里的 emoji 只是示意)+ 时间 + 全文;
  悬停浮现「从这里播放 / 改 / 删」;点时间 → 小浮层调时间(±5 秒按钮 + 直接输入 mm:ss)。
- **删减标记**:`at_ms` 落在任一删减区间 `[start, end)` 内 → 书签与备注栏都带「该时段已删减」小标签。
- **加备注**:播放条 `.transport` 加「加备注」按钮;`svelte:window` 的 keydown 在以下条件**都满足**时响应 `M`:
  无修饰键、非输入法组合中(`!e.isComposing`)、焦点不在 input/textarea/`[contenteditable]`/`.ProseMirror` 内、
  本笔记不在录制中。按下后在播放位置插入一个**草稿书签**(未保存):回车保存(空文字 = 时间标记)、Esc 丢弃。
- **删除撤销**:删除后 toast「已删除备注 · 撤销」,撤销 = 带原 id 调 `add_memo`。

### 备注栏的展开/收起

- 主区宽度 ≥ 900px 默认展开,否则默认收起(用容器宽度判定,不是窗口宽度——左侧栏开合会改变主区宽度)。
- 用户手动展开/收起后,记住选择(前端 `localStorage`,键按页面区分:`memoColumn.record` / `memoColumn.note`),
  之后不再按宽度自动切换。读写 `localStorage` 全部 try/catch,读不到就按宽度默认。

### 托盘(`src-tauri/src/tray.rs`)

`menu_items` 在 `pause_toggle` 旁、同一条件(录制中且会话已入槽)下插入 `("add_memo", tr!("加备注…","Add memo…"))`,
`on_menu_event` 分支调 `memo::mark_now(app, "tray")`。补 `menu_items` 单测。

### 文案

`record.memo.*`(录制页、浮窗)、`notes.memo.*`(笔记页、书签、备注栏)、`settings.memoShortcut.*`;
托盘用 `tr!`。NodeView/装饰里用 `tStatic`。全部中英双语,过 `i18n.test.ts` 与 `noHardcodedCjk` 护栏。

## 分期与 PR 拆分

按本仓「一次只开一个 PR、base 恒 master」的纪律,顺序合并:

| 期 | 内容 | PR |
|---|---|---|
| P0 spike | ① 浮窗路线 A 实测(主窗口会不会被带到前台、焦点能否还回去);② PM 装饰 widget 不触骨架锁、不进保存载荷。各写一页结论附在 #729。 | 不合入 |
| P1 | `store::memos` + 五个命令 + `memos_changed` 事件;续录钳位;录制页输入框、书签、备注栏;笔记页书签(装饰)、备注栏、加/改/删/调时间/撤销;删减标记。 | PR 1 |
| P2 | 全局快捷键分发改造 + `MemoShortcutHook` + 设置项;浮窗;托盘「加备注…」。 | PR 2 |
| P3 | 导出、MCP `get_note`/`search_notes`、Aing 精修与拟题注入、ai_logs 快照;侧栏搜索(若评审通过)。 | PR 3 |

P1 合并后就已经能用(录制页与笔记页);P2 补齐「不切回应用也能记」;P3 让备注被 AI 和外部读到。

## 测试

**Rust 单测**

- `store::memos`:增删改的排序与持久化;`expected_updated_at` 冲突;同 id 新增报错;坏档改名后重建;
  `clamp_after` 只动超出的条目、只改时间不改文字;文字超长拒绝。
- 时间点校验:录制中不能超过当前时间、会后不能超过时长、负数钳到 0。
- 快捷键分发:录制键只触发开停录、备注键只触发 `mark_now`(把分发抽成纯函数测)。
- `MemoShortcutHook`:进入 Recording 发注册、回 Idle 发注销、其它迁移不动作。
- `menu_items`:录制中有 `add_memo`、非录制没有。
- 导出:备注按时间插入三种渲染;圈选范围过滤;删减时段内的备注带标记。
- MCP:`get_note` 两种 segments 输出都带 `memos`;`search_notes` 命中备注。
- Aing:备注为空时 prompt 与现在逐字节一致;分块只带本时间窗内的备注且有字数上限;texts 条数不受影响;
  拟题把备注放在正文前;源码自审——`refine/*.rs` 不调用备注写函数;Aing 整轮跑完后 `memos.json` 字节不变。

**前端单测(vitest)**

- 段落与备注按时间归并(相同时间、早于第一段、晚于最后一段)。
- 删减点判定(半开区间边界)。
- `M` 键守卫(各种焦点与修饰键组合)。
- 备注栏默认展开/收起与记忆。

**真机冒烟(合并前做,清单写进 PR 描述)**

1. 录制中在输入框慢慢打一句,看时间点是不是开始打字那一刻。
2. 在会议软件里按 ⌥⌘M:钉住、弹框、回车后焦点回到会议软件;Esc 留下时间标记。
3. 托盘「加备注…」同上;停录后托盘里不再有这一项、⌥⌘M 释放给其它应用。
4. 设置里把录制键改成别的再保存——录音中备注键仍然有效。
5. 暂停中加备注,时间点等于暂停位置。
6. 停录 → 续录,上一场末尾之后记的备注被钳到末尾。
7. 笔记页:播放到某处按 `M` 加备注;在原始稿与修订稿两个视图里书签位置正确、可改可删可撤销;
   编辑原始稿段落、保存修订稿后书签仍在且没有被写进正文。
8. 删减一段包含备注的音频:备注仍显示并带「该时段已删减」。
9. 重新转文字后书签落回正确位置。
10. 重新 Aing:修订稿用词受备注影响(如备注里写对的人名);`memos.json` 未被改动。
11. 导出 Markdown 含备注;MCP `get_note` 能读到备注。
12. 窄窗口:备注栏自动收起;手动展开后记住。
13. 双轨笔记(mic + system):书签落点与听感一致。

## 待评审时确认的点

以下是我做的取舍,讨论里没有直接定过;**2026-11-03 评审已全部确认**:

1. ✅ **已确认(2026-11-03)**:导出按时间插入原文(不另设汇总区);删减时段内的备注照样导出并标注(与界面一致)。
2. ✅ **侧栏搜索是否搜备注**:现在侧栏只按标题筛笔记,没有全文搜索。若要搜备注,需要新增一个轻量命令
   一次读出所有笔记的备注文字做匹配(笔记多时略有开销)。**一期做**(已确认)——「我记得写过一句『封版』,
   是哪场会?」正是备注最常见的找法。录制页的场内搜索一定会搜到备注。
3. ✅ **点备注栏只滚动原文,不跳播放**:书签上有「从这里播放」按钮。理由:会中点它不该打断录音页的任何播放
   (会中本来就禁止回放),会后也常常只是想看上下文。
4. ✅ **快捷键/托盘的备注按下即写盘**,浮窗里 Esc 不删除它(留作时间标记)。若误按,需要到录制页删掉。
5. ✅ **浮窗开着再按 ⌥⌘M = 先存当前、再钉一条新的**(连按 = 连续钉多个时刻)。
6. ✅ **Windows**:浮窗与还焦点只做基本实现,真机未验,随 P2 冒烟清单单列。
