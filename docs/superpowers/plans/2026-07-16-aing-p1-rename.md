# Aing Phase 1 · Plan 1 — 用户可见文案改名(精修 → Aing)

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development 或 executing-plans 按任务逐条实现。步骤用 `- [ ]` 勾选。

**Goal:** 把所有**用户/助手可见**的中文「精修」文案改为「Aing」(直接混排写法),零行为变更。

**Architecture:** 纯文案替换。不动内部标识符(`refine` 模块/函数/结构体)、不动文件名(`refined.json`)、不动数据格式、不动 hook 事件 key —— 这些延后到「引擎 plan」(与格式改造一起一次迁移)。

**Tech Stack:** SvelteKit(Svelte/TS)、Rust(仅 agent 可见字符串:MCP 工具描述 / skill 模板 / ailog 标签映射前端侧)。

## Global Constraints

- 写法固定为 **Aing 直接混排**,统一术语表(见下),整库一致。
- git 提交信息**不加** `Co-Authored-By` 或任何 Claude/Generated 署名。
- **保留(不得改)**:hook 事件 key `refine_started` / `refine_finished`(稳定契约);内部 Rust 标识符 `refine`/`spawn_refine`/`RefineState`/`refined.json`/`stages.llm`(延后到引擎 plan)。
- 前端验证 `npm run check` 0 error 0 warning;后端 `cargo check` 无 error。

## 术语表(替换真值源)

| 原 | 改 |
|---|---|
| 精修 / 智能精修 | Aing |
| 会后精修 | 会后 Aing |
| 精修方式 | Aing 方式 |
| 精修稿 | Aing 稿 |
| 重新精修 / 确认重新精修 | 重新 Aing / 确认重新 Aing |
| 正在精修… | Aing 中… |
| 精修中 | Aing 中 |
| 精修失败 / 本地精修结果 | Aing 失败 / 本地 Aing 结果 |
| 尚无精修稿 / (精修稿为空) | 尚无 Aing 稿 / (Aing 稿为空) |
| 精修开始 / 精修完成(hook 标签) | Aing 开始 / Aing 完成(key 不变) |
| 精修分块 / Agent 精修 / 精修写回(ailog 标签) | Aing 分块 / Agent Aing / Aing 写回 |
| 精修与标题生成 / 精修完成后发通知 | Aing 与标题生成 / Aing 完成后发通知 |

代码**注释**里的「精修」一并按上表替换(dev-facing,但用户要求弃用该词);仅替换中文「精修」二字,`refined.json`/`refine` 等英文标识符不动。

---

### Task 1: 前端用户可见 UI 文案(Svelte)

**Files (Modify):** `src/routes/settings/+page.svelte`、`src/routes/ai/+page.svelte`、`src/routes/ai/logs/+page.svelte`、`src/routes/notes/[id]/+page.svelte`、`src/routes/hooks/+page.svelte`、`src/routes/hooks/[id]/+page.svelte`

- [ ] **Step 1: 按术语表替换渲染文案**

逐文件用 `grep -n 精修 <file>` 列出,按术语表替换**渲染到界面的字符串**(标题/标签/描述/banner/按钮/aria-label/placeholder)。重点串(before → after):
- settings `会后精修` → `会后 Aing`;其描述里 `…自动用大模型精修转写稿…` → `…自动用大模型 Aing 转写稿…`
- ai `智能精修`(section-title、注释)→ `Aing`;`精修方式` → `Aing 方式`;`用本机已登录的 AI 助手精修` → `…AI 助手 Aing`;`用 OpenAI 兼容的在线接口精修` → `…在线接口 Aing`;`精修失败(如 Agent 未登录)时保留原文` → `Aing 失败(…`;`三项配齐后精修生效` → `…后 Aing 生效`;`精修与标题生成的每次对外 AI 调用` → `Aing 与标题生成…`
- ai/logs `暂无记录。精修与标题生成…` → `…Aing 与标题生成…`
- notes/[id] banner:`部分段落精修失败，已保留原文，可重新精修。` → `部分段落 Aing 失败，已保留原文，可重新 Aing。`;`LLM 精修失败，当前展示本地精修结果。` → `LLM Aing 失败，当前展示本地 Aing 结果。`;切换按钮 `精修稿` → `Aing 稿`;`尚无精修稿` → `尚无 Aing 稿`;`（精修稿为空）` → `（Aing 稿为空）`;`确认重新精修` → `确认重新 Aing`;`正在精修…`/`重新精修` → `Aing 中…`/`重新 Aing`;`已被精修过滤` → `已被 Aing 过滤`
- hooks 首页 intro `…精修完成后发通知…` → `…Aing 完成后发通知…`;状态图 `aria-label` 里 `精修中依次转移,停止后进入精修,精修完成即结束` → `Aing 中依次转移,停止后进入 Aing,Aing 完成即结束`;图内节点/边标签文本「精修中」「精修开始」「精修完成」→ `Aing 中`/`Aing 开始`/`Aing 完成`(SVG `<text>` 内容,非 event key)
- 其余注释里的「精修」按术语表替换

- [ ] **Step 2: 验证**

Run: `npm run check`  Expected: 0 errors 0 warnings。
再 `grep -rn 精修 src/routes` 应只剩(若有)与 event-key 无关的漏网,确认为 0。

- [ ] **Step 3: 提交**

```bash
git add src/routes
git commit -m "用户可见文案 精修→Aing(直接混排):设置/AI/笔记/钩子页,零行为变更"
```

---

### Task 2: 前端库与数据标签(TS/Svelte lib)

**Files (Modify):** `src/lib/hooks.svelte.ts`、`src/lib/ailog.ts`、`src/lib/models.ts`、`src/lib/notes.ts`、`src/lib/mcp.ts`、`src/lib/Sidebar.svelte`、`src/lib/SpeakerChips.svelte`

- [ ] **Step 1: 替换 hook 标签与 ailog 标签(key 不变)**

- `hooks.svelte.ts`:`{ value: "refine_started", label: "精修开始" }` → `label: "Aing 开始"`;`refine_finished` → `label: "Aing 完成"`。**value 不变。**
- `ailog.ts`:`refine_chunk: "精修分块"`→`"Aing 分块"`;`agent_refine: "Agent 精修"`→`"Agent Aing"`;`mcp_apply: "精修写回"`→`"Aing 写回"`。**map 的 key 不变。**
- `models.ts`/`notes.ts`/`mcp.ts`/`Sidebar.svelte`/`SpeakerChips.svelte`:仅注释里的「精修」按术语表替换(如「精修稿」→「Aing 稿」)。这些是注释,不影响运行。

- [ ] **Step 2: 验证**

Run: `npm run check`  Expected: 0/0。
`grep -rn 精修 src/lib` → 0。

- [ ] **Step 3: 提交**

```bash
git add src/lib
git commit -m "前端库 精修→Aing:hook/ailog 显示标签(key 不变)+ 注释"
```

---

### Task 3: 后端 agent 可见字符串 + 注释

**Files (Modify):** Rust 侧含「精修」的文件(见 `grep -rln 精修 src-tauri/src`)。重点是 **agent/用户可见**的:`src-tauri/src/mcp/tools.rs`、`src-tauri/src/mcp/server.rs`、`src-tauri/src/mcp/skill_template.md`、`src-tauri/src/mcp/cli_query.rs`(MCP 工具描述 / skill 文档,Agent 会读到)。其余文件里的「精修」是日志/注释(dev-facing),一并按术语表替换以彻底弃用该词。

- [ ] **Step 1: 替换**

`grep -rn 精修 src-tauri/src` 逐处按术语表替换中文「精修」二字。**不动**:任何英文标识符、`refined.json` 字面量、hook event key 字符串、`stages.llm`。MCP 工具/skill 里如「读精修稿并写回」→「读 Aing 稿并写回」。

- [ ] **Step 2: 验证**

Run: `cargo check --manifest-path src-tauri/Cargo.toml`  Expected: 无 error。
`grep -rn 精修 src-tauri/src` → 0。

- [ ] **Step 3: 提交**

```bash
git add src-tauri/src
git commit -m "后端 精修→Aing:MCP 工具/skill 描述(agent 可见)+ 日志/注释;英文标识符与 refined.json 不动"
```

---

## Self-Review
- **Spec 覆盖**:本 plan 只做 spec Phase 1 的「改名(用户可见部分)」;`refined.json`→`aing.json`、`stages.aing`、内部标识符改名 → 归入引擎 plan(与格式改造一次迁移),spec 已注明可拆。
- **占位符**:替换以术语表为真值源 + 每任务 grep 驱动定位 + 结尾 `grep 精修 → 0` 兜底,无 TBD。
- **一致性**:hook event key、内部 `refine` 标识符、`refined.json` 全程保留;仅中文「精修」→「Aing」。风险面小、零行为变更。
