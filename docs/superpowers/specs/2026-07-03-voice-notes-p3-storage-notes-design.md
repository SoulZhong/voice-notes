# P3 存储与笔记 — 设计文档

- **日期**:2026-07-03
- **状态**:已通过头脑风暴评审,待编写实现计划
- **上游**:总设计 `2026-06-30-voice-notes-design.md` §6(笔记存储)、§8(错误处理);P2 之后的下一阶段
- **前置现状**:P1/P1.5/P1.6/P2 已完成——双源(mic/system)实时转写可用,但**停止录制后内容即丢**,总设计核心原则「会议进行中绝不丢内容」尚未兑现。P3 兑现它。

## 1. 目标与范围

**目标**:边录边落盘 + 崩溃可恢复;录完的会议能回看、改名、删除、导出。做完 P3,应用达到"可日常使用"。

| 包含 | 不包含(后续阶段) |
|---|---|
| 定稿段实时落盘(JSONL 追加) | 说话人区分(P4;schema 已预留位) |
| 崩溃恢复(「已中断」笔记可看可导出) | SQLite 索引 / 全文搜索(目录扫描足够,YAGNI) |
| 笔记列表(主页):日期倒序、标题过滤 | 详情页编辑段落文字(与说话人改名一起做更顺) |
| 笔记详情(只读,带时间戳/来源徽章) | SRT/VTT 导出(总设计已列二期) |
| 自动命名、改名、删除(带确认) | 模型管理引导、暂停/电平表(候选 P5) |
| 导出 Markdown / 纯文本 | |
| Final 事件补时间戳(start_ms/end_ms) | |

## 2. 数据模型与目录布局

```
<app_data_dir>/notes/               # app_data_dir = ~/Library/Application Support/com.teemo.voice-notes
  20260703-150432/                  # 文件夹名 = 会议开始时间(本地时区) = note id
    meta.json                       # 元数据,原子写(临时文件 + rename)
    segments.jsonl                  # 每条定稿段一行,追加写 + flush
    transcript.md / transcript.txt  # 导出时按需生成
```

**meta.json**:

```json
{
  "schema_version": 1,
  "id": "20260703-150432",
  "title": "2026-07-03 15:04 会议",
  "started_at": "2026-07-03T15:04:32+08:00",
  "ended_at": null,
  "state": "recording"
}
```

- `state`:`recording | complete`。正常停止时置 `complete` 并填 `ended_at`。
- 时长不单独存,由 `ended_at - started_at`(或中断时最后一段的 `end_ms`)推得。

**segments.jsonl** 每行:

```json
{ "seq": 12, "source": "mic", "text": "……", "start_ms": 83210, "end_ms": 86540, "speaker": null }
```

- `speaker` 预留 `null`,P4 说话人区分落地时 schema 不变(schema_version 不必升)。
- `start_ms/end_ms` 为**相对会议开始**的毫秒,按各源 16kHz 样本钟换算。
- **只落定稿段,partial 不落盘**:崩溃最多丢当前未定稿的一句(受 VAD max_speech_duration 上限约束,≤15s)。这是「不丢内容」原则的明确代价边界。

**选型记录**:总设计 §6 写的是单一 `transcript.json`;P3 改为 `segments.jsonl` 追加写——它是 transcript.json 的崩溃安全变体:每段一行 append+flush,写入成本恒定不随会议时长增长,崩溃后已写行天然完好,无需修复/重放逻辑。评审时对比过「全量原子重写 transcript.json」(写放大)与「SQLite 主存储」(违背本地文件优先取向),均不取。

## 3. 管线前置改动:Final 带时间戳

现状:`pipeline::segmenter::Segment` 只有 `samples`,丢掉了 Silero VAD 给的样本偏移;`FinalJob`/`FinalEvent` 只有 `source + text`。

改动(P3 内完成,是落盘与详情页的前置):

- `Segment` 增加 `start: usize`(相对该源流开始的样本偏移;Silero `SpeechSegment.start` 直接给);`MockSegmenter` 同步维护。
- `FinalJob` 带 `start_samples`,ASR worker 换算 `start_ms = start_samples * 1000 / 16000`,`end_ms = start_ms + len * 1000 / 16000`。
- `FinalEvent` 增加 `start_ms: u64, end_ms: u64`;前端 `events.ts` 同步。

## 4. Rust 架构:store 模块

新增 `src-tauri/src/store/`,两个角色,职责分离:

### NoteWriter(录制期,会话持有)

- `NoteWriter::create(notes_dir, started_at) -> Result<NoteWriter>`:建文件夹、写 meta(`state=recording`)、打开 `segments.jsonl` append 句柄。
- `append_final(&mut self, seg: SegmentRecord) -> Result<()>`:序列化一行 + `write + flush`。
- `finalize(&mut self, ended_at) -> Result<()>`:原子更新 meta(`ended_at`、`state=complete`)。
- **写失败不中断录制**:失败的段进内存待写队列,下次 `append_final`/`finalize` 时先重试队列;错误通过 status 事件上报 UI。

### NoteStore(静态读写,IPC command 用)

- `list(notes_dir) -> Vec<NoteMeta>`:扫描子目录读 meta.json,按 `started_at` 倒序;meta 损坏的以文件夹名兜底展示。
- `load(id) -> Note { meta, segments }`:逐行解析 jsonl;**不可解析的尾行跳过**(崩溃截断容忍),中间行损坏同样跳过并计数上报。
- `rename(id, title)`:原子更新 meta。
- `delete(id)`:`remove_dir_all`(UI 已确认)。
- `export(id, format: md|txt) -> PathBuf`:生成到会议文件夹内,返回路径。

### 会话集成与 IPC

- `start_recording`:创建 `NoteWriter`,随会话编排传入 ASR worker;worker 每 emit 一条 `final` 事件,同步 `append_final`(同一结构,seq 单调递增)。
- `stop_recording`:排干后 `finalize`。
- 新增 5 个 Tauri command:`list_notes / get_note / rename_note / delete_note / export_note`;`start_recording` 返回值增加 `note_id`(停止后前端跳转详情用)。
- `StatusEvent` 增加可选落盘警告字段(如 `storage: "ok" | "degraded"`),UI 据此显示横幅。

### 崩溃恢复

启动后 `list()` 中 `state=recording` 且无活动会话的笔记,前端显示「已中断」徽章;内容完好,可看/改名/导出。**纯展示逻辑,不做磁盘修复**——JSONL 追加写保证了这一点。不自动改写其 meta(保持诚实记录)。

## 5. UI 结构

| 路由 | 内容 |
|---|---|
| `/`(新主页) | **笔记列表**:标题过滤框、日期倒序;每项显示标题/日期/时长(+「已中断」徽章),行内操作:改名、删除(确认对话)、导出;顶部「开始录制」按钮 → `/record` |
| `/record` | 现有录制视图整页迁移,功能不变(双 partial、源徽章、降级横幅);新增落盘警告横幅;停止后自动跳转 `/notes/[id]` |
| `/notes/[id]` | **详情(只读)**:复用录制视图的段渲染(我/对方徽章 + `mm:ss` 时间戳);标题就地编辑;导出按钮(md/txt)→ 生成后经 tauri-plugin-opener「在 Finder 中显示」 |

**导出 Markdown 格式**:

```markdown
# 2026-07-03 15:04 会议
2026-07-03 15:04 – 16:12(1 小时 8 分)

**[我] 00:01:23** 今天开会讨论一下项目进度。
**[对方] 00:01:31** 好的,先看上周的问题。
```

纯文本同结构去掉加粗。说话人区分落地后 `[我]/[对方]` 将替换为说话人名,导出代码按 `speaker` 字段为空与否分支,P3 已留好。

## 6. 错误处理

| 场景 | 处理 |
|---|---|
| 追加落盘失败(磁盘满/权限) | 段进内存待写队列,后续重试;status 上报 `storage: degraded`,UI 横幅「落盘异常,内容暂存内存」;**绝不中断录制** |
| jsonl 尾行截断(崩溃) | `load` 跳过不可解析行 |
| meta.json 损坏 | 列表以文件夹名/文件时间兜底显示,segments 仍可读 |
| 录制中崩溃 | 重启后笔记带「已中断」徽章,内容 = 崩溃前所有定稿段 |
| 删除/改名/导出失败 | 显式报错提示,不静默 |
| notes 目录不存在 | 首次使用时创建;创建失败阻止开录并报错 |

## 7. 测试策略

- **store 单测(tempdir,无设备无模型)**:create→append→finalize→list→load 全链路;rename/delete/export;截断尾行容忍;meta 损坏兜底;待写队列重试(用只读目录/注入失败模拟);meta 原子写(写后文件始终完整可解析)。
- **时间戳单测**:`Segment.start` 经 MockSegmenter → FinalJob → 毫秒换算正确;双源各自独立样本钟。
- **会话集成测试**:MockCapture 喂 fixture 跑完整会话 → 断言 `segments.jsonl` 行数与 final 事件数一致、seq 单调、finalize 后 `state=complete`。
- **前端**:列表渲染/过滤、「已中断」徽章、详情段渲染、导出调用的基本测试;导出 md 内容用快照。
- **人工冒烟**:真实录一段 → 强杀进程 → 重启确认「已中断」笔记内容完好;正常录完 → 详情/改名/删除/导出走一遍。

## 8. 开放问题 / 实现期再定

- `app_data_dir` 经 Tauri `PathResolver` 获取;测试中 store 一律注入自定义目录,不碰真实路径。
- 双源样本钟与墙钟的微小漂移(重采样/丢帧)对时间戳的影响可接受(展示用途,非字幕级精度);字幕导出(二期)再校准。
- 待写队列的内存上界(极端持续写失败):倾向不设上界——内存里丢内容比 OOM 更早违背原则,且几小时文本量级仅 MB 级。
