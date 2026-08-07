# 文件重转写(录音双方案 · 第三期)设计

> 2026-08-07。母 spec:`2026-08-06-audio-scheme-ab-design.md` §`TranscribeInput`。
> 本文是该节的落地细化与两处偏离记录;冲突处以本文为准(母 spec 的架构原则不变)。

## 背景与动机

母 spec 已确认:**本仓无从文件重转写的能力**,ASR 只在录制期实时跑。三期把这条链路
从零建起来。直接动机有二:

1. **修复受污染历史笔记**。实时 AEC3 在内置扬声器场景上线以来一贯失效
   (2026-08-07 定因:SCK 参考交付迟到破坏因果性,已修,PR#76),约 40 场录音的
   转写文字被回声污染。清洗后的干净音频(`mic.m4a`)一直在盘上,文字却只能靠
   重转写重新生成——这是修复它们的唯一途径。
2. **补齐 A/B 对比的第三条腿**。回放切换(二期)之外,「混音是否伤转写准确率」
   需要固定回放、只切 ASR 来源才能验证,故文件 ASR 独立于回放切换。

## 目标 / 非目标

**目标**

1. `TranscribeInput` 双实现:`DualTrackInput`(双轨,声纹保真)/ `MixedInput`
   (成品轨,按母 spec §降级口径)
2. 详情页可对单笔记发起重转写,来源可选(双轨 / 成品轨),与录制、refine 互斥
3. MCP 工具 `retranscribe_note`,支持外部批量驱动 40 场修复
4. 人工说话人归属尽量不丢:声纹种子命中 + 时间重叠继承兜底
5. 破坏性覆盖有退路:一次性备份 + 提交门 + 原子切换

**非目标**

- 不做 mixed 轨的离线补生成(属二期;无 mixed 产物时按钮置灰并说明)
- 不做回放方案切换(二期)
- 不自动串联「再 Aing」:重转写只标失效,LLM 花销由用户按需触发
- 不改实时链路;`segment_worker`/`FinalSink` 原样不动

## 管线

新模块 `src-tauri/src/retranscribe/`,全离线,单笔记串行(全局同时只跑一个任务):

```
track_pcm(note_dir, src) ─→ SileroSegmenter(每轨新实例) ─→ PendingSegment{samples, start_ms, source}
     (m4a/wav → 16k f32)        (离线切段)                        │
                                                                  ▼
                  Recognizer(new_recognizer 新实例) ─→ 语言过滤 ─→ split_final 段内切分
                                                                  │
                                                                  ▼
                  离线回声去重(时间轴重叠 + 文本相似)【仅双轨】
                                                                  │
                                                                  ▼
                  说话人归属(SpeakerRegistry::with_seeds,只读)─→ 时间重叠继承兜底
                                                                  │
                                                                  ▼
                  NoteLock 下原子提交:segments.jsonl / speakers.json / 清抑制表 / aing 标失效
```

要点(全部复用既有积木,新造的只有粗体两处):

- 解码走 `store::transcode::track_pcm`(wav 直读,m4a 经 afconvert 解临时文件,
  成败都清 tmp)。**绝不用 `decode_note_to_wav`**——它成功后删 m4a,是续录专用的
  破坏性路径。
- 时间轴口径与 `refine::slice_range` 相同:`文件内毫秒 + offset_ms == 段时间轴毫秒`,
  即 `start_ms = offset_ms + 样本位置 / 16`。`offset_ms` 从 `audio.json` 读。
- 切段每轨一个新 `SileroSegmenter` 实例(它是 per-instance 绝对样本号计数器,
  不能跨轨复用——`lib.rs` 补识路径同款注释)。
- 识别器用 `new_recognizer(current_asr, provider)` 起**独立实例**,不碰常驻
  `recognizer_cache` 槽,避免与实时录制争用。
- 语言过滤与段内切分复用 `session.rs` 的 `is_foreign_final` / `split_final`
  (需要的话调 `pub(crate)` 可见性),与实时链路口径一致。
- **离线回声去重**(新造,纯函数):实时链路的回声去重靠墙钟 hold 计时
  (`Instant::now()`),离线全部数据已知,改为按时间轴重叠 + 文本相似判定
  mic/system 跨轨重复,判中者弃 mic 侧。判据为「时间重叠占比 ≥ 阈值 且 文本相似
  ≥ 阈值」,阈值初值在实施计划中定标并由单测锁死。仅双轨模式需要;清洗后的 mic 轨
  大多已无回声,此层是兜底不是主力。
- **说话人归属**(新造编排,组件全复用):见下节。

### `MixedInput` 前置完整性校验

`mixed_track()` 返回 `Some` **不等于**内容完整(一期文档明示:回滚失败、混音线程
panic 后 `Drop` 补合法头,两条残轨路径都没有盘上标记)。消费前必须交叉校验:拿两条
源轨的 `sync.track_ms` 对 `mixed` 的 `duration_ms`(扣掉后启动源的前导偏移),
偏差超容限即拒绝重转写并在 UI 说明原因(容限初值 **500ms**,与已知的起流错峰残余
同量级,待实测校准)。校验不过 ≠ 轨损坏,只是不可信,不删不改。

## 说话人归属

组件复用:`SherpaEmbedder` + `SpeakerRegistry::with_seeds(快照, seed_clusters)`。
编排为三步:

1. 每段 `embed` + `assign_tracked`。种子经 `load_voiceprint_seeds`(含嵌入模型标签
   一致性门禁)。双轨模式三闸完整(0.68 同信道 + AS-Norm z 通道 + 裸阈下限);
   Mixed 模式按母 spec §降级口径:z 通道关、`Source::Mixed` 不分信道。
2. 声纹未命中到已建档人物的段(拿到的是场内 S 编号),按与**旧 segments** 的时间轴
   重叠继承旧 speaker 标注:同 source 优先,取重叠占比最大者;Mixed 模式与任意
   source 的旧段比。人工改名/关联过人物的归属由此保住。
3. 用 registry snapshot + 继承结果重建 `speakers.json` 整表(`person_id` 关联一并
   带过来)。

### 偏离母 spec ①:双轨模式也只读声纹库

母 spec §降级口径写双轨模式「写入种子簇正常、参与自动归并」——那是给**新录笔记**
A/B 的口径。重转写的主用例是修复历史笔记,其旧音频已经污染过一轮声纹库,拿重转写
结果再回写等于二次污染。故**重转写路径一律只读声纹库**:不设 enroller、不写种子、
不参与自动归并(`take_merges` 只用于场内簇归并,不落库)。将来若要给新录笔记的
重转写开回写,再单独评估。

## 提交与安全网

| 环节 | 做法 |
|---|---|
| 互斥 | 全程 `NoteLock::acquire`(与录制 `NoteWriter`、编辑整表重写、refine 提交同一把锁,跨进程互斥免费);后端 `reject_if_active` + 每笔记 in-flight 标记,重转写 ⊥ refine 互相拒绝;前端 `disabled={recording.isLive \|\| refining \|\| retranscribing}` |
| 备份 | **首次**重转写前把 `segments.jsonl` 复制为 `segments.orig.jsonl`(已存在则不覆盖)——破坏性覆盖的唯一回退路(偏离母 spec ②:母 spec 只要求旁文件+原子切换,本文加一次性备份) |
| 原子性 | 新 segments 写 tmp 再 rename(照抄 `write_jsonl_atomic`);`speakers.json` 走 `write_speakers_atomic` |
| 抑制表 | `segment-suppressions.jsonl` 一并清空(seq 全变,不清会按旧 seq 误伤新段) |
| 提交门 | 识别器构造失败 → 整体放弃;新结果 0 段、或 `[识别失败]` 占位段超 50% → 整体放弃不落盘,旧文本原样保留 |
| aing 失效 | `RefinedDoc` 加 `stale: bool`(serde default false),提交时置 true;详情页 Aing 页签提示「段落已重转写,需要再 Aing」,旧润色稿仍可读。不删 aing.json(全仓本无删除路径,不新开) |
| align.json | 删除(它是旧 mic 时间戳的展示侧纠正,新段时间戳直接来自切段,旧映射不再适用) |

## 入口与进度

- **详情页**:「重转写」按钮 + 来源选择(双轨 / 成品轨)。成品轨无产物或完整性
  校验不过 → 置灰并提示(补生成属二期)。破坏性操作走二段确认(照「再 Aing」
  `confirmRefine` 样板)。按钮可用性同互斥表。
- **进度**:照 `RefineProgress` 模式,`Msg::RetranscribeProgress{note_id, stage,
  state}` → lifecycle actor 统一 emit `retranscribe` 事件(`src/lib/events.ts` 注册)。
  阶段:`decode` → `transcribe:<track>` → `attribute` → `commit`;state 沿用
  running/ok/error。
- **MCP 工具** `retranscribe_note(note_id, input?)`(默认 dual):同步跑完返回摘要
  {旧段数、新段数、声纹命中数、继承数、弃用数}。40 场批量修复由外部按篇串行驱动,
  不做内建批量队列。

## 测试

- **纯逻辑单测**(不碰模型/盘):时间轴映射(offset_ms 口径,含中途出现的轨)、
  重叠继承(边界:零重叠、并列、跨 source)、离线回声去重、Mixed 降级门
  (MockEmbedder + registry,断言 z 通道关闭、无库回写)、提交门(0 段 / 占位段超限)。
- **集成**:fixture WAV + `MockSegmenter` / `ContentDigestRecognizer` 走全管线,
  断言 segments.jsonl / speakers.json / 备份 / 抑制表清空 / stale 位;notelock
  互斥(持锁时发起 → 拒绝)。
- **真机冒烟必做**(Chromium 假通过是惯犯,PR#65/#66 前科):真实历史笔记重转写
  前后对照、录制中按钮置灰、进度事件、MCP 批量两篇。清单随 PR 出。

## 已知限制

1. 重转写后的段时间戳来自离线切段,与旧段边界不重合是常态;时间重叠继承在旧归属
   本身就错(污染音频算出来的)时会照抄错误——这是兜底不是纠错,声纹命中才是主力。
2. Mixed 模式识别率预期低于双轨(母 spec 已知限制 2),对比须以双轨为基线。
3. 历史笔记若 m4a 转码时有损(AAC),重转写基于有损音频,与当年基于 WAV 的实时
   转写不完全可比。
4. `[识别失败]` 占位段 ≤50% 时照常提交,残留占位段与实时链路同待遇(可编辑/删除)。
