# 录音音频保留与播放 + 声纹样本试听 设计

日期:2026-07-05
状态:已定稿(后台会话自主决策,备选方案与取舍记录在文内)

## 目标

1. 录制时**保留原始音频**,笔记详情页可**播放**,播放进度**跟随高亮对应 ASR 段落**,点击段落时间戳可跳转。
2. 全局声纹库里**每个人物保留一段代表性录音样本**,声纹管理页可**试听确认**"这个 P# 到底是谁"。

## 非目标(YAGNI)

- 音频压缩(Opus/FLAC):v1 直接 WAV,后续量大再做压缩/清理策略。
- 倍速播放、波形图:后续增强。
- 每场笔记内 S# 说话人的独立样本:详情页点段落即可听到该说话人,不重复建设。
- 导出带音频:导出仍是纯文本/Markdown。

## 关键事实(现状约束)

- `run_segment_worker` 中,每源(mic/system)独立线程把原生帧归一为 **16kHz 单声道 f32** 后喂给 VAD;段的 `start_ms/end_ms` 直接由该流的**累计样本数**换算(样本钟)。暂停 = 丢帧 = 时间轴冻结。
- 因此:**把同一路重采样流旁路写成 WAV,文件位置与段时间戳按构造精确对齐**,无需任何对时逻辑。
- 续录:`base_ms` = 上场最大 `end_ms`,新段时间戳整体 +base_ms。
- 声纹:段 PCM 在 ASR worker `process_final` 处可得(`registry.assign` 返回簇 id);停止时 `DiarEvent::Snapshot` → `upsert_from_session` 决定入库人物。
- `hound` 已在依赖;Tauri v2 asset protocol 支持 Range,可流式播放大 WAV。

## 方案选型

### 音频保留形态(选 A)

- **A. 每源连续 WAV(mic.wav / system.wav),前端统一时钟驱动双 `<audio>` 同步** ← 选定
  - 后端零共享状态:每源 worker 写自己的文件,失败隔离,与本仓"简单、诚实降级"哲学一致。
  - 对齐按构造成立;单源场景(常见)自然退化为单文件。
- B. 后端实时混音单 audio.wav:播放端最简,但混音器要处理双流漂移/一路停摆/迟到写入,复杂度和"一个 bug 丢两路音频"的风险都在后端;放弃。
- C. 每段独立小文件:丢弃 VAD 判静音的间隙(echo/残渣被丢的段也没了),"保留录音"名不副实;连续播放要拼接调度;放弃。

格式:WAV PCM s16le 16kHz 单声道,≈115MB/小时/源。个人会议工具可接受,文档记录后续压缩方向。

### 播放端(HTMLAudio + 自有时钟,不用 WebAudio 全量解码)

`decodeAudioData` 会把 1h 音频按 44.1/48k 解成数百 MB 内存;`<audio>` 走 asset 协议流式 + Range,内存恒定。选后者。

## 数据模型

笔记目录新增:

```
notes/<id>/
  mic.wav        # 16k mono s16,可缺(该源未启动/写失败)
  system.wav
  audio.json     # {"schema_version":1,"tracks":{"mic":{"offset_ms":0},...}}
```

`offset_ms`:该 WAV 的 0 时刻对应笔记时间轴的毫秒。需要它的原因:**轨道可以中途才出现**——续录旧笔记(feature 上线前无 WAV)、或第一场 system 被拒二场授权成功。时间轴位置 = offset_ms + 文件内时刻。

声纹库新增:

```
voiceprints/<Pn>.wav   # 该人物的代表性样本(≤15s,16k mono s16)
```

## 写入路径(Rust)

新模块 `store/audio.rs`:

- `AudioTrackWriter::open(note_dir, source, base_ms)`:
  - 文件不存在 → 写 44 字节头,`audio.json` 记 `offset_ms = base_ms`(新建笔记即 0)。
  - 已存在 → 修复陈旧头(崩溃恢复:按实际文件长度回写 RIFF/data 尺寸),再 `set_len` 到 `base_ms - offset_ms` 对应字节(截尾段末尾的静音/被丢段;不足则 set_len 零填充)——保证续录新音频落位即对齐。
  - 打开失败 → 返回 Err,调用方降级为不录音频(eprintln,不影响转写)。
- `append(&[f32])`:clamp→s16le 追加,**每 ≥1s 刷盘并回写头部尺寸**(任意时刻文件都是合法 WAV,崩溃最多丢 1s 音频)。写失败:eprintln 一次并永久停写(音频是增值层,绝不拖垮转写)。
- `Drop` 兜底 finalize(补头+flush),worker 任何退出路径都收尾。
- `list_tracks(note_dir) -> Vec<TrackInfo{source, path, offset_ms, duration_ms}>`(详情页命令用)。

接线:

- `start_session` 新参 `audio_sinks: Vec<(Source, Box<dyn FnMut(&[f32]) + Send>)>`;`run_segment_worker` 新参 `audio_sink: Option<…>`,在**暂停闸之后、VAD accept 之前**调用——写入的样本与 segmenter 计数的样本严格同源。
- `spawn_session` 在 writer 建好后为每个成功配置的源 `AudioTrackWriter::open`,包成闭包传入。

## 播放路径(前端)

- 新命令 `note_audio_info(id) -> Vec<TrackInfo>`(路径为绝对路径;非活动笔记顺带修复陈旧 WAV 头,活动笔记跳过修复避免与写入线程竞争)。
- `tauri.conf.json` 开启 asset 协议,scope 限 `$APPDATA/notes/**` 与 `$APPDATA/voiceprints/**`;前端 `convertFileSrc(path)`。
- 新组件 `lib/AudioPlayer.svelte`:
  - 隐藏 `<audio>` × N;自有时钟(rAF + performance.now)驱动 UI 与文字跟随;有轨道覆盖当前时刻时以该轨道 `currentTime` 为准(音频即真时钟),否则墙钟推进(轨道间隙)。
  - 每帧对各轨:期望位置 = 时钟 − offset;在界内 → 确保播放且偏差 >0.3s 时回拉;界外 → 暂停。
  - 进度条沿用 DESIGN.md download-card 进度条形态(轨 `hairline`、填充 `accent`、6px、rounded-full),按钮用 button-secondary,时间 tabular-nums。
  - 对外:`tracks` prop、`currentMs` bindable、`seek(ms)`。
- 详情页:有轨道才显示播放器;播放中 `currentMs ∈ [start_ms,end_ms)` 的段加高亮底色(accent-tint)并 `scrollIntoView(nearest)`;段落时间戳变为可点按钮 → `seek(start_ms)`。录制中的笔记不显示播放器(文件在写,避免边写边播的半态)。

## 声纹样本路径

- ASR worker 维护 `簇id → 最长段样本(截 15s)`:`process_final` 返回归属簇 id 并在内部处理簇合并时迁移样本(loser 样本更长则归 winner)。
- `DiarEvent::Snapshot` 变体扩展为 `{ snaps, samples: Vec<(String, Vec<f32>)> }`。
- lib.rs Snapshot 分支:`upsert_from_session` 后,对每个"已关联人物"(snap.person 或本次新入库 new_links)的簇,若 `voiceprints/<pid>.wav` **不存在**则写入样本(`VoiceprintStore::write_sample_if_missing`)。只写首个样本:样本是"确认此人是谁"的稳定参照,不随会话滚动。
- `merge_person`:winner 无样本时继承 loser 的样本文件(rename),否则删 loser 的;`delete_person` 连带删样本。均在 VoiceprintStore 内完成(它持有 root)。
- `list_people` 返回值增加 `sample_path: Option<String>`;管理页出现「试听」按钮(共享一个 Audio 实例,点击切换播放)。

## 错误处理原则

- 音频/样本全链路是**增值层**:任何失败 eprintln + 降级(不写/不显示播放器/无试听按钮),绝不影响转写落盘与录制主流程。
- WAV 头随每次刷盘回写 → 崩溃后文件仍可播;续录 open 时再校一遍。
- 删除笔记 = 删目录,音频自然清理;audio.json 缺失/损坏按全 0 offset 容忍。

## 测试

- store/audio:头写入/追加/finalize 可被 hound 读回;续录 pad/truncate 与 offset 语义;陈旧头修复;audio.json 往返与损坏容忍。
- segment_worker:sink 收到的样本 == accept 的样本;暂停期不写。
- session:audio_sinks 正确接线(closure 收样本按源隔离)。
- asr worker:Snapshot 携带样本、上限截断、合并迁移。
- voiceprints:write_sample_if_missing / merge 继承 / delete 连删。
- 前端:`npm run build` + svelte-check 通过;播放器逻辑以纯函数拆出的部分(轨道期望位置计算)不单测(无 test harness),靠冒烟。
