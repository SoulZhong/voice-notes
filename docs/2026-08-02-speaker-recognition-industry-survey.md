# 业界声纹识别与说话人分离方案调研(2024–2026)

> 项目级调研文档。姊妹篇:`2026-08-02-speaker-recognition-accuracy-analysis.md`(本项目准确度根因分析),
> 系统现状见 `speaker-identification-architecture.md`。调研时间 2026-08,来源附于各节。

## TL;DR(与本项目最相关的五条)

1. 本项目"embedding+聚类+会后重聚类"的架构路线与 pyannote/VBx 同源,**方向不劣**;差距在缺失的配套件:分数归一化、重叠处理、质量门控。
2. **裸余弦单阈值做跨会话认人不是业界做法**——开集识别标配 AS-Norm(cohort 自适应归一)+ 时长/质量感知校准(QMF);这正是本项目实时识别层缺的。
3. 嵌入模型有免费升级空间:现用 CAM++ zh(CN-Celeb EER 6.78%)→ **ERes2NetV2 zh(6.14%),sherpa-onnx 已支持 ONNX**,换模型即得。
4. 会议语料**约 20% 时长是重叠语音**,完全不处理的代价可量化(OSD+重分割约 7.4% 相对 DER 改善);比语音分离前置便宜得多。
5. 商业世界里真正做"跨会议认人"的都走**注册制或治理完备的被动积累**(Teams 双轨+一年自动删除;Otter 因被动采集声纹吃了 BIPA 集体诉讼)——本项目的被动积累+人工整理路线在技术上主流,合规上(全本地)天然占优。

## 一、Diarization 管线 SOTA(开源/学术)

| 方案 | 范式 | 指标量级 | 流式 | 重叠 | 本地部署 |
|---|---|---|---|---|---|
| pyannote 3.1(MIT) | embedding+聚类 | AMI≈12–14% DER | ✗ | OSD+重分割 | PyTorch,权重 gated |
| pyannote 4.0 community-1(CC-BY) | 同上 | AMI-headset ~17.0% | ✗ | 同上 | 同上 |
| pyannoteAI precision-2(闭源 API) | — | 自建基准比 3.1 好 28% | WebSocket | ✓ | ✗ |
| NeMo **Streaming Sortformer** v2(CC-BY 可商用) | 端到端,无聚类 | 优于 EEND-GLA/LS-EEND 流式基线;2–4 人;含中文会议验证 | **✓(0.32s chunk)** | 天然 | GPU 优先,CPU 无公开基准 |
| EEND 系(LS-EEND 等) | 端到端 | CALLHOME 12.11 / DIHARD-III 19.61 | 部分 | 天然 | 学术级,无 ONNX |
| VBx / 谱聚类 | x-vector+HMM 聚类 | 跨域稳健基线 | ✗ | 需外配 OSD | 可 |
| diart | 端到端局部分割+增量聚类 | 延迟 500ms–5s 可调 | **✓** | **重叠感知** | PyTorch |

来源:[pyannote community-1](https://www.pyannote.ai/blog/community-1) · [HF diarization-3.1](https://huggingface.co/pyannote/speaker-diarization-3.1) · [NVIDIA Streaming Sortformer](https://developer.nvidia.com/blog/identify-speakers-in-meetings-calls-and-voice-apps-in-real-time-with-nvidia-streaming-sortformer/) · [HF sortformer v2.1](https://huggingface.co/nvidia/diar_streaming_sortformer_4spk-v2.1) · [diart](https://github.com/juanmc2005/diart) / [arXiv:2109.06483](https://arxiv.org/pdf/2109.06483) · [VBx](https://arxiv.org/abs/2310.02732) · [Awesome-Speaker-Diarization](https://github.com/DongKeon/Awesome-Speaker-Diarization)

注意许可证陷阱:Sortformer **v1 是 CC-BY-NC(不可商用)**,v2/streaming 才是 CC-BY。

## 二、声纹嵌入模型(重点看中文列)

3D-Speaker 官方对比(2024,覆盖中文最全):

| 模型 | 参数量 | VoxCeleb1-O EER | **CN-Celeb EER** | 3D-Speaker EER |
|---|---|---|---|---|
| ECAPA-TDNN | 20.8M | 0.86% | 8.01% | 8.87% |
| ResNet34 | 6.3M | 1.05% | 6.92% | 7.29% |
| **CAM++(现用)** | 7.2M | 0.65–0.73% | **6.78%** | 7.75% |
| ERes2Net-base | 6.6M | 0.84% | 6.69% | 7.21% |
| **ERes2NetV2** | 17.8M | 0.61% | **6.14%** | 6.52% |
| ERes2Net-large | 22.5M | 0.52% | 6.17% | 6.34% |

- CAM++ 的定位是"效率最优而非性能最优"(7.2M 参数),这正是 sherpa-onnx 选它当中文默认的理由。
- **ERes2NetV2 zh 已有 sherpa-onnx ONNX 版**,是现成的免训练替换项(本项目已内建 `rebuild_for_model` 换模型重算质心的机制,见架构文档)。
- 天花板参考(无中文指标):ECAPA2 0.34%、ReDimNet2 0.287%(VoxCeleb1-O),**均未见 ONNX/sherpa 支持**。
- 注意:上表是**干净朗读基准**;远场/AEC/压缩信道下绝对数值会显著恶化,只作相对排序参考。

来源:[3D-Speaker](https://github.com/modelscope/3D-Speaker) · [arXiv:2403.19971](https://arxiv.org/pdf/2403.19971) · [CAM++ 论文](https://arxiv.org/pdf/2303.00332) · [sherpa-onnx 模型库](https://huggingface.co/csukuangfj/speaker-embedding-models/tree/main) · [ECAPA2](https://arxiv.org/pdf/2401.08342)

## 三、打分与校准(开集识别的正确姿势)

- **AS-Norm 是工业标准**:拿一批"冒名者 cohort"(如库内其他人的质心)算分数分布,对原始余弦做自适应 z-norm(top-N=100~300);2025 年有可训练变体 TAS-Norm。裸余弦单阈值的已知病:信道失配时同人分数系统性走低、短语音分数偏移方向不定、无法区分"全局易混对"与"个体特例"。[arXiv:2504.04512](https://arxiv.org/abs/2504.04512)
- **PLDA 地位下降**:现代大 margin 训练的嵌入上,cosine+AS-Norm ≈ PLDA,前者因简单胜出;强域失配时 PLDA 仍有优势。[arXiv:2204.03965](https://arxiv.org/pdf/2204.03965)
- **QMF 质量校准**:按时长/SNR 对分数做回归校准,是对付短语音阈值两头出错的标准解。[IEEE 6584746](https://ieeexplore.ieee.org/document/6584746/)
- **多注册融合**:embedding 层面平均(质心法)优于逐条打分取 max/mean;进阶是 attention 加权质心。本项目"主质心+会话变体取 max"介于两者之间,属合理设计。
- 本项目现状:**整理层已有 S-Norm(建议/自动归并),但实时识别层(种子匹配 0.68)是裸余弦**——校准缺口在实时层。

## 四、流式与"先粗标后精修"架构

- diart 证实了"重叠感知分割 + 增量聚类"的在线范式;Sortformer 则代表"端到端免聚类"新范式(GPU 依赖)。
- **"实时粗标 + 会后全局重聚类"是公认架构**,本项目已是此形态;文献指出的关键缺口:①在线阶段无重叠检测;②会后精修若只改修订稿、**不回写声纹库,在线错误会固化进库**(本项目正是如此,见根因分析 C6)。
- 在线聚类的"早期错误不可撤销传播"是文献明确的固有问题;2025 年有工作证明**用户轻量纠错回灌可把 DER 从 36.3% 降到 24.7%(相对 -32%)**。[arXiv:2509.18377](https://arxiv.org/abs/2509.18377)

## 五、重叠语音

- AMI 会议语料**约 20%(口径 24.5–27%)时长有重叠**;发声区间 15% 是双人重叠。
- 不处理=重叠段至少漏一人 + 混合嵌入污染簇质心;OSD+重分割约带来 7.4% 相对 DER 改善。[arXiv:1910.11646](https://arxiv.org/abs/1910.11646)
- 处理成本排序:**OSD+重分割(便宜,pyannote 有现成模型)≪ TS-VAD ≪ 语音分离前置(Sepformer,贵)**。

## 六、短语音 / 远场 / 信道失配

- **<2s 是嵌入可靠性跳崖点**:20s→2s 某系统 EER 6.34%→23.89%;3.6s→2.1s 相对涨 46%。缓解:分层阈值、短段延迟判定、蒸馏补偿(可挽回 >65% 损失)。[arXiv:1810.10884](https://arxiv.org/abs/1810.10884)
- 有损压缩(system 信道正是)与远场混响都会显著降嵌入判别力。[arXiv:2509.02771](https://arxiv.org/html/2509.02771)
- 按信道分质心 vs 训练信道补偿:无直接对比文献;从 PLDA 域适配文献推断,**小团队用"按信道分质心+不跨信道直接比"更稳妥**(本项目库侧已分信道,但实时匹配未分,见根因分析 C2)。此条为推断。

## 七、商业 API 与产品实践

**API 侧**(跨会话认人能力为筛选主轴):

| 服务 | diarization | 跨会话认人(voiceprint) | 流式 | 备注 |
|---|---|---|---|---|
| pyannoteAI precision-2 | 强(自称) | ✓(注册 ≤30s 纯净语音) | ✓ | €19/月起 |
| Speechmatics | 内置免费 | **✓ 文档最完整**(5–30s 注册,单会话 ≤50 已知人) | ✓ | 工程参考首选 |
| AssemblyAI | 85–95%(人多则降) | ✗(仅单文件内改名;流式不支持) | — | |
| Deepgram nova-3 | ✓ | 未见公开资料 | ✓ | |
| ElevenLabs Scribe / Gladia | ✓(32 / 12 人上限) | 未见 | — | |
| 腾讯云 | ✓ | **✓ 唯一公开全生命周期接口**(注册/1:N/更新,上限 1000 声纹) | ✓ | 0.017 元/分钟 |
| 讯飞 / 阿里云 | 宣称 90% / ✓ | 阿里需朗读数字串注册 | — | |

**产品侧**(会议笔记怎么做"这是谁"):

- **Teams**:双轨注册(念文本主动注册 + 数次会议被动积累),治理完备——租户三开关、用户自行 opt-in、**一年未用自动删除**。业界治理标杆。
- **Otter**:主动+被动混合,2025 年因未经与会者同意采集声纹在 BIPA 下被集体诉讼(2025-12 合并诉状)。被动积累的合规风险实例。
- **Granola**:明确**不做跨会议声纹记忆**,靠上下文推断+人工改名——规避派代表。
- **Zoom**:主持人代授权模式,隐私争议中。
- 本项目定位:被动积累 + 人工整理收件箱,**全本地存储**——技术上与 Teams 被动轨同型,合规风险天然低于云端方案(数据不出机)。

## 八、准确度的现实预期

- 即便商业旗舰,真实会议(250+ 文件基准)**说话人计数正确率也只有 70%**(pyannoteAI precision-2;上代 50%)。
- DIHARD-III(难场景)SOTA DER ≈14.5%;人多/重叠多时"说话人混淆"超过漏检成为主导误差。
- **开集库越大误识越高**是已证趋势(VoxWatch):gallery 增长推高整体相似度、抬升 false-alarm,无公开数值曲线但方向明确——对本项目启示:**126 人且持续膨胀的库本身就在推高误识率,控库容(整理层清理)就是提准确率**。
- AEC 后音频对说话人识别的量化影响:**无公开研究**(缺口)。

## 已确认查不到的信息

sherpa-onnx 各模型 CPU RTF 公开基准;ECAPA2/ReDimNet 的 ONNX 可用性;按信道分质心 vs 信道补偿的直接对比;AEC 对 ID 准确率的量化;gallery 规模-误识率函数曲线;Fireflies 声纹机制(官方信源自相矛盾);Read.ai 机制。
