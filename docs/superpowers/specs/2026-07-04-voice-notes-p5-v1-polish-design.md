# P5 — v1 收尾：模型管理 + 录制视图完善 + 段落编辑（设计文档）

日期：2026-07-04
分支：p5-v1-polish（特性分支 → PR → squash 合入 master）
上游文档:总设计 docs/superpowers/specs/2026-06-30-voice-notes-design.md

## 0. 背景与目标

v1 的最后一块。三个目标：

1. **模型管理**：应用能交给非开发者使用——首次启动检测模型缺失，应用内引导下载
   （进度、断点续传、校验、镜像加速），替代手动 `fetch_models.sh`。前提是把模型目录
   从编译期烙死的 `CARGO_MANIFEST_DIR/models` 迁到运行时解析的 app data dir。
2. **录制视图完善**：暂停/恢复、计时器、麦克风电平表。
3. **段落编辑**：详情页支持逐段改文本、删除段落、改说话人归属，回写 segments.jsonl。

另并入 7 项历史终审遗留小修（§4）。三块合一个阶段、一个 PR。

## 1. 模型管理

### 1.1 模型目录解析

新模块 `src-tauri/src/models.rs`。目录解析顺序（`models_root()`）：

1. `VN_MODELS` 环境变量（测试/高级用户覆盖）；
2. debug 构建（`cfg(debug_assertions)`）且 `CARGO_MANIFEST_DIR/models` 存在 → 用它
   （开发机零迁移，现有模型原地可用）；
3. 生产默认：`app_data_dir()/models`（不存在则创建）。

`app_data_dir` 需要 AppHandle：`setup` 阶段解析一次存入 `OnceLock<PathBuf>`；
`lib.rs` 现有 4 处 `models_dir()` 调用点（sense_voice_dir / speaker_model_path /
vad_path / 预载）全部收敛到 `models::root()`。集成测试不经 app 启动，走 1/2 兜底
（现状不变）。

### 1.2 工件清单（manifest）

运行时需要 3 个工件，清单硬编码在 `models.rs`（url、类型、最终文件、SHA256、大小）：

| 工件 | 类型 | 体积 | 最终产物 |
|---|---|---|---|
| silero_vad.onnx | 单文件 | ~2MB | silero_vad.onnx |
| 3dspeaker CAM++ 声纹 | 单文件 | ~28MB | 3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx |
| SenseVoice-small | tar.bz2 ~1GB | 解压 ~380MB | sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/{model.onnx, tokens.txt} |

- **校验**：对**最终文件**算 SHA256 与钉死常量比对（常量从开发机现有模型文件计算，
  实现任务中生成）。下载流本身只核对字节数 = Content-Length（tarball 无法预知内部
  哈希，续传后统一在解压/落位后校验）。
- **完整性判定**（`models_status` command）：逐工件报 `present`（最终文件齐且大小
  匹配）。**录制可用性 = SenseVoice + VAD 均 present**；声纹缺失仅降级（现状已支持，
  录制照常）。
- whisper-base 不在清单内（仅测试/备选用途）；`fetch_models.sh` 保留为开发/CI 工具。

### 1.3 下载器

依赖：`ureq`（阻塞、rustls，不引 tokio）+ `tar` + `bzip2`。

- `download_models` command：后台线程逐个下载缺失工件。写 `<name>.part`，
  HTTP `Range` 断点续传（服务端不支持 Range 则从头重下）。
- 进度事件 `model_download`：`{ artifact, received_bytes, total_bytes, phase, message }`，
  phase ∈ downloading | extracting | verifying | done | error。节流 ≤4Hz。
- 单文件：`.part` 下完 → 校验 → rename 落位。
- tarball：下完 → 解压到 `models_root/.tmp-extract/` → 校验最终文件 → rename 进位
  → 删 `.part` 与临时目录（启动时清扫残留 `.tmp-extract`）。
- `cancel_models_download` command：AtomicBool 信号；取消保留 `.part` 供续传。
- 防重入：下载已在进行中再调 `download_models` → Err。
- 全部完成后触发模型预载：把 `setup` 里的预载逻辑抽成函数，下载 done 后再跑一次
  （录制无需重启即可用）。
- 防御：`start_recording` / `resume_recording` 入口检查录制必需工件，缺失 →
  `Err("模型缺失…")`（正常路径 UI 已挡，此为兜底）。

### 1.4 镜像加速

- `app_data_dir()/settings.json`：`{ "mirror_prefix": string, "mirror_enabled": bool }`，
  原子写（沿用 tmp+rename 模式）。`get_settings` / `set_settings` commands。
- 开启时下载 URL = `mirror_prefix` 拼接原 URL（ghproxy 类前缀，默认值给一个可用样例，
  卡片内可改）。默认关闭 = GitHub 直连。

### 1.5 UI：录制页内嵌下载卡片

- 录制页挂载时查 `models_status`。录制必需工件（SenseVoice / VAD）缺失 → 录制控制区
  被**下载卡片**替代：缺失工件清单与总大小、「下载」按钮、每工件+整体进度条、镜像
  开关与前缀输入、取消/重试（续传）、错误信息展示。下载动作总是一键补齐**全部**缺失
  工件（含声纹）。
- 仅声纹缺失（录制必需已齐）→ 不出大卡片，显示小提示条「说话人区分需补下声纹模型」
  + 补下按钮，不挡录制。
- 下载全部完成 → 卡片消失、录制可用。笔记浏览/导出全程不受影响。

## 2. 录制视图完善

### 2.1 暂停 / 恢复

**语义**：暂停期间不产生任何转写与落盘；时间轴不前进（与续录 base_ms 的「活跃时长」
语义一致）；采集与 VPIO 保持运行（AEC 偏好连续运行），帧在 segment_worker 入口被闸断
丢弃。暂停瞬间把在途语句 flush 为 final（不丢已说的话），并清空 partial 槽。

**实现**：
- `Arc<AtomicBool> paused` 贯穿各 segment_worker；worker 检测到 false→true 跳变时对
  segmenter 执行尾段 flush（复用 stop 的尾段路径），此后丢帧直至复位。
- `RecordingHandle::set_paused(bool)`；commands `pause_recording` / `unpause_recording`
  （命名刻意区别于续录 `resume_recording`）；guard：非录制中调用 → Err。
- `ActiveSession` 记 `paused`；`recording_status` 返回之；status 事件 state 新增
  `"paused"`（恢复时重发 `"recording"`，携带原 system_audio/diarization/note_id）。
- 前端：`recording.svelte.ts` 增 paused 态（注意 `"paused"` 不得触发 finals 清空的
  `"recording"` 分支——恢复时重发的 `"recording"` 需以 note_id 相同为准跳过清空，
  复用 resuming 同类守卫）；录制页与侧栏按钮同步（录制中：暂停+停止；暂停中：
  恢复+停止）。paused 时清 partial 显示。stop 在 paused 下直接可用。

### 2.2 计时器

- 后端为真值源：`ActiveSession` 记开始 Instant 与暂停累计时长（pause/unpause command
  时刻更新）；`recording_status` 返回 `elapsed_active_ms`（续录时加 base_ms 起算，
  显示笔记总活跃时长）。
- 前端本地 1s tick 渲染，挂载/状态翻转时用 `recording_status` 对账（冷刷新不丢表）。
  paused 时表停走。

### 2.3 麦克风电平表

- mic 路 segment_worker 帧入口（**闸前**）计算 RMS→线性 0..1（或 dBFS 映射），
  节流 ~80-100ms 经新增 `on_level` 回调 → `lib.rs` emit `level` 事件 `{ rms }`（仅 mic）。
- 前端：录制页顶部细电平条。因在闸前计算，暂停时电平表继续活跃——用户可确认麦克风
  仍在工作。停止后归零。

### 2.4 录制页控制条

录制页顶部集中控制条：开始/暂停/恢复/停止按钮 + 计时器 + 电平条。侧栏原有开录/停止
入口保留并与 paused 态联动。

## 3. 段落编辑（详情页）

### 3.1 后端

- `NoteStore` 新增编辑原语，统一模式：整文件读 → 内存改 → 原子重写 segments.jsonl
  （tmp+rename，沿用单文件原子哲学）。损坏行（不可解析）原样保留在原位置，不因编辑
  丢失。
- **段定位与乐观并发**：以 `seq` 为主键定位。实现任务需先验证 `NoteWriter::resume`
  的 seq 延续性——若续录会重复 seq，则改用 `(seq, source, start_ms)` 三元组。command
  一律携带 `expected_text` 做乐观校验：重读时不匹配 → Err，前端提示刷新后重试。
- commands（均 guard：目标是活动会话笔记 → Err，与 rename_note 同模式）：
  - `edit_segment(note_id, seq, expected_text, new_text)`：改 text；new_text trim 后为空 → Err
    （提示用删除）。
  - `delete_segment(note_id, seq, expected_text)`：物理删行。
  - `set_segment_speaker(note_id, seq, expected_text, speaker_id)`：speaker_id 为既有 id，
    或哨兵 `"new"` → 后端分配 `S<max+1>` 写入 speakers.json（name 空、无 centroid、
    count=0——续录时 registry_snapshot 对空质心项已兼容编号）。只改 segment 的 speaker
    字段，**不回灌声纹质心**（离线编辑不影响聚类）。
- 删除/改归属后 speakers.json 不清孤儿说话人（无害，见 §6 非目标）。

### 3.2 前端（详情页）

- 每段 hover 出操作：**编辑**（内联 textarea，Enter/失焦保存、Esc 取消）、**删除**
  （confirm）、**说话人徽章点击** → 菜单（既有说话人列表 + 「新说话人」）。
- 仅非 active 笔记可编辑（active 时隐藏操作入口；后端另有 guard）。
- 保存成功后重载 getNote（简单可靠）；乐观冲突 Err → 提示并重载。

## 4. 随手修遗留项（并入本阶段）

1. 详情页过滤空白段（P3 遗留）。
2. 详情页段列表按 `start_ms` 稳定排序（P4.5 遗留，消除 ECHO hold 造成的交错）。
3. 说话人 chips 数值排序（S2 < S10，P4 遗留）。
4. `speakerColor` 对非 `S<n>` 形态 id 的兜底色（P4 遗留）。
5. 详情页 h1 标题改名补键盘入口（P3 遗留 a11y）。
6. 侧栏改名不吹掉详情页未提交编辑态（P3.5 遗留；本阶段详情页有编辑功能后升级为必修
   ——notesVersion 刷新时保留编辑中状态或提示）。
7. 中途挂载水合：冷刷新与「已在录制」对账分支用 getNote 回灌 finals+speakers
   （P3.5 + P4.5 遗留合并项）。

**明确不做**：ECHO 三常量与聚类阈值二轮校准（挂起，等冒烟报告数据驱动）；rename 持锁
收窄、MERGE_CHECK_INTERVAL 冗余等纯内部优化项。

## 5. 测试策略

- **models.rs**：目录解析顺序、manifest 完整性判定、Range 头/续传偏移计算、SHA256
  校验函数、小 fixture tar.bz2 的解压进位与 .tmp-extract 清扫——全部纯函数/本地文件
  单测，不引测试 HTTP 服务器；真网络下载走人工冒烟。
- **暂停**：MockCapture+MockSegmenter 确定性测试——pause 后不再产 final、在途句被
  flush、unpause 恢复产段、paused 下 stop 干净退出。计时累计逻辑 command 层单测。
- **电平**：RMS 计算与节流单测。
- **段落编辑**：NoteStore 单测——改文本/删行/换说话人/新建说话人编号、乐观冲突拒绝、
  活动笔记拒绝、损坏行经重写保留、空文本拒绝。
- 前端 `npm run check` + `npm run build`；端到端人工冒烟清单（下载全流程含断点续传、
  暂停恢复计时、编辑三操作）在计划收尾任务中列出。

## 6. 非目标

- 模型版本升级/切换、多识别模型选择、部分工件单独下载 UI（一键补齐全部缺失项）。
- 编辑撤销/重做、段落合并/拆分、批量编辑。
- 说话人改归属回灌声纹质心；孤儿说话人自动清理。
- 全局设置页（镜像设置就地放在下载卡片内）。
- 录音原始音频留存（v1 之外）。
