# 侧栏「AI」页签 —— 设计

日期:2026-07-10
状态:待实现
关联:侧栏页签体系 `src/lib/Sidebar.svelte`;设置页 `src/routes/settings/+page.svelte`(1441 行,七区块)

## 背景与目标

设置页已承载七个区块,其中「智能精修」(LLM 精修配置)与「AI 助手接入」(八家 MCP 注册/Claude Code 技能/允许 AI 控制录制)是产品的 AI 能力面,埋在设置里不易发现。将其移出,左侧单开一个名为「AI」的页签承载。

**成功标准**:侧栏出现第三个竖排页签「AI」,点击进入 `/ai` 页,原两区块的全部功能行为不变;设置页瘦身为五区块;所有指向「设置 → AI 助手接入」的文案与跳转同步更新。

**非目标(YAGNI)**:不改任何 AI 功能逻辑(纯 UI 搬移);不给 AI 页签的侧栏中部造列表内容;「语音模型」(本地 ASR/声纹模型)是录音基础能力,留在设置。

## 决策记录(brainstorm 拍板)

1. 「AI」页 = **智能精修的大模型配置 + AI 助手接入,不再分两类**(单页平铺,无两大分区标题)。
2. 「语音模型」留在设置。
3. **入口改沉底工具区(2026-07-10 二次拍板,推翻初版"第三页签")**:页签栏语义=内容集合(录音/搭子),配置类入口归沉底工具区——「AI」常驻入口放「设置」上方,交互与「设置」完全同构(`/ai` 时 footer 高亮 AI,页签栏与中部列表照常)。初版实现的第三 vtab/ai 页签派生/中部留空分支全部撤销。

## 设计

### 导航

- `src/lib/Sidebar.svelte` 的 tab-rail **保持两页签**「录音 / 会议搭子」不变;页签派生**不加 ai 分支**(`/ai` 与 `/settings` 同样落 notes 默认)。
- `nav-footer` 在「设置」链接**上方**加「AI」`nav-link`:`class:current={$page.url.pathname === "/ai"}`,`href="/ai"`,16px 线性 SVG 图标(四角星,currentColor)+ 文字「AI」,与设置链接同形态。
- `/ai` 页与全部指向文案(「左侧 AI 页」)不受影响。

### 新页 `src/routes/ai/+page.svelte`

单页平铺,从上到下(无「两类」大标题,页标题「AI」):

1. **智能精修 + 大模型配置**(自设置「智能精修」区块整体迁入):精修开关、预设选择(DeepSeek/千问/豆包 Ark/Kimi/OpenAI)、base_url / model / api_key 三字段及保存逻辑(refine_* 四字段,失败回弹本地 state 对齐 DOM 的既有惯例)。
2. **AI 助手接入**(自设置「AI 助手接入」区块整体迁入):八家 Agent 状态列表(注册/移除/修复)、手动配置卡片(剪贴板)、「Claude Code 技能」行(install/uninstall/status,unmanaged 不给按钮)、「允许 AI 控制录制」开关、`mcpBusy` 门闩等全部逻辑。

纯搬移:markup + script 状态 + 保存/注册逻辑 + 所需 scoped 样式(settings-row / rows / section-title / segmented 等既有形态类)随迁复制;token 全用 app.css 现有值,DESIGN.md 无需新增形态。

### 设置页瘦身

`src/routes/settings/+page.svelte` 删去「智能精修」(约 :795-835)与「AI 助手接入」(约 :836-909)两区块及其 script 段(refine 本地镜像、MCP 状态/注册/技能/剪贴板逻辑),保留 通用 / 存储 / 录制 / 语音模型 / 关于。

### 指向更新(全仓一致性)

grep 实测涉及以下文件,凡「设置 → AI 助手接入」语义一律改为指向左侧「AI」页:

| 文件 | 内容 |
|---|---|
| `src-tauri/src/mcp/uds.rs` | 门控拒绝文案「请在 voice-notes 的『设置 → AI 助手接入』开启…」→「请在 voice-notes 左侧『AI』页开启…」(**注意其测试若断言原文需同步**) |
| `src-tauri/src/mcp/server.rs` | 同上文案出现处 |
| `src-tauri/src/mcp/skill_template.md` | 「设置 → AI 助手接入」表述 → 「左侧『AI』页」 |
| `src/lib/WelcomeOverlay.svelte` | 欢迎页 connect 步的相关表述(如有跳转/文案) |
| `src/routes/record/+page.svelte` | 存量用户 MCP 提示条「去设置」→ `goto("/ai")` 文案改「去 AI 页」 |
| `README.md` / `README.en.md` | 「设置 → AI 助手接入」表述同步 |

### 边界

1. 直接输入 `/ai` URL 或从欢迎页/提示条跳入:页面自立(onMount 自拉 settings 与 MCP 状态),与设置页同惯例。
2. 录制中进入 `/ai`:与原设置页相同——注册/技能等操作不受录制影响;涉及 settings 写入的保存沿用既有 WRITE_LOCK/互斥,行为不变。
3. 设置页与 AI 页各自独立读写 settings(get→改→set 整包惯例),两页不会同时挂载,无并发新风险。

## 测试与验证

- `npm run check` 0/0;`cargo test` 全绿(uds.rs 门控文案若有断言原文的测试,更新断言)。
- 真机冒烟:三页签切换高亮/路由正确(含 `/ai` 直达与刷新);/ai 页精修保存、预设切换、八家注册/移除、技能安装/卸载、允许控制开关逐项与原行为一致;录音页提示条跳转 `/ai`;设置页五区块正常、无 AI 残留;CLI `record start` 在门控关时返回的新文案指向「AI」页。

## 影响面

- 改:`src/lib/Sidebar.svelte`、`src/routes/settings/+page.svelte`、`src/routes/record/+page.svelte`、`src/lib/WelcomeOverlay.svelte`、`src-tauri/src/mcp/uds.rs`、`src-tauri/src/mcp/server.rs`、`src-tauri/src/mcp/skill_template.md`、`README.md`、`README.en.md`。
- 新增:`src/routes/ai/+page.svelte`。
- 后端逻辑:零改动(仅文案字符串)。
