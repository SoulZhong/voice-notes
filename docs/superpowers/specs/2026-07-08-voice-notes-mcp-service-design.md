# voice-notes MCP 服务 设计

日期:2026-07-08。目标:把 voice-notes 发布成 MCP(Model Context Protocol)服务,本地 Agent(Claude Code / Claude Desktop / Cursor / Codex CLI / Gemini CLI 等)可直接检索会议笔记、查询录制状态、(可选)控制录制;应用初始化时引导用户一键注册到本机已安装的 Agent,设置页可随时增删;README 提供「让 Agent 自己安装本 App」的引导。

> 本设计在用户离线的后台会话中产出,关键取舍以「备选方案+理由」形式记录,待用户审阅拍板;未获批准前不做任何实现。

## 一、动因与场景

用户的会议记忆都在 voice-notes 里,但目前只能靠 App 内翻找。接入 MCP 后,典型场景:

- 「上周和张三开会说的交付日期是哪天?」→ Agent 调 `search_notes` / `get_note` 直接引用原文回答。
- 「把今天的周会纪要整理成邮件」→ Agent 拉取笔记全文(含精修稿)加工。
- 「现在开始录制,标题写『需求评审』」→ Agent 控制录制(可选能力,默认关)。
- Agent 写日报/周报时,自动汇总本周所有会议要点。

**隐私边界的变化必须显式承认**:本产品卖点是「完全本地、内容零外泄」;而 MCP 把笔记文本交给 Agent 后,内容会进入 LLM 上下文(多数 Agent 的模型在云端)。因此:注册行为本身即知情同意入口,引导页与 README 必须写明这一点;录制控制类敏感能力另设独立开关且默认关闭。

## 二、总体架构(方案选型)

### 备选方案

| 方案 | 说明 | 优点 | 缺点 |
|---|---|---|---|
| A. App 内置 HTTP MCP(Streamable HTTP, localhost 端口) | Agent 注册一个 URL | 实现集中;实时能力天然可用 | App 必须常驻运行;固定端口冲突/漂移;localhost 需要鉴权(token 分发麻烦);部分 Agent 对 HTTP MCP 支持参差 |
| B. 纯 stdio,直接读数据文件 | Agent spawn 本 App 二进制(带子命令),进程内直读笔记目录 | stdio 是所有 Agent 的最大公约数;App 不运行也能查询;零端口零鉴权 | 拿不到「活」状态(录制中/实时转写);写操作与 GUI 并发有双写风险 |
| **C. stdio 入口 + Unix socket 桥(推荐)** | Agent spawn stdio 进程;查询类直读文件;状态/控制/写操作经 UDS 转发给运行中的 GUI 实例,GUI 未运行则该类工具明确报错 | 兼容性最好 + 查询不依赖 App 运行 + 活能力齐全;UDS 文件权限 0600,无网络暴露 | 多一层进程间协议(很薄,JSON 行协议即可) |

**选 C**。核心价值(笔记检索)在 App 不运行时也可用;录制控制等「活」能力只在 App 运行时经 UDS 提供;写操作一律经 UDS 由 GUI 进程执行,规避 stdio 进程与 GUI 双写同一笔记目录的竞态——GUI 未运行时写类工具直接报「请先启动 voice-notes」。

### 进程形态

同一二进制,argv 分流(在 `main.rs` 进入 `app_lib::run()` 之前判断):

```
voice-notes                      # 无参:正常 GUI 启动(现状)
voice-notes mcp serve            # stdio MCP 服务进程(无 GUI、无 Dock 图标)
voice-notes mcp register [...]   # headless 注册 CLI(供欢迎页/设置页/Agent/脚本调用)
voice-notes mcp unregister [...]
voice-notes mcp status [--json]
```

不引入独立 sidecar 二进制:App bundle 里就一个可执行文件(`/Applications/voice-notes.app/Contents/MacOS/voice-notes`),Agent 配置指向它,升级覆盖同路径,注册不失效。开发态 `current_exe()` 指向 target/debug 下的二进制,同样成立。

注意:macOS LaunchServices 打开 App 不带这些参数,不会误入 CLI 分支;`mcp serve` 分支绝不触碰 GUI/托盘/快捷键初始化。

### MCP 协议实现

- 库:官方 Rust SDK **rmcp**(modelcontextprotocol/rust-sdk),stdio transport;协议版本随 SDK(2025-06-18,向下兼容协商)。备选是手写 JSON-RPC(stdio 的 MCP 报文并不复杂),但 rmcp 提供 initialize 协商、tools/list、schema 派生宏,省力且跟进协议演进;体积增量可接受。
- 需要 tokio(rmcp 依赖):仅 `mcp serve` 分支启动 runtime,GUI 路径零变化。
- **只暴露 tools,不做 resources/prompts**(v1)。理由:各 Agent 客户端对 tools 支持最普遍,resources 支持参差;YAGNI。

### 数据访问

- stdio 进程复用现有 `settings::load(app_data)` + `resolve_data_root` 找到数据根,再复用 `store::NoteStore`(纯文件读取)与 `store::refined::load_refined`。**新增约束:`NoteStore` 及相关读取路径不得引入 GUI/AppHandle 依赖**(现状即满足,设计上固化)。
- GUI 侧对笔记文件均为原子写(已有 `write_refined_atomic` 等),stdio 只读并发安全;个别读到半迁移状态(用户正在迁移数据目录)按普通 IO 错误返回。

### UDS 桥(GUI ⇄ stdio)

- GUI 启动时在 `app_data/mcp.sock` 上 listen(权限 0600),退出时清理;socket 路径固定在 app_data(不随数据目录迁移)。
- 协议:每行一个 JSON 请求/响应(`{"op":"recording_status"}` → `{"ok":true,...}`),薄封装现有 `#[tauri::command]` 同名逻辑;不复用 tauri IPC。
- stdio 进程按需连接:连不上 = App 未运行,依赖 UDS 的工具返回带指引的错误(isError=true,文本「voice-notes 未在运行,请先启动应用」)。

## 三、MCP 工具清单(v1)

命名不带前缀(客户端会自然带上 server 名 `voice-notes`)。所有输出为结构化 JSON 文本;时间戳一律 RFC3339 + 毫秒偏移双给。

### 查询类(直读文件,App 无需运行)

| 工具 | 参数 | 返回 |
|---|---|---|
| `list_notes` | `limit`(默认 20,≤100)、`offset`、`from`/`to`(RFC3339,可选) | 笔记摘要数组:id、标题、开始时间、时长 ms、说话人数、是否有精修稿 |
| `search_notes` | `query`(必填)、`limit`(默认 20) | 命中数组:note_id、标题、命中句(seq、text、speaker、start_ms)、上下文各一句 |
| `get_note` | `note_id`(必填)、`format`:`"segments"`(默认,逐句含说话人/时间戳)/`"markdown"`/`"text"`、`prefer_refined`(默认 true,有精修稿给精修稿并标注) | 笔记全文 |
| `list_speakers` | — | 全局声纹库人物:id、名字、登记时间、出现的笔记数 |

搜索实现:遍历笔记目录做子串匹配(大小写不敏感),不建索引。个人会议笔记量级(数百场 × 每场几百句)全扫 <100ms,YAGNI;若未来量级上来再谈索引。

### 状态/实时类(经 UDS,App 需运行)

| 工具 | 参数 | 返回 |
|---|---|---|
| `recording_status` | — | state(idle/recording/paused/stopped)、note_id、elapsed_ms、system_audio、diarization |
| `get_live_transcript` | `tail`(默认 50 句) | 当前录制会话已定稿的最近 N 句(source/speaker/text/start_ms) |

`get_live_transcript` 数据源:GUI 侧会话已有 finals 落盘 + 内存态,UDS handler 从当前 session 取;无活动会话返回明确提示。

### 控制类(经 UDS + 独立开关,默认关闭)

| 工具 | 参数 | 行为 |
|---|---|---|
| `start_recording` | `title`(可选) | 等价于点「开始录制」;返回 note_id |
| `stop_recording` | — | 停止并返回 note_id |

设置项 `mcp_allow_control`(默认 false)关闭时,这两个工具**仍出现在 tools/list**(便于 Agent 发现能力)但调用返回「已被用户在设置中禁用,请到 设置 → AI 助手接入 开启」。始终列出的理由:多数客户端缓存 tools 列表,动态增删易踩客户端兼容坑。

### 明确不做(v1)

- 写/编辑类工具(改字、删句、改说话人、合并人物):风险大收益小,Agent 场景以读为主。v2 再议。
- `export_note` 落盘导出:`get_note(format="markdown")` 已覆盖内容获取,Agent 自己会写文件。
- 音频访问:音频文件大且 Agent 用不上,不暴露路径之外的读取工具。

## 四、注册引导(核心交互)

### 支持的 Agent 与配置落点(macOS)

| Agent | 检测依据 | 配置文件 | 写法 |
|---|---|---|---|
| Claude Code | `~/.claude` 或 PATH 有 `claude` | `~/.claude.json` 顶层 `mcpServers`(user scope) | JSON |
| Claude Desktop | `~/Library/Application Support/Claude/` | 同目录 `claude_desktop_config.json` | JSON |
| Cursor | `~/.cursor/` | `~/.cursor/mcp.json` | JSON |
| Codex CLI | `~/.codex/` | `~/.codex/config.toml` `[mcp_servers.voice-notes]` | TOML |
| Gemini CLI | `~/.gemini/` | `~/.gemini/settings.json` `mcpServers` | JSON |

统一注册条目(JSON 形态,TOML 同义转写):

```json
{
  "voice-notes": {
    "command": "/Applications/voice-notes.app/Contents/MacOS/voice-notes",
    "args": ["mcp", "serve"]
  }
}
```

`command` 取注册时的 `current_exe()` 规范化路径。Windsurf/Cline 等第二梯队暂不内置(配置落点各异且用户少),由「手动配置」卡片覆盖;后续按需追加只是查表加行。

### 注册器(Rust 模块 `mcp/registry.rs`)行为

- **读-改-写,只动自己的键**:解析目标文件(JSON 用 serde_json 保留未知字段;TOML 用 toml_edit 保留注释与排版),仅 upsert/remove `voice-notes` 条目;写回前留 `.bak` 备份;文件不存在则创建含单条目的最小结构;解析失败(用户手写坏了)**不写入**,报错并指引手动配置。
- **幂等**:重复注册 = 覆盖同名条目;unregister 不存在时静默成功。
- **路径漂移自愈**:GUI 每次启动扫描五个落点,发现 `voice-notes` 条目的 command 与 `current_exe()` 不一致(用户移动过 App/从 DMG 换装),静默改正并在设置页显示一条「已更新 N 个 Agent 的注册路径」的临时提示。开发态二进制(路径含 `target/`)不触发自愈,避免开发机上把用户配置指向 debug 构建。
- CLI 出口:`mcp register --agent claude-code|claude-desktop|cursor|codex|gemini|auto`(auto=所有检测到的)、`--dry-run` 打印将写入的内容;`mcp status --json` 输出各 Agent 检测/注册状态,供前端与外部 Agent 消费。

### 欢迎页(WelcomeOverlay)新增一步

现状:欢迎 → 模型下载 → 进录制页。改为模型下载完成后插入**可跳过**的「连接 AI 助手」步:

- 文案:一句场景价值(「让 Claude/Cursor 直接查你的会议笔记」)+ 一句隐私提示(「注册后,Agent 检索到的笔记内容会进入其模型上下文」)。
- 列出检测到的 Agent(checkbox,默认全选),未检测到任何 Agent 则整步自动跳过。
- 「注册所选」/「跳过」。注册结果逐项打勾/报错,不阻塞进入主界面。
- 已完成 onboarding 的存量用户不重放欢迎页:改由设置页入口承接(下节),另在升级后首启的录制页顶部给一次性提示条「新:接入 AI 助手 → 设置」。

### 设置页新增分组「AI 助手接入」

位置:「智能精修」之后。遵循 DESIGN.md 组件规范(section-title / row / toggle 与现有分组同形态)。

- **Agent 列表**:每行 = Agent 名 + 状态徽章(未安装 / 未注册 / 已注册 / 路径已修复)+ 行尾「注册」/「移除」按钮。数据来自 registry 扫描,进入设置页时刷新。
- **允许 AI 控制录制** toggle(`mcp_allow_control`,默认关),行下小字说明风险(「开启后,已接入的 Agent 可远程开始/停止录制」)。
- **手动配置**卡片:折叠展示上面的 JSON snippet(command 为本机真实路径)+「复制」按钮,covering 未内置的 Agent。
- 隐私说明一行小字 + 链接到 README 对应章节。

### settings.json 新增字段

| 字段 | 类型/默认 | 语义 |
|---|---|---|
| `mcp_allow_control` | bool false | 允许 MCP 控制录制 |
| `mcp_onboarded` | bool false | 欢迎页/提示条是否已展示过(存量用户升级提示一次性) |

注:「已注册到哪些 Agent」不进 settings——真值源是各 Agent 的配置文件本身,每次扫描得出,避免双真值源漂移(与 autostart 同思路)。

## 五、README 改动

### 新章节「接入 AI 助手(MCP)」(置于「配置」之后)

1. 功能一句话 + 场景示例两三条。
2. 自动方式:欢迎页勾选,或 设置 → AI 助手接入。
3. CLI 方式(headless):

```bash
/Applications/voice-notes.app/Contents/MacOS/voice-notes mcp register --agent auto
```

4. 手动配置 snippet(JSON + Codex 的 TOML)。
5. 工具列表表格(名称/用途/是否需要 App 运行)。
6. **隐私提示**:加粗说明「笔记内容经 Agent 会进入其 LLM 上下文,是否上云取决于你的 Agent;本 App 自身仍不联网上传」。

### 「让 Agent 安装本 App」引导

在「安装」章节顶部加提示块,给用户一段可直接粘贴给任意 Agent 的指令(Agent 阅读 README 时也能自发现):

```text
把下面这段话发给你的 AI 助手(Claude Code / Codex 等),它会帮你完成安装与接入:

请帮我安装 voice-notes 并接入 MCP:
1. 从 https://github.com/SoulZhong/voice-notes/releases 下载最新的 voice-notes_*_aarch64.dmg;
2. 挂载并把 voice-notes.app 拷入 /Applications,然后执行
   xattr -d com.apple.quarantine /Applications/voice-notes.app;
3. 执行 /Applications/voice-notes.app/Contents/MacOS/voice-notes mcp register --agent auto
   把它注册为 MCP 服务,并用 mcp status --json 确认;
4. 提醒我手动打开一次 App 完成模型下载。
```

要点:第 3 步依赖本设计的 CLI,使 Agent 无需理解各家配置格式;第 4 步明确模型下载必须 GUI 完成(1GB 下载不适合塞进 CLI 静默做)。README.en.md 同步英文版。

## 六、错误处理

- UDS 不可达:依赖 App 的工具返回 isError + 启动指引(见上);查询类永不因此失败。
- note_id 不存在 / 参数越界:isError + 明确消息,不 panic。
- 注册目标文件解析失败:拒写 + 报错 + 指引手动配置;绝不覆盖用户手写配置。
- stdio 进程崩溃:Agent 侧会重启进程,服务无状态,天然安全。
- 数据目录迁移进行中:读取报 IO 错误即返回,不重试(迁移是分钟级操作,Agent 重试自然恢复)。

## 七、测试

- **registry 单测**:JSON/TOML upsert 幂等、保留无关键与注释、备份生成、坏文件拒写、路径自愈(含 `target/` 豁免)。
- **MCP e2e 脚本**(dev 依赖,CI 可跑):spawn `voice-notes mcp serve`,走 stdio 发 initialize / tools/list / tools/call(list_notes、get_note、search_notes)对着 tempdir 假数据断言;错误路径(不存在的 note_id、UDS 未连)各一条。
- **UDS 桥集成**:GUI 测试进程内起 listener,stdio 侧 recording_status 往返一致。
- 手工冒烟:Claude Code 真机注册 → `claude mcp list` 可见 → 会话内检索一条真实笔记;Codex TOML 写入后 `codex` 可用。

## 八、分期

| 期 | 内容 | 交付判据 |
|---|---|---|
| M1 | argv 分流 + rmcp stdio 服务 + 4 个查询工具 + registry 模块 + `mcp register/unregister/status` CLI + README 双语 | Claude Code 注册后能查笔记 |
| M2 | 欢迎页「连接 AI 助手」步 + 设置页「AI 助手接入」分组 + 存量用户一次性提示 + 路径自愈 | 全 UI 流程可用 |
| M3 | UDS 桥 + recording_status / get_live_transcript + 控制类工具与 `mcp_allow_control` 开关 | Agent 可查录制状态;开关关闭时控制被拒 |

M1 独立可发版(纯增量,不碰 GUI);M2/M3 各自单 PR。

## 九、风险与开放问题

- **rmcp 版本尚在快速演进**:锁定 minor 版本,升级走显式 PR;若其 stdio 接口与 tokio 版本同 Tauri 冲突,回退方案是手写 JSON-RPC(协议面小,工具 schema 手写)。
- **App 未签名**:Agent 侧 spawn 一个被 quarantine 的二进制会失败——README 安装步骤已含 `xattr -d`,注册 CLI 在检测到 quarantine 属性时给出明确报错。
- **多版本并存**(用户同时有 /Applications 与 target/debug):路径自愈的 `target/` 豁免已覆盖;仍以「最后一次注册者为准」为语义,不做进一步仲裁。
- 开放问题(待用户拍板):① 控制类工具是否连 `pause/resume` 也要;② 欢迎页默认全选还是默认不选(当前设计:检测到即全选,倾向转化率;若更重隐私可改默认不选);③ 第二梯队 Agent(Windsurf/Cline/iFlow)是否有实际需求。
