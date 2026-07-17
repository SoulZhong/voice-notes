# Aing Phase 2b · Plan B — 图谱页(浏览/搜索/过滤 + 详情面板 + 空态) Implementation Plan

> **For agentic workers:** 本 plan 是 UI。数据/结构/行为按下述任务落地;**视觉/交互观感靠 frontend-design 定调 + 截图迭代**(DESIGN.md 是真值源,用户拍板),不追求一次到位。不含力导图(=Plan C)。

**Goal:** 新增「图谱」页签 + `/graph` 路由:可搜索/按 kind 过滤的实体列表 + 点选右侧详情面板(出现笔记/共现实体)+ 实体点击导航(人→会议搭子)+ 无实体空态。

**Architecture:** 纯前端,消费 Plan A 已合入 master 的四命令(`graph_entities`/`graph_data`/`entity_detail`/`note_entity_links`,全部失败降级空)。新 `src/lib/graph.ts`(类型+绑定)+ `src/routes/graph/+page.svelte`(页)+ `Sidebar.svelte`(页签)。力导图与笔记页高亮可点归 Plan C。

**Tech Stack:** SvelteKit 5(runes:`$state`/`$derived`/`onMount`)、`invoke` from `@tauri-apps/api/core`、app.css 设计 token、DESIGN.md(Raycast 风)。无新依赖(d3-force 属 Plan C)。

## Global Constraints

- **优雅降级红线**:无实体 / 图谱失败 → 命令返回空 → 页面显示**空态引导**(「配置大模型并重新 Aing 以启用知识图谱」+ 跳 `/ai`),绝不报错/白屏。`entity_detail` 返回 `null` → 面板空态。
- **DESIGN.md 是 UI 真值源**:Raycast 近黑阶梯 / 药丸 / 紧圆角 / 字重 500;**禁 emoji 与 Unicode 符号**(图标走线性 SVG);颜色只用 app.css token(`--ink`/`--ink-secondary`/`--ink-faint`/`--accent`/`--accent-tint`/`--surface`/`--surface-soft`/`--hairline`/`--hairline-strong`/`--primary`)。kind 着色克制单色系;人实体用 `speakerInk`(与会议搭子色点体系一致)。落地后**补登 DESIGN.md**新组件(graph-list / entity-panel / filter-chips)。
- **契约注意(Plan A opus 终审 Minor)**:①别假设 `entity_detail.note_count === notes.length`(标题查不到的笔记会被跳过,note_count 可能更大);②人实体点击去 `/speakers/[global_id]`——global_id 即 person_id(is_person=true 时)。
- **不改后端、不加命令**(Plan A 已够);不动 Rust。`npm run check` 0/0、`npm run test`(vitest)绿。
- **分支**:`feature/aing-p2b-plan-b-graph-page`,从 master(含 `77961a9`)开。`git add` 显式路径禁 `-A`;提交无署名尾注。

---

## File Structure

- **Create** `src/lib/graph.ts`:七个 TS 类型(镜像 ipc)+ 四个 `invoke` 绑定 + `kindLabel(kind)` 纯函数(kind→中文标签,vitest)。
- **Create** `src/routes/graph/+page.svelte`:图谱页(列表 + 搜索 + kind 过滤 + 详情面板 + 空态 + 深链)。
- **Modify** `src/lib/Sidebar.svelte`:加「图谱」竖排页签(会议搭子/钩子之间)+ `tab` 派生加 `/graph` 分支。
- **Modify** `DESIGN.md`:补 graph 页新组件条目。

参考既有:`src/routes/speakers/+page.svelte`(onMount+$state+invoke 页结构)、`src/lib/notes.ts`(invoke 绑定 + `speakerInk`/`formatDate` 复用)、`src/lib/Sidebar.svelte:248-276`(页签)。

---

## Task 1: `src/lib/graph.ts` — 类型 + 绑定 + kindLabel

**Files:** Create `src/lib/graph.ts`;Create `src/lib/graph.test.ts`(vitest)

**Interfaces produced:** `EntitySummary`/`EdgeRow`/`GraphData`/`EntityNoteRef`/`RelatedEntity`/`EntityDetail`/`EntityLink` 类型;`graphEntities()`/`graphData()`/`entityDetail(id)`/`noteEntityLinks(id)` 绑定;`kindLabel(kind): string`。

- [ ] **Step 1: 写 kindLabel 的失败测试**(`src/lib/graph.test.ts`)

```ts
import { describe, it, expect } from "vitest";
import { kindLabel } from "./graph";

describe("kindLabel", () => {
  it("已知 kind 给中文标签", () => {
    expect(kindLabel("person")).toBe("人");
    expect(kindLabel("org")).toBe("组织");
    expect(kindLabel("project")).toBe("项目");
    expect(kindLabel("product")).toBe("产品");
    expect(kindLabel("term")).toBe("术语");
    expect(kindLabel("decision")).toBe("决议");
    expect(kindLabel("task")).toBe("任务");
    expect(kindLabel("place")).toBe("地点");
    expect(kindLabel("date")).toBe("日期");
  });
  it("未知 kind 原样返回(前向兼容,不吞新类型)", () => {
    expect(kindLabel("tool")).toBe("tool");
    expect(kindLabel("")).toBe("");
  });
});
```

- [ ] **Step 2: 跑测试确认失败** — Run: `npm run test -- graph` → FAIL(`kindLabel` 未定义)

- [ ] **Step 3: 实现 `src/lib/graph.ts`**

```ts
import { invoke } from "@tauri-apps/api/core";

/** 图谱实体摘要(列表 / 力导图节点)。镜像 ipc::EntitySummary。 */
export interface EntitySummary {
  id: string;
  kind: string;
  name: string;
  aliases: string[];
  is_person: boolean;
  note_count: number;
  mention_total: number;
}

/** 一条共现边(a<b,weight=共享笔记数)。镜像 ipc::EdgeRow。 */
export interface EdgeRow {
  a: string;
  b: string;
  weight: number;
}

/** 力导图数据(Plan C 用)。镜像 ipc::GraphData。 */
export interface GraphData {
  nodes: EntitySummary[];
  edges: EdgeRow[];
}

/** 详情面板「出现的笔记」一项(已联查标题)。镜像 ipc::EntityNoteRef。 */
export interface EntityNoteRef {
  id: string;
  title: string;
  started_at: string;
  mention_count: number;
}

/** 详情面板「共现实体」一项。镜像 ipc::RelatedEntity。 */
export interface RelatedEntity {
  id: string;
  kind: string;
  name: string;
  shared_notes: number;
}

/** 实体详情(右侧面板)。镜像 ipc::EntityDetail。 */
export interface EntityDetail {
  id: string;
  kind: string;
  name: string;
  aliases: string[];
  is_person: boolean;
  note_count: number;
  mention_total: number;
  notes: EntityNoteRef[];
  related: RelatedEntity[];
}

/** 笔记页高亮点击导航:局部实体→全局 id。镜像 ipc::EntityLink(Plan C 笔记页消费)。 */
export interface EntityLink {
  local_id: string;
  global_id: string;
  is_person: boolean;
}

const KIND_LABELS: Record<string, string> = {
  person: "人",
  org: "组织",
  project: "项目",
  product: "产品",
  term: "术语",
  decision: "决议",
  task: "任务",
  place: "地点",
  date: "日期",
};

/** kind→中文标签;未知 kind 原样返回(不吞大模型新造的类型)。 */
export function kindLabel(kind: string): string {
  return KIND_LABELS[kind] ?? kind;
}

/** 全部实体(列表),note_count 降序。图谱失败/空 → []。 */
export const graphEntities = () => invoke<EntitySummary[]>("graph_entities");
/** 力导图数据(Plan C)。 */
export const graphData = () => invoke<GraphData>("graph_data");
/** 单实体详情;不存在/失败 → null。 */
export const entityDetail = (id: string) => invoke<EntityDetail | null>("entity_detail", { id });
/** 笔记局部实体→全局 id(Plan C 笔记页)。 */
export const noteEntityLinks = (id: string) => invoke<EntityLink[]>("note_entity_links", { id });
```

- [ ] **Step 4: 跑测试确认通过** — Run: `npm run test -- graph` → PASS;`npm run check` → 0/0

- [ ] **Step 5: 提交**

```bash
git add src/lib/graph.ts src/lib/graph.test.ts
git commit -m "graph 前端:图谱命令 TS 类型+绑定+kindLabel"
```

---

## Task 2: Sidebar「图谱」页签 + `/graph` 路由骨架 + 空态

**Files:** Modify `src/lib/Sidebar.svelte`;Create `src/routes/graph/+page.svelte`

**Consumes:** `graphEntities`(Task 1)。

行为:页签点击 `goto("/graph")`(已在 `/graph` 再点回根);`tab` 派生加 `startsWith("/graph")→"graph"`。页面 onMount 拉 `graphEntities()`;为空 → 空态引导(文案 + 「去 AI 设置」跳 `/ai`);非空 → 占位列出实体名(Task 3 做全)。**视觉先粗排,截图后迭代。**

- [ ] **Step 1: Sidebar 页签**(`src/lib/Sidebar.svelte`)—— `tab` 派生加分支;在「会议搭子」按钮后插「图谱」按钮:

```svelte
<button
  class="vtab"
  class:active={tab === "graph"}
  onclick={() => { if ($page.url.pathname !== "/graph") goto("/graph"); }}>图谱</button
>
```
`tab` 派生里(`$page.url.pathname.startsWith("/speakers") ? "people" : ...` 链)加 `: $page.url.pathname.startsWith("/graph") ? "graph"`。

- [ ] **Step 2: `/graph` 页骨架 + 空态**(`src/routes/graph/+page.svelte`)

```svelte
<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { graphEntities, kindLabel, type EntitySummary } from "$lib/graph";

  let entities = $state<EntitySummary[]>([]);
  let loaded = $state(false);

  onMount(async () => {
    try {
      entities = await graphEntities();
    } catch {
      entities = [];
    }
    loaded = true;
  });
</script>

<div class="graph-page">
  {#if loaded && entities.length === 0}
    <div class="empty">
      <p class="empty-title">还没有知识图谱</p>
      <p class="empty-desc">配置大模型并对笔记「重新 Aing」后,人物、组织、项目等实体会自动汇入这里。</p>
      <button class="empty-cta" onclick={() => goto("/ai")}>去 AI 设置</button>
    </div>
  {:else}
    <ul class="ent-list">
      {#each entities as e (e.id)}
        <li>{e.name} · {kindLabel(e.kind)} · {e.note_count} 笔</li>
      {/each}
    </ul>
  {/if}
</div>

<style>
  /* 先按 DESIGN token 粗排,截图后迭代 */
  .graph-page { padding: 20px; }
  .empty { max-width: 380px; margin: 12vh auto 0; text-align: center; }
  .empty-title { font-weight: 500; color: var(--ink); margin: 0 0 6px; }
  .empty-desc { color: var(--ink-secondary); font-size: 13px; line-height: 1.6; margin: 0 0 16px; }
  .empty-cta { background: var(--primary); color: var(--surface); border: 0; border-radius: 8px; padding: 7px 14px; font-weight: 500; cursor: pointer; }
</style>
```

- [ ] **Step 3: 验证** — `npm run check` 0/0;dev 里 `/graph` 页签出现且可点、有实体走列表、无实体走空态(截图法见下)。

- [ ] **Step 4: 提交**

```bash
git add src/lib/Sidebar.svelte src/routes/graph/+page.svelte
git commit -m "图谱页:Sidebar 页签 + /graph 路由骨架 + 空态引导"
```

---

## Task 3: 实体列表 + 搜索 + kind 过滤

**Files:** Modify `src/routes/graph/+page.svelte`

行为:顶栏搜索框(按 name/alias 子串过滤,大小写不敏感)+ kind 过滤药丸(「全部」+ 数据里实际出现的 kind,动态生成,`kindLabel` 显示,点选单选);列表按 note_count 降序显示 name / kind 徽章 / `N 笔 · M 提及`;人实体名前色点(`speakerInk`)。`$derived` 组合过滤。**布局/密度截图迭代。**

- [ ] **Step 1** 搜索/过滤状态与派生:

```ts
  let query = $state("");
  let kindFilter = $state<string>("all");
  const kinds = $derived([...new Set(entities.map((e) => e.kind))]);
  const shown = $derived(
    entities.filter((e) => {
      if (kindFilter !== "all" && e.kind !== kindFilter) return false;
      const q = query.trim().toLowerCase();
      if (!q) return true;
      return e.name.toLowerCase().includes(q) || e.aliases.some((a) => a.toLowerCase().includes(q));
    }),
  );
```

- [ ] **Step 2** 顶栏 + 列表标记(替换 Task 2 的占位 `<ul>`;搜索框 `bind:value={query}`;kind 药丸 `class:active={kindFilter===k}`;行显示 name + kind 徽章 + 计数)。人实体色点 `style="background: {speakerInk(e.id)}"`(从 `$lib/notes` 引 `speakerInk`;非人不显色点或用 kind 中性色)。

- [ ] **Step 3** DESIGN.md 补 `graph-list` / `filter-chips` 条目。

- [ ] **Step 4** 验证 `npm run check` 0/0 + 截图迭代。

- [ ] **Step 5** 提交:`git add src/routes/graph/+page.svelte DESIGN.md` → `git commit -m "图谱页:实体列表 + 搜索 + kind 过滤"`

---

## Task 4: 详情面板 + 实体导航 + 深链

**Files:** Modify `src/routes/graph/+page.svelte`

行为:
- 点列表项 → 若 `is_person` 直接 `goto("/speakers/" + e.id)`(e.id 即 person_id);否则选中,右侧面板拉 `entityDetail(e.id)`。
- 面板:name + kind 徽章 + 别名(有才显)+ `note_count 笔 · mention_total 提及`(**注:面板列出的笔记数可能 < note_count,是标题查不到的笔记被跳过,属正常**)+ 「出现的笔记」列表(点 → `goto("/notes/"+n.id)`)+ 「相关实体」列表(点 → 选中切换 / 人则跳 speakers)。`entityDetail` 返回 `null` → 面板空态。
- 深链:`onMount` 读 `$page.url.searchParams.get("e")`,有则自动选中并打开该实体面板(供 Plan C 笔记页高亮点击跳来)。

- [ ] **Step 1** 选中态 + 详情加载:

```ts
  import { page } from "$app/stores";
  import { entityDetail, type EntityDetail } from "$lib/graph";
  let selected = $state<string | null>(null);
  let detail = $state<EntityDetail | null>(null);
  async function select(e: { id: string; is_person: boolean }) {
    if (e.is_person) { goto("/speakers/" + e.id); return; }
    selected = e.id;
    detail = await entityDetail(e.id).catch(() => null);
  }
  onMount(() => {
    const e = $page.url.searchParams.get("e");
    if (e) select({ id: e, is_person: false });
  });
```
(与 Task 2 的 onMount 合并:先 `graphEntities()` 再处理深链。)

- [ ] **Step 2** 面板标记 + 笔记/相关实体点击导航(如上行为)。

- [ ] **Step 3** DESIGN.md 补 `entity-panel` 条目。

- [ ] **Step 4** 验证 `npm run check` 0/0 + 截图迭代(列表→点选→面板→点笔记跳转→人实体跳 speakers→深链 `/graph?e=e:xxx`)。

- [ ] **Step 5** 提交:`git add src/routes/graph/+page.svelte DESIGN.md` → `git commit -m "图谱页:详情面板 + 实体/笔记导航 + 深链"`

---

## 截图迭代方法(UI 拍板)
dev 已在跑(master),真实图谱数据随批量 re-Aing 增长。截图:playwright MCP navigate `localhost:1420/graph`(dev 的 Tauri invoke 在浏览器里报错是预期噪声——空态/静态布局仍可看);或 shim `window.__TAURI_INTERNALS__` 塞假 `graph_entities`/`entity_detail` 返回值渲染真实布局(参 memory 的 shot.mjs 套路)。双主题各截。用户看图拍板 → 改 → 再截,直到满意再进 Task 提交。

## Self-Review(对照 spec §UI / Plan B)
- Sidebar 页签 + `/graph` → Task 2 ✓;列表+搜索+kind 过滤 → Task 3 ✓;详情面板+导航+深链 → Task 4 ✓;空态 → Task 2 ✓;类型+绑定 → Task 1 ✓。
- 不含力导图(Plan C)✓;不改后端 ✓;降级红线(空→空态,detail null→面板空态)✓。
- 契约:note_count≥列出笔记数已在面板文案/注释声明 ✓;人→/speakers/[person_id] ✓。
- 禁 emoji、token 着色、DESIGN.md 补登 ✓(各 Task 尾)。
