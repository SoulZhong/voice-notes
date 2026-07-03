# P3.5 UX 重构 — 设计文档

- **日期**:2026-07-03
- **状态**:已确认,待编写实现计划
- **上游**:P3 spec `2026-07-03-voice-notes-p3-storage-notes-design.md`;P3 冒烟反馈驱动
- **反馈原文**:①应左右布局,历史列表在左、主区域在右,不要二级页面;②点「开始录制」应直接录音,不要再点一次;③按下后等待数秒才开始,应立即开始。

## 1. 目标与范围

| 包含 | 不包含 |
|---|---|
| 识别器常驻复用(启动预载 + 停录归还),二次开录零加载 | VAD/其他模型常驻(silero ~2MB 非瓶颈) |
| 左右布局:侧栏(主按钮+列表)+ 主区域(路由) | 列表/详情/录制的功能变更(P3 功能原样保留) |
| 一键开录:侧栏「开始录制」= invoke + 跳 /record | 模型下载引导(候选 P5) |
| 全局录制状态 store(修根「导航丢状态」类问题) | |
| `/` 重定向最近一场笔记(无笔记显示空态) | |

决策记录:空闲主区域 = 最近一场笔记(用户选定);模型常驻策略 = 启动即预载、录后不释放(~400MB 常驻,用户接受)。

## 2. 后端:识别器常驻

- `AppState` 增加 `recognizer_cache: Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>`(`Recognizer: Send` 已有约束)。
- **启动预载**:`tauri::Builder.setup()` 起后台线程:锁住 cache → 加载 SenseVoice → 存入。**加载期间持锁**,使「预载中用户点录制」自然阻塞到就绪,且永不双重加载。预载失败(模型缺失等)cache 留空、仅 eprintln,不阻塞启动。
- **取用**:`start_recording` 加载线程 `cache.lock().take()`;`None` 时现场加载(兜底,行为同 P3)。
- **归还**:
  - `run_asr_worker` 结束时返回 recognizer:`JoinHandle<Box<dyn Recognizer>>`。
  - `RecordingHandle::stop(self) -> Option<Box<dyn Recognizer>>`(join asr 线程取回;panic 时 None)。
  - 所有调用 stop 的路径(正常 stop_recording、mic 缺失、竞态孤儿)把返回值放回 cache;`start_session` Err 时 sources 已消费但 recognizer 已传入——该路径 session 内部 worker 已启动即失败,recognizer 随 asr 线程返还,同样回收。
- 效果:首次开录最多等预载收尾(应用启动数秒后即就绪);之后每次开录零模型加载,"recording" 状态几百毫秒内可达。

## 3. 前端:左右布局与全局状态

```
┌────────────┬──────────────────────────┐
│ [● 开始录制] │  主区域(路由内容)          │
│ 过滤框      │  /            → 跳最近笔记 │
│ ── 笔记列表 │  /record      → 实时转写流 │
│  …         │  /notes/[id]  → 笔记详情   │
└────────────┴──────────────────────────┘
```

### 全局录制状态 `src/lib/recording.svelte.ts`

runes 模块级 store:`status / systemAudio / noteId / storageDegraded / finals / partialMic / partialSystem`,及 `init()`(注册 partial/final/status/storage 监听 + `recording_status` 初始化,幂等)与 `start()`/`stop()`(invoke 封装;`start()` 内清空上场数据)。layout `onMount` 调 `init()`。状态跨路由存活,侧栏按钮与录制页共读。P3 的 `recording_status` 重挂载查询保留(应用冷启动时初始化)。

### 布局与组件

- **`src/routes/+layout.svelte`**:左侧栏 `<Sidebar/>` + 右主区域 `{@render children()}`;flex 布局,侧栏固定宽(~260px),主区域滚动。
- **`src/lib/Sidebar.svelte`**(逻辑自现 `/` 列表页迁移):
  - 顶部主按钮:idle →「● 开始录制」(调 `recording.start()` 成功后 `goto('/record')`);recording →「■ 停止」(调 `recording.stop()`,跳转仍由 status stopped+note_id 驱动)。
  - 过滤框 + 笔记列表(改名/两步删除/「录制中」「已中断」徽章,行为不变;「录制中」项点击 → `/record`)。
  - 刷新时机:挂载、status 事件(recording/stopped)、改名/删除后。
  - 删除当前正在查看的笔记 → `goto('/')`。
- **`src/routes/+page.svelte`(`/`)**:onMount 取 `listNotes()`,有笔记 → `goto('/notes/'+最新id, replaceState)`;无 → 空态提示「点击左上角开始第一场录制」。
- **`src/routes/record/+page.svelte`**:去掉本地事件监听与开始/停止按钮,改读全局 store;保留转写流、双 partial、系统声音横幅、落盘横幅。stopped+noteId → 跳详情的导航放在全局 store 的 status 监听里(监听只在 layout 注册一次,天然只触发一次,任何页面上停止都能正确跳转)。
- **`src/routes/notes/[id]/+page.svelte`**:不变(在主区域内渲染)。

## 4. 错误处理

| 场景 | 处理 |
|---|---|
| 预载失败 | cache 空,开录现场加载兜底;错误路径同 P3 |
| 预载中点录制 | take() 阻塞至预载完成,随后正常起会话(无双重加载) |
| stop 归还时 asr 线程 panic | 返 None,cache 留空,下次现场加载 |
| 录制中删除正在查看的笔记 | 后端已拒;侧栏删除其他笔记后若为当前详情页 → 回 `/` |

## 5. 测试

- Rust:`run_asr_worker` 归还语义(结束返回同一 recognizer 实例可复用);`RecordingHandle::stop` 返还链路;session 既有测试适配新签名。
- 前端:`npm run check` + `npm run build`。
- 人工冒烟:①启动后数秒点「开始录制」应即刻进入录制;②停止后**再次开录应几乎瞬时**;③录制中点侧栏笔记查看、再点「录制中」项回录制页,状态完好可停止;④删除当前查看笔记回 `/`;⑤`/` 冷启动自动打开最近笔记。
