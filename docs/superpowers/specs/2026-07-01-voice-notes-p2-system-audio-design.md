# P2 系统声音采集 — 设计文档

- **日期**：2026-07-01
- **状态**：已通过头脑风暴评审，待编写实现计划
- **分支**：p2-system-audio（从 master）
- **前置**：P1 / P1.5（Silero VAD 按句分段）/ P1.6（SenseVoice）已合并入 master

## 1. 目标与范围

在现有"麦克风 → 16kHz 单声道 → Silero VAD 按句分段 → SenseVoice 本地识别 → SvelteKit 字幕"管线之上，新增 **macOS 系统声音采集**（会议软件里对方的声音），做成 **mic + system 两个音频源**，一起喂进识别管线，转写文本按来源（我 / 对方）标记显示。全程离线、不丢内容的原则不变。

### 范围内
- ScreenCaptureKit 系统声音采集，隐藏在现有 `AudioCapture` trait 后面。
- 双源汇入**单个识别 worker**（每源独立 VAD 分段）。
- 顺带解决 3 个账本 P2 待办（双路长驻线程让它们从"可选"变"必须"）：
  1. 识别移出采集回调（消除 `bounded(256)` 背压导致的音频卡顿）。
  2. 真正的停止 / 取消（现 `stop_recording` 是 no-op）。
  3. 模型加载完成后再发 `recording` 状态（消除 recording→error 闪烁）。
- 屏幕录制权限缺失时**降级为仅麦克风**继续。

### 范围外（YAGNI / 留给后续阶段）
- ❌ 说话人区分 / 聚类（diarization）—— 需 pyannote 分段 + 说话人嵌入模型，独立难题，推到 P3。因此 `final` 事件本阶段只加 `source`，不加 `speaker_id / 时间戳`。
- ❌ 音频源开关（开录前单独关 mic / system）—— 本阶段总是两路都采 + 自动降级。
- ❌ 录制中热插入新源（授权屏幕录制后需重新开录才生效）。
- ❌ 存储 / 导出 / 笔记列表。

## 2. 关键设计决策（头脑风暴结论）

| 问题 | 决策 | 理由 |
|---|---|---|
| P2 装多少 | 双源采集 + 3 个健壮性修复；diarization 推 P3 | 双路长驻线程让背压/停止/状态时机从"可选"变"必须" |
| 识别层怎么组织 | 两路各自 VAD 分段 → **汇入单识别 worker** | VAD 必须按源独立（重叠语音不能混音）；单 SenseVoice 实例省内存（~350MB 而非 ~700MB）；并顺带把 ASR 移出采集回调 |
| SCKit 从 Rust 调用 | **screencapturekit 牌**（先 spike 验证），不行回退 Swift 垫片 | 纯 Rust、与 P1 的 cpal 全 Rust 构建一致、无额外工具链；牌已封装 delegate + CMSampleBuffer 音频抽取 |
| 转写 UI | **单一时间流 + 源徽章**（我 / 对方），两条 live partial | 最贴会议纪要阅读习惯，改动最小；final 事件加 source |
| 权限降级 UX | **降级 + 可操作横幅**（打开系统设置按钮），re-record 生效 | 符合设计文档 §8"降级继续"原则，且告诉用户去哪授权 |

## 3. 架构与数据流

```
[Mic 采集]   (cpal, 原生帧)  ─┐   每源一条"分段 worker":
                              │     to_mono → resample 16k → 独立 VAD 实例
[System 采集](SCKit, 原生帧) ─┤     ├ 完成句      → Final{source, samples}  (进 finals 队列)
                              │     └ 节流 partial → 写"每源最新 partial 槽"(覆盖式)
                              ▼
                   finals 队列(不丢) + 每源 latest-partial 槽(合并/可丢)
                              ▼
                   [单 ASR worker] (一个 SenseVoice 实例, 串行)
                     循环: 先清空 finals → emit final{source,text}
                           空闲时才取最新 partial → emit partial{source,text}
```

> 路由说明：完成句作为 `FinalJob{source, samples}` 进 **finals 队列**（不丢）；partial **不进队列**——分段 worker 直接覆盖对应源的 **latest-partial 槽**（`Option<PartialJob{source, samples}>`）。ASR worker 先排空 finals 队列，再在空闲时读取并清空 partial 槽。两条路径分离，因此 final 天然优先、partial 只保留最新一版。

**背压与降级模型**：finals 走不丢的队列（保证"会议进行中不丢内容"）；partials 走"每源一个覆盖式槽"，采集侧只覆盖最新一版、ASR 侧空闲时才取。忙时旧 partial 被自动覆盖丢弃、final 永远优先——即设计文档"快流降频、慢流定稿不丢"的落地。采集 / 分段线程永不阻塞在推理上。

## 4. 组件与边界（单一职责、接口清晰）

| 单元 | 职责 | 依赖 |
|---|---|---|
| `Source { Mic, System }` | 来源标记，接线时确定并随 WorkItem/事件流转 | — |
| `AudioCapture`（trait 不变） | `start(sink) / stop` | — |
| `Microphone`（已存在） | cpal 麦克风 → AudioFrame | cpal |
| `SystemAudioCapture`（新，`audio/system.rs`） | SCKit 系统声音 → AudioFrame，隔离全部平台代码 | screencapturekit 牌 |
| 分段 worker（新，由现 `run_pipeline` 拆出） | 单源：归一 / 重采样 / VAD → 完成句进 finals 队列、partial 覆盖本源槽 | Segmenter |
| `FinalJob{source, samples}` | finals 队列载荷（不丢） | — |
| `PartialJob{source, samples}` | 每源 latest-partial 槽载荷（覆盖式、可丢） | — |
| ASR worker（新） | 持单 Recognizer，串行消费，emit 带 source 的事件；finals 优先、partial 覆盖合并 | Recognizer |
| `RecordingHandle`（新） | 持两路 capture + 各 worker join 句柄 + 停止信号 | — |

- `AudioFrame` **不动**（不加 source 字段——来源在接线时已知，YAGNI）。
- 单一平台相关点仍是 `AudioCapture` 的实现；换平台 / 换 SCKit 集成方式（牌 ↔ Swift 垫片）只换 `SystemAudioCapture`，管线与 UI 不感知。

## 5. 三个健壮性修复的落点

1. **ASR 移出采集回调**：识别进入独立 ASR worker，采集 / 分段线程与推理彻底解耦。finals 队列 + 每源 partial 槽取代原来在 drain 循环里内联 `recognize()`。
2. **真停止 / 取消**：`RecordingHandle.stop()` → 停两路 capture → 分段 worker 见帧通道关闭后 **flush 尾段**（不丢最后一句）→ 关 finals 队列 → ASR worker 排干剩余 finals 后退出并 join。`lib.rs` 持 handle，`stop_recording` 变为真正停止；`AppState` 保存 handle。
3. **recording 状态时机**：先构建 recognizer + 两个 VAD + 启动两路 capture，**全部就绪后**才 emit `status: recording`；任一初始化失败先 emit error，消除 recording→error 闪烁。

## 6. IPC 契约变更

```
partial → { source: "mic" | "system", text }
final   → { source: "mic" | "system", text }
status  → { state, system_audio: "on" | "denied" | "unavailable" }
```

- `final` 本阶段只加 `source`；`speaker_id / start_ts / end_ts` 随 diarization + 存储在后续阶段补。
- `status.system_audio`：`on` 正常 / `denied` 未授权屏幕录制 / `unavailable` 已授权但流启动失败——驱动前端降级横幅。

## 7. 界面（`src/routes/+page.svelte` + `$lib/events`）

- 单一时间流，每条 final 带徽章：**我**（mic，蓝）/ **对方**（system，绿）。
- **两条 live partial** 行（每源一条，浅色斜体），各自被同源 final 清掉。
- **降级横幅**：`system_audio != on` 时显示"系统声音不可用（未授权屏幕录制）" + [打开系统设置] 按钮（经 tauri-opener 打开屏幕录制设置面板）+ "授权后重新开录生效"提示。
- 保留 start / stop；stop 现在真正停止。

## 8. 错误处理（在设计文档 §8 之上）

| 场景 | 处理 |
|---|---|
| 系统采集未授权 / 启动失败 | 降级仅麦克风，`system_audio=denied/unavailable`，继续；**绝不阻塞 mic** |
| mic 与 system 都起不来 | emit error status，不进录制 |
| 某段 ASR 失败 | 该 final 标 `[识别失败]` 占位，worker 不死，继续后续段 |
| 停止时 ASR 正在识别一段 | 识别完当前段 → 排干 finals → 退出（有界，不悬挂） |

## 9. 测试策略

- **分段 worker 单测**：MockCapture 喂 fixture + 指定 Source → 断言 WorkItem 带对来源、finals 数正确（确定性，无设备）。
- **ASR worker 单测**：喂两源混合的 Final / Partial → 断言 finals 一条不丢、partial 被覆盖合并、事件带 source（用 CountingRecognizer）。
- **汇入集成测试**：两个 MockCapture（不同 fixture）→ 整会话 → 断言两源 finals 都带标记出现。
- **停止测试**：MockCapture 发完转空闲 → `stop()` → 断言优雅排干 + worker join + 尾段 flush。
- **SCKit 采集**：设备无法单测 → Task 1 spike 验证真帧；人工冒烟（会议软件放音，确认"对方"行出现）；管线测试统一用 mock trait。
- **前端**：徽章渲染、两条 partial、降级横幅（交互 / 快照）。
- 顺带补账本要的**非 ignored VAD 分段测试**（提交小 fixture）。

## 10. 实现顺序（供 writing-plans 细化）

1. **SCKit spike**：用 screencapturekit 牌拿到系统声音的 f32 帧（含权限流），验证可行；不行则切 Swift 垫片。这是最大不确定性，先打掉。
2. `Source` 枚举 + `SystemAudioCapture` 实现 `AudioCapture`（含未授权 → 可识别错误，供降级）。
3. `WorkItem` + 分段 worker（从 `run_pipeline` 拆出，去掉内联识别）。
4. ASR worker（finals 队列 + 每源 partial 槽，串行，emit 带 source）。
5. `start_session() → RecordingHandle` 编排 + 真停止；`lib.rs` 接线、就绪后发 recording、降级逻辑；`ipc.rs` 加字段。
6. 前端：源徽章、两条 partial、降级横幅。
7. 补非 ignored VAD 分段测试。

## 11. 开放问题 / 实现期再定

- screencapturekit 牌对系统声音（loopback）音频的支持完整度与 API 稳定性（Task 1 spike 定夺；回退 Swift 垫片）。
- macOS 26 上 ScreenCaptureKit 系统声音采集所需的确切权限（屏幕录制）与授权 API 调用方式；打开系统设置面板的 URL scheme。
- SCKit 交付的采样率 / 声道（通常 48kHz 立体声）→ 复用现有 `to_mono` + `resample_linear` 归一到 16kHz 单声道。
- 尾段 flush 与停止排干的超时上界（防止 ASR 卡死时停止悬挂）。
