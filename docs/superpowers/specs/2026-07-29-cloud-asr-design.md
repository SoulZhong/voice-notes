# 云端 ASR 设计(火山 / 阿里,流式)

> 2026-07-29。经 brainstorming 逐题确认的设计稿。
> 决策记录:流式 WebSocket 接入 / 厂商断句 + 本地音频环形缓冲 / 断网缓存补识 /
> 凭证明文 settings.json / 欢迎页二选一分支。

## 1. 目标与非目标

**目标**

- 新增「识别方式」维度:`local`(现状,本地四选一)或 `cloud`(云端 API)。
- 云端支持两家厂商:火山引擎(豆包流式大模型 ASR)、阿里云(DashScope 实时识别)。
- 首启欢迎页可选择识别方式;设置页随时可改;录制中锁定(沿现有切型保护)。
- 纯云端安装只需下载声纹小模型(约 28MB),不强制下载本地 ASR 大模型。
- 断网不丢录音:音频照常落盘,重连后对缺口补识。

**非目标**

- 云端说话人区分(声纹始终本地,CAM++/ERes2NetV2 不变)。
- 云端会后精修(refine 体系已独立存在,不动)。
- 本地引擎任何行为变化(`asr_mode=local` 时全链路与现状逐字节一致)。
- 凭证加密存储(沿用 refine_api_key 的明文先例,设置页同款注明)。

## 2. 厂商产品线与适配层

| | 实时流 | 补识(批式) | 凭证 |
|---|---|---|---|
| 火山 | 豆包大模型流式语音识别 sauc(WebSocket,二进制帧:4 字节头 + JSON/音频负载) | 录音文件识别极速版 flash(HTTP) | AppKey + AccessToken(header 鉴权) |
| 阿里 | DashScope 实时语音识别(WebSocket task 协议:run-task/continue-task/result 事件;模型 paraformer-realtime-v2 或其 2026 后继) | DashScope 文件转写 flash | API Key(Bearer) |

### 2.1 协议核实结果(2026-07-29 已完成,替代原"实现期核实"步骤)

**火山**(文档 docs.volcengine.com/docs/6561/1354869、1631584):

- 流式:`wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async`(官方推荐变体,结果变化才返回);v3 二进制帧(4 字节头:版本/头长、消息类型/flags、序列化/压缩;大端 u32 长度;gzip 可选);鉴权 header `X-Api-App-Key`/`X-Api-Access-Key`/`X-Api-Resource-Id: volc.bigasr.sauc.duration`(小时版;2.0 代为 volc.seedasr.sauc.*,请求体 model_name 恒为 "bigmodel")。
- 首帧 full client request JSON:audio{format:pcm,rate:16000,bits:16,channel:1} + request{model_name,enable_itn,enable_punc,show_utterances:true};音频 200ms/包;末包 flags=0b0010。
- 响应:result.utterances[]{text,start_time,end_time,definite,words[]{text,start_time,end_time}} → 正好映射 DefiniteUtterance。
- 补识:`POST /api/v3/auc/bigmodel/recognize/flash`(Resource-Id volc.bigasr.auc_turbo,base64 音频,同构 utterances 响应,≤2h/100MB)。

**阿里**(文档 help.aliyun.com/zh/model-studio/fun-asr-realtime-websocket-api 等):

- 流式:`wss://dashscope.aliyuncs.com/api-ws/v1/inference/`,`Authorization: bearer <key>`;task 协议(文本帧 run-task/finish-task JSON + 二进制帧裸 PCM 100ms/3200B);模型选 **fun-asr-realtime**(2026 主线,中英混说,句级+字级时间戳,支持 heartbeat 保活防 60s 静默断连);事件 result-generated → payload.output.sentence{begin_time,end_time,text,sentence_end,words[]},sentence_end=true 为定稿。
- ⚠️ **补识偏差(对原设计的修正)**:DashScope 录音文件转写仅收公网 URL,不适用本地文件;补识改走 `qwen3-asr-flash` 多模态 HTTP(base64,单次 ≤5 分钟/10MB,**无时间戳**)。因此补识统一为:**本地 Silero 先把缺口音频切段(≤15s),逐段调 transcribe_batch,时间戳取本地段边界**(火山同走此路径,代码统一;火山返回的段内 utterance 时间戳作为增强保留)。

两家模型名/Resource-Id 集中定义为常量,升级单点替换。

### CloudAsr trait(适配层边界)

```rust
// src-tauri/src/asr/cloud/mod.rs
pub trait CloudAsr: Send {
    /// 打开一条流式识别连接(每音频源一条)。返回推流句柄与事件接收端。
    fn open_stream(&self) -> anyhow::Result<CloudStream>;
    /// 批式补识(断网缺口用):16kHz 单声道 f32 → 定稿句子列表。
    fn transcribe_batch(&self, samples: &[f32]) -> anyhow::Result<Vec<DefiniteUtterance>>;
}

pub struct CloudStream {
    /// 推 16kHz 单声道 f32(内部转厂商要求的 PCM 编码/帧长)。
    pub push: Box<dyn FnMut(&[f32]) -> anyhow::Result<()> + Send>,
    /// 结束推流(触发厂商吐尾包)。
    pub finish: Box<dyn FnOnce() -> anyhow::Result<()> + Send>,
    pub events: crossbeam_channel::Receiver<CloudEvent>,
}

pub enum CloudEvent {
    /// 未定稿文本(替代现有每秒 partial 重识,逐字上屏)。
    Interim { text: String },
    /// 定稿句子。时间为流内毫秒,由会话层映射到源时间轴。
    Definite(DefiniteUtterance),
    /// 连接终止(错误或服务端关闭)。触发重连状态机。
    Closed { error: Option<String> },
}

pub struct DefiniteUtterance {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    /// 逐词时间戳(厂商提供则填,与本地 tokens/timestamps 同语义喂 diarization 切分;
    /// 缺失 → 段级降级,与本地 Qwen3 路径一致)。
    pub words: Vec<CloudWord>,   // { text, start_ms, end_ms }
    /// 厂商语种标签,通常为空 → 语言过滤走文本兜底(与 Paraformer 路径一致)。
    pub lang: String,
}
```

实现:`asr/cloud/volcano.rs`、`asr/cloud/aliyun.rs`。WebSocket 用
tokio + tokio-tungstenite(tokio 已在依赖树);协议帧的构造/解析写成**纯函数**
(字节序列 in/out),单测直接覆盖,不碰网络。

## 3. 会话集成(厂商断句 + 音频环形缓冲)

```
音频源(mic/system)
  └→ 前处理(to_mono/重采样16k/AEC/暂停闸)          ← 不变(segment_worker)
       ├→ CloudStream.push(推流)
       └→ 音频环形缓冲(每源,60s,f32)               ← 新增,仅存内存
            Interim  → 现有 per-source partial 槽
            Definite → 合成 final 段:
                       · start_ms/end_ms:流内时间 + 会话起点偏移
                       · tokens/timestamps:由 words 换算(有则)
                       · 按 [t0,t1] 切环形缓冲音频 → 现有 split_final 声纹路径
                       ↓
            现有下游零改动:回声去重 / 语言过滤 / diarization /
            抑制记账(SuppressedFinal)/ 落盘(append_final)
```

- 云端模式下 **Silero 不参与断句**(厂商服务端 VAD 断句);silero_vad.onnx
  工件保留下载(1MB,体积可忽略,避免 required_now 逻辑分叉过细)。
- mic/system 各一条 WS 连接(两路并发,计费按两路,设置页注明)。
- 暂停:暂停闸挡在推流上游,行为与本地一致(暂停期间不推流不计费)。
- 会话层新增 `run_cloud_asr_worker`,与现有 `run_asr_worker` 平行;二者共享
  final 处理函数(从现有 worker 中抽出的纯逻辑),不复制粘贴。

## 4. 断网补识

- `Closed{error}` → 状态事件「云端识别中断,重连中」;指数退避重连
  (1s/2s/4s/…上限 30s,不封顶重试直到停录)。
- 断连区间记为缺口 `[t_lost, t_recovered]`(按源)。恢复后启动补识任务:
  - 音频来源:**内存环形缓冲(每源 5 分钟)**。录制中读取写入中的 WAV 不可靠
    (头部长度未定稿),不采用;缺口超出 5 分钟的部分落 `[识别失败]` 占位,
    原始音频仍在(keep_audio),可事后重识。
  - 缺口音频先经本地 Silero 切段(≤15s),逐段调 `transcribe_batch`(§2.1),
    时间戳取本地段边界。
  - 走 `transcribe_batch`(厂商 flash 接口),返回句子照常 append 落盘
    (segments.jsonl 是 append-only,seq 只增;句子携带真实 start_ms,
    展示层按时间排序——双源交错本就如此,补识段不需要特殊插入逻辑)。
  - 补识失败 → 缺口整段落 `[识别失败]` 占位(现有占位语义,可事后重试)。
- 停录时若仍在断连:照常收尾,缺口按上述规则处理;录音文件永不受影响。

## 5. 设置与数据模型

`settings.json` 新增(全部带 serde 默认,老配置零迁移):

| 字段 | 默认 | 说明 |
|---|---|---|
| `asr_mode` | `"local"` | `"local"` / `"cloud"`。老用户缺字段 → local,行为不变 |
| `cloud_asr_provider` | `"volcano"` | `"volcano"` / `"aliyun"` |
| `volc_app_key` / `volc_access_key` | `""` | 火山凭证,明文(同 refine_api_key 先例) |
| `dashscope_api_key` | `""` | 阿里凭证,明文 |

- `models::required_now` 与 `recording_ready` 感知 asr_mode:
  cloud 模式下本地 ASR 大模型工件不再必需;vad 工件保持恒必需(1MB,云端虽不
  用它断句,但保留可避免 required_now 分叉与回切本地时的缺件);声纹仍按现状
  (增值,缺失只降级说话人区分)。cloud 模式的 `recording_ready` =
  必需工件齐 **且** 对应厂商凭证字段非空。
- `set_settings` 沿现有保护:录制中禁止改 asr_mode / provider / 凭证。
- SegmentRecord 不加引擎来源字段(YAGNI;调研文档 §4.1 的 provenance 设计
  归 pass2 大改造,不在本期)。

## 6. 前端

**设置页 · 识别区**:顶部新增「识别方式」分段控件(本地模型 / 云端 API)。

- 本地:现有四选一 radio 原样。
- 云端:厂商 radio(火山 / 阿里)+ 对应凭证输入框 + 「测试连接」按钮
  (后端 `test_cloud_asr` 命令:真实开一条流并立即关闭,验证鉴权/网络,
  返回成功或厂商错误文案)+ 一行隐私注明「录音音频将实时上传至所选厂商」。
- 录制中整区禁用(沿现有 recording.isLive 锁)。

**欢迎页(WelcomeOverlay)**:「开始使用」前新增一步二选一:

- 「本地识别 · 隐私优先」:现有流程(下载默认模型约 1GB)。
- 「云端识别 · 更准更快」:录入凭证 → 测试连接 → 只下载声纹小模型(约 28MB)。
- 两者下方注明可随时在设置中更改;云端分支明示音频上传。

**README**:「完全本地、隐私零外泄」表述限定为本地模式,云端模式单独说明。

## 7. 错误处理一览

| 场景 | 行为 |
|---|---|
| 凭证缺失/错误(开录时) | 开录失败,danger 提示「请先在设置中配置云端凭证」 |
| 鉴权失败(厂商 401/403) | 透出厂商错误文案;录制中则走断连-重连态(重试仍失败即持续提示) |
| 限流/配额(429 等) | 同断连处理,退避重连;状态条透出厂商 message |
| 断连 | 状态条「重连中」;音频照录;恢复后补识(§4) |
| 补识失败 | 缺口 `[识别失败]` 占位,原始音频在,可事后重试 |
| 网络抖动致 Definite 时间戳乱序 | 照常 append,段携带真实 start_ms,展示层按时间排序(同双源交错现状) |

## 8. 测试策略

1. **协议纯函数单测**:两家的帧构造/报文解析,golden 字节序列/JSON 进出。
2. **会话逻辑 mock 测试**:`MockCloudStream` 脚本化 interim/definite/断连/重连/
   补识时序,复用 session.rs `asr_worker_tests` 的 mock 风格,不碰网络;
   覆盖:partial 槽覆盖语义、final 时间轴映射、缺口记账、补识插回排序、
   停录时仍断连的收尾。
3. **真实 API 集成测试**:`#[ignore]`,凭证走环境变量(`VN_VOLC_APP_KEY` 等),
   无凭证自动跳过;各厂商一条「真实音频流式识别出中文」+「flash 补识出中文」。
4. **前端**:svelte-check;欢迎页/设置页交互人工冒烟。
5. **回归**:`asr_mode=local` 下全量既有测试必须零变化通过。

## 9. 实现顺序建议(供 writing-plans 细化)

1. ✅ 联网核实两家协议(端点/鉴权/帧格式/模型名/计费资源 ID),回填本文档 §2。
2. ✅ settings 字段 + required_now/recording_ready 感知(TDD)。
3. ✅ CloudAsr trait + MockCloudStream + 会话 cloud worker(TDD,mock 驱动)。
4. ✅ 火山 adapter(帧纯函数 → 真实连通 ignored 测试)。
5. ✅ 阿里 adapter(同上)。
6. ✅ 断网补识状态机(mock 时序 TDD)。
7. ✅ 设置页 UI + test_cloud_asr 命令。
8. ✅ 欢迎页分支 + README 措辞。
9. 全量回归 + 真机冒烟(两家各录一段真会议)——回归已跑绿(见 Task 13);真机冒烟待用户执行。
