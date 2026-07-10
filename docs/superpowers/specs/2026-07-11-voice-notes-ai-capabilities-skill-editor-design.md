# AI 页:Agent 功能列表 + Skill 在线编辑 —— 设计

日期:2026-07-11
状态:待实现(续 `ai-tab` 分支 / PR #23)
关联:`/ai` 页 `src/routes/ai/+page.svelte`;MCP 工具 `src-tauri/src/mcp/server.rs`;skill 机制 `src-tauri/src/mcp/skill.rs`

## 背景与目标

`/ai` 页现在只有接入配置,用户看不到「接入之后 Agent 到底能干什么」;skill 是黑盒文件,想微调提示词只能去文件系统里改。本设计:①在 `/ai` 页展示 Agent 可调用的功能清单(MCP 工具 + CLI 命令);②skill 支持页内查看/编辑/保存。

**成功标准**:清单与代码同源不漂移;编辑保存后 skill 变「用户自管」(升级不覆盖),可一键恢复默认;全部行为有测试或守卫。

**非目标(YAGNI)**:不做 skill 模板变量的可视化编辑(直接编辑最终文件);不做多 Agent 的 skill(仍仅 Claude Code);不做工具的启停配置(展示即全部)。

## 决策记录(brainstorm 拍板)

1. **编辑即接管**:保存时自动剥离受管标记(`managed-by: voice-notes` 行)→ 状态变 Unmanaged(用户自管),应用升级自愈不再触碰;「恢复默认」一键回受管版。
2. **清单范围 = MCP 十工具 + CLI 命令**。

## 设计

### A. 后端

**`mcp_capabilities()` Tauri 命令**(注册进 invoke_handler):返回
```json
{
  "tools": [{ "name": "list_notes", "desc": "列出会议笔记(倒序分页…)", "gate": "none|app|control" }],
  "cli":   [{ "cmd": "voice-notes notes list --json", "desc": "列出笔记" }]
}
```
- 清单由 `server.rs` 内 `pub fn catalog() -> serde_json::Value`(或等价结构体)静态给出,**与 `#[tool]` 定义同文件相邻**;desc 与工具描述一致(允许适度精简),gate 按事实:查询四工具 `none`,status/live `app`,start/stop/pause/resume `control`。
- CLI 清单:`notes list/search/get`、`speakers list`、`record start/stop/pause/resume/status/live` 的一行用法+描述(静态)。
- **防漂移守卫测试**:优先对照 rmcp router 的工具名集合断言 `catalog.tools[].name` 完全一致;若 router 不便实例化,退而 `include_str!("server.rs")` 计数 `#[tool(` 与 catalog 长度断言,并断言每个 name 出现在源码中。

**skill 读写两命令**(`skill.rs` 增 `read_in/save_in(home)` 可注入纯函数 + 包装):
- `mcp_skill_read() -> { content: String, state: String }`:读安装文件;NotInstalled 时返回 `rendered()` 默认稿(供预览/首次编辑),state 照实回 `not_installed`。
- `mcp_skill_save(content: String)`:**剥离受管标记行**(含其前后紧邻的空行归一,避免残留空洞)→ 目录按需创建 → tmp+rename 原子写。保存后 status 自然判 Unmanaged。
- 「恢复默认」复用现有 `mcp_skill_install`(重写受管渲染稿)。

### B. 前端(`/ai` 页,两个新区块)

**「Agent 能调用什么」**(接入列表之后):settings-row 卡片;工具行=等宽 `name` + 描述 + 徽章(`需应用运行` / `需允许控制`,`none` 不显徽章);CLI 行=等宽 `cmd` + 描述。数据来自 `mcp_capabilities()`,onMount 拉取,失败显示既有 warn 横幅样式。

**Skill 编辑卡**(技能行改造):技能行加「查看/编辑」按钮 → 展开卡:
- 等宽 `textarea`(`.snippet` 同族样式,高度 ~360px 可拖);
- 按钮:「保存」(旁注小字:保存后应用升级不再自动更新此文件)、「恢复默认」(`confirm` 确认后调 install,重拉内容)、「收起」;
- 状态徽章沿用四态,Unmanaged 显示「已自定义」;保存/恢复后重拉 `mcp_skill_read` + `mcp_skill_status` 刷新。
- 未安装时编辑默认稿并保存 = 以自管身份首次落盘(目录由后端创建)。

### 边界与错误处理

1. 保存内容为空白 → 后端拒绝(「内容为空」),防误清。
2. 保存/读取 IO 失败 → 错误经既有 error 横幅显示,textarea 内容保留不丢。
3. 「恢复默认」覆盖用户编辑 → 前端 `confirm` 确认(危险操作惯例)。
4. `mcp_capabilities` 为纯静态数据,不依赖 App 运行状态。
5. skill 文件被外部改动(编辑卡打开期间)→ 保存即覆盖(单机低风险,不做乐观锁,记为已知取舍)。

## 测试与验证

- **cargo**:catalog 防漂移守卫;`save_in` 剥标记(有/无标记两态)、原子写、空内容拒绝、读回一致;`read_in` NotInstalled 返回默认稿。
- **前端**:`npm run check` 0/0;浏览器 shim 冒烟(/ai 渲染两新区块)。
- **真机冒烟**:清单显示与徽章正确;编辑保存 → 状态变「已自定义」→ `~/.claude/skills/voice-notes/SKILL.md` 无受管标记;恢复默认 → 回「当前版本」;升级自愈不再动自管文件(靠既有 Unmanaged 语义,单测已覆盖)。

## 影响面

- 改:`src-tauri/src/mcp/server.rs`(catalog)、`src-tauri/src/mcp/skill.rs`(read/save)、`src-tauri/src/lib.rs`(注册三命令)、`src/lib/mcp.ts`(封装)、`src/routes/ai/+page.svelte`(两区块)。
- 后端既有逻辑零改动(纯新增)。
