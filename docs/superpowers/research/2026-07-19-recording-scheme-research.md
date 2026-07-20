# 录音方案系统调研:各场景忠实还原 + 快速 ASR

> 2026-07-19。四路并行调研(采集前处理链 / 存档保真 / 快速 ASR / 竞品对标)的综合报告。
> 只做方案,不动代码。证据链接见各节;竞品源码级证据来自 screenpipe/meetily 仓库实读。

## 0. 结论速览

1. **现有架构方向被全面验证,不需推倒**:双路独立分轨(mic/system 不混音)是无 bot 会议笔记产品的行业共识形态(Granola/MacWhisper/Krisp/screenpipe 全部如此);「实时快稿 + 会后精修」两遍式与 Azure 2025 年产品化的 Post-Stream Refinement 同构;分轨双文件存档已是说话人归属的最优结构(「最好的 diarization 是不需要 diarization」)。
2. **最大的还原度隐患在存档层**:当前 mic 轨落盘的是 AEC 处理**后**的信号——AEC 双讲削声等处理损伤**不可恢复**。档案学与产品实践(Zoom Original Sound、Plaud RAW、Riverside)一致:原始音频是不可再生资产,处理链应可版本化重跑。
3. **最反直觉的发现**:对现代 ASR,**前置降噪弊大于利**——2025 年系统性研究 40 组配置(4 模型×10 噪声条件)中全部是带噪原始音频 WER 更低(增强使 WER 绝对上升 1.1%–46.6%)。NS 应只服务「人听的回放」,送 ASR 的信号处理越少越好(回声是相关干扰必须除,环境噪声留给模型扛)。
4. **快速 ASR 的最大跃升点**是会后二遍终稿换 FireRedASR-AED int8(已进 sherpa-onnx,中文 SOTA 档);实时层 SenseVoice 在「CPU 实时+中英混说」约束下仍是 2026 年最优,不动。
5. **两个可能改变产品形态的平台机会**:macOS 14+ 的 `AUVoiceIOOtherAudioDuckingConfiguration`(官方 API 调 VPIO ducking,可能让「保持外放音量」模式的整条自建软 AEC 链降级为兜底);CoreAudio Process Tap(14.4+,cpal 0.17.3 起可用)把系统声采集的权限从「屏幕录制」降为「仅系统音频」,竞品正集体迁移。

---

## 1. 场景矩阵:还原难点 × 现状 × 对策

| 场景 | 还原难点 | 现状评估 | 对策(→ §节) |
|---|---|---|---|
| **耳机线上会议** | 几乎无回声;唯一坑是经典蓝牙开麦 A2DP→HFP 切换(音质断崖+延迟漂移) | 最好的场景;但默认 VPIO 仍在做不必要的处理 | 耳机检测→尽量少处理;HFP 时提示用户音质受限(§2.4) |
| **外放线上会议** | 扬声器回声;蓝牙外放延迟 300–1200ms 超 AEC3 工作窗 | 已是业界正确形态:VPIO/AEC3 二选一(单一所有者规则)+蓝牙预对齐+DTLN 残余+文本去重 | AUVoiceIO ducking API 实验(§2.1);送 ASR 的链路去 NS(§2.3) |
| **线下多人围桌** | 远场衰减、混响、多人重叠;单机最难场景 | 最弱场景:单 mic 近讲假设,远端人声电平低、混响重 | 引导切 Wide Spectrum 麦克风模式;外接会议麦白名单(设备自带 AEC 则关软件 AEC);重叠分离/去混响放离线精修(§2.5) |
| **混合会议** | 同室他人设备外放自己的声音=新回声源(Google Meet 靠超声波设备协同,单机产品做不了) | 文本级回声去重兜底恰好覆盖这类 | 维持;记为已知边界(§2.4) |
| **纯系统声**(record_system_only) | 无 | 数字原声,最干净;已有 | 不动 |

---

## 2. 采集与前处理层

### 2.1 AEC:混合范式已验证;VPIO ducking 有官方新解

- 行业格局:混合式(线性 AEC + 神经残余抑制)是当前最优范式(ICASSP AEC Challenge 冠军 ByteAudio、微软 DeepVQE 均此结构)。本项目「AEC3/VPIO + DTLN-aec 离线残余」正是该范式的本地化变体,**方向正确**。
- WebRTC APM 固定顺序 高通→AEC3→NS→AGC2;AEC3 设计覆盖约 20–200ms 延迟,蓝牙漂移是其最难场景——「蓝牙预对齐 + AEC3」是业界同款做法(Interspeech 2024 有专门的低复杂度预对齐估计器可对表)。
- **关键机会**:macOS 14+ `AUVoiceIOOtherAudioDuckingConfiguration`(WWDC23)提供 `mEnableAdvancedDucking`(按语音活动动态调节)+ `mDuckingLevel`(档位)。「保持外放音量」模式存在的根因就是 VPIO ducking 压外放 12–16dB——**若该 API 实测有效,默认路径可变成「VPIO + Min ducking 配置」,整条自建 AEC3 软链降级为兜底**,复杂度大降。当年确认「Min 档仍 ducking」时是否用过这个 API 值得复查。建议:一个下午的 spike 实验,优先级最高。
- 神经 AEC 演进方向:超轻量化(EchoFree 278K 参数/30 MMACs 对标 DeepVQE-S)。DTLN-aec(0.97ms/帧 @i5)实时化用作 Windows 声学 AEC 在算力上完全可行(接续 Windows 分支的 stub 后续路线)。
- 单一所有者规则(每平台只能有一个回声消除者,绝不叠加)已被现架构遵守;外接会议麦(Jabra/Anker 自带硬件 AEC)接入时应按设备名白名单**关掉软件 AEC**。

### 2.2 存档保真:原始信号是不可再生资产(本报告最重要的架构建议)

- 现状:mic 轨在 segment_worker 内 AEC **之后**落盘(设计初衷:回放与转写一致)。代价:AEC 双讲削声、AGC 抬底噪等处理损伤永久固化,未来更强的模型救不回。
- 业界一致实践:原始只读不可变、处理作用于副本、可版本化重跑(Zoom Original Sound 旁路、Plaud RAW WAV 双份、Riverside 本地原始轨是卖点)。本项目已有的「离线清洗」形态天然适配:**system 轨本身就是 AEC 参考,存原始 mic 轨 = AEC 永远可离线重跑**。
- 方案选项(按侵入度):
  - A(推荐):mic 落盘改存 **pre-AEC 原始**,实时 AEC 只喂 VAD/ASR;回放用的「干净版」由离线清洗产出并缓存(aing 标记已有 soft_aec 场次概念,扩为处理链版本号)。空间不变,回放链要接受「刚录完回放=原始(可能带回声),清洗后=干净」的时序。
  - B(保守):双份落盘(原始 WAV 会后即压)——空间×2 期间窗口,逻辑最简单。
  - C(现状+):维持现状,但把 AEC 后的削声段(AEC 统计可标)记进元数据,至少让精修层知道哪里不可信。
- 采样率/编码:**16k 存档维持**(Whisper/新一代 ASR/pyannote/ECAPA 全部 16k 输入,48k 只为回放质感,3× 空间不值);32kbps AAC 在「WER 无显著劣化」安全区(Opus 32k 与未压缩统计不显著、24k 相对差 <1.5%,AAC 略逊但同档);零成本优化=afconvert 换 **HE-AAC**(`-d aach`,同码率感知质量明显更好);Opus 24k VBR 是技术最优但 Rust 侧 Symphonia 无 Opus 解码、macOS 生态摩擦,列为可选。
- 分轨双文件维持不动(业界最佳结构;多轨单容器 m4a/mka 播放器生态差,Craig/Riverside 都是每源一文件)。

### 2.3 降噪/AGC:为「人耳」和「ASR」分道

- **NS 伤 ASR 有量化实锤**(arXiv 2512.17562 等):现代 ASR 自带噪声鲁棒性,前置增强抹掉细粒度谱结构反而升 WER。现状「保持外放音量」链 AEC3+NS(Moderate)+AGC2 的输出同时喂 ASR——**值得 A/B:送 ASR 的分支只过 AEC(必须,相关干扰)不过 NS**;NS 只留给回放轨/离线清洗(清洗链 NS High 用途是人听,合理)。
- AGC:实时 AGC2 仅为 VAD 可切段与回放电平服务(现有参数是实验选定的,保留);更忠实的做法是存档后按 EBU R128(语音 −16~−19 LUFS)做**逐轨离线响度归一化**,现代 ASR 对电平本身不敏感。
- 轻量 NS 备选(若要给回放轨升级):GTCRN(48K 参数,RTF 0.07,已有 sherpa-onnx 集成)> RNNoise;DeepFilterNet3 效果更强但 ~40ms 延迟。去混响(WPE 类)只放离线,不上实时链。

### 2.4 设备场景细节

- 耳机检测→少处理(不开 AEC、NS 最轻),忠实度最高;经典蓝牙 HFP 检测→UI 提示。
- 蓝牙外放:预对齐已有;回放侧失真是另一件事(meetily 的 BLUETOOTH_PLAYBACK_NOTICE 式用户文档已在 backlog)。
- 同室多设备(混合会议):单机无解(Google 用超声波协同),文本去重兜底,记为边界。

### 2.5 线下远场(当前最弱场景的提升路径)

- **MacBook 麦阵不可自建波束成形**(系统只暴露单声道;Voice Isolation/Wide Spectrum 由用户在控制中心按 app 选,API 只读)。可做:检测线下场景时**引导用户切 Wide Spectrum**(避免系统把远处人声当噪声压掉)——成本≈一段引导 UI,可能是被忽视的最大远场开关。
- 外接会议麦(Jabra Speak/Anker PowerConf 类)是硬件级提升;接入时白名单关软件 AEC(单一所有者)。
- 混响/重叠:实时不做(业界共识 CSS/EEND 类放批处理),归入离线精修管线的远期项。

---

## 3. 快速 ASR 层

### 3.1 模型:实时层不动,二遍终稿升级

- 2026 格局:纯中文 SOTA 已被 FireRedASR2/Qwen3-ASR 抬高,但叠加「CPU 实时+中英混说+sherpa-onnx 可用」后,**SenseVoice-Small 仍是快稿最优**(CS-Dialogue 混说基准 MER 6.71% 全场最低;Paraformer 英文瘸,Whisper 系慢且混说差)。
- **升级项:FireRedASR-AED-large zh_en int8 做会后二遍终稿**(sherpa-onnx 官方已导出;中文 4 基准平均 CER 2.89% vs SenseVoice 2.96%,纯中文 AISHELL 0.57% SOTA 档)。注意坑:输入上限 60s(按 VAD 段喂没问题)、1.1B 模型 CPU 只适合离线批式、模型分发 ~1GB(走既有 GitHub release 分发链,牢记 DTLN 404 教训:**必须发 public release + 匿名实测**)。迁移成本:天级(OfflineRecognizer 换 config + 后台低优先级调度)。
- 观察项:Qwen3-ASR(开源中文 SOTA 但 GPU/vLLM 路线,无 ONNX,有复读 bug)等社区 ONNX 化;Parakeet 无中文,忽略。
- 加速结论:**CoreML/ANE/DirectML 明确不投入**(sherpa-onnx CoreML 实测常比 CPU 慢,动态 shape 坑);int8+合理线程数即可。

### 3.2 延迟:两遍式补齐,不换真流式

- 业界标准形态=FunASR "2pass"(流式出灰字+句末离线重解码,声学时延 480–600ms)。现架构「段级+在途段周期重识别」是同构民间版,首字延迟受重识别周期限制。
- 低成本改良:在途段重识别周期缩到 1–2s 且只重识别尾部窗口(天级);若产品要亚秒级字幕,可加 sherpa-onnx 流式 zipformer 双语模型做灰字层、段完成后 SenseVoice 终稿覆盖(1–2 周,但 2023 年模型混说质量一般)——**建议先不做**,当前会议笔记场景段级延迟可接受。

### 3.3 VAD 与分段

- 确认 silero 模型版本,非 v5 则升(半天);可 A/B TEN-VAD(2025.7 进 sherpa-onnx)。
- 分段策略:静音切段 + **最长 15s 强切**(SenseVoice 甜点 5–15s;过短缺上下文伤混说语种判定,过长延迟高且重叠段退化)+ 段间保留 0.3–0.5s 音频重叠防切字。
- 标点驱动语义分段:sherpa-onnx CT-Transformer 中英标点模型对累计文本定句,作为 UI 分句与 LLM 精修输入单元(周级)。

### 3.4 准确率增强

- **热词与 SenseVoice 不兼容**(sherpa-onnx hotwords 仅 transducer+modified_beam_search)——短期靠 LLM 精修层带术语表(零成本,已有精修链),不值得为热词换模型。
- 说话人:现有「双路 embedding 同一 Registry」已是分层归因思想(通道归因打底=mic/system 天然分离;声纹细分 system 侧)。竞品印证这正是行业形态(Granola Me/Them、MacWhisper、screenpipe pyannote+CAM++)。
- ITN:少量 FST 规则覆盖数字/日期/单位即可(sherpa-onnx rule-fsts)。

---

## 4. 采集路线(平台层)

- **CoreAudio Process Tap(macOS 14.4+)**:竞品集体迁移中(screenpipe 已落地按 pid 的进程级 tap,可只抓会议 app 免噪音)。权限从「屏幕录制」(紫色指示器+吓人弹窗,Granola 专门写文档解释)降为独立 TCC 类别「系统音频录制·仅音频」。Rust 路径:cpal 0.17.3+ 开箱(仅整机 loopback;进程级 tap 需自绑 Core Audio,screenpipe `process_tap/macos.rs` 是现成参考)。工程前提:app 必须签名(未签名 TCC 弹窗不触发)、`NSAudioCaptureUsageDescription`。SCK 作为 <14.4 降级保留。注:cpal 升 0.17 有 breaking changes(SampleRate 类型等),独立评估。
- **screenpipe 值得对表的防御性工程**:8s 收流超时、30s 全零判设备被抢占、VPIO 连挂 3 次自动降级 HAL、系统声侧 VAD 阈值放宽(0.15)防 BGM 误杀——与我们刚落地的 FrameTap/Resilient 互补,可择项吸收。
- **远端说话人完美分离的终极路径(仅 Zoom 对本地开放)**:Zoom Meeting SDK raw data 可拿 per-user 音轨(无需特批,需 host 授录制权);更轻:引导用户开 Zoom 本地录制「每人单独音频文件」。Teams(仅 Azure bot、最多 4 路)/Meet(3 路复用+Developer Preview)对本地产品封死。列为 P2 产品探索。

---

## 5. 行动建议汇总

**P0(原则/验证,零到天级成本)**
1. AUVoiceIO ducking 配置 API spike:若有效,默认模式解决外放 ducking,软 AEC 链降为兜底(§2.1)。
2. 「送 ASR 的信号去 NS」A/B 验证(§2.3)。
3. 确认 silero VAD 版本 + SenseVoice int8/线程配置(§3.3/3.1)。
4. 固化原则:处理链版本化,精修稿记录「模型+音频版本」(§2.2/§3.1)。

**P1(高性价比,天到周级)**
5. mic 轨存 pre-AEC 原始信号(方案 A),回放干净版由离线清洗产出(§2.2)。
6. FireRedASR-AED int8 会后二遍终稿(§3.1)——快速 ASR 准确率跃升最大单项。
7. 分段策略调优(15s 强切+重叠)+ CT-Transformer 标点语义分段(§3.3)。
8. 线下场景引导:Wide Spectrum 提示 + 会议麦白名单关软件 AEC + HFP 提示(§2.4/2.5)。
9. afconvert 换 HE-AAC(一行)(§2.2)。

**P2(观察/远期)**
10. CoreAudio Process Tap 迁移(等 cpal 0.17 升级评估一并做)(§4)。
11. Windows 实时声学 AEC = DTLN-aec 流式化(§2.1)。
12. 流式 zipformer 灰字层(若字幕延迟成为诉求)(§3.2)。
13. Zoom per-user 音轨 / 本地分轨录制引导(§4)。
14. Opus 存档、48k 采集(回放质感诉求出现时再议)(§2.2)。

**明确不做**
- 混音单轨(所有调研再次反证);sherpa-onnx CoreML/ANE/DirectML;实时去混响/盲源分离;为热词引入 transducer;多轨单容器存档。

---

## 6. 主要来源

前处理:Chromium APM 文档、Switchboard AEC3 解析、Apple AUVoiceIO ducking 文档/WWDC23、ICASSP AEC Challenge(ByteAudio)、DeepVQE(arXiv 2306.03177)、EchoFree(arXiv 2508.06271)、DTLN-aec(arXiv 2010.14337)、When De-noising Hurts(arXiv 2512.17562)、GTCRN、Google Meet adaptive audio 官方博客。
存档:Whisper #870、pyannote-3.1 模型卡、Amazon Opus-WER 研究、IBM 编码-WER 实测、Hydrogenaudio 听测、Zoom/Otter/Plaud 官方文档、Azure Post-Stream Refinement 官方博客、Craig/Riverside。
ASR:CS-Dialogue(arXiv 2502.18913)、FireRedASR2S、sherpa-onnx 官方模型库/文档(FireRed/Dolphin/hotwords/punctuation/VAD)、Qwen3-ASR 技术报告(arXiv 2601.21337)、FunASR 2pass 文档、Google Deliberation(arXiv 2003.07962)、silero v6 讨论、sherpa-onnx #2910(CoreML 慢)、whisper.cpp #548。
竞品:Granola/superwhisper/MacWhisper 官方文档、screenpipe 源码(Cargo.toml/device.rs/process_tap/aec.rs/segment.rs)、Plaud/讯飞官方、Recall.ai 文档与博客、Zoom Meeting SDK/RTMS 文档、Teams unmixed 文档、Meet Media API 文档、Apple Core Audio taps 文档、cpal PR #1003/releases、AudioCap。
