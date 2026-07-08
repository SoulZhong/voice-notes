# voice-notes 查询 CLI + Agent Skill 设计

日期:2026-07-09。MCP 服务(spec 2026-07-08)之外再开两个接入面:**查询 CLI**(脚本与无 MCP 配置的 Agent 直接用)与 **Agent Skill**(教会 Agent 什么时候、怎么组合这些工具)。方向与范围已获用户确认("全部都要实现"),继续落在 mcp-service 分支。

## 一、查询 CLI

### 命令面

```
voice-notes notes list   [--limit N] [--offset N] [--from RFC3339] [--to RFC3339] [--json]
voice-notes notes search <query> [--limit N] [--json]
voice-notes notes get    <note-id> [--format md|txt|json]     # 默认 md
voice-notes speakers list [--json]
```

- **实现 = `mcp::tools` 四个纯函数套参数解析**:CLI 输出与 MCP 工具同一 JSON 形状(同源,永不漂移)。
- 默认人读输出(制表符分列,不引入宽度对齐库——CJK 定宽对齐不值得);`--json` 输出与对应 MCP 工具完全一致的 JSON。`notes get` 无 `--json`,由 `--format json` 承担(= MCP `get_note` 的 segments 结构)。
- 退出码沿用既有约定:0 成功 / 1 执行错(如 note-id 不存在,错误进 stderr)/ 2 用法错(打印用法)。
- 每层子命令有用法文案(裸 `voice-notes notes` → 用法 + 退出码 2)。
- **状态/控制类不进 CLI v1**:一次性命令里"App 必须在跑 + 开关门控"的语义体验差且有误触风险,MCP 已覆盖。

### 进程形态

main.rs 拦截扩展为 `mcp | notes | speakers | skill` 四个词,统一进 `mcp::cli_entry(args)`(新增,内部分发:`mcp` → 既有 cli_main,`notes`/`speakers` → cli_query.rs,`skill` → skill.rs)。不触碰 GUI 路径的约束不变。

## 二、Agent Skill

### 内容(SKILL.md)

模板内嵌进二进制(`include_str!`,源在 `src-tauri/src/mcp/skill_template.md`),安装时渲染 `{{VERSION}}` 占位。结构:

- **frontmatter**:`name: voice-notes`、`description`(触发条件:用户问会议内容/要会议纪要/写周报/找会上决议待办时)、注释行 `managed-by: voice-notes v{{VERSION}}`(受管标记,自愈依据,注明"自动管理,手改会被覆盖")。
- **能力地图**:先 `search_notes` 定位再 `get_note` 取全文;`prefer_refined` 语义(有 AI 精修稿默认用它);查询类无需 App 运行,状态/控制类需 App 运行且控制受设置开关门控(被拒时提示用户去 设置 → AI 助手接入)。
- **工作流配方**:①会议纪要(get_note markdown → 按 主题/决议/待办 归纳);②周报汇总(list_notes from=本周 → 逐条 get 要点);③找决议/待办(search 关键词族:"决定/定了/负责/deadline/下周")。
- **降级路径**:MCP 工具不可用时改用 CLI `voice-notes notes … --json`(绝对路径 /Applications/voice-notes.app/Contents/MacOS/voice-notes),并给注册指引(`mcp register --agent auto`)。
- **隐私提醒**:笔记内容进入上下文即离开本机,引用会议原文前确认用户意图。

### 分发与生命周期

- `voice-notes skill install|uninstall|status`:写入/删除 `~/.claude/skills/voice-notes/SKILL.md`(Claude Code 用户级 skills 目录);幂等;status 输出 NotInstalled / Installed(current) / Installed(stale)(stale = 文件内容 ≠ 当前版本渲染结果)。
- **只发 Claude Code 一家(v1)**:其他 Agent 的等价物(Cursor rules 等)观望。
- **启动自愈**:GUI 启动时(与注册路径自愈同一后台线程)若已安装且内容 stale **且文件含受管标记**,静默重写为当前版本;无标记(用户自建/手改删了标记)不动。
- **设置页**:「AI 助手接入」分组加一行「Claude Code 技能」(row-desc 说明工作流价值 + 状态),行尾 安装/移除;tauri 命令 `mcp_skill_status/install/uninstall`。
- 欢迎页不加(保持轻量,设置页与 README 承接)。

## 三、README

「接入 AI 助手(MCP)」章节内追加(中英同步):

- 「命令行直查(无需 MCP)」小节:四条命令示例 + `--json` 说明。
- 「Claude Code 技能」小节:`skill install` 一行命令 + 一句价值说明。

## 四、真值源纪律

CLI 输出、MCP 工具、SKILL.md 三处描述必须同源:CLI 直接复用 `mcp::tools`;SKILL.md 里工具语义只写使用策略,具体参数指向 README 工具表,不另抄一份。

## 五、测试

- cli_entry 分发与退出码单测;notes/speakers 各命令对 tempdir fixture 的人读/JSON 输出断言(复用 tools 测试的 fixture 手法,VN_APP_DATA 注入,ENV_VAR_LOCK 串行)。
- skill.rs:install 幂等/uninstall 幂等/status 三态/受管标记判定/自愈重写与"无标记不动"(home 注入,与 registry 同法)。
- README/SKILL.md 人工校对;全量回归(cargo test / npm check / build)。

## 六、分期

单期单 PR 内 5 个任务:T1 cli_entry 重构+speakers list 打样 → T2 notes 三命令 → T3 skill 模块+CLI → T4 GUI(命令/自愈/设置页行) → T5 README 双语+对账回归。
