# P4 说话人区分 + AEC — 设计文档

- **日期**:2026-07-03
- **状态**:已通过头脑风暴评审,待编写实现计划
- **上游**:总设计 `2026-06-30-voice-notes-design.md` §4(双流与说话人区分);P3/P3.5 已合入(PR #3);冒烟反馈驱动(「我/对方」重复 = 回声、多方无法区分)
- **分支**:`p4-diarization-aec`(栈在 `p3-storage-notes` 之上)

## 1. 目标与范围

| 包含 | 不包含 |
|---|---|
| 说话人区分:两路声纹汇入同一在线增量聚类 → 全局「说话人 1..N」 | 段内多说话人切分(段级单标签,二期) |
| 段定稿即带 speaker 标签(1–2 秒内上屏);簇合并事件回写 | 会后重聚精修(需存全程音频) |
| speakers.json 持久化 + chips 改名实时回填;「我」靠认领 | 声纹跨会议记忆(每场会议独立聚类) |
| AEC:mic 采集换 CoreAudio VoiceProcessingIO(spike 先行,可回退) | 文本级回声去重(AEC 实测后再定,见开放问题) |
| 降级链:模型缺失→回 P3 我/对方行为 | Windows/Linux AEC |

决策记录(评审确认):两路都聚类(非 mic 固定「我」);实时增量聚类(非会后批处理);改名持久化;AEC 与 diarization 同阶段做。

## 2. 识别管线扩展

```
段定稿(VAD 完整句) ──→ ASR worker:
   1. SenseVoice 识别 → text            (现有)
   2. SpeakerEmbedding 提取 → 向量       (新)
   3. SpeakerRegistry.assign(向量) → "S3" (新,在线聚类)
   → FinalEvent { ..., speaker: "S3" } + 落盘 SegmentRecord.speaker
```

### 组件

- **`SpeakerEmbedder`**(trait,新):`embed(&mut self, samples: &[f32]) -> Result<Vec<f32>>`。真实实现包 sherpa-onnx speaker embedding 模型(3D-Speaker 系 ONNX,几十 MB,加入 `fetch_models.sh` 与启动预载);测试用 MockEmbedder。模型实例与 recognizer 同样常驻复用:`AppState` 增加并列的 `embedder_cache` 槽,预载/取用/归还策略与 `recognizer_cache` 完全一致。
- **`SpeakerRegistry`**(纯逻辑,新,无模型依赖):
  - `assign(&mut self, embedding, source) -> SpeakerId`:与各簇质心算余弦相似度;≥ 阈值(初始 0.6,实现期在 fixture 上校准)归入并增量更新质心(running mean),否则新建 `S{n}`,记录 source 进 `sources`。
  - `check_merges(&mut self) -> Vec<(SpeakerId, SpeakerId)>`:周期性(每 N 次 assign)检查簇间质心相似度,超合并阈值时合并(小簇并入大簇),返回合并对。
  - 短段守卫:时长 < ~1s 的段**不建新簇**——相似度达标则归入,否则返回 None(该段 speaker 为 null)。
  - 单线程持有于 ASR worker,无锁需求。
- **事件扩展**:
  - `FinalEvent` 增加 `speaker: Option<String>`。
  - 新事件 `"speakers"`:`{ speakers: [{ id, name }] }`——新说话人出现、改名、合并后全量推送(简单可靠,量小)。
  - 合并发生时:ASR worker 通过回调通知会话层,会话层**重写落盘**(把 segments.jsonl 中被并簇的 speaker id 改写——jsonl 逐行重写到临时文件后原子替换)并 emit `"speakers"`;前端按新表重渲染。

### 与双流的关系

两路段串行进同一 ASR worker(现有结构),天然共享同一 Registry;来源只作 `sources` 元数据,不影响聚类。

## 3. 数据模型与改名

- **`SegmentRecord.speaker`**:`null` → `"S1"` 等(schema_version 不变,P3 预留位)。
- **`speakers.json`**(每会议文件夹,原子写):

```json
{ "S1": { "name": "说话人 1", "sources": ["system"] },
  "S2": { "name": "我", "sources": ["mic"] } }
```

- **改名**:录制页与详情页顶部**说话人 chips 条**(id → 名字),点击就地编辑 → 新 command `rename_speaker(note_id, speaker_id, name)` 更新 speakers.json → emit `"speakers"`(录制中)/前端刷新(详情)。前端一律按 id 查名渲染,改名即全文回填。「我」= 用户把自己那簇改名(chips 提供快捷「这是我」)。
- **读取**:`get_note` 返回 `Note` 增加 `speakers: HashMap<String, SpeakerMeta>`;导出 label 优先 speakers.json 名字,缺省「说话人 N」;speaker null 的段退回 我/对方(P3 行为)。
- **`NoteWriter`**:录制期持有 speakers 表,新说话人/合并/改名时原子写 speakers.json(与 meta 同策略)。

## 4. AEC(回声消除)

- **问题**:外放时对方声音被 mic 复录,同句话在两路各出一遍。
- **方案**:`Microphone`(cpal)替换为 **CoreAudio VoiceProcessingIO**(Apple 内建 AEC/降噪,FaceTime 同款)——扬声器信号在系统层从 mic 消除,治本。
- **Spike 先行**(P4 第一个任务):用 coreaudio-rs 起 VPIO 单元拿 f32 帧,验证:①可行性与权限;②输出采样率/声道;③AEC 处理后音频对 ASR 与声纹嵌入质量的影响(实测转写一段)。不可行 → 回退 Swift 垫片(P2 SCKit 同策略);质量不可接受 → AEC 降级为可选开关。
- `AudioCapture` trait 不变,新实现 `VpioMicrophone` 与现有 `Microphone` 并存;VPIO 初始化失败运行时回退 cpal(日志,不阻塞录制)。
- 文本级去重不做;AEC 实测后残留重复再评估(开放问题)。

## 5. UI

- **说话人 chips 条**(录制页 + 详情页,转写区上方):`[我] [说话人 2 ✎] [说话人 3 ✎]`,点击就地改名,「这是我」快捷项;录制中随 `"speakers"` 事件增长。
- **徽章升级**:转写段徽章从 我/对方 改为说话人名(speaker null 时退回 我/对方);颜色按 speaker id 从固定调色板取(稳定、区分度高,深浅色主题各一组)。
- 来源(mic/system)降级为次要信息(徽章 tooltip 或小图标),不再是主标识。
- 降级横幅:embedding 模型缺失时提示「说话人区分不可用(缺模型)」+ 重试下载入口(沿用 fetch_models 思路,提示手动执行;应用内下载引导仍属 P5)。

## 6. 错误处理

| 场景 | 处理 |
|---|---|
| embedding 模型缺失/加载失败 | speaker 全 null,UI 回 P3 我/对方行为 + 横幅提示;录制不受影响 |
| 单段提嵌入失败/过短 | 该段 speaker null,不影响后续 |
| 合并重写 segments.jsonl 失败 | 保留旧 id(前端映射表仍可渲染),日志 + storage degraded 事件 |
| VPIO 初始化失败 | 回退 cpal 采集,日志;AEC 缺席仅意味着外放场景可能重复 |
| speakers.json 损坏 | 以 id 兜底显示「说话人 N」 |

## 7. 测试策略

- **SpeakerRegistry 单测**(合成向量,确定性):阈值归簇/新建;质心 running mean;短段不建簇;合并检测与小并大;sources 记录。
- **ASR worker 集成**:MockEmbedder(按脚本返回向量)+ CountingRecognizer → 断言 FinalEvent.speaker 序列与落盘一致、speakers.json 内容正确、合并后 jsonl 重写正确。
- **真实模型**:多说话人预录 fixture 的 `#[ignore]` 集成测试(段→嵌入→聚类数正确)。
- **前端**:chips 渲染/改名回填/徽章调色板走查;`npm run check`。
- **AEC**:spike 报告 + 人工冒烟(外放开会,确认同句话不再两路重复;耳机场景不回归)。

## 8. 开放问题 / 实现期再定

- sherpa-rs 0.6.8 是否暴露 speaker embedding API(疑为 `speaker_id` 模块)——spike/计划期核对,不行则升级 sherpa-rs 或直接 sherpa-onnx C API。
- 聚类阈值与合并阈值的具体数值——用 fixture 校准,作为常量并注释来源。
- AEC 后 mic 音频对声纹的影响(VPIO 有 AGC/降噪)——spike 实测。
- 残留回声重复是否需要文本级去重兜底——AEC 冒烟后定。
- embedding 模型选型(3D-Speaker ERes2Net vs CAM++ 等)——以 sherpa-onnx 官方发布、中文效果优先,计划期定并写死下载地址。
