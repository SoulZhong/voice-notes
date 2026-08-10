# ASR 准确率提升调研:Qwen 大参数模型 / 候选模型全景 / 工程手段

> 2026-08-11。三路并行联网调研(Qwen3-ASR 大参数本地化、其他候选模型全景、不换模型的工程手段)
> + 仓库现状核对的综合报告。承接
> [2026-07-28 ASR 模型选型调研](./2026-07-28-asr-model-comparison.md) 与
> [ASR 系统提升方案](../../asr-system-improvement-plan.md)。
> 标注口径:**[官方]** = 官方文档/技术报告;**[三方]** = 社区实测;**[推测]** 单独注明。

## 0. 结论速览

1. **Qwen 更大参数 = 只有 1.7B,实时链路不可行,二遍链路可行但要换推理路线。**
   开源家族至今仅 0.6B / 1.7B(无 4B/8B,Flash API 参数量未公开)。1.7B 中文相对降错约
   14-17%(AISHELL-2 3.15→2.71,WenetSpeech meeting 6.88→5.88)。但 sherpa-onnx **没有
   1.7B 工件**(feature request #3535 无人接),CPU 实测 RTF ~0.4 贴着实时天花板、内存
   int8 估 4.5-6GB——实时链路(需每秒 partial)必然击穿。会后二遍链路可走 llama.cpp/GGUF
   (跨平台,Q8_0,RTF~0.4)或 MLX(仅 macOS,RTF~0.03-0.1,附送 ForcedAligner 时间戳)。
2. **本次最大发现:FireRedASR2(小红书,2026-02 开源,Apache-2.0),已进 sherpa-onnx,
   现有 1.13.4 运行时零改造可加载。** AED int8 变体:中文 CER 3.05%(4 公开集均值,优于
   Qwen3-ASR-1.7B 官表 3.76%)、**词级时间戳 + 置信度**(正中 diarization 依赖)、单线程
   CPU RTF 0.333。CTC int8 变体 RTF 0.173,可挑战实时链路。这是"开源可 CPU 跑的中文精度
   天花板",集成成本近零。
3. **准确率的最大杠杆不在实时引擎,在"会后二遍 + 热词 + 保守纠错"三件套。**
   仓库现状:重转写框架(PR#77)用的是与实时相同引擎;Qwen3 热词口传 `None`;LLM 润色
   未带术语表。三者都是已有管线上的增量改造(天级),业界量化收益:热词对实体召回相对提升
   25-75%,GER 三阶段纠错相对 CER 降约 20%。
4. **音频前端"做减法":不要给 ASR 前面加降噪。** 2025-12 系统性研究(4 ASR × 10 噪声条
   件)显示降噪后全部劣于原始带噪音频。该做的是:VAD pre-roll padding(打句首漏字)、
   查双 AGC 叠加、确认 48k→16k 走高质量重采样;Silero VAD v6(2025-08,噪声错误 -16%)
   可无痛升级。
5. **评测集(P0)仍是一切的前提**,与 07-23/07-28 两份文档结论一致:golden set 至今未建,
   没有它换任何模型/参数都无法证明是改善。工具(`tools/asr_eval.py`,CER/MER)已就绪,缺
   的只是 3-5 段真实会议标注数据。

## 1. 仓库现状核对(2026-08-11)

- 本地四引擎:SenseVoice-Small(默认实时)/ Paraformer-zh / Whisper-base / Qwen3-ASR
  0.6B int8(RTF 0.18-0.34);云端:火山 bigmodel 流式(`sauc/bigmodel_async`)+ 阿里
  `fun-asr-realtime` / `qwen3-asr-flash`(批式)。
- **热词未接**:`lib.rs:720` Qwen3Recognizer 实例化传 `hotwords=None`,engine 的口留好了。
- **二遍重转写未用强模型**:`lib.rs:2402` retranscribe 走 `current_asr()`——与实时同引擎。
- **ASR golden 评测集不存在**(仅 `scripts/refine_golden.py` 润色回归)。
- VAD 用 sherpa 发布的 silero_vad.onnx(版本旧于 v6)。

## 2. Qwen 更大参数模型:本地可行性

### 2.1 家族与精度 [官方]

| 测试集 | 0.6B | 1.7B | Flash-1208(API) |
|---|---|---|---|
| AISHELL-2 test | 3.15 | **2.71** | 2.53 |
| WenetSpeech net / meeting | 5.97 / 6.88 | **4.97 / 5.88** | 4.60 / 5.80 |
| LibriSpeech clean / other | 2.11 / 4.55 | **1.63 / 3.38** | 1.33 / 2.40 |

- 1.7B 实际总参 ~2.0B(300M AuT encoder + Qwen3-1.7B decoder)。开源家族无更大尺寸,
  无更新 checkpoint(截至 2026-08);Flash 与开源版同源 Qwen3-Omni 基座,参数量未公开。
- 中英 code-switch 分尺寸评测:官方与第三方均无,**查不到**。
- [三方] 量化敏感度:1.7B 在 Q4_K_M 下退化(WER 3.28% vs Q8_0 2.84%),**1.7B 必须 Q8**;
  0.6B 对 Q4 不敏感。

### 2.2 部署路线现状

| 路线 | 1.7B 支持 | 实测(1.7B) | 备注 |
|---|---|---|---|
| sherpa-onnx(现栈) | **无工件**,#3535 无人接 | — | 0.6B int8 是唯一官方工件 |
| llama.cpp/GGUF | 已合并(mtmd,2026-04) | M3 Air Q8_0 短句 ~3.1s/条;x86 CPU q4 混合 RTF 0.390 | 跨平台;>2min 长音频空输出 bug #21847(按 VAD 段喂可绕开) |
| MLX(仅 macOS) | 有(pip `mlx-qwen3-asr`) | M4 Pro RTF ~0.03-0.1,8-bit ~3.4GB | 附原生 ForcedAligner(<6ms MAE);需 Python sidecar |
| antirez/qwen-asr 纯 C | 有(BF16) | M3 Max 离线 RTF≈0.23 | 峰值内存 6.6-7.1GiB,过重 |
| OpenVINO(Windows) | 有(社区 int8) | 峰值内存 ~4.8GB | 无 RTF 数据 |

### 2.3 结论

- **实时链路:不上 1.7B。** 现栈无工件;CPU RTF 贴 0.5 天花板而 partial 重识别需要远高于
  1x 的吞吐;内存对 16GB 低配机太重;~15% 相对降错补偿不了实时性崩掉。
- **会后二遍链路:1.7B 是合理"精修档"**,但意味着引入第二运行时(llama.cpp 跨平台最省事,
  1.7B 用 Q8_0;macOS 可选 MLX 更快且解决时间戳)。若坚持 sherpa-onnx 单栈,现实选项是
  催/贡献 #3535,或二遍继续 0.6B+云端兜底。
- 已知风险同 0.6B:热词注入放大空音频幻觉(#3509,沿用现有电平门控/VAD 前置设防);
  无 token 时间戳,sherpa 生态短期无解(#3552),llama.cpp/MLX 路线有外挂对齐器可用。

## 3. 其他候选模型全景(2026-08)

### 3.1 对比表(候选按链路分组;基准口径不同,横比只看同一行来源)

**实时引擎候选(RTF<0.5):**

| 模型 | 参数 | 许可 | 中文精度 | 本地运行时 | CPU RTF | 时间戳 | 热词 |
|---|---|---|---|---|---|---|---|
| SenseVoice-Small(现役) | 234M | 可商用需署名 | AISHELL-1 2.96 | sherpa-onnx;官方 llama.cpp/GGUF(2026-06 新增) | ~0.1 | token 级 | 无 |
| **FireRedASR2-CTC int8** | ~0.7B(740MB) | Apache-2.0 | CTC 版未单独公布(家族 AED 3.05) | **sherpa-onnx ≥1.12.27(现栈已覆盖)**,伪流式 | **0.173**[官方] | 有 | 无 |
| Fun-ASR-Nano-2512 | 0.8B | 开源(条款落地前核) | 官方工业口径 WER 9.38;FireRed 对比表 4.55 | sherpa-onnx ≥1.12.22;社区 GGUF | ~0.09[三方 GGUF] | 字级 | **有(prompt 式)** |

**会后二遍候选(RTF<5):**

| 模型 | 参数 | 许可 | 中文精度 | 本地运行时 | CPU RTF | 时间戳 | 热词 |
|---|---|---|---|---|---|---|---|
| **FireRedASR2-AED int8** | ~1.2B | Apache-2.0 | **3.05**(4 集均值;对比表中优于 Qwen3-1.7B 3.76 / Doubao 3.69) | **sherpa-onnx(现栈零改造)** | **0.333**[官方] | **词级+置信度** | 无 |
| Qwen3-ASR-1.7B Q8 | ~2.0B | Apache-2.0 | AISHELL-2 2.71 | llama.cpp / MLX(§2) | ~0.4 | 外挂对齐器 | 有(prompt) |
| Belle-whisper-v3-turbo-zh | 809M | Apache-2.0 | WenetSpeech meeting 13.36 | whisper.cpp | 1-2x CPU | 段级 | 无 |

**排除**:FireRedASR2-LLM(2.89%,最强但 GPU 级)、Kimi-Audio / Step-Audio 2 / GLM-4-Voice /
Baichuan-Audio / MiniCPM-o(7B+ 全部无 CPU 转写路径)、NVIDIA Parakeet zh-en(仅 Riva NIM,
拿不到权重)、原版 Whisper turbo(meeting 20.3%,不及格)、腾讯混元 Hy ASR 3.0(2026-08-04
preview,WER 3.34%,但 API 内测未开源,观察)。

### 3.2 重点:FireRedASR2(2026-02-12 开源)

- 一体化系统 FireRedASR2S:ASR + FireRedVAD(100+ 语种)+ FireRedLID(方言识别 97.18%)+
  FireRedPunc 中英标点;支持普通话、20+ 方言、英语、**中英混说**;AED 变体官方确认词级时间
  戳 + 置信度。
- sherpa-onnx 官方工件:`fire-red-asr2-ctc-zh_en-int8-2026-02-25`(740MB,RTF 0.173)、
  `fire-red-asr2-zh_en-int8-2026-02-26`(AED,encoder 779MB + decoder 398MB,RTF 0.333),
  需 ≥1.12.27——**现用 1.13.4 已覆盖**。
- 短板:无热词(sherpa ContextGraph 仅 transducer);1.2GB 体积。

### 3.3 Fun-ASR-Nano-2512(阿里,SenseVoice 官方接班人)

- 0.8B = SenseVoice encoder + Qwen3-0.6B decoder,真流式训练对齐,**全场唯一
  "本地 + 热词 + LLM 上下文"齐活**;sherpa-onnx config 已含 Hotwords/SystemPrompt 字段。
- 普通话基准弱于 FireRed(对比表 4.55 vs 3.05);LLM decoder 幻觉/漏字需长音频专测。
- 更大的 Fun-ASR / Flash / Realtime 是百炼 API,未开源。本应用云端阿里通道已在用
  `fun-asr-realtime` / `qwen3-asr-flash`。

### 3.4 其他动向

- SenseVoice 无新 checkpoint(Small 仍是唯一开源权重);2026-06 官方加 llama.cpp/GGUF
  单二进制路径(对本应用意义有限)。
- Paraformer 系无新版;SeACo 热词未进 sherpa,要用得引 FunASR runtime,不值。
- transcribe.cpp / audio.cpp("语音界 llama.cpp")均早期,不建议替换 sherpa-onnx。

## 4. 不换模型的改善手段(量化收益 × 成本)

按收益/成本比排序:

1. **VAD padding + 前端审计(天级)**
   - `speech_pad_ms` 两侧 200-500ms pre-roll 打句首漏字(共识做法;调低 min_speech 反而
     引噪声幻觉,sherpa issue #3035 同判断);拼接按样本数去重叠。
   - Silero VAD v6(2025-08-26):噪声域错误 -16%,API 兼容 v5,换文件即可;TEN VAD 端点
     更快但噪声鲁棒差(ESC-50 噪声 0.87 vs 0.42),会议场景不如 Silero v6。
   - **不加降噪**:2025-12 系统研究(4 ASR × 10 噪声条件 40 组)降噪后全劣于原始带噪
     (+1.1 到 +46.6 绝对 semWER);现代 ASR 自带噪声鲁棒性,降噪抹掉声学特征。AEC 是
     结构性干扰,保留。
   - 查双 AGC(VPIO 自带 + 其他增益叠加会泵效应)与重采样质量(线性插值有可测损失,
     确认走 soxr/rubato sinc 类)。
2. **二遍转写换强引擎 + 热词注入二遍(天级-周级)**
   - 重转写框架已在(PR#77),改造点:二遍允许更长段(30-60s VAD 重切 + 少量重叠)、
     引擎独立于实时设置(首选 FireRedASR2-AED,备选 Qwen3-1.7B/llama.cpp 或云端)。
   - 不做词级 ROVER(两系统融合典型仅 ~4% 相对收益):**二遍全文为准 + 按时间戳回贴说话
     人 + 低置信段保留一遍文本对照**,接近全部收益而成本低。Otter/飞书妙记/通义听悟公开
     信息也只能确认"双轨 + 精修替换",无工程细节。
   - 热词收益量化:动态词表偏置对 bias 词相对提升 25-75%;SeACo 论文热词召回 12%→60%;
     AssemblyAI 工业口径关键术语 +21%~57%。词表自动构建:日历(标题/参会人)+ 声纹人名库
     + 历史笔记高频专名(LLM 提取),按会前相关性截断到几十~几百词。
   - 落点:本地 Qwen3/Fun-ASR-Nano 的 prompt 热词口 + 云端两家的热词 API + LLM 润色
     glossary,三处;实时 SenseVoice 路径无热词入口,放弃。
3. **保守 GER 纠错升级现有润色(天级)**
   - SenseVoice 是 greedy CTC 拿不到 N-best,可行形态是"1-best 保守改写":glossary 注入 +
     编辑距离护栏 + 只改低置信区间 + 原文可回退。三阶段框架(判断-纠错-验证)+ 大模型
     相对 CER 降 ~21%(AISHELL 系)。
   - 本地小 LLM 零样本收益很小(Qwen2-7B 级"轻微改善"),微调后才显著(LoRA <10k 平行对);
     短期用云端大模型做会后纠错,长期攒数据。
4. **置信度导出与"标记而非删除"(天级-周级)**
   - CTC 后验取 token log-prob 按词聚合(min 聚合利于揪错词)是成熟做法;注意 softmax 过
     度自信需校准。sherpa-onnx 有进行中的 log-prob API(PR #2897,未确认合并、SenseVoice
     未明确列入);近期路径是 fork 导出或应用层直跑 logits。
   - 用途:低置信下划线标注 → 优先送二遍 → GER 只许改低置信区间,把 2/3 串成闭环;也是
     提升方案 Phase 1"过滤改标记"的技术件。
5. **域自适应微调(押后,但数据留存立刻做)**
   - FunASR 有 SenseVoice finetune 脚本(小数据必须 LoRA;社区口径 ~8h 音频 +14% 术语准
     确率,未经同行评审);ms-swift 支持 Qwen3-ASR LoRA。微调后 ONNX 重导出是隐藏成本。
   - **现在就该做的:留存"原文→用户改稿"纠错对**,成本近零,是未来 GER 微调/域微调的
     训练集。

## 5. 建议路线图

对齐 [提升方案](../../asr-system-improvement-plan.md) 的阶段:

1. **P0(不变,最高优先):建 golden 评测集**。3-5 段真实会议,JSONL(reference/entities),
   接 `asr_eval.py --max-cer --max-mer --min-entity-recall`。下面每一步都拿它验收。
2. **P1 快赢(天级)**:VAD pre-roll padding;Silero v6 升级;前端审计(双 AGC/重采样);
   Qwen3 引擎热词口接通(声纹人名 + 最近笔记专名,先手动词表验证收益)。
3. **P2 bake-off(评测集就绪后)**:FireRedASR2-AED int8(二遍首选)、FireRedASR2-CTC
   int8(实时挑战者)、Fun-ASR-Nano-2512(热词差异化)三者 + 现役四引擎,同 PCM 横评
   CER/MER/实体召回/RTF/内存。FireRed 两个变体在现栈零运行时改造,成本最低,先做。
4. **P3 双引擎架构落地**:实时 SenseVoice(或 CTC 胜者)+ 会后二遍强模型(bake-off 胜者,
   FireRedASR2-AED 概率最大)+ 热词 + 保守 GER + 置信度标记。Qwen3-1.7B 仅当 bake-off
   显示 FireRed 不够、且愿意引入 llama.cpp 第二运行时再考虑。
5. **持续**:留存用户纠错对;跟踪 sherpa-onnx #3535(Qwen3-1.7B)/#3552(时间戳)/
   #2897(置信度)与腾讯 Hy ASR 3.0 开源动向。

## 6. 主要来源

- Qwen3-ASR:https://github.com/QwenLM/Qwen3-ASR · 技术报告 https://arxiv.org/abs/2601.21337 ·
  https://huggingface.co/Qwen/Qwen3-ASR-1.7B · sherpa 文档 https://k2-fsa.github.io/sherpa/onnx/qwen3-asr/ ·
  issues k2-fsa/sherpa-onnx#3535/#3509/#3552 · llama.cpp#21847 ·
  https://github.com/antirez/qwen-asr · https://github.com/moona3k/mlx-qwen3-asr ·
  https://github.com/shershah1024/qwen3-asr-llamacpp · https://github.com/HaujetZhao/Qwen3-ASR-GGUF
- FireRedASR2:https://github.com/FireRedTeam/FireRedASR2S ·
  https://k2-fsa.github.io/sherpa/onnx/FireRedAsr/pretrained.html · sherpa-onnx CHANGELOG v1.12.27
- Fun-ASR:https://github.com/modelscope/FunASR · https://github.com/HaujetZhao/Fun-ASR-GGUF ·
  https://help.aliyun.com/zh/model-studio/asr-model/
- Whisper 系:https://huggingface.co/BELLE-2/Belle-whisper-large-v3-turbo-zh
- 热词:https://k2-fsa.github.io/sherpa/onnx/hotwords/ · SeACo https://arxiv.org/abs/2308.03266 ·
  https://arxiv.org/abs/2505.19179 · AssemblyAI keyterms(2025-10)
- GER:https://arxiv.org/abs/2505.24347 · https://arxiv.org/pdf/2508.07285 ·
  https://arxiv.org/abs/2601.21347
- 二遍/融合:WhisperX https://arxiv.org/abs/2303.00747 · MOVER https://arxiv.org/abs/2508.05055
- VAD:Silero v6 https://github.com/snakers4/silero-vad/discussions/678 ·
  TEN VAD https://huggingface.co/TEN-framework/ten-vad · sherpa#3035
- 前端:降噪伤 ASR https://arxiv.org/pdf/2512.17562(2025-12) · AGC https://pmc.ncbi.nlm.nih.gov/articles/PMC9027119/
- 置信度:sherpa-onnx PR#2897 · https://arxiv.org/abs/2212.08703 · https://arxiv.org/pdf/2101.05525
- 其他:腾讯 Hy ASR 3.0 https://www.qbitai.com/2026/08/465973.html(2026-08-04)
