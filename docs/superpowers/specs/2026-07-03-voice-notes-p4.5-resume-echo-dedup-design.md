# P4.5 会议续录 + 跨路回声去重 — 设计文档

- **日期**:2026-07-03
- **状态**:已确认,待编写实现计划
- **上游**:P4 spec;冒烟反馈驱动(①中断的会议要能继续记录;②线上会议同一人发言经他人电脑外放进入 mic,被识别成两个人且转写重复)
- **分支**:`p4-diarization-aec`(续)

## 1. 会议续录

**语义**:任何非活动笔记(已中断或已完成)可「继续录制」;新段追加进同一笔记,seq 续接,**时间轴连续**(新段时间戳紧接既有最大 end_ms);停止后照常 finalize(complete + 新 ended_at)。决策记录:时间轴连续(非真实间隔)、已完成的也可续录(中场休息场景)。

### 后端

- `NoteWriter::resume(notes_dir, id) -> Result<NoteWriter>`:读 meta(损坏 → Err)→ 置 `state=recording`、`ended_at=None` 原子写;扫 segments.jsonl 取 `next_seq`(最大可解析 seq+1)与 `base_ms`(最大 end_ms);打开 append 句柄;加载 speakers.json 进内存表。新建路径 `base_ms=0`——`on_final` 落盘/emit 前时间戳统一 `+ base_ms`。
- **说话人连续性(质心快照)**:
  - `SpeakerMeta` 增加 `centroid: Option<Vec<f32>>`(serde default + skip_serializing_if,旧文件兼容)与 `count: u64`(serde default)。
  - `SpeakerRegistry::snapshot() -> Vec<ClusterSnapshot { id, centroid, count, sources }>`;`from_snapshot(snaps) -> Self`(next_id = 已见最大 S 号 + 1;无质心项跳过但计入编号)。
  - worker 结束(finals 通道断开)时发一次 `DiarEvent::Snapshot(Vec<ClusterSnapshot>)`(不随普通事件走,避免事件风暴);lib.rs 侧写进 writer 内存表;`finalize`/`persist_speakers` 落盘。
  - 续录时从 speakers.json 复原 registry → **同一人保持同一编号与名字**;无快照(旧笔记/曾降级)→ 空 registry,从最大 S 号+1 续编,旧名保留。
- `resume_recording(note_id)` command:运行守卫与 start_recording 一致(拒绝已有活动会话);与 start_recording 共用提取出的会话启动函数(`NoteTarget::New | Resume(String)`);失败路径沿用 `abort_or_finalize`(续录笔记 next_seq>0 ⇒ 走 finalize 保全,不会误删目录)。

### 前端

- 详情页头部「继续录制」按钮(录制中禁用);store 增 `resume(noteId)`:pending 守卫 → `getNote` 把既有段与 speakers **灌进转写区**(录制页看到全程)→ invoke → 成功 goto `/record`。
- store 的 "recording" 事件无条件清空 finals/speakers——续录时不能清:`resume()` 置一次性 `resuming` 标志,recording 事件遇标志只清 partial,用后复位。
- 详情页中断横幅文案提示可续录。

## 2. 跨路回声去重

**问题**:他人电脑外放会议音频 → 同一句话经房间声学进入 mic(AEC 只能消本机扬声器);声道染色使两份声纹分裂为两个说话人,转写重复两条。

**方案(mic 侧延迟对照,system 权威)**:ASR worker 内,mic 段识别出文本后**不立即处理**,入 `pending_mic` 缓冲(hold ≤ `ECHO_HOLD_MS`);期间任一 system 段与之「时间邻近 + 文本高相似」→ 判定回声,**丢弃 mic 段**(不嵌入、不聚类、不落盘、不上屏);hold 到期无匹配 → 正常 release(嵌入/聚类/emit)。system 段永不延迟(远端语音零额外时延);mic 段 final 晚 ~2.5s 定稿,partial 实时不受影响。

- 常量(集中定义,注释校准来源):`ECHO_HOLD_MS = 2500`、`ECHO_WINDOW_MS = 2500`(时间邻近:区间交叠或起点差 < 窗)、`ECHO_SIM_THRESHOLD = 0.6`。
- 文本归一:去空白/标点、ASCII 小写;相似度 = max(1 - Levenshtein/较长串长, 短串被长串完全包含 ? 1.0 : 0)(手写 Levenshtein,段文本短无性能问题;不引新依赖)。
- 双向覆盖:system 段到达时先对照 `pending_mic`(丢匹配者)再入 `recent_system` 环形缓冲(保留 ~10s);mic 段到达时先对照 `recent_system`(命中即丢)再入 pending。
- 会话结束:pending 全部 release(排干,不丢内容)→ 再发 Snapshot。
- 局限(接受并记录):system 版若晚于 mic 版 2.5s 以上定稿则漏网;线下双源同句(极罕见)可能误杀 mic 侧——文本相似阈值 + 跨源限定把误杀面压到最低。

## 3. 测试

- 续录:writer 单测(resume 的 meta 翻转/seq 续接/base_ms/speakers 加载;快照存取 roundtrip;from_snapshot 编号续接);集成(resume 后 append 时间戳偏移正确);前端 check/build。
- 去重:worker 级单测(ScriptedRecognizer 可控文本):①mic 先 system 后同文本 → 只出 system;②system 先 mic 后 → mic 被丢;③文本不同/时间远 → 两条都出;④结束时 pending 排干;⑤hold 到期正常 release。
- 人工冒烟:中断/完成笔记各续录一次(说话人编号连续);他人电脑外放场景同句只出一条、单说话人。

## 4. 开放问题

- ECHO 三常量按真实会议二轮校准。
- 质心快照使 speakers.json 每人 +~1KB,规模无虞;跨会议声纹记忆仍不做。
