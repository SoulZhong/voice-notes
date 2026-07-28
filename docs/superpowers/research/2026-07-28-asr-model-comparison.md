# ASR 模型选型调研:VibeVoice / Qwen3-ASR / SenseVoice

> 2026-07-28。四路并行调研(VibeVoice、Qwen3-ASR、SenseVoice 现状、本仓耦合面代码探查)
> 的综合报告,附两项已落地的代码实验。承接
> [2026-07-19 录音方案调研](./2026-07-19-recording-scheme-research.md) 与
> [ASR 系统提升方案](../../asr-system-improvement-plan.md)。

## 0. 结论速览

1. **短期保留 SenseVoice-Small 作为实时引擎**。它在"CPU 实时 + 中英混合 + 一体化标点/ITN/语言 ID"细分仍是合理默认;2026-07 开源仓库活跃(商用条款已澄清、新增 GGUF 路径),但阿里云商业 API 侧已标记下线,官方定位降为"CPU/边缘选项",钦定接班人是 Fun-ASR-Nano-2512。
2. **VibeVoice-ASR 明确排除**。它已不是纯 TTS(2026-01 微软开源了 ASR,7B/MIT,2026-07 出 BitNet CPU 版),但批处理范式(60 分钟一口吃)与本项目"边录边出字"正交,GPU 版 16-24GB 显存、BitNet 版 RTF 0.77 且无流式,中文精度(AISHELL-4 WER 21.4%)反而三者最弱。
3. **Qwen3-ASR-0.6B 是唯一值得 bake-off 的挑战者**(2026-01-29 起 Apache 2.0 开源,sherpa-onnx 已官方集成 int8):精度大幅领先(中英混说 WER 7.4% vs SenseVoice 18.7%)、支持热词(context biasing,正是本项目现架构拿不到的能力)。三个必须先验证的风险:无原生 token 时间戳(diarization 硬前提)、自回归解码的 partial 预览延迟、幻觉形态改变导致全套过滤阈值需重校。
4. **不立即迁移**的理由与 [提升方案 §1](../../asr-system-improvement-plan.md) 一致:评测集尚未建立,换模型无法证明是改善。本次已把评测工具补齐英文 WER 语义(MER 指标,见 §5)。
5. **CoreML provider 实验已做,负结果**:release 构建下 CoreML EP 识别 92.6ms vs CPU 81.0ms(慢 ~14%),加载也更慢。默认保持 CPU 正确,实验开关保留(`settings.asr_provider`)。

## 1. 三个候选的真实身份(2026-07)

### 1.1 Microsoft VibeVoice:TTS 起家,现役重点已是 ASR

- 2025-08-25 首发为长篇多说话人 TTS(1.5B/7B);两周后因声音克隆滥用,微软删除 TTS 代码并禁用 HF 上的 7B(1.5B 权重未撤,社区备份见 vibevoice-community/VibeVoice)。
- **2026-01-21 转向开源 VibeVoice-ASR**(7B,MIT):单次 60 分钟长音频,输出 Who/When/What(说话人+时间戳+文本),50+ 语言、中英 code-switching、热词。已入 HF Transformers 正式版与 Azure AI Foundry Labs。
- 2026-07-23 发布 ASR-BitNet:1.58GB 三值量化,纯 CPU 推理(3 线程 RTF 0.77),配套 VibeASR.cpp;**无流式**。
- 中文基准明显偏弱:AISHELL-4 WER 21.4%(对照英文 MLC-Challenge 7.99%)。
- 仓库活跃(~50.6k stars),官方免责声明仍写"不建议未经测试用于商业场景"。

### 1.2 Qwen3-ASR:从 API-only 到完全开源

- 2025-09 的 Qwen3-ASR-Flash 是 DashScope API-only;**2026-01-29 开源 0.6B / 1.7B 全家(Apache 2.0)** + Qwen3-ForcedAligner-0.6B(非自回归强制对齐,11 语种、≤5 分钟、平均误差 42.9ms)。
- 架构:AuT 音频编码器(约 180M/300M)+ Qwen3 LLM 解码器;52 语言/方言(含 22 种中国方言)。
- 精度(官方技术报告 + 第三方):AISHELL-2 WER 2.71%(Whisper-lv3 5.06%);WenetSpeech meeting 5.88%(Whisper 19.11%);中英混说社区实测 WER 7.4%(Whisper 14.9%,SenseVoice-Small 18.7%);语种识别 97.9%。
- 部署:vLLM(流式,0.6B TTFT 92ms)为主;**sherpa-onnx 已官方集成 0.6B int8**(2026-03,含 hotwords 支持,覆盖 macOS/Windows);社区有 GGUF/纯 C(antirez qwen-asr,M3 Max 0.6B 约 7-13x 实时,峰值内存 ~2.8GB)/MLX 路线。
- 热词:system prompt 注入任意上下文做 biasing(API 版最多 1 万 token)。
- 弱点:开源版无说话人分离/情感事件;时间戳靠外挂 ForcedAligner;流式仅 vLLM 且精度略降;LLM 解码有空输入幻觉倾向(sherpa-onnx issue #3509);CPU 上自回归吞吐远低于 234M 非自回归模型。

### 1.3 SenseVoice:仍活跃,但被官方分层降位

- 仓库 2026-07-27 仍在提交(SenseVoiceSmall **商用许可澄清**:允许商用、需署名);2026-06 新增 llama.cpp/GGUF 路径(q8 约 254MB、内置 VAD、单二进制)。
- 只有 Small(234M)开源;Large 从未放出。
- 2026 年精度位次:AISHELL-1 CER 2.96%,属中游——大幅领先 Whisper 全系(5.14%),但落后 FireRedASR2(0.55%)、Qwen3-ASR、Fun-ASR、Paraformer-Large(1.68%)。真实会议音频上 7.81% CER 仍优于 Paraformer(10.18%)。
- CPU 速度仍是护城河:sherpa-onnx int8 在 RK3588 A76 单线程 RTF 0.099;本仓 M 系 Mac 实测 5 秒音频识别 81ms(release)。
- 风险信号:阿里云百炼 API 侧标记"即将下线"(开源侧不受影响);FunASR 官方 2026 选型指南把它定位为边缘选项,旗舰是 Fun-ASR-Nano-2512(0.8B = SenseVoice encoder + Qwen3-0.6B decoder,Apache 2.0,真流式)。

## 2. 对照本项目约束的对比

本项目硬约束(代码事实):CPU-only(sherpa-rs 0.6.8 `get_default_provider()` 硬编码 cpu)、准实时(VAD 段 ≤15s + 每秒 partial 重识别)、双路音源共用单模型实例、macOS Apple Silicon + Windows x64、模型经 sherpa-rs 加载。

| 维度 | SenseVoice-Small(现用) | Qwen3-ASR-0.6B | VibeVoice-ASR |
|---|---|---|---|
| 参数/内存 | 234M,int8 ~250MB | ~0.6B,int8 ~2.8GB 内存 | 7B(GPU 16-24GB)/ BitNet 1.58GB |
| CPU 实时性 | 单线程 RTF ~0.1 | ~7-13x 实时(M3 Max),慢数倍 | BitNet RTF 0.77,不可行 |
| 中文精度 | AISHELL-1 CER 2.96%(中游) | AISHELL-2 WER 2.71%;混说 7.4% | AISHELL-4 WER 21.4%(最弱) |
| token 时间戳 | 有,sherpa 直接透出 | **需外挂 ForcedAligner 二次推理** | 内建(段级结构化) |
| 语言 ID | 有(本仓强依赖) | 有(97.9%) | 有 |
| 标点+ITN | 模型内建(本仓强依赖) | 标点内建;ITN 无显式开关 | 内建 |
| 热词 | **无**(sherpa 热词仅 Transducer) | **有**(sherpa-onnx 已实现) | 有 |
| 流式 | 批式,靠小而快伪装实时 | 流式仅 vLLM/GPU;CPU 批式 | 纯批式 60 分钟 |
| 许可 | 商用许可已澄清(需署名) | Apache 2.0 | MIT(官方不建议直接商用) |
| 接入成本 | 已接入 | 小时级(sherpa-rs 栈内) | 需全新运行时 |

## 3. 本仓耦合面(换模型前必读)

来自代码探查(文件:行 以 2026-07-28 工作区为准):

1. **token 时间戳是 diarization 硬前提**:`session.rs:394-398`,timestamps 缺失或与 tokens 不等长 → 段内说话人切分**静默退化**为整段单说话人(commit `1b58798` 起不再重跑 ASR 兜底)。Whisper 路径已处于该退化状态。
2. **`lang` 字段是语言过滤主判据**:`session.rs:174-197` 解析 `<|zh|>` 形式;缺 lang 只剩假名/谚文占比 30% 的文本兜底。注意 `lang` 不落盘(`store/mod.rs` SegmentRecord 无此字段),会后过滤的语种漂移规则实为死代码。
3. **标点/ITN 全靠模型内建**:`asr/sense_voice.rs` `use_itn: true`;仓内未接独立标点模型,段落合并是"text 直接拼接"。
4. **BPE `▁` 假设**:`diar/split.rs:141` 绑定 sherpa tokenization。
5. **全套阈值对着 SenseVoice 输出校准**:会后过滤(`refine/filter.rs:10-20`,scripts/refine_golden.py)、回声文本相似度 0.6、AEC 残渣 RMS 0.012(`session.rs:24-51`)。换模型后幻觉形态改变,需重校。
6. **热词在现架构不可得**:sherpa-onnx 热词仅 Transducer + modified_beam_search;SenseVoice/Paraformer/Whisper 均不支持——这是换模型的主要动机之一,Qwen3-ASR 路线可直接解锁。
7. **模型工件按字节数+sha256 精确 pin**:`models/mod.rs`,新模型需补齐 manifest。
8. **"能出字"层面替换是小时级**:`Recognizer` trait 单方法,实例化点唯一(`lib.rs new_recognizer`),参照 42 行的 `asr/paraformer.rs`。

## 4. 逐模型意见

- **VibeVoice-ASR:排除**。形态(批式长音频)、资源(显存/RTF)、中文精度三重不匹配。即使做"会后二遍终稿",FireRedASR-AED(中文 SOTA、已进 sherpa-onnx,见 07-19 调研)也是更好候选。
- **Qwen3-ASR-0.6B:进入 bake-off**。解决精度与热词两大真实痛点;三风险(时间戳、partial 延迟、阈值重校)必须实测后再决策。sherpa-onnx int8 路线与现有技术栈零摩擦。
- **SenseVoice-Small:保留为实时基线**。被超越但未被证伪;超越者均付出体积/延迟/管线复杂度代价。
- **顺带纳入 bake-off**:Fun-ASR-Nano-2512(官方接班人、同 encoder、真流式、Apache 2.0)——某种意义上比 Qwen3 更像"SenseVoice 的自然升级"。
- **架构终局可能是双引擎**:实时快稿 SenseVoice + 会后终稿高精度模型(FireRedASR-AED 或 Qwen3-ASR-1.7B),与提升方案 Phase 4 的 pass2 设计吻合。

## 5. 本次已落地的改动与实验记录(2026-07-28)

1. **评测工具补齐混合语种指标**:`tools/asr_eval.py` 新增 `tokenize()`(CJK 按字、ASCII 词按词)与 **MER**(mixed error rate:中文=CER 语义,英文=WER 语义,单一指标覆盖 code-switch),门禁 flag `--max-mer`。此前英文按字符算 CER 会把错词摊薄。测试:`tools/test_asr_eval.py`(5 例全过)。
2. **provider 实验开关**:`settings.asr_provider`(无 UI,手改 settings.json;空 = sherpa 默认 CPU),经 `asr::provider_override()` 透传到 SenseVoice/Paraformer/Whisper 三个识别器(`lib.rs new_recognizer` 唯一入口)。前端保存走"取全量-改一项-写回",手改值不会被覆盖。
3. **CoreML 实验(无稳定收益)**:`cargo test --release --test sense_voice_it sense_voice_coreml -- --ignored --nocapture`
   - 旧栈(sherpa-onnx 1.12.9):CPU 81.0ms vs CoreML 92.6ms;新栈(1.13.4):CPU 128.8ms vs CoreML 82.4ms —— 两轮方向相反,结论:**与 CPU 同量级、在测量噪声内,无稳定收益**;默认保持 CPU。开关保留,供未来模型(尤其 LLM-decoder 类)复测。
4. **实验入口测试**:`src-tauri/tests/sense_voice_it.rs::sense_voice_coreml_provider_smoke_and_timing`(ignored,需本机模型),可随时复跑对比 provider。

### 5.1 追记(同日):Qwen3-ASR 已接入应用

调研结论当天即落地(比原计划提前,因发现 sherpa-rs 已被官方弃用,顺势整体迁移):

1. **依赖迁移**:sherpa-rs 0.6.8(弃用,捆绑 sherpa-onnx v1.12.9)→ k2-fsa 官方 `sherpa-onnx`/`sherpa-onnx-sys` 1.13.4。默认**静态链接**(含 onnxruntime),macOS dylib 与 Windows DLL 分发面整体消失(tauri.conf frameworks 只剩 webrtc 的 abseil;tauri.windows.conf.json resources 清空;CI DLL 占位步骤删除)。
2. **新引擎层** `asr/engine.rs`:四选型统一走 sys C API,从 C 端 JSON 解析结果——因官方安全封装丢弃 lang 字段,而语言过滤强依赖它。迁移契约用真模型测试锚死:SenseVoice `lang="<|zh|>"`、tokens/timestamps 等长(`tests/sense_voice_it.rs`)。
3. **Qwen3-ASR 0.6B int8 成为第四选型**(设置页 radio,工件 879MB 含 sha256 pin):
   - 实测(M 系 Mac,4.1s 中文 fixture):识别 0.74-1.41s,**RTF 0.18-0.34,满足一遍门槛 <0.5**;文本与 SenseVoice 一致。
   - 能力形状确认:**无 token 时间戳**(timestamps=0 → diarization 段级降级,`qwen3_it.rs` 有回归锚)、lang 空(语言过滤走文本兜底)、**热词配置冒烟通过**(engine 已留 hotwords 口,待术语库接入)。
4. **待办**:Windows 侧静态链接(MT 运行时)与 MSVC 默认 MD 的潜在冲突,`cargo check` 不做最终链接暴露不了,首个 Windows 打包版本需人工验证;bake-off(同 PCM 横评 CER/MER)仍等评测集(§6 P0 不变)。
5. **上游补丁已备好未提交**:给官方 Rust 绑定补 `lang/emotion/event` 字段的 patch(+18 行,含示例更新)已在本地验证(实测输出 `lang=<|zh|>`),提交材料与 PR 文案见开发机 scratchpad `sherpa-onnx-upstream/PR_DESCRIPTION.md`,由维护者自行决定是否提交;合并后可考虑退役 `asr/engine.rs` 的 JSON 解析回到安全 API(engine 仍承担统一配置与 Qwen3 热词口)。

## 6. 后续实验/开发路线

1. **P0 补评测数据**(提升方案 Phase 0):录 3-5 段真实会议做 golden set(JSONL:reference/hypothesis/entities),接 `asr_eval.py --max-cer --max-mer --min-entity-recall` 进 CI。**没有这步,一切迁移都是盲飞。**
2. **P1 Qwen3-ASR-0.6B bake-off**(sherpa-rs 栈内,预计 1-2 天):
   - 新增 `asr/qwen3.rs` 适配层(sherpa-rs 需确认 0.6.x 是否已暴露 Qwen3 config;未暴露则升级 sherpa-rs 或走 sherpa-onnx C API);
   - 验证:目标机型 P95 RTF < 0.5(含每秒 partial 路径)、`timestamps.len()==tokens.len()` 是否成立(预期不成立 → 评估 ForcedAligner 外挂成本或接受 diarization 段级退化)、空音频/AEC 残渣幻觉形态、热词注入效果(实体召回);
   - 同 PCM 与 SenseVoice/Paraformer 横评 CER/MER/实体召回/RTF。
3. **P1.5 Fun-ASR-Nano-2512 纳入同一 bake-off**(transcribe.cpp/llama.cpp 或 FunASR 运行时,接入成本高于 Qwen3 的 sherpa 路线,先只做离线精度对比)。
4. **P2 双引擎架构**:实时层不动,按提升方案 Phase 4 落 pass2,二遍模型由 bake-off 数据决定。

## 7. 主要来源

- VibeVoice:https://github.com/microsoft/VibeVoice · https://huggingface.co/microsoft/VibeVoice-ASR · BitNet 报告 https://arxiv.org/html/2607.21075 · 下架事件 https://news.ycombinator.com/item?id=45148114
- Qwen3-ASR:https://github.com/QwenLM/Qwen3-ASR · 技术报告 https://arxiv.org/abs/2601.21337 · sherpa-onnx 集成 https://k2-fsa.github.io/sherpa/onnx/qwen3-asr/index.html · https://github.com/antirez/qwen-asr
- SenseVoice:https://github.com/FunAudioLLM/SenseVoice · FunASR 2026 选型指南 https://www.funasr.com/en/blog/which-funasr-model.html · Fun-ASR-Nano https://huggingface.co/FunAudioLLM/Fun-ASR-Nano-2512
- 横评:https://ruoqijin.com/blog/asr-deep-dive-2025-2026 · FireRedASR https://arxiv.org/pdf/2501.14350
