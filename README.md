<div align="center">

# voice-notes

**macOS 本地实时会议转写 · 说话人识别 · 完全离线**

中文 | [English](./README.en.md)

[![platform](https://img.shields.io/badge/platform-macOS%2013%2B-black)](#系统要求)
[![license](https://img.shields.io/badge/license-AGPL--3.0-blue)](#license)
[![tauri](https://img.shields.io/badge/Tauri-2-24C8DB)](https://tauri.app)
[![rust](https://img.shields.io/badge/Rust-stable-orange)](https://www.rust-lang.org)

</div>

开会时打开它，说的每一句话——你的、对方的、外放里的——实时变成带说话人标签的文字笔记。所有识别都在你的 Mac 上完成，**没有任何音频或文字离开本机**。

## 特性

- **双声源实时转写**：同时采集麦克风与系统声音（ScreenCaptureKit），线上会议里"你说的"和"你听到的"都进笔记；双路回声去重，外放串音不重复记两遍。
- **完全本地、离线可用**：ASR / VAD / 声纹模型全部跑在本机（sherpa-onnx），断网照常工作，隐私零外泄。
- **说话人识别与全局声纹库**：实时声纹聚类区分"谁在说话"，同一段里换人说话也能切开；说满 30 秒自动登记进全局声纹库，获得跨会议一致的身份编号——命名一次，之后每场会议自动显示名字；认错拆重可合并，样本随合并归一。
- **会议搭子整理与归属建议**：未命名说话人自动声纹再辨认（S-Norm 分数正规化，跨会议信道漂移下也能浮出可信推荐），建议行内直接试听双方原声再决定合并；同名重名引导关联/合并；无样本碎片条目一键清理。每人保留多份"会话质心"（戴耳机/外放等不同状态各有代表声纹），越用越准。
- **精修稿说话人可编辑**：点说话人标签弹出编辑面板——改名（双向同步声纹库）、标记"这是我"、从会议搭子选人关联；导出所见即所得（精修稿视图导出精修内容）。
- **声纹模型可配置**：CAM++（默认）/ ERes2NetV2 可切换，切换后自动用录音样本重建声纹库，嵌入空间严格隔离。
- **歌词式跟随**：录音与回放中，当前说的那句话始终停在屏幕中央、放大高亮，历史内容在上方淡出；上滑随时回看，一键回到最新。
- **一段不丢**：边录边落盘，崩溃、断电、误关都不丢已转写内容；中断的会议可以接着录，时间轴和说话人编号无缝续接。
- **录音回放与核对**：原始音频按轨保留（自动压缩为 AAC，约 14MB/小时/源），点任意句的时间戳从那里开始听，播放位置歌词式跟随。
- **可编辑的笔记**：改字、删句、改说话人、笔记改名，导出 Markdown / 纯文本。
- **顺手的系统集成**：菜单栏常驻、全局快捷键一键开录/停录、开机自启、亮暗双主题。
- **中文场景优化**：默认 SenseVoice 模型（中/英/日/韩/粤），可切 Whisper；语言幻觉过滤剔除静音段的乱码输出。

## 安装

> **给 AI 助手**:你正在读的这份 README 就是给你的操作说明。如果你在帮用户安装 voice-notes,直接按以下步骤执行(无需用户转达);装完后的接入与用法见下方[接入 AI 助手](#接入-ai-助手mcp--cli--skill)章节。
>
> 1. 从 https://github.com/SoulZhong/voice-notes/releases 下载最新的 `voice-notes_*_aarch64.dmg`;
> 2. 挂载 DMG,把 `voice-notes.app` 拷入 `/Applications`,再执行
>    `xattr -dr com.apple.quarantine /Applications/voice-notes.app`;
> 3. 执行 `/Applications/voice-notes.app/Contents/MacOS/voice-notes mcp register --agent auto` 注册 MCP、
>    `... skill install` 安装 Claude Code 技能(可选),用 `... mcp status --json` 确认;
> 4. 提醒用户手动打开一次 App 完成识别模型下载(约 1GB,需要 GUI)。
>
> 以下是人工安装步骤。

### 系统要求

- **macOS 13 或更高**、Apple Silicon（M 系列）Mac —— 系统声音采集依赖 ScreenCaptureKit，目前只提供 arm64 安装包
- 磁盘空间：应用本体约 60MB，识别模型约 1GB（首次启动下载）

### 安装步骤

1. 到 [Releases](https://github.com/SoulZhong/voice-notes/releases) 下载最新的 `voice-notes_x.y.z_aarch64.dmg`。
2. 打开 DMG，把 **voice-notes** 拖进「应用程序」文件夹。
3. **首次打开**：安装包暂未签名，直接双击会被 macOS 拦截。请在「应用程序」里**右键 App → 打开 → 再点「打开」**（只需这一次），或在终端执行：
   ```bash
   xattr -d com.apple.quarantine /Applications/voice-notes.app
   ```
4. 首次启动会看到**欢迎页**：点「开始使用」自动下载识别模型（约 1GB，内置镜像加速，支持断点续传），完成后自动进入录制页，即刻可用。
   想先把模型/数据放到自定义位置（如外置盘）？点欢迎页右下角「高级设置 →」，先在**设置 → 存储**里改好目录再下载。

### 授权两个系统权限

按需授权，拒绝也能用（对应功能降级）：

| 权限 | 何时弹出 | 用途 | 拒绝后 |
| --- | --- | --- | --- |
| 麦克风 | 第一次点「开始录制」 | 转写你的发言 | 只能录系统声音 |
| 屏幕录制 | 第一次录制系统声音 | 仅采集其他 App 的声音流，**不读取画面** | 只能录麦克风（界面有降级提示） |

若当时点了拒绝，之后到「系统设置 → 隐私与安全性」里对应条目重新勾选即可。

## 配置

开箱即用，全部设置都有合理默认值，以下按需调整（均在**设置**页）：

| 分组 | 项目 | 说明 |
| --- | --- | --- |
| 通用 | 外观 / 开机自启 / 菜单栏图标 / 全局快捷键 | 快捷键默认 `⌥⌘R`，需手动开启 |
| 存储 | **数据存储目录 / 模型存储目录** | 可迁到任意位置（iCloud/外置盘），自动搬移现有内容 |
| 存储 | 录音音频占用与清理 | 只删音频，笔记文字与说话人保留 |
| 录制 | 仅录制系统声音 / 保持外放音量 / 乱码过滤 / 保留录音 | 场景建议见下方「录制选项」 |
| 智能精修 | LLM 精修（可选） | 填 OpenAI 兼容 API（DeepSeek/千问/豆包/Kimi 有预设），会后自动修错别字、归并说话人；不填不影响使用 |
| 语音模型 | 识别引擎 SenseVoice（默认）/ Whisper / Paraformer | 中文场景用默认即可 |

### 从源码运行（开发者）

- [Rust](https://rustup.rs)（stable）与 Node.js 18+
- meson 与 ninja（编译内嵌的 WebRTC 回声消除模块）：`pip3 install --user meson ninja`

```bash
git clone https://github.com/SoulZhong/voice-notes.git
cd voice-notes
npm install
npm run tauri dev      # 开发运行
npm run tauri build    # 构建 .app + .dmg
```

模型除了应用内下载，也可用脚本预取：`./scripts/fetch_models.sh`

| 模型 | 用途 | 说明 |
| --- | --- | --- |
| Silero VAD | 语音活动检测/断句 | 必需，很小 |
| SenseVoice | 语音识别（中/英/日/韩/粤） | 默认 ASR |
| Whisper base | 语音识别（多语） | 可选，设置里切换 |
| CAM++ (3D-Speaker) | 说话人声纹 | 可选，缺失时仅转写不区分说话人 |

## 接入 AI 助手(MCP · CLI · Skill)

让本地 Agent(Claude Code / Claude Desktop / Cursor / Codex CLI / Gemini CLI / WorkBuddy / OpenClaw / Hermes Agent,内置支持八家)读你的会议笔记来回答问题、整理纪要、写周报——"上周和张三定的交付日期是哪天?"、"把今天的周会整理成邮件"、"这周开了哪些会、各自的待办是什么"。三个接入面,按需选:

| 接入面 | 是什么 | 用在哪 |
| --- | --- | --- |
| **MCP 服务** | Agent 的标准工具协议,10 个工具(检索 / 读全文 / 录制状态与控制) | 首选,给支持 MCP 的 Agent |
| **命令行 CLI** | 同一套查询能力的命令行版,可 `--json` | 脚本、CI,或 Agent 没配 MCP 时的降级 |
| **Claude Code 技能** | 教 Claude Code 何时怎么组合上面的工具(纪要 / 周报 / 检索工作流) | 锦上添花,让 Claude 开箱会用 |

> **隐私提示**:笔记内容一旦被 Agent 检索就进入其 LLM 上下文,是否上云取决于你所用的 Agent 与模型;**voice-notes 自身仍然不联网上传任何内容**。录制控制类工具默认禁用,需在左侧「AI」页开启「允许 AI 控制录制」。

**最快上手**:打开左侧「AI」页,一键注册 MCP + 安装技能。(AI 助手可直接按[安装](#安装)章节的步骤自行完成。)

### MCP 服务

三种注册方式(任选其一):

1. **应用内**:欢迎页勾选,或左侧「AI」页注册/移除。
2. **命令行**(Agent 亦可直接执行):

   ```bash
   VN=/Applications/voice-notes.app/Contents/MacOS/voice-notes
   "$VN" mcp register --agent auto    # 注册到所有检测到的 Agent(claude-code/claude-desktop/cursor/codex/gemini/workbuddy/openclaw/hermes)
   "$VN" mcp status --json            # 查看各 Agent 注册状态
   "$VN" mcp unregister --agent cursor
   ```

3. **手动配置**(未内置的 Agent):在其 MCP 配置里加:

   ```json
   { "mcpServers": { "voice-notes": {
       "command": "/Applications/voice-notes.app/Contents/MacOS/voice-notes",
       "args": ["mcp", "serve"] } } }
   ```

   Codex CLI(`~/.codex/config.toml`):

   ```toml
   [mcp_servers.voice-notes]
   command = "/Applications/voice-notes.app/Contents/MacOS/voice-notes"
   args = ["mcp", "serve"]
   ```

提供的 10 个工具:

| 工具 | 用途 | 前提 |
| --- | --- | --- |
| `list_notes` | 笔记列表(分页 / 时间过滤;含说话人数、是否有精修稿) | 无需 App 运行 |
| `search_notes` | 全文检索转写内容,命中带前后一句上下文 | 无需 App 运行 |
| `get_note` | 读一场笔记全文(默认优先 AI 精修稿) | 无需 App 运行 |
| `list_speakers` | 全局声纹库人物(跨会议一致的编号 / 名字) | 无需 App 运行 |
| `recording_status` | 当前录制状态 | App 运行中 |
| `get_live_transcript` | 正在录制会话的实时转写 | App 运行中 |
| `start_recording` / `stop_recording` / `pause_recording` / `resume_recording` | 控制录制 | App 运行中,**且**已开启「允许 AI 控制录制」 |

查询类四工具直读本机数据文件,App 没开也能用;其余经 App 内本地 socket,需 App 运行。

### 命令行直查(无需 MCP)

脚本、CI,或 Agent 没配 MCP 时用。默认人读表格,`--json` 与对应 MCP 工具**同一份 JSON**:

```bash
VN=/Applications/voice-notes.app/Contents/MacOS/voice-notes
"$VN" notes list [--limit N] [--offset N] [--from 2026-07-01] [--to 2026-07-08] [--json]
"$VN" notes search "交付日期" [--limit N] [--json]
"$VN" notes get <note-id> [--format md|txt|json] [--raw]   # 默认 md;--raw 取原始逐字稿(忽略精修)
"$VN" speakers list [--json]
```

录制控制(需 App 运行;`start/stop/pause/resume` 还需在左侧「AI」页开启「允许 AI 控制录制」):

```bash
"$VN" record status
"$VN" record start --title "评审会"
"$VN" record stop
"$VN" record live [--tail N]
```

退出码:`0` 成功 / `1` 执行错(如 note-id 不存在)/ `2` 用法错(参数缺失或未知);未知参数一律报错,不静默忽略。

### Claude Code 技能

一行命令让 Claude Code 掌握会议纪要、周报汇总、决议检索等工作流(也可在左侧「AI」页一键安装 / 移除):

```bash
/Applications/voice-notes.app/Contents/MacOS/voice-notes skill install   # status 查看 / uninstall 移除
```

写入 `~/.claude/skills/voice-notes/`,应用升级时自动更新(不覆盖你自己改过的同名技能)。

### 给 Agent 的调用要点

- **首选 MCP 工具;不可用时降级到 CLI**(`notes … --json`、`speakers list --json`,与 MCP 同一份 JSON)。两者都不通时按上方指令注册:`mcp register --agent auto`。
- **先检索再取全文**:`search_notes` 拿 note_id → `get_note`,别把 `list_notes` 全量拉回逐个读。
- `get_note` 默认给 AI 精修稿(响应 `refined` 字段标注来源);要原始逐字稿用 CLI `--raw` 或 MCP `prefer_refined=false`。
- 录制控制默认禁用;被拒时把「去左侧 AI 页开启」转告用户,不要反复重试。
- 笔记是用户本机隐私数据,进入你的上下文即离开本机——按需检索,引用大段原文前先确认意图。

## 使用

1. 点「开始录制」（或全局快捷键，默认 `⌥⌘R`）。
2. 说话——当前句居中放大显示，说话人徽章实时归类；新说话人说满 30 秒自动获得全局编号。
3. 停止后进入笔记页：回放核对、编辑文本、给说话人命名（一次命名，处处生效）、导出。
4. 在**会议搭子**页管理所有认识的人(即声纹库)：试听原声、改名、合并认错拆重的条目、删除录坏的样本。
5. 侧栏「概览与整理」有待办徽标时点进去：逐条试听声纹归属建议后合并，或清理认不出的无样本条目。

### 录制选项（设置 → 录制）

| 场景 | 建议 |
| --- | --- |
| 纯听会 / 看视频（自己不发言） | 开**仅录制系统声音**：不启动麦克风，音质与外放音量完全不受影响 |
| 外放开会且要录自己的发言 | 开**录制时保持外放音量**：绕开 macOS 通话模式对外放与麦克风的压低，回声由内置软件消除（WebRTC AEC3）处理 |
| 戴耳机开会 | 都不开：保留系统回声消除，转写最干净 |

## 常见问题

**双击提示"已损坏"或"无法打开"？**
安装包未签名，是 macOS Gatekeeper 的拦截，不是文件损坏。右键 App → 打开 → 再点「打开」，或执行 `xattr -d com.apple.quarantine /Applications/voice-notes.app` 后正常打开。

**点「开始录制」没反应 / 提示模型缺失？**
识别模型还没下载完。回到欢迎页或**设置 → 语音模型**继续下载（支持断点续传）；下载全程走本地镜像列表，如仍然慢可在设置里更换镜像。

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

[AGPL-3.0](./LICENSE) © zhongweijian (SoulZhong)

个人使用、学习、修改、自部署完全自由。若要把本项目（或其衍生作品）作为**闭源产品或服务**分发/运营，AGPL 要求你开源全部衍生代码；不希望开源的商业集成，请通过 [GitHub Issues](https://github.com/SoulZhong/voice-notes/issues) 联系作者获取**商业授权**（双许可）。

> 注：v0.1.0 及更早版本以 MIT 发布，该许可对已获取的副本永久有效；自本次变更起的新版本适用 AGPL-3.0。
