# AI 助手接入扩展 —— 设计

日期:2026-07-10
状态:待实现
关联:MCP 服务(PR #17)、CLI+Skill(PR #17 追加);registry `src-tauri/src/mcp/registry.rs`、CLI `src-tauri/src/mcp/`、skill `src-tauri/src/mcp/skill_template.md`

## 背景与目标

voice-notes 已能把自己作为本地 stdio MCP server 注册进 5 家 Agent(Claude Code / Claude Desktop / Cursor / Codex / Gemini),并提供查询 CLI + 一份 Claude Code Skill。本次扩展做两件事:

1. **skill 全能力(含控制)**:CLI 现在只能查询,录制控制(start/stop/pause/resume)只走 MCP。给 CLI 加 `record` 控制子命令,让**任何能跑 shell 的 Agent**(经 skill)拥有和 MCP 同等的全能力。
2. **新增 3 家 Agent 接入**:WorkBuddy(腾讯云 CodeBuddy)、OpenClaw、Hermes Agent。三家均已核实为真实 MCP host(见「调研结论」)。

**成功标准**:`voice-notes record start/stop/...` 经 UDS 桥驱动 GUI 录制(门控/未运行行为与 MCP 一致);三家新 Agent 可被 `mcp register` 正确写入各自配置并被 `mcp status` 检测;现有 5 家不受影响。

## 调研结论(2026-07-10,对官方文档/源码逐条核实)

**三家新 Agent(全部真实 MCP host)**:

| Agent | 配置文件 | 容器键 | 格式 | 检测目录 |
|---|---|---|---|---|
| WorkBuddy | `~/.workbuddy/mcp.json` | 顶层 `mcpServers` | JSON | `~/.workbuddy` |
| OpenClaw | `~/.openclaw/openclaw.json` | 嵌套 `mcp.servers` | JSON(JSON5) | `~/.openclaw` |
| Hermes Agent | `~/.hermes/config.yaml` | 顶层 `mcp_servers` | YAML | `~/.hermes` |

- WorkBuddy 的 stdio 条目与现有 JSON 家族**完全一致**(`{command, args}`)。
- OpenClaw 的存储键路径是**嵌套** `mcp.servers.<name>`(非顶层 `mcpServers`);文件是 JSON5(允许注释/尾逗号)。其 CLI `openclaw mcp add` 写的是标准 JSON。
- Hermes 是 YAML,`mcp_servers:` 映射,条目 `{command, args, env?}`。

**现有 5 家审计(全部核实,无需修改)**:Claude Code(`~/.claude.json` 顶层 `mcpServers` = 用户级作用域,已比对本机文件实证)、Claude Desktop(`~/Library/Application Support/Claude/claude_desktop_config.json`)、Cursor(`~/.cursor/mcp.json`)、Codex(`~/.codex/config.toml` 的 `[mcp_servers.*]`)、Gemini(`~/.gemini/settings.json`)—— 配置路径/格式/键名与代码一致,零漂移。

## 决策记录(brainstorm 拍板)

1. **范围 = 三家接入 + skill 全能力控制**,一份 spec 四部分,各自独立可测。
2. **OpenClaw/Hermes 用写文件、扩展格式**(不 shell 到它们的 CLI):macOS 从 Finder 启动的 GUI 应用 PATH 极窄,shell-out 找不到二进制会脆断;写文件与现有 registry 机制一致。
3. **现有 5 家无需改**;`"type":"stdio"` 补写、`CLAUDE_CONFIG_DIR` 处理均为**非目标**(当前都能跑,YAGNI)。

## 架构

### Part A —— CLI 录制控制 + skill 文档

`bridge::call(op, extra) -> Result<Value, String>`(`src-tauri/src/mcp/bridge.rs`)是 stdio→GUI 的 UDS 客户端,MCP server 的 6 个活能力工具都走它;成功回 `data`、失败回人话(含「未运行」`NOT_RUNNING`、控制被门控拒绝的原文)。CLI 控制直接复用它,零重复。

- **新模块** `src-tauri/src/mcp/cli_control.rs`:`pub fn record_cli(args: &[String]) -> i32`。子命令与 op 映射:
  - `start [--title X]` → `call("start", {"title": X})`(X 为空则不带 title)
  - `stop` → `call("stop", {})`;`pause` → `call("pause", {})`;`resume` → `call("resume", {})`
  - `status` → `call("status", {})`;`live [--tail N]` → `call("live", {"tail": N})`
  - 成功:人读默认打印(status 打 `state/elapsed_ms/note_id`;start/stop 打 `note_id`;live 打转写文本),`--json` 直出 `data`。
  - 失败:`Err` 文案打到 stderr、退出码 1。用法错(未知子命令/未知 flag)退出码 2——沿用 `cli_query` 的 `reject_unknown_flags` 风格,不静默。
- **接线**:`cli_entry`(`mod.rs` 分发表)加 `"record" => cli_control::record_cli(...)`;`main.rs` 拦截词表加 `record`(两处一一对应,与现有注释要求一致)。
- **门控零改动**:GUI 侧对每个控制 op 现读 `mcp_allow_control`,CLI 自动继承;被拒/未运行由 bridge 转达。
- **skill 文档**(`skill_template.md`):在「工具与降级路径」补一段「控制录制(CLI)」,列出 `record start/stop/pause/resume/status`,并保留「优先 MCP、其次 CLI」的框架 + 控制需用户开启的既有提示。
- **README**(中/英):命令行小节补控制命令。

### Part B1 —— WorkBuddy(一行)

`AGENTS` 数组加一行,现有 JSON writer 直接支持:
```rust
AgentDef { key: "workbuddy", name: "WorkBuddy", detect_rel: ".workbuddy",
           config_rel: ".workbuddy/mcp.json", fmt: Fmt::Json(&["mcpServers"]) }
```
(见下方 B2 对 `Fmt::Json` 携带键路径的改造。)

### Part B2 —— OpenClaw(JSON,嵌套键路径)

OpenClaw 与其它 JSON 家族只差**容器键路径**(嵌套 `mcp.servers` vs 顶层 `mcpServers`)。改造 `Fmt::Json` 携带键路径:

```rust
pub enum Fmt {
    Json(&'static [&'static str]), // server 容器的键路径,如 &["mcpServers"] 或 &["mcp","servers"]
    Toml,
    Yaml,
}
```
- `Fmt` 仍是 `Copy + PartialEq`(`&'static [&'static str]` 是 Copy)。
- `upsert_json`/`remove_json`/`read_command` 改为**按键路径逐级 `entry`/`get`**(而非硬编码 `"mcpServers"`),末级放/取 `voice-notes`。逐级容器不是对象则拒写(沿用现有保护)。
- 现有 5 家的 Json 行改为 `Fmt::Json(&["mcpServers"])`(纯机械);Codex 仍 `Fmt::Toml`。
- OpenClaw 行:
```rust
AgentDef { key: "openclaw", name: "OpenClaw", detect_rel: ".openclaw",
           config_rel: ".openclaw/openclaw.json", fmt: Fmt::Json(&["mcp","servers"]) }
```
- **JSON5 取舍**:用 serde_json 读;若用户文件带 JSON5 注释/尾逗号导致解析失败,沿用现有「不是合法 JSON,拒绝写入」保护(清晰报错、不损坏),不引 json5 依赖。工具自身 `openclaw mcp add` 写的是标准 JSON,常态可解析。

### Part B3 —— Hermes(YAML,新格式)

- 新增依赖:一个维护中的 YAML 库(`serde_yaml_ng`)。
- 新增 `Fmt::Yaml` 变体 + `upsert_yaml`/`remove_yaml` + `read_command` 的 Yaml 分支:
  - 读(或新建)→ 解析为 YAML 映射 → 确保顶层 `mcp_servers` 为映射 → 插入/删除 `voice-notes`(条目 `{command: <exe>, args: ["mcp","serve"]}`)→ 序列化 → `write_with_backup`(权限保留 + `.vn.bak`)。
  - 解析失败(非法 YAML)→ 拒写并清晰报错(与 json/toml 一致)。
  - `read_command` 取 `mcp_servers.voice-notes.command`。
- Hermes 行:
```rust
AgentDef { key: "hermes", name: "Hermes Agent", detect_rel: ".hermes",
           config_rel: ".hermes/config.yaml", fmt: Fmt::Yaml }
```
- **注释取舍**:serde 往返会丢 YAML 注释;`.vn.bak` 兜底。Hermes 的 config.yaml 常由其 CLI 生成、少有手写注释,风险低。

### 横切

- **UI**:欢迎页 connect 步与设置页「AI 助手接入」列表由后端 `registry.status()`(遍历 `AGENTS`)驱动,新增 3 行自动出现。**实现时核对前端**是否有硬编码代理数量/名单;有则同步改。
- **README**(中/英):「五家」文案改「八家」;代理列表补 WorkBuddy/OpenClaw/Hermes。
- **quarantine/自愈/权限保留/幂等**等既有横切逻辑对新家自动适用(它们走同一 register/unregister/heal 路径)。

## 边界与错误处理

1. 键路径逐级容器存在但类型不对(如 `mcp` 是字符串而非对象)→ 拒写、清晰报错。
2. OpenClaw JSON5 带注释解析失败 → 拒写(已知取舍)。
3. Hermes 非法 YAML → 拒写。
4. 三家配置文件不存在 → 新建(父目录按需创建,`write_with_backup` 已支持)。
5. 权限位:三家均走 `write_with_backup`,保留用户收紧过的权限(有既有回归测试模式)。
6. `record` 控制:App 未运行 → `NOT_RUNNING` 指引;`mcp_allow_control` 关 → 转达开启指引;均不静默、退出码 1。

## 测试与验证

- **Part A**:`cli_control` 参数解析纯逻辑单测——`start --title` 解析、`live --tail N` 解析、未知子命令/flag → 退出码 2、各子命令的 op 映射正确。UDS 往返已由 `bridge.rs` 现有集成测试覆盖(假 GUI 监听端)。
- **Part B(registry)**:三家各补注册器测试,镜像现有 5 家的模式——register 写入正确键路径/格式、unregister 幂等移除、read_command 回读、坏文件拒写、**权限位 0600 保留**(有既有回归断言可套)。
  - `Fmt::Json` 键路径改造:补一条「嵌套路径(OpenClaw 式 `mcp.servers`)正确放/取」单测,并确认现有顶层路径测试仍过。
  - `Fmt::Yaml`:upsert/remove/read 单测 + 已有 YAML 内容保留其它 server 的断言。
- **真机冒烟**(macOS,发版前):对本机装了的新家跑 `voice-notes mcp register --agent <key>` → 打开对应工具确认 voice-notes 出现在其 MCP 列表;`voice-notes record status`/`start`/`stop`(需 App 运行 + 开启允许控制)驱动录制。

## 影响面

- 改:`src-tauri/src/mcp/registry.rs`(`Fmt` 改造 + 3 行 AgentDef + YAML writer + 现有 Json 行机械改)、`src-tauri/src/mcp/mod.rs`(`record` 分发)、`src-tauri/src/main.rs`(词表)、`src-tauri/src/mcp/skill_template.md`、`README.md`/`README.en.md`、`src-tauri/Cargo.toml`(YAML 依赖)。
- 新增:`src-tauri/src/mcp/cli_control.rs`。
- 前端:`AGENTS` 驱动的列表自动扩展(按需核对硬编码)。
- 现有 5 家 AgentDef:**已审计,无需改**(仅 Json 行随 `Fmt::Json(path)` 改造机械带上键路径)。

## 非目标(YAGNI)

- 不给 JSON stdio 条目补 `"type":"stdio"`(当前省略可跑)。
- 不处理 Claude Code 的 `CLAUDE_CONFIG_DIR` 重定位(极少见)。
- 不 shell-out 到 openclaw/hermes 的 CLI(macOS GUI PATH 脆断)。
- 不为非文件型接入(如纯 GUI/marketplace)做特殊机制——三家新目标都是文件型。
- 不把 skill 安装进新家的 skill/规则系统(新家走 MCP;skill 安装仍是 Claude Code 专属,仅更新内容)。
