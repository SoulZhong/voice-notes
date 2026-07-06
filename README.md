<div align="center">

# voice-notes

**macOS 本地实时会议转写 · 说话人识别 · 完全离线**

中文 | [English](./README.en.md)

[![platform](https://img.shields.io/badge/platform-macOS%2013%2B-black)](#系统要求)
[![license](https://img.shields.io/badge/license-MIT-blue)](#license)
[![tauri](https://img.shields.io/badge/Tauri-2-24C8DB)](https://tauri.app)
[![rust](https://img.shields.io/badge/Rust-stable-orange)](https://www.rust-lang.org)

</div>

开会时打开它，说的每一句话——你的、对方的、外放里的——实时变成带说话人标签的文字笔记。所有识别都在你的 Mac 上完成，**没有任何音频或文字离开本机**。

## 特性

- **双声源实时转写**：同时采集麦克风与系统声音（ScreenCaptureKit），线上会议里"你说的"和"你听到的"都进笔记；双路回声去重，外放串音不重复记两遍。
- **完全本地、离线可用**：ASR / VAD / 声纹模型全部跑在本机（sherpa-onnx），断网照常工作，隐私零外泄。
- **说话人识别与全局声纹库**：实时声纹聚类区分"谁在说话"，同一段里换人说话也能切开；说满 10 秒自动登记进全局声纹库，获得跨会议一致的身份编号——命名一次，之后每场会议自动显示名字；认错拆重可合并，样本随合并归一。
- **歌词式跟随**：录音与回放中，当前说的那句话始终停在屏幕中央、放大高亮，历史内容在上方淡出；上滑随时回看，一键回到最新。
- **一段不丢**：边录边落盘，崩溃、断电、误关都不丢已转写内容；中断的会议可以接着录，时间轴和说话人编号无缝续接。
- **录音回放与核对**：原始音频按轨保留（自动压缩为 AAC，约 14MB/小时/源），点任意句的时间戳从那里开始听，播放位置歌词式跟随。
- **可编辑的笔记**：改字、删句、改说话人、笔记改名，导出 Markdown / 纯文本。
- **顺手的系统集成**：菜单栏常驻、全局快捷键一键开录/停录、开机自启、亮暗双主题。
- **中文场景优化**：默认 SenseVoice 模型（中/英/日/韩/粤），可切 Whisper；语言幻觉过滤剔除静音段的乱码输出。

## 快速开始

### 系统要求

- macOS 13 或更高（系统声音采集依赖 ScreenCaptureKit）
- [Rust](https://rustup.rs)（stable）与 Node.js 18+
- meson 与 ninja（编译内嵌的 WebRTC 回声消除模块）：`pip3 install --user meson ninja`
- 权限：麦克风（转写你的发言）、屏幕录制（仅用于采集系统声音，不采集画面）

### 从源码运行

```bash
git clone https://github.com/SoulZhong/voice-notes.git
cd voice-notes
npm install
npm run tauri dev      # 开发运行
npm run tauri build    # 构建 .app
```

### 模型下载

首次启动后在**设置 → 模型**里一键下载（内置镜像加速，适合国内网络），或用脚本预取：

```bash
./scripts/fetch_models.sh
```

| 模型 | 用途 | 说明 |
| --- | --- | --- |
| Silero VAD | 语音活动检测/断句 | 必需，很小 |
| SenseVoice | 语音识别（中/英/日/韩/粤） | 默认 ASR |
| Whisper base | 语音识别（多语） | 可选，设置里切换 |
| CAM++ (3D-Speaker) | 说话人声纹 | 可选，缺失时仅转写不区分说话人 |

## 使用

1. 点「开始录制」（或全局快捷键，默认 `⌥⌘R`）。
2. 说话——当前句居中放大显示，说话人徽章实时归类；新说话人说满 10 秒自动获得全局编号。
3. 停止后进入笔记页：回放核对、编辑文本、给说话人命名（一次命名，处处生效）、导出。
4. 在**声纹库**页管理所有认识的人：试听原声、改名、合并认错拆重的条目。

### 录制选项（设置 → 录制）

| 场景 | 建议 |
| --- | --- |
| 纯听会 / 看视频（自己不发言） | 开**仅录制系统声音**：不启动麦克风，音质与外放音量完全不受影响 |
| 外放开会且要录自己的发言 | 开**录制时保持外放音量**：绕开 macOS 通话模式对外放与麦克风的压低，回声由内置软件消除（WebRTC AEC3）处理 |
| 戴耳机开会 | 都不开：保留系统回声消除，转写最干净 |

## 常见问题

**为什么要屏幕录制权限？**
macOS 采集系统声音（别的 App 放出来的声音）只能走 ScreenCaptureKit，它归在"屏幕录制"权限下。本应用只取音频流，不读取任何画面。

**开始录制后，外放声音变小 / 会议对方说我声音变小？**
这是 macOS 通话模式（VPIO 回声消除）的固有行为。按上表打开「仅录制系统声音」或「录制时保持外放音量」即可根治。

**数据存在哪里？**
默认在应用数据目录，可在设置里迁移到任意位置（如 iCloud/外置盘）。每场会议一个文件夹：`meta.json` + `segments.jsonl`（逐句转写）+ 音频轨 + `speakers.json`，纯文本格式，随时可被其他工具读取。

**支持 Windows / Linux 吗？**
目前仅 macOS（系统声音采集、回声消除、菜单栏都依赖平台能力）。转写管线本身是跨平台的 Rust，欢迎贡献其他平台的音频采集层。

## 工作原理

```
麦克风 ──┐                            ┌─ 实时字幕（歌词式跟随）
         ├─ VAD 断句 ─ ASR 识别 ─ 声纹归簇 ──┼─ 逐句落盘 segments.jsonl
系统声音 ─┘   (Silero)  (SenseVoice)  (CAM++)  └─ 全局声纹库（跨会议身份）
             回声去重 · 语言过滤 · 段内说话人切分
```

技术栈：[Tauri 2](https://tauri.app)（Rust 后端 + 系统集成）、[SvelteKit](https://svelte.dev)（界面）、[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx)（本地推理）。界面设计遵循仓库根的 [DESIGN.md](./DESIGN.md)。

## 开发

```bash
npm run check                 # 前端类型检查
cd src-tauri && cargo test    # 后端测试
```

## License

[MIT](./LICENSE)
