# 侧栏「AI」页签 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 侧栏新增「AI」页签,`/ai` 页承载原设置页的「智能精修」与「AI 助手接入」两区块(单页平铺,不分两大类),设置页瘦身,全仓指向文案同步。

**Architecture:** 纯 UI 搬移,后端零逻辑改动(仅门控文案字符串)。页签沿用「路由派生、零独立状态」写法;`/ai/+page.svelte` 自立(onMount 自拉 settings 与 MCP 状态),从设置页整体迁移 markup + script + 所需 scoped 样式。

**Tech Stack:** SvelteKit(Svelte 5 runes)、Tauri invoke(`$lib/mcp`、`$lib/settings` 既有封装)。

## Global Constraints

- **零功能逻辑改动**:精修保存、八家注册/移除/修复、技能 install/uninstall、允许控制开关的行为与原设置页逐项一致(纯搬移)。
- `/ai` 页**单页平铺,无「智能精修 / AI 助手接入」两大分区标题**(brainstorm 拍板);页标题「AI」。
- 「语音模型」留在设置;设置页保留 通用/存储/录制/语音模型/关于 五区块。
- 文案:凡「设置 → AI 助手接入」语义一律改为指向左侧「AI」页;无 Unicode 符号字符,复用 DESIGN.md 现有形态与 token。
- 每步后 `npm run check` 0 error/0 warning;改 Rust 后 `cd src-tauri && cargo test` 全绿。

---

### Task 1: 侧栏第三页签 + `/ai` 骨架页

**Files:**
- Modify: `src/lib/Sidebar.svelte`(tab 派生 :23、tab-rail :192-205、中部列表区 :216 起的分支)
- Create: `src/routes/ai/+page.svelte`(骨架)

**Interfaces:**
- Produces:路由 `/ai`;`tab === "ai"` 状态。Task 2 在骨架页内填充内容。

- [ ] **Step 1: tab 派生加 ai 分支**

`Sidebar.svelte:23` 改为:
```ts
  const tab = $derived(
    $page.url.pathname.startsWith("/speakers") ? "people"
    : $page.url.pathname.startsWith("/ai") ? "ai"
    : "notes",
  );
```

- [ ] **Step 2: tab-rail 加第三个 vtab**

在「会议搭子」按钮(:199-201)之后、`</nav>` 前加:
```svelte
    <button
      class="vtab"
      class:active={tab === "ai"}
      onclick={() => { if (tab !== "ai") goto("/ai"); }}>AI</button
    >
```

- [ ] **Step 3: 中部列表区 ai 分支留空**

现有中部为 `{#if tab === "people"} …人物索引… {:else} …笔记列表… {/if}`(:216 起)。改为三分支,ai 渲染空:
```svelte
  {#if tab === "people"}
    <!-- 既有人物索引原样 -->
  {:else if tab === "ai"}
    <!-- AI 页签无列表内容(拍板留空) -->
  {:else}
    <!-- 既有笔记列表原样 -->
  {/if}
```
(只加分支,不动两侧既有内容。)

- [ ] **Step 4: `/ai` 骨架页**

Create `src/routes/ai/+page.svelte`:
```svelte
<script lang="ts">
  // AI 页:智能精修大模型配置 + AI 助手接入(Task 2 自设置页迁入)。
</script>

<div class="page">
  <header class="topbar"><h1>AI</h1></header>
</div>

<style>
  .page { padding: 0 1.5rem 2rem; }
  .topbar { position: sticky; top: 0; background: var(--canvas); padding: 1.1rem 0 0.6rem; }
  h1 { font-size: 1.15rem; font-weight: 600; margin: 0; }
</style>
```
(topbar 吸顶与全应用一致;若设置页 topbar 结构不同,以设置页现状为准对齐——Task 2 迁样式时统一。)

- [ ] **Step 5: 类型检查 + 提交**

Run: `npm run check` → 0/0。
```bash
git add src/lib/Sidebar.svelte src/routes/ai/+page.svelte
git commit -m "feat(ui): 侧栏 AI 页签 + /ai 骨架页"
```

---

### Task 2: 内容搬移 —— 设置页两区块整体迁入 `/ai`,设置页瘦身

**Files:**
- Modify: `src/routes/ai/+page.svelte`(填充)
- Modify: `src/routes/settings/+page.svelte`(删两区块及其 script/样式专属部分)

**Interfaces:**
- Consumes:Task 1 的骨架页;`$lib/mcp` 与 `$lib/settings`(getSettings/setSettings)既有导出。
- Produces:功能完整的 `/ai` 页;五区块的设置页。

**这是逐字搬移任务**——不重写逻辑。以下为**迁移清单**(行号按当前 settings/+page.svelte,动手前先 grep 校准):

**A. markup 迁入 /ai**(去掉两个 `<h2 class="section-title">`,平铺;其余逐字):
- 「智能精修」`<section>` 全部内容::795-835(精修开关行、预设 segmented、base_url/model/api_key 三行)。
- 「AI 助手接入」`<section>` 全部内容::836-909(八家状态列表、注册/移除/修复按钮、手动配置卡片、Claude Code 技能行、允许 AI 控制录制开关)。

**B. script 迁入 /ai**(在骨架 script 内重建,逐字拷贝函数体):
- import::27-36 的 `$lib/mcp` 全部导出 + `AgentStatus` 类型;`getSettings/setSettings/Settings` 自 `$lib/settings`(对照设置页现有 import 路径)。
- 状态:`refineOn/refineBaseUrl/refineModel/refineKey`(:92-95)、`PRESETS` 常量(applyPreset 附近,grep `PRESETS` 定位)、`mcpAgents/mcpAllowControl/mcpSnippet/mcpSnippetOpen/mcpHealed/mcpBusy/mcpError`(:105-111)、`skillState/skillBusy`(:114-115)。
- 函数(逐字):`applyPreset`(:453)、`saveRefine`(:458)、`refreshMcp`(:468)、`refreshSkill`(:476)、`toggleSkill`(:484)、`mcpToggleRegister`(:500)及其后所有 MCP/skill/剪贴板/允许控制相关函数(grep `mcp|skill|snippet|AllowControl` 在 :453-574 间逐一核对,一个不漏)。
- **自立加载**:/ai 不迁设置页庞大的 `refreshSettings`,新写精简版:
```ts
  onMount(() => {
    (async () => {
      try {
        const s = await getSettings();
        refineOn = s.refine_enabled;
        refineBaseUrl = s.refine_base_url;
        refineModel = s.refine_model;
        refineKey = s.refine_api_key;
        mcpAllowControl = s.mcp_allow_control;
      } catch { /* 首载失败:控件保持默认,操作时会再报错 */ }
    })();
    refreshMcp();
    refreshSkill();
    mcpManualSnippet().then((v) => (mcpSnippet = v)).catch(() => {});
    mcpHealedCount().then((n) => (mcpHealed = n)).catch(() => {});
  });
```
- **保存**:迁移 `saveRefine` 所依赖的 `saveSetting`(:329,get→mut→set 通用保存)——/ai 拷一份同名函数(逐字),或若其内部依赖设置页特有状态(横幅/回弹),精简为 /ai 所需的最小等价(读新鲜值→改→存→失败回弹本地 state),行为语义不变。「允许 AI 控制录制」开关沿用其现有 toggle 函数(在 :453-574 段内,一并迁)。

**C. 样式迁入 /ai**:从设置页 `<style>` 拷贝 /ai 用到的类(`.rows`、`.settings-row`(或实际类名)、`.section-title` 若仍用于小标题、segmented、按钮、agent 列表、卡片等)——以「/ai 模板里出现的每个 class 在 /ai 的 style 里都有定义」为完成标准(svelte-check 不查 class,须人工核对 + 浏览器冒烟)。

**D. settings 页删除**:
- markup :795-909 两个 `<section>` 整体删除。
- script:删 B 清单中迁走的 import/状态/函数;`syncLocalFromSettings` 里 :182-186 的 refine/mcpAllowControl 五行删除;onMount 里 :257-260 的 refreshMcp/refreshSkill/snippet/healed 四行删除。
- style:仅当某类只被已删区块使用时才删(grep 该类名在 settings 模板剩余部分无引用),否则保留。

- [ ] **Step 1: 按清单 A-C 填充 /ai 页**(逐字搬移 + 自立加载/保存)

- [ ] **Step 2: 按清单 D 瘦身 settings 页**

- [ ] **Step 3: 校验**

Run: `npm run check` → 0/0(未用变量/缺失引用在此暴露,逐一清)。
Run: `grep -n "refine\|mcpAgents\|skillState" src/routes/settings/+page.svelte | head` → 应无残留(除非属于保留区块)。

- [ ] **Step 4: 浏览器冒烟(Playwright shim 套路)**

`npm run dev` 起 localhost:1420,用既有 `__TAURI_INTERNALS__` shim 注入假 invoke,打开 `/ai` 与 `/settings`:
- /ai 渲染出精修配置与八家列表(假数据),无样式缺失(裸元素/错位);
- /settings 五区块正常,无 AI 残留;
- 侧栏三页签切换高亮正确。

- [ ] **Step 5: 提交**

```bash
git add src/routes/ai/+page.svelte src/routes/settings/+page.svelte
git commit -m "feat(ui): 智能精修+AI 助手接入迁入 /ai 页,设置页瘦身五区块"
```

---

### Task 3: 指向文案同步(后端 + 前端 + 文档)

**Files:**
- Modify: `src-tauri/src/mcp/uds.rs:103`、`src-tauri/src/mcp/server.rs:115`、`src-tauri/src/mcp/skill_template.md`、`src/routes/record/+page.svelte:108-121,320`、`src/lib/WelcomeOverlay.svelte:122`、`README.md`、`README.en.md`

**Interfaces:** 无(字符串)。

- [ ] **Step 1: 后端门控文案**

`uds.rs:103`:
```rust
const CONTROL_DENIED: &str = "已被用户禁用:请在 voice-notes 左侧「AI」页开启「允许 AI 控制录制」";
```
`server.rs:115` tool description 中「设置 → AI 助手接入」→「左侧 AI 页」。
Run: `cd src-tauri && cargo test mcp 2>&1 | tail -5` → 全绿(若有测试断言旧文案,更新断言为新文案)。

- [ ] **Step 2: skill 模板 + README**

`skill_template.md` 中「设置 → AI 助手接入」表述 →「左侧「AI」页」(grep 定位,逐处改)。
`README.md` / `README.en.md` 同语义处同步(英文用 the "AI" tab in the sidebar)。

- [ ] **Step 3: record 页提示条 + WelcomeOverlay**

`record/+page.svelte`:`dismissMcpHint(true)` 的 `goto("/settings")`(:116)→ `goto("/ai")`;按钮文案「去设置」(:320)→「去 AI 页」。
`WelcomeOverlay.svelte:122`「设置 → AI 助手接入 移除」→「左侧 AI 页移除」。

- [ ] **Step 4: 全仓复查**

Run:
```bash
grep -rn "设置 → AI 助手接入" src src-tauri/src README.md README.en.md | grep -v target
```
Expected: 无命中(docs/superpowers 历史 spec 不改)。

- [ ] **Step 5: 校验 + 提交**

Run: `npm run check` → 0/0;`cd src-tauri && cargo test 2>&1 | tail -3` → 全绿。
```bash
git add -A
git commit -m "docs(ui): 门控/引导/README 指向改为左侧 AI 页"
```

---

## 收尾(控制器执行)

- [ ] `npm run check` 0/0;`cargo test` 全绿。
- [ ] 真机冒烟:三页签切换与 `/ai` 直达刷新;/ai 精修保存/预设/注册/移除/技能/允许控制逐项与原行为一致;录音页提示条跳 `/ai`;门控关时 `voice-notes record start` 返回新文案。
- [ ] 推分支 `ai-tab` → PR → 用户确认后 squash 合入。
