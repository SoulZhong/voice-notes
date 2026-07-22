# ASR 全链路流程图

本文描述当前实现中的实时 ASR（Automatic Speech Recognition，自动语音识别）链路，以及停止录制后的精修链路。

## 1. 端到端总览

```mermaid
flowchart TB
    subgraph Input["音频输入层"]
        MIC["麦克风输入<br/>CPAL 或 macOS VPIO"]
        SYS["系统音频<br/>ScreenCaptureKit"]
    end

    subgraph Normalize["统一音频格式与回声处理"]
        MICMONO["转单声道"]
        SYSMONO["转单声道"]
        MICRS["流式重采样<br/>设备采样率 → 16 kHz"]
        SYSRS["流式重采样<br/>设备采样率 → 16 kHz"]
        LEVEL["麦克风电平统计<br/>AEC 前 RMS，100 ms 上报"]
        AECREF["AEC Render<br/>系统声作为远端参考"]
        AECCAP["AEC Capture<br/>消除麦克风中的外放回声"]
    end

    subgraph PerSource["每个音源独立的 segment worker"]
        PAUSE{"是否暂停？"}
        SINK["可选音频旁路<br/>写入 16 kHz WAV"]
        VAD["Silero VAD<br/>语音检测与断句"]
        FINISHED["完成段 FinalJob<br/>source + PCM + start/end"]
        PARTIAL["在途段 PartialJob<br/>每源覆盖式最新槽"]
    end

    subgraph ASRWorker["单 ASR worker：final 优先，空闲处理 partial"]
        PICK["串行取任务"]
        REC["选定识别器推理<br/>SenseVoice / Paraformer / Whisper"]
        RESULT["Transcript<br/>text + lang + tokens + timestamps"]
        LANG{"语言过滤命中？"}
        SPLIT{"段长 ≥ 3 秒<br/>且声纹模型可用？"}
        CHANGE["1.5 秒声纹滑窗<br/>检测段内换人点"]
        TOKENS{"token 时间戳可用？"}
        GROUP["按换人边界分配 tokens"]
        REASR["按音频子段重新 ASR"]
        SUBS["一个或多个 SubFinal"]
    end

    subgraph Guard["跨音源过滤与说话人处理"]
        SOURCE{"音源"}
        SYSFLOW["系统声立即处理<br/>登记 recent_system"]
        MICHOLD["麦克风段暂存<br/>默认 2.5 秒；系统 partial 在途时最长约 15 秒"]
        ECHO{"与系统声时间邻近<br/>且文本相似度 ≥ 0.6？"}
        RESIDUE{"与系统声重叠 ≥ 80%<br/>且 mic RMS ≤ 0.012？"}
        DIAR["提取说话人 embedding<br/>在线聚类/身份匹配"]
        FINAL["FinalSegment 事件"]
        DROP["丢弃文本段<br/>并清空对应 partial"]
        RETRACT["必要时追溯撤回<br/>已放行的 mic 回声段"]
    end

    subgraph Persist["实时结果消费"]
        ACTOR["Lifecycle actor"]
        DISK["追加 segments.jsonl<br/>更新 speakers.json / meta.json"]
        UI["发送前端事件<br/>实时字幕与说话人标签"]
        LIVE["CLI / MCP<br/>读取实时转写"]
    end

    MIC --> MICMONO --> MICRS --> LEVEL --> AECCAP
    SYS --> SYSMONO --> SYSRS --> AECREF
    AECREF -. "共享远端参考" .-> AECCAP
    AECREF --> PAUSE
    AECCAP --> PAUSE
    PAUSE -- "是：丢帧；首次进入时 flush 在途句" --> FINISHED
    PAUSE -- "否" --> SINK --> VAD
    VAD -- "句尾/15 秒硬切/停止 flush" --> FINISHED
    VAD -- "当前尚未结束的句子" --> PARTIAL
    FINISHED --> PICK
    PARTIAL -. "ASR 空闲时 best-effort 预览" .-> PICK
    PICK --> REC --> RESULT --> LANG
    LANG -- "是：日/韩标签或假名/谚文占比过高" --> DROP
    LANG -- "否" --> SPLIT
    SPLIT -- "否" --> SUBS
    SPLIT -- "是" --> CHANGE --> TOKENS
    TOKENS -- "是" --> GROUP --> SUBS
    TOKENS -- "否" --> REASR --> SUBS
    SUBS --> SOURCE
    SOURCE -- "system" --> SYSFLOW --> DIAR
    SOURCE -- "mic" --> MICHOLD --> ECHO
    ECHO -- "是" --> DROP
    ECHO -- "否" --> RESIDUE
    RESIDUE -- "是" --> DROP
    RESIDUE -- "否" --> DIAR
    SYSFLOW -. "新 system final 回查最近 30 秒 mic" .-> RETRACT
    RETRACT --> DROP
    DIAR --> FINAL --> ACTOR
    ACTOR --> DISK
    ACTOR --> UI
    ACTOR --> LIVE
```

## 2. 单音源采集、重采样与 VAD 细节

```mermaid
flowchart LR
    FRAME["AudioFrame<br/>samples + sample_rate + channels"]
    MONO["to_mono<br/>多声道求平均"]
    RS{"采样率变化？"}
    NEWRS["重建 StreamResampler<br/>相位从新设备重新开始"]
    STREAMRS["连续线性重采样至 16 kHz<br/>跨音频块保存相位与尾样本"]
    LEVEL["mic only<br/>累计 1600 样本计算 RMS"]
    PAUSED{"paused"}
    FLUSH["首次暂停：VAD flush<br/>当前语句作为 final 发出"]
    DROP["暂停期间丢帧<br/>录音时间轴冻结"]
    AEC{"AEC role"}
    RENDER["system：push 参考<br/>音频原样向后"]
    CAPTURE["mic：10 ms 分帧消回声<br/>不足一帧则内部滞留"]
    WAV["可选 audio_sink<br/>与送入 VAD 的 PCM 完全同源"]
    ACCEPT["Silero accept_waveform"]
    VADSPEECH{"VAD 当前认为有语音？"}
    CUR["累积 current<br/>用于 partial 预览"]
    CLEAR["清空过时 current"]
    READY{"VAD 已产生完成段？"}
    LONG{"段长 > 15 秒？"}
    QUIET["在 10.5–15 秒范围内<br/>搜索最低能量 100 ms 窗"]
    FINAL["FinalJob<br/>时间由样本偏移换算"]
    PARTIAL["每累计 partial_interval<br/>覆盖 partial_slot"]

    FRAME --> MONO --> RS
    RS -- "是" --> NEWRS --> STREAMRS
    RS -- "否" --> STREAMRS
    STREAMRS --> LEVEL --> PAUSED
    PAUSED -- "是，首次" --> FLUSH --> FINAL
    PAUSED -- "是，后续" --> DROP
    PAUSED -- "否" --> AEC
    AEC -- "Render" --> RENDER --> WAV
    AEC -- "Capture" --> CAPTURE --> WAV
    AEC -- "无" --> WAV
    WAV --> ACCEPT --> VADSPEECH
    VADSPEECH -- "是" --> CUR --> PARTIAL
    VADSPEECH -- "否" --> CLEAR
    ACCEPT --> READY
    READY -- "是" --> LONG
    LONG -- "否" --> FINAL
    LONG -- "是" --> QUIET --> FINAL
```

Silero 当前固定参数：

| 参数 | 当前值 | 作用 |
|---|---:|---|
| 采样率 | 16 kHz | 所有 ASR/VAD 的统一输入格式 |
| `threshold` | 0.5 | 高于阈值视为语音 |
| `min_speech_duration` | 0.25 s | 更短活动不构成有效语音段 |
| `min_silence_duration` | 0.6 s | 连续静音超过该值结束一句 |
| `max_speech_duration` | 15 s | 超长独白的目标上限；代码另做硬切兜底 |
| `window_size` | 512 samples | VAD 推理窗口 |

## 3. 识别器选择与输出差异

```mermaid
flowchart LR
    SETTINGS["settings.json<br/>asr_model"] --> FACTORY{"new_recognizer"}
    FACTORY -- "sense_voice 或未知值" --> SV["SenseVoice full precision<br/>language=auto, ITN=true"]
    FACTORY -- "paraformer" --> PF["Paraformer large int8<br/>greedy/default decode"]
    FACTORY -- "whisper" --> WH["Whisper base int8<br/>language=auto/default decode"]
    SV --> SVOUT["text + lang<br/>tokens + timestamps"]
    PF --> PFOUT["text + tokens + timestamps<br/>lang 通常为空"]
    WH --> WHOUT["只有 text<br/>其余字段为空"]
    SVOUT --> COMMON["统一 Transcript"]
    PFOUT --> COMMON
    WHOUT --> COMMON
```

| 能力 | SenseVoice | Paraformer | Whisper base |
|---|---|---|---|
| 默认选择 | 是 | 否 | 否 |
| 模型精度 | 全精度优先，int8 兜底 | int8 | int8 优先 |
| 中文 | 支持 | 中文专用 | 支持 |
| 中英混合 | 自动语种 | 英文较弱 | 自动语种 |
| 语言标签 | 有 | 通常无 | 当前适配层未透传 |
| token 时间戳 | 有 | 有 | 当前适配层未透传 |
| 段内换人后的文本处理 | 按 token 分组 | 按 token 分组 | 子段重新识别 |
| 热词/领域词 | 未接入 | 未接入 | 未接入 |

## 4. Final 段处理与所有丢弃路径

```mermaid
flowchart TB
    JOB["FinalJob"] --> CALL["recognizer.recognize"]
    CALL --> OK{"成功或 panic？"}
    OK -- "错误/panic" --> PLACEHOLDER["生成 [识别失败]<br/>worker 继续运行"]
    OK -- "成功" --> FOREIGN{"language_filter 开启<br/>且命中外语判定？"}
    FOREIGN -- "是" --> D1["丢弃整段"]
    FOREIGN -- "否" --> INTRA["可选段内换人切分"]
    PLACEHOLDER --> SOURCE
    INTRA --> SOURCE{"source"}

    SOURCE -- "system" --> SYSTEM["立即 process_final"]
    SOURCE -- "mic" --> PENDING["进入 pending_mic"]

    SYSTEM --> MATCHPENDING["与 pending mic 比较"]
    MATCHPENDING --> P1{"低 RMS 高重叠残渣？"}
    P1 -- "是" --> D2["丢弃 pending mic"]
    P1 -- "否" --> P2{"文本相似度高且时间邻近？"}
    P2 -- "是" --> D3["丢弃 pending mic"]
    P2 -- "否" --> PROCESSSYS["处理 system final"]
    PROCESSSYS --> RETRO["回查 recent_mic<br/>必要时发 EchoRetract"]

    PENDING --> EXISTINGSYS{"最近 system 已存在？"}
    EXISTINGSYS -- "高重叠、低 RMS" --> D4["丢弃 mic 残渣"]
    EXISTINGSYS -- "文本相似、时间邻近" --> D5["丢弃 mic 回声"]
    EXISTINGSYS -- "未命中" --> HOLD["等待 hold 到期"]
    HOLD --> PROCESSMIC["处理 mic final"]

    PROCESSSYS --> EMBED
    PROCESSMIC --> EMBED["说话人 embedding"]
    EMBED --> ASSIGN["在线 SpeakerRegistry<br/>匹配已有簇/新建/合并/人物种子"]
    ASSIGN --> EMIT["on_final<br/>text + time + speaker + RMS"]

    D1 --> CLEAR["on_partial(source, 空串)"]
    D2 --> CLEAR
    D3 --> CLEAR
    D4 --> CLEAR
    D5 --> CLEAR
```

### 语言过滤

开启时满足任一条件便丢弃整个 final：

- SenseVoice 语言标签为 `ja` 或 `ko`；
- 文本字母类字符中，假名或谚文比例大于 30%。

### 回声与残渣过滤

| 规则 | 当前条件 | 动作 |
|---|---|---|
| 文本回声 | 时间区间重叠，或起点差小于 2.5 秒；同时文本相似度 ≥ 0.6 | 丢弃 mic 段 |
| AEC 残渣 | mic 段与 system 段重叠比例 ≥ 0.8，且 mic RMS ≤ 0.012 | 丢弃 mic 段 |
| 追溯撤回 | system final 到达后，在最近 30 秒已放行 mic 中发现回声 | 从落盘和 UI 撤回精确匹配段 |

## 5. 段内说话人切分

```mermaid
sequenceDiagram
    participant W as ASR worker
    participant A as 主 ASR
    participant E as Speaker embedder
    participant D as 换人点检测
    participant R as 子段重识别

    W->>A: 整个 VAD 段只识别一次
    A-->>W: text/lang/tokens/timestamps
    alt 无 embedder 或段长小于 3 秒
        W-->>W: 保留整段
    else 可做段内切分
        loop 每 500 ms 移动一次 1.5 s 窗口
            W->>E: 提取窗口声纹 embedding
            E-->>W: embedding 或失败
        end
        W->>D: 相邻有效窗余弦相似度
        D-->>W: 相似度低于 0.55 的边界
        W->>W: 去掉会产生小于 1.2 秒子段的边界
        alt tokens 与 timestamps 等长且非空
            W->>W: token 按时间边界分组拼接
        else 时间戳缺失或异常
            loop 每个音频子段
                W->>R: 再执行一次 ASR
                R-->>W: 子段文本
            end
        end
    end
```

## 6. 实时 partial 与 final 的调度关系

```mermaid
flowchart LR
    SYSSEG["system segment worker"] --> SYSP["system partial_slot<br/>始终只保留最新版本"]
    MICSEG["mic segment worker"] --> MICP["mic partial_slot<br/>始终只保留最新版本"]
    SYSSEG --> Q["unbounded finals queue<br/>完成段永不主动丢弃"]
    MICSEG --> Q
    Q --> WORKER["单 ASR worker"]
    SYSP -. "final 队列暂时为空时轮询" .-> WORKER
    MICP -. "final 队列暂时为空时轮询" .-> WORKER
    WORKER --> FE["前端字幕"]
```

- final 使用队列，设计目标是不丢完成句。
- partial 使用覆盖式槽，旧预览允许被新预览替换。
- 只有一个识别 worker，因此两个音源和 partial/final 共用一个模型实例，避免并发推理争用内存。
- final 优先；会议负载过高时，partial 可能更新较慢，但完成句仍保留。

## 7. 停止录制后的精修链路

```mermaid
flowchart TB
    STOP["停止录制"] --> DRAIN["关闭采集并 flush VAD<br/>排干所有 final"]
    DRAIN --> FINALIZE["finalize note<br/>meta/segments/speakers 完整落盘"]
    FINALIZE --> TRANSCODE["可选 WAV → M4A<br/>失败保留 WAV"]
    FINALIZE --> LOCAL["本地 refine"]

    subgraph Refine["会后精修"]
        RAW["读取原始 segments.jsonl"]
        FILTER["A3 短段幻觉过滤<br/>生成 discarded_seqs，不改原始记录"]
        RECLUSTER["A1 从保存音频重提 embedding<br/>全局离线重聚类"]
        PARA["按说话人和时长合并段落"]
        LLM{"refine_enabled<br/>且执行器配置可用？"}
        POLISH["A2 LLM/本机 Agent 精修<br/>同音字、实体统一、口头语、排版"]
        FALLBACK["关闭或失败：保留原文<br/>记录阶段状态"]
        OUT["原子写 refined.json"]
    end

    LOCAL --> RAW --> FILTER --> RECLUSTER --> PARA --> LLM
    LLM -- "是" --> POLISH --> OUT
    LLM -- "否/失败" --> FALLBACK --> OUT
    OUT --> DETAIL["笔记详情默认展示 refined<br/>可切回原始逐字稿"]
```

### 会后短段幻觉规则

精修稿会丢弃以下原始段，但 `segments.jsonl` 不删除：

1. 白名单优先保留，例如“好、对、嗯、行、可以、OK”；
2. 时长小于 2.5 秒且有效字符数为 0–2，判为幻觉；
3. 时长小于 3 秒、语言为粤/日/韩且有效字符不超过 4，判为语言漂移。

## 8. 主要组件与代码位置

| 层 | 组件 | 职责 | 实现位置 |
|---|---|---|---|
| 采集 | `AudioCapture`、CPAL、VPIO、ScreenCaptureKit | 获取 mic/system 原生音频帧 | `src-tauri/src/audio/` |
| 格式统一 | `StreamResampler`、`to_mono` | 转 16 kHz 单声道，保持跨块相位连续 | `audio/resample.rs`、`audio/mod.rs` |
| 回声处理 | `AecRender`、`AecCapture`、`AlignState` | system 作参考，清理 mic 外放回声 | `audio/aec.rs`、`audio/aec_align.rs` |
| 分段 worker | `run_segment_worker` | 暂停闸、电平、音频旁路、VAD、partial/final 生产 | `pipeline/segment_worker.rs` |
| VAD | `SileroSegmenter` | 语音检测、断句、15 秒硬切 | `pipeline/silero.rs` |
| 识别器 | `Recognizer`、`Transcript` | 三种模型的统一接口 | `asr/mod.rs` |
| 模型适配 | `SenseVoiceRecognizer` | 默认中英混合识别，透传语言和 token 时间戳 | `asr/sense_voice.rs` |
| 模型适配 | `ParaformerRecognizer` | 中文识别，透传 token 时间戳 | `asr/paraformer.rs` |
| 模型适配 | `WhisperRecognizer` | Whisper base 识别，目前仅透传文本 | `asr/whisper.rs` |
| ASR 调度 | `run_asr_worker` | final 优先、partial 预览、过滤、回声去重、说话人处理 | `session.rs` |
| 段内切分 | `detect_change_points`、`group_tokens_by_boundaries` | 按声纹换人点拆分长段文字 | `diar/split.rs` |
| 说话人 | `SpeakerRegistry`、`SpeakerEmbedder` | 实时聚类、已有身份匹配、簇合并 | `diar/registry.rs`、`diar/mod.rs` |
| 生命周期 | lifecycle actor | 顺序消费 final/说话人事件，协调落盘和 UI | `lifecycle/actor.rs`、`lifecycle/consumers.rs` |
| 存储 | `SessionWriter`、`NoteStore` | `segments.jsonl`、`speakers.json`、`meta.json` | `store/writer.rs`、`store/notes.rs` |
| 会后过滤 | `is_hallucination` | 标记精修稿要跳过的短垃圾段 | `refine/filter.rs` |
| 会后聚类 | `recluster` | 从保存音频执行全局说话人重聚类 | `refine/recluster.rs` |
| 文本精修 | HTTP LLM / Agent executor | 同音字、实体、口头语与排版修正 | `refine/llm.rs`、`refine/agent.rs` |

## 9. 质量诊断切面

```mermaid
flowchart LR
    A["原始设备音频"] --> B["16 kHz / AEC 后 PCM"] --> C["VAD 段"] --> D["模型原始 Transcript"] --> E["过滤/回声去重后 final"] --> F["refined.json"]
    A -. "设备、增益、削波" .-> QA["采集质量"]
    B -. "AEC 损伤、重采样" .-> QB["音频处理质量"]
    C -. "漏检、切词、超长段" .-> QC["分段质量"]
    D -. "CER/WER、实体错误" .-> QD["模型质量"]
    E -. "误删、重复、段内错切" .-> QE["规则质量"]
    F -. "修错率、语义篡改率" .-> QF["精修质量"]
```

这六个观测点必须分开评估。只比较最终稿，无法判断错误来自采集、VAD、模型还是后处理。
