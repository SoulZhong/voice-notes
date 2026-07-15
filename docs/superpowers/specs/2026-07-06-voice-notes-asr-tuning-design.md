# ASR 精修管线 + Paraformer 选型 设计

日期:2026-07-06。分支 worktree-asr-tuning。范围:A1 离线重聚类 + A2 LLM 精修 + A3 幻觉过滤强化 + B Paraformer 接入,用户拍板"四项全做"。

## 背景:与豆包的真实会议对比

样本:一场真实会议录音(约 54 分钟,具体笔记不入库),第三方转写工具同场纪要在 samples/。两边时间轴经文本对齐确认偏移≈0,可逐段对比。

| 维度 | 豆包 | 本系统 |
|------|------|--------|
| 说话人 | 7 人 | 45 个标签(40 个为碎片簇) |
| 文本 | 11651 字,155 段落 | 19014 字,450 碎段 |

**说话人识别是最大差距**:
- 过分裂:真实约 7 人打出 45 标签;豆包说话人 1 被摊到 25 个标签。
- 错误合并更严重:第二大簇 S8(865s)混入豆包说话人 2(429s)+说话人 4(237s)+说话人 1(177s),纯度 50%。在线单遍聚类质心带偏后无法回头。主簇 S13 纯度 90%。
- 声纹库零命中(库内 3 人 name 全空),7 段 speaker=None。
- 根因:嵌入按 ASR 段实时提取(中位段长 5.3s,62 段 <2s,短段声纹不可靠),registry 在线聚类(assign 0.62/merge 0.74)无会后全局重聚类兜底。

**ASR 文本:底子不差,输在后处理**:
- 实体不一致最痛:领域专名/机构名/生僻术语被本地 ASR 逐次拼成不同的近音错字(同一实体一场内出现三四种写法)。第三方工具全程一致(热词表+LLM 后处理),这是本地纯声学模型短板。
- 同音错字:带垃圾的、思情(精力)、肯计(肯定)、闭话(闭环)。
- 幻觉垃圾段 14 个:低 RMS 短段出「男人。」「播放。」「隔你。」;粤语漂移「唔,好嘅」。
- 口头语(嗯/呃/结巴)未清理;英文连写(PEVCVP);450 碎段不合并成段落。
- 亮点:多处比豆包更准(瀑布式 vs 豆包误识"分布式";发起这个项目 vs 豆包"发行项目")。SenseVoice 声学底子可用,差距在分离层与后处理层。

## 目标

1. A1:会话结束后离线全局重聚类,说话人标签数接近真实人数,拆开污染簇。
2. A2:LLM 云 API 精修(可开关):错字纠正+实体归一+口头语清理+段落合并。
3. A3:幻觉过滤强化,杀掉垃圾短段且不误杀真实短应答。
4. B:Paraformer-large 作为第三个 ASR 选型接入。

## 非目标

- 不动录制实时链路(VAD→ASR→在线聚类照旧,实时字幕体验不变)。
- 不做热词(sherpa-rs 0.6.8 未暴露 hotwords;实体一致性由 A2 承担)。
- 不做交叠说话(overlap)分离;pyannote 式完整离线管线(方案 C)另行立项。
- 不改声纹库增量入库逻辑。

## 已确认决策

- A2 默认关,配好 key 后引导打开。
- api_key 明文存 settings.json(本地单机应用,设置页注明;后续可迁 Keychain)。
- B 不做热词,实体交给 A2。
- LLM 接口统一 OpenAI 兼容 chat completions,预设 DeepSeek/通义 Qwen/豆包(Ark)/Kimi/OpenAI/自定义(base_url+model+api_key)。

## 一、总体架构

新增**会后精修管线**,stop→finalize 后异步执行,三步串行:

```
原始 segments.jsonl ──> ① 幻觉过滤(A3) ──> ② 离线重聚类(A1) ──> ③ LLM 精修(A2,可关) ──> refined.json
```

- 原始数据永不改写;精修产物单独落盘 `notes/<id>/refined.json`。
- UI 详情页默认展示精修稿,可一键切回原始逐字稿。
- ②③任一步失败不影响既有功能;笔记页提供「重新精修」手动触发。

## 二、A1 离线重聚类

停止后从 mic.m4a/system.m4a 解码 PCM(复用 afconvert 路径),按段边界切片:

- 时长 ≥1.5s 的段重提 CAM++ 嵌入(模型已有,无新下载),全局两两相似度 + **AHC 平均链接**聚类;阈值以本场样本对豆包 7 人 ground truth 校准初值,做成常量可调。
- 碎片治理:总时长 <8s 的小簇并入最近大簇;短段(<1.5s)不提嵌入,归属按「时间相邻 + 在线标签」投票。
- 聚类后跑声纹库种子匹配(沿用 SEED_ASSIGN_THRESHOLD 0.68),命中带出真实姓名。
- 重聚类结果只写 refined.json(paragraphs 的 speaker/name 字段自带映射),不改 speakers.json、**不回写声纹库累计**(停止时已入库,避免双计)。

## 三、A2 LLM 精修

设置页新增「智能精修」区块:总开关(默认关)、服务商预设+自定义三字段。

请求形态:按 A1 重聚类后的说话人连续区间分块(每块 ≤3k 字),低温度,要求结构化 JSON 返回。单次调用完成四件事:

1. 同音错字纠正(只改音近字,禁止改写语义);
2. 实体归一:全文人名/产品名/术语一致化;
3. 口头语轻度清理:嗯/呃/结巴重复,保留语气;
4. 同说话人相邻段合并成段落,保留起始时间戳。

失败/超时:该块保留原文,整体标「部分精修」,可重跑。开关旁明示「会议文本将发送至所选服务商」。

## 四、A3 幻觉过滤强化

对短段(<2s)做联合判定:RMS 极低 × 文本仅 1-2 有效字 × 语种漂移(yue/ja/ko)加权,命中打 `discarded` 标记(不删数据,精修稿不展示,原始稿灰显)。被标记段不参与 A1 重聚类,也不送入 A2。

- **保守优先**:真实短应答白名单(好/对/嗯/行/OK/可以…)永不过滤。本场 seq394「好。」seq399「对.」必须放过,「男人。」「播放。」「隔你。」必须命中。
- 阈值用本场 14 个垃圾段做回归夹具校准。

## 五、B:Paraformer-large 接入

照搬 whisper 接入先例:

- manifest 新增 sherpa-onnx 导出 paraformer-zh int8 工件(URL/sha256/字节数 **plan 阶段核实钉死**)。
- `asr_model` 增加第三值 `"paraformer"`;识别器工厂加 `ParaformerRecognizer`(sherpa-rs 0.6.8 已封装,greedy_search)。
- 设置页单选加一项,注明「中文准确率更高,英文较弱,约 800MB」。
- 结果带 tokens+timestamps → 段内说话人分离不降级(优于 whisper);lang 标签非 SenseVoice 格式 → 语言过滤直通(沿用 whisper 降级路径,A3 的 RMS×短段规则仍有效)。
- 默认仍 SenseVoice,下一场录制生效(沿用现机制)。

## 六、数据模型

```
notes/<id>/refined.json
{ schema_version, generated_at, llm_model,
  stages: { filter: done|skipped, recluster: done|failed, llm: done|partial|off },
  paragraphs: [ { speaker, name?, start_ms, end_ms, text, source_seqs: [seq...] } ] }
```

`source_seqs` 保留到原始段映射,UI 可做精修↔原始对照。settings.json 增加:

```
refine_enabled: bool(default false)
refine_provider: 预设名或 "custom"
refine_base_url / refine_model / refine_api_key: String
```

## 验证

- 本场会议做 golden 夹具:脚本输出聚类标签数、对豆包混淆矩阵纯度、垃圾段过滤命中/误杀率,A1/A3 调参有量化回归。
- A2 用 mock server 测分块/失败/重试/部分精修;不真调云。
- Paraformer 固定 wav 断言非空文本 + timestamps 非空。

## 边界与错误处理

- 续录会话(快照恢复):精修在最终停止后跑一次全场;录制中不允许精修。
- refined 存在时再录同 id 续段:作废旧 refined 并提示重跑。
- 精修管线全程 spawn 后台线程,panic 捕获,失败落 stages 状态,UI 可见可重试。
- 无网/无 key/开关关:①②照跑(纯本地),③跳过,refined 仍生成(stages.llm=off)。
