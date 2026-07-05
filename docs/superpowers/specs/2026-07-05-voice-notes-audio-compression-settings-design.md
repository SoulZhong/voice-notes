# 音频压缩 + 系统设置页 + ASR 选型 设计

日期:2026-07-05。单阶段单 PR(用户拍板"一期全做")。

## 目标

1. **音频压缩**:笔记音频从 WAV(≈115MB/h/源)转 AAC m4a(~32kbps 单声道,≈14MB/h/源,8 倍压缩),含历史笔记回溯。
2. **系统设置页**(`/settings`):可指定数据存储目录与模型目录(自动迁移)、模型下载/删除/镜像管理集中、ASR 模型选型。
3. **ASR 选型**:SenseVoice(默认)/ Whisper 可切,下一场录制生效。

## 已确认决策

- 有损语音档(AAC ~32kbps),不做无损/双档位。
- 转码引擎:`/usr/bin/afconvert` 子进程(macOS 内建,零新依赖零 FFI;编码/解码同一工具)。
- 时机:停止录制后后台转码本场 + 启动后台回溯历史;**不做录制时直接编码**。
- 续录已压缩笔记:m4a 解码回 WAV 再走现有续录逻辑,停止后整体重编码;每次续录旧音频多一代有损,**已确认接受**。
- 存储目录变更 = 自动迁移(复制→校验→删旧),不做双位置合并、不做只记路径。
- 分期:一期全做,单特性分支单 PR squash。

## 一、音频压缩

### 录制路径不动

录制仍写 WAV:`store/audio.rs` 的崩溃安全(1s 刷盘回写头)、对齐不变式(文件内毫秒+offset_ms==段时间轴)、续录截断/零填充全部原封。压缩只发生在录制结束之后,是纯增值层:任何失败只降级保留 WAV,绝不影响录制与转写。

### 转码流程(stop 后 + 启动回溯,同一队列)

全局串行转码队列(单后台线程,常驻 lib.rs 状态):

1. 入队来源:①stop_recording 定稿后把本场笔记入队;②启动扫描所有 `state=complete` 且目录中存在 `<source>.wav` 的笔记。
2. 录制进行中暂停出队(不与实时推理争 CPU);当前正在录的活动笔记永不入队。
3. 单笔记处理:对每个 `<source>.wav`:
   a. 先 `repair_wav_header`(陈旧头会让 afconvert 少读尾部);
   b. `afconvert -f m4af -d aac -b 32000 <source>.wav <source>.m4a.tmp`;
   c. 校验:exit code 0 + 读回 m4a 时长与 WAV 数据长度换算时长差 <100ms(时长经 `afinfo` 或 afconvert 解码探测);
   d. 写 audio.json(该 track 增 `codec:"aac"` 与 `duration_ms`)→ rename `.m4a.tmp`→`.m4a` → 删除 WAV。
4. 幂等/崩溃恢复按构造成立:`.m4a.tmp` 残留 = 转码中崩溃,启动清理重转;m4a 与 wav 并存 = 删 WAV 前崩溃,校验 m4a 后删 WAV;任一步失败 → 保留 WAV、清理 tmp、eprintln,该笔记跳过(下次启动重试)。

### audio.json 扩展

`TrackMeta` 增可选字段:`codec: Option<String>`("aac")、`duration_ms: Option<u64>`(m4a 不能按字节换算时长,转码时写死)。serde default + skip_serializing_if,旧文件双向兼容。

### 轨道枚举与播放

`list_tracks`:某源存在 `<source>.m4a` 时优先上报它(path 指 m4a,duration 取 audio.json 的 `duration_ms`,缺失视为损坏跳过该轨),否则回落现有 WAV 逻辑。活动笔记(录制中)天然只有 WAV,走现状。前端 AudioPlayer 只消费 `path/duration_ms`,零改动;asset 协议 scope 已覆盖 notes 目录整树,m4a 无需新授权。`repair_stale_tracks` 仅对 WAV 轨道有意义,m4a 轨道跳过。

### 续录交互

resume_recording 入口,对目标笔记:

1. 从转码队列摘除排队项;若正被转码,等当前文件完成;
2. 对每个 `<source>.m4a`:`afconvert -f WAVE -d LEI16@16000 -c 1 <source>.m4a <source>.wav`,校验后删 m4a、清该 track 的 codec/duration 字段;
3. 现有续录逻辑(截断/零填充对齐 base_ms)原封执行;停止后经队列整体重编码。

解码 1-2h 音频约数秒,发生在开录准备阶段(用户感知为开录稍慢);解码失败 → 本场该源不保留音频(与现有音频建档失败同姿态),转写照常。

### 不动的部分

声纹样本 `voiceprints/<P#>.wav`(15s 一次性小文件)保持 WAV;导出、rms 诊断、segments 时间轴、回声去重全部无感。

## 二、系统设置页

### settings.json 扩展(仍在 app_data_dir,自举不受迁移影响)

```
{ mirror_enabled, mirror_prefix,            // 现有
  data_dir: Option<String>,                 // None = app_data_dir
  models_dir: Option<String>,               // None = 现状解析
  asr_model: String }                       // "sense_voice"(默认) | "whisper"
```

serde default 全兼容旧文件。settings.json 本体永远留在 app_data_dir(否则找不到指针)。

### 路径解析

- 新增 `data_root(app)` = `settings.data_dir`(存在且有效)否则 app_data_dir。`notes_dir`、声纹库(voiceprints.json + voiceprints/)全部挂 data_root 下。
- `models::root()` 解析顺序:VN_MODELS env → `settings.models_dir` → debug 的 src-tauri/models → app_data_dir/models。实现:OnceLock 改为 RwLock,启动注入设置值、设置变更时更新(录制中禁改,无竞态窗口)。
- asset 协议 scope:启动时与迁移成功后对新 data_root 运行时 `allow_directory`(Tauri v2 scope API)。

### 目录迁移(数据目录与模型目录同一套机制)

设置页两行「数据存储目录」「模型存储目录」,各显当前路径 + 「更改…」按钮:

1. 原生目录选择器(新增 `tauri-plugin-dialog` 依赖,Rust+JS 两端);
2. 目标必须是空目录或不存在(自动创建);非空报错——防误吞用户文件、也防删旧时误删;
3. 录制中/下载中/转码中按钮禁用;迁移开始前暂停转码队列(等当前文件完成),迁移期间禁止开录(全局 guard,姿态同下载互斥);
4. 后台复制整树 → 校验(文件数+总字节数一致)→ 写 settings + 更新运行时路径/scope → 删旧目录;
5. 失败:清理新目录残留、保持旧配置、错误横幅。中途崩溃:settings 未写 = 旧配置照常,新目录残留下次迁移前清理(迁移开始前若目标含残留即视为非空报错,提示用户手动处理)。

### 模型管理集中

设置页「模型」区块:列 manifest 全部工件(名称/体积/状态),每项下载(复用现有下载命令与进度事件)/删除(删工件落位文件);删除在录制中/下载中禁用,删后 `recording_ready` 自然变 false,录制页现有缺模型引导卡自动复现。镜像开关+前缀输入从 ModelDownloadCard 移入设置页(下载卡只留下载引导,配置入口文案指向设置页)。

### 页面与导航

`/settings` 新路由,侧栏底部「设置」入口;区块:存储、模型、语音识别(ASR 选型)。视觉严格按 DESIGN.md(温暖极简、hairline、悬停显影、禁 emoji/Unicode 符号图标)。

## 三、ASR 模型选型

- 设置页「语音识别」单选:SenseVoice(默认,推荐——中文为主、带语言过滤与段内分离全功能)/ Whisper。
- manifest 增加 whisper 工件(sherpa-onnx 导出 whisper base int8,tar.bz2;URL/sha256/字节数 **plan 阶段核实填死**),`required_for_recording` 语义改为「当前选型所需」:`recording_ready()` = vad + 选中 ASR 工件齐。未下载所选模型时设置页就地提示下载。
- worker 识别器工厂:按 `settings.asr_model` 实例化 `SenseVoiceRecognizer` 或 `WhisperRecognizer`(asr/whisper.rs P1 已有,按 sherpa-rs 0.6 现接口修通)。常驻识别器(P3.5)按设置重建:设置变更且未在录制时触发重载,**下一场录制生效**。
- **降级明确并写进设置页选项说明**:
  - Whisper 无 token 级时间戳 → `Transcript.timestamps` 空 → 段内说话人分离退化为段级单标签(下游已有空值容忍路径,验证覆盖);
  - Whisper 语言标签格式与 SenseVoice 不同 → 语言幻觉过滤对非 SenseVoice 标签直通不滤。

## 错误处理总则

转码/迁移/模型删除全是增值层姿态:失败只降级(保留 WAV / 保持旧目录旧配置 / 报错横幅),绝不阻塞或破坏录制与转写落盘。所有子进程调用固定绝对路径 `/usr/bin/afconvert`、`/usr/bin/afinfo`。

## 测试

- settings:新字段 roundtrip、旧文件(仅镜像字段)解析兼容。
- audio.json:codec/duration 字段新旧双向兼容。
- 转码:真 afconvert 编→校验→删 WAV roundtrip(dev/CI 均 mac);解码回 WAV 后续录对齐(复用现有 open_existing 测试姿态);`.m4a.tmp` 残留清理、m4a+wav 并存幂等收敛;陈旧头笔记先修复再转码不丢尾部。
- 迁移:复制-校验-删旧 roundtrip、目标非空拒绝、失败回退不动旧数据。
- models root:解析次序(env > settings > debug 目录 > app_data)。
- 识别器工厂:按设置选型;whisper 空 timestamps 下游段内分离退化路径。

## 验收冒烟

1. 录一场 → 停止 → 稍后 wav 消失、m4a 出现,播放跟随高亮准确(允差感知不出);
2. 旧笔记(合并前录的)启动后被回溯压缩,播放正常;
3. 续录一条已压缩笔记 → 正常追加,停止后重新变 m4a;
4. 设置页改数据目录 → 迁移完成,笔记列表/声纹库完好,新录一场落新目录;
5. 改模型目录 → 迁移后 models status 正常,能直接开录;
6. 切 Whisper(先经设置页下载)→ 录一场,转写出字、说话人段级标签正常;切回 SenseVoice 恢复全功能。

## 已知取舍(记录在案)

- 续录已压缩笔记 = 旧音频多一代有损(32kbps 语音几代内可接受,用户已确认)。
- AAC 编码器 priming 理论上引入毫秒级对齐偏差,由冒烟第 1 项验证(m4a 容器 edit list 通常已消除);若实测超允差,回退方案为 duration/offset 校正,不动架构。
- Whisper 选型下语言过滤与段内分离降级(见 §三);两降级文案就地展示,不做功能补齐。
- 迁移不支持跨卷断点续传:失败整体回退重来(个人工具数据量可控)。
- 转码进行中发起迁移:由后端暂停转码队列并等待 in-flight 转完(pause_and_wait),而非 UI 层禁用迁移入口——体验更优的实现偏离(用户无需盯着下载态等它结束)。
- 目录迁走后暂不支持一键"迁回默认":app_data 根始终非空(settings.json 等自举文件常驻),迁回会撞"目标目录非空"守卫;记为 backlog,当前需手动清理后迁移。
- 解码失败的 m4a 改名 `.bad` 永久留存(取证语义,便于事后排查损坏样本),不自动清理——个人工具磁盘占用可接受,宁留证据不静默丢弃。
