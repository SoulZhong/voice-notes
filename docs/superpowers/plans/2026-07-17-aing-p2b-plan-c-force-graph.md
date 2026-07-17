# Aing Phase 2b · Plan C — 力导图 + 笔记页高亮可点 Implementation Plan

> **For agentic workers:** UI 收官件。数据/交互按下述落地;力导图观感靠 frontend-design + 截图迭代(DESIGN.md 真值源)。这是 Aing Phase 2 的最后一块。

**Goal:** `/graph` 主区未选实体时展示**力导向实体共现图**(d3-force + SVG,节点=实体/边=共现,拖拽/hover/点击导航);笔记页 Phase 2a 的静态实体高亮变**可点**(人→会议搭子、非人→图谱实体)。

**Architecture:** 纯前端,消费 Plan A 已合入 master 的 `graph_data`(节点+共现边)与 `note_entity_links`(笔记局部→全局 id)。新 `src/lib/ForceGraph.svelte`(自足力导图组件)+ 接进 `/graph` 主区占位处 + 改 `notes/[id]/+page.svelte` 高亮 span 为可点。

**Tech Stack:** SvelteKit 5、`d3-force`(**新增 npm 依赖**,仅 force 模块算物理)、SVG 渲染、app.css token、DESIGN.md。

## Global Constraints

- **规模封顶(必须)**:926+ 实体不可全画。力导图**只取有共现边的实体、按 note_count 降序 top-N**(常量 `MAX_NODES=60`),只保留两端都在所选节点集里的边;被截断时角标提示「显示前 N 个,用左侧列表/搜索看全部」。
- **优雅降级红线**:`graph_data` 空/失败 → 主区回退现有引导占位(不塌);节点<2 或无边 → 不渲染图,显示占位。`note_entity_links` 失败/空 → 高亮保持 Phase 2a 的不可点纯文本,不报错。
- **DESIGN.md 真值源**:禁 emoji(图标线性 SVG);节点色=kind(克制单色系,人实体用 `speakerColor`/`speakerInk` 会议搭子色系);边/hover 用 token;节点大小=note_count 映射(min~max 半径 clamp)。落地补登 DESIGN.md `force-graph` 条目。
- **契约**:节点点击 人(is_person 或 id 不以 `e:` 开头)→ `/speakers/[id]`,非人→ `/graph?e=<encodeURIComponent(id)>`(与侧栏 pickEntity 一致);笔记页 span 点击同规则。
- **无障碍/性能**:力导仿真在 `onMount` 起、组件销毁 `sim.stop()`;`prefers-reduced-motion` 时跳过入场动画/减少 tick。不改后端。`npm run check` 0/0、`npm run test` 绿。
- **分支** `feature/aing-p2b-plan-c-force-graph`(从 master `e5cfbc3`);`git add` 显式路径禁 `-A`;提交无署名尾注。

## File Structure

- **package.json**:加 `d3-force` + `@types/d3-force`(devDep)。
- **Create** `src/lib/ForceGraph.svelte`:入参 `{nodes, edges, onPick}`;内部选 top-N、跑 d3-force、SVG 画边+节点、拖拽/hover/点击。
- **Modify** `src/routes/graph/+page.svelte`:占位分支改为——有足够图数据时渲染 `<ForceGraph>`,否则保留引导占位;主区加载 `graphData()`。
- **Modify** `src/routes/notes/[id]/+page.svelte`:onMount 拉 `noteEntityLinks(id)`,把 `.entity-mention` span 变可点(局部 ent_N→全局 id 映射,点击导航)。
- **Modify** `DESIGN.md`:补 `force-graph` 条目。

## Task 1: 加 d3-force 依赖 + ForceGraph 组件(骨架:节点/边渲染 + top-N)
**Files:** package.json;Create `src/lib/ForceGraph.svelte`
- 入参:`nodes: EntitySummary[]`、`edges: EdgeRow[]`、`onPick: (id, isPerson) => void`。
- 选点:过滤出现在任一边里的实体 → 按 note_count 降序取前 `MAX_NODES=60` → 边只留两端都在集里的。
- d3-force:`forceSimulation(nodes).force("charge", forceManyBody().strength(-160)).force("link", forceLink(edges).id(d=>d.id).distance(48)).force("center", forceCenter(w/2,h/2)).force("collide", forceCollide(r+4))`;`sim.on("tick", ...)` 更新节点 x/y 到 `$state`;组件 `onDestroy(() => sim.stop())`。
- SVG:`<svg>` 内先画边 `<line>`(stroke `hairline-strong`,stroke-width 按 weight clamp,opacity .5),再画节点 `<circle>`(r=clamp note_count 映射 5~16,fill=kind 色/人用 speakerColor)+ `<text>` 标签(节点旁,`ink`,小字)。容器尺寸用 `ResizeObserver` 或 clientWidth/Height。
- 截断提示:选点数 < 有边实体总数时,角标 caption「显示前 60 个,用左侧列表看全部」。
- 步骤:装依赖 → 写组件 → `/graph` 里临时挂上传 graphData → `npm run check` → 截图看布局 → 提交。

## Task 2: 交互(拖拽 / hover 高亮邻居 / 点击导航)+ 接进 /graph
**Files:** `src/lib/ForceGraph.svelte`、`src/routes/graph/+page.svelte`
- 拖拽:节点 `onpointerdown` → 设 `fx/fy` 跟指针、`sim.alphaTarget(0.3).restart()`,松开清 `fx/fy`、`alphaTarget(0)`。
- hover:悬节点高亮其相邻节点+边,其余淡化(opacity)。
- 点击:`onPick(node.id, !node.id.startsWith("e:") || node.is_person)`;`/graph` 传的 onPick = 人 `goto("/speakers/"+id)`、非人 `goto("/graph?e="+encodeURIComponent(id))`。
- `/graph` 主区:`{:else}` 占位分支改为——`graphData()` 结果满足(有边、≥2 节点)时 `<ForceGraph nodes edges onPick>`,否则原引导占位;选中实体时仍走详情面板(力导图只在「未选」态占主区)。
- `npm run check` → 截图迭代(布局/节点密度/色/hover)→ 提交。

## Task 3: 笔记页实体高亮可点
**Files:** `src/routes/notes/[id]/+page.svelte`
- onMount(或修订稿就绪后)`noteEntityLinks(id)` → `Map<local_id, {global_id, is_person}>`。
- Phase 2a 的 `<span class="entity-mention" title=…>` 改为:有映射时渲染为可点(`role=button`/`<button>` 或加 onclick),点击 人→`/speakers/[global_id]`、非人→`/graph?e=<encodeURIComponent(global_id)>`;无映射(旧笔记/图谱失败)保持纯文本 span 不可点。
- 保持单色 accent-tint 观感;可点态加 hover 反馈(underline/加深)。`npm run check` → 截图验一条有实体的笔记 → 提交。

## Task 4: DESIGN.md `force-graph` 条目 + 收尾
- 补 DESIGN.md;全套 `npm run check` 0/0、`npm run test` 绿;截图目检双主题。

## Self-Review(对照 spec §UI / Plan C)
- 力导图(节点/边/top-N 封顶/拖拽/hover/点击导航)→ Task 1-2 ✓;笔记页高亮可点 → Task 3 ✓;d3-force 依赖 → Task 1 ✓;降级(空/失败回占位、无映射不可点)✓;DESIGN.md ✓。
- 规模封顶(MAX_NODES=60 + 只画有边)防 926 节点糊/卡 ✓;导航契约与侧栏一致 ✓。
