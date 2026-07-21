<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import { graphData, type GraphData } from "$lib/graph";
  import {
    pendingReview,
    semanticEntityDetail,
    type PendingReviewItem,
    type SemanticEntityDetail,
  } from "$lib/knowledge";
  import { DEFAULT_KNOWLEDGE_FILTER } from "$lib/knowledgeView";
  import { KNOWLEDGE_CHANGED_EVENT } from "$lib/knowledgeGovernance";
  import { graphFilter } from "$lib/graphFilter.svelte";
  import { noteGraphState } from "$lib/noteGraph.svelte";
  import ForceGraph from "$lib/ForceGraph.svelte";
  import EntityGovernance from "$lib/EntityGovernance.svelte";
  import PendingReviewPanel from "$lib/PendingReviewPanel.svelte";
  import RelationDrawer from "$lib/RelationDrawer.svelte";

  let graph = $state<GraphData>({ nodes: [], edges: [] });
  let loaded = $state(false);
  let detail = $state<SemanticEntityDetail | null>(null);
  let detailLoading = $state(false);
  let detailError = $state("");
  let pendingItems = $state<PendingReviewItem[]>([]);
  let ctxMenu = $state<{ id: string; name: string; isPerson: boolean; x: number; y: number } | null>(null);
  let detailGeneration = 0;

  const selected = $derived($page.url.searchParams.get("e"));
  const reviewOpen = $derived($page.url.searchParams.get("review") === "1");
  const relationId = $derived($page.url.searchParams.get("r"));
  const inspectorOpen = $derived(Boolean(selected || reviewOpen || relationId));
  const detailFilter = { ...DEFAULT_KNOWLEDGE_FILTER, include_history: true };

  const filteredGraph = $derived.by(() => {
    if (graphFilter.kind === "all") return graph;
    const keep = new Set(graph.nodes.filter((node) => node.kind === graphFilter.kind).map((node) => node.id));
    return {
      nodes: graph.nodes.filter((node) => keep.has(node.id)),
      edges: graph.edges.filter((edge) => keep.has(edge.a) && keep.has(edge.b)),
    };
  });

  async function loadGraph() {
    try {
      graph = await graphData();
    } catch {
      graph = { nodes: [], edges: [] };
    } finally {
      loaded = true;
    }
  }

  async function loadPending() {
    try {
      pendingItems = await pendingReview(DEFAULT_KNOWLEDGE_FILTER);
    } catch {
      pendingItems = [];
    }
  }

  async function loadDetail(id: string) {
    const generation = ++detailGeneration;
    detailLoading = true;
    detailError = "";
    try {
      const value = await semanticEntityDetail(id, detailFilter);
      if (generation !== detailGeneration) return;
      detail = value;
      if (!value) detailError = "该实体还未进入当前语义索引，请稍后刷新。";
    } catch (cause) {
      if (generation !== detailGeneration) return;
      detail = null;
      detailError = `实体治理信息读取失败：${cause instanceof Error ? cause.message : String(cause)}`;
    } finally {
      if (generation === detailGeneration) detailLoading = false;
    }
  }

  onMount(() => {
    void Promise.all([loadGraph(), loadPending()]);
  });

  $effect(() => {
    if (graphFilter.mode === "note" && noteGraphState.status === "idle") {
      void noteGraphState.load();
    }
  });

  $effect(() => {
    const id = selected;
    if (!id) {
      ++detailGeneration;
      detail = null;
      detailLoading = false;
      detailError = "";
      return;
    }
    void loadDetail(id);
  });

  async function refreshKnowledge() {
    const id = selected;
    const generation = ++detailGeneration;
    const [nextGraph, nextPending, nextDetail] = await Promise.all([
      graphData(),
      pendingReview(DEFAULT_KNOWLEDGE_FILTER),
      id ? semanticEntityDetail(id, detailFilter) : Promise.resolve(null),
    ]);
    graph = nextGraph;
    pendingItems = nextPending;
    loaded = true;
    if (generation === detailGeneration) {
      detail = nextDetail;
      detailLoading = false;
      detailError = id && !nextDetail ? "该实体还未进入当前语义索引，请稍后刷新。" : "";
    }
    window.dispatchEvent(new CustomEvent(KNOWLEDGE_CHANGED_EVENT));
  }

  function updateQuery(change: (params: URLSearchParams) => void) {
    const url = new URL($page.url);
    change(url.searchParams);
    goto(url.pathname + url.search);
  }

  function pickNode(id: string, isPerson: boolean) {
    if (isPerson) goto("/speakers/" + encodeURIComponent(id));
    else updateQuery((params) => { params.set("e", id); params.delete("review"); params.delete("r"); });
  }

  function pickNoteNode(id: string) {
    goto("/notes/" + encodeURIComponent(id));
  }

  function openRelation(id: string) {
    updateQuery((params) => { params.set("r", id); });
  }

  function closeRelation() {
    updateQuery((params) => { params.delete("r"); });
  }

  function closeReview() {
    updateQuery((params) => { params.delete("review"); });
  }

  function closeEntity() {
    updateQuery((params) => { params.delete("e"); });
  }

  function openCtxMenu(
    id: string,
    name: string,
    isPerson: boolean,
    clientX: number,
    clientY: number,
  ) {
    ctxMenu = { id, name, isPerson, x: clientX, y: clientY };
  }

  function closeCtxMenu() {
    ctxMenu = null;
  }

  function openContextGovernance() {
    if (!ctxMenu) return;
    const target = ctxMenu;
    closeCtxMenu();
    pickNode(target.id, target.isPerson);
  }

  function handleEscape(event: KeyboardEvent) {
    if (event.key !== "Escape") return;
    if (ctxMenu) {
      closeCtxMenu();
      return;
    }
    if (document.querySelector("dialog[open]")) return;
    if (relationId) closeRelation();
    else if (reviewOpen) closeReview();
    else if (selected) closeEntity();
  }
</script>

<svelte:window onkeydown={handleEscape} />

<div class="graph-main">
  {#if graphFilter.mode === "note"}
    {#if noteGraphState.data.edges.length > 0 && noteGraphState.data.nodes.length >= 2}
      <ForceGraph
        nodes={noteGraphState.data.nodes}
        edges={noteGraphState.data.edges}
        onPick={(id) => pickNoteNode(id)}
        query={graphFilter.query}
        showLegend={false}
      />
    {:else if noteGraphState.data.nodes.length > 0}
      <div class="placeholder">
        <p class="ph-title">笔记之间还没有连接</p>
        <p class="ph-desc">当两篇笔记提到同一个实体时，它们会在这里连成一条边。多 Aing 几篇笔记后就会出现关联。</p>
      </div>
    {:else if noteGraphState.status === "loading" || noteGraphState.status === "idle"}
      <div class="placeholder">
        <p class="ph-title">文章视角</p>
        <p class="ph-desc">这里把每篇笔记画成一个节点，共享实体越多的笔记靠得越近。正在加载。</p>
      </div>
    {:else if noteGraphState.status === "error"}
      <div class="placeholder">
        <p class="ph-title">文章图谱加载失败</p>
        <p class="ph-desc">图谱是增值索引，失败不会影响笔记内容。可以稍后重新加载。</p>
        <button class="empty-cta" onclick={() => noteGraphState.load()}>重新加载文章图谱</button>
      </div>
    {:else}
      <div class="placeholder">
        <p class="ph-title">还没有进入图谱的笔记</p>
        <p class="ph-desc">配置大模型并对笔记「重新 Aing」后，笔记会按共享实体在这里建立连接。</p>
      </div>
    {/if}
  {:else}
    <!-- This stage never branches on `selected`: changing ?e= updates only the
         edge inspector, so the ForceGraph component and its camera stay mounted. -->
    <div class="entity-stage" class:with-inspector={inspectorOpen}>
      <div class="canvas-shell" aria-label="知识图谱画布">
        {#if loaded && graph.nodes.length === 0}
          <div class="placeholder">
            <p class="ph-title">还没有知识图谱</p>
            <p class="ph-desc">配置大模型并对笔记「重新 Aing」后，人物、组织、项目等实体会汇入这里。</p>
            <button class="empty-cta" onclick={() => goto("/ai")}>前往 AI 设置</button>
          </div>
        {:else if filteredGraph.nodes.length >= 2 && graph.edges.length > 0}
          <ForceGraph
            nodes={filteredGraph.nodes}
            edges={filteredGraph.edges}
            onPick={pickNode}
            onContextMenu={openCtxMenu}
            query={graphFilter.query}
          />
        {:else if loaded && graph.nodes.length >= 2}
          <div class="placeholder">
            <p class="ph-title">没有匹配的实体关系</p>
            <p class="ph-desc">当前类型筛选下没有足够的关系可画，可以清除筛选后继续探索。</p>
            <button class="empty-cta" onclick={() => (graphFilter.kind = "all")}>清除实体类型筛选</button>
          </div>
        {:else}
          <div class="placeholder">
            <p class="ph-title">知识图谱</p>
            <p class="ph-desc">从左侧选择实体，或继续 Aing 笔记以建立更多连接。</p>
          </div>
        {/if}
      </div>

      {#if inspectorOpen}
        <aside class="edge-inspector" aria-label="知识治理检查器">
          {#if relationId}
            <RelationDrawer {relationId} onClose={closeRelation} onChanged={refreshKnowledge} />
          {:else if reviewOpen}
            <PendingReviewPanel
              items={pendingItems}
              onClose={closeReview}
              onChanged={refreshKnowledge}
              onOpenRelation={openRelation}
            />
          {:else if selected}
            <button class="inspector-close" type="button" aria-label="关闭实体治理检查器" onclick={closeEntity}>×</button>
            {#if detailLoading && !detail}
              <p class="inspector-state" aria-live="polite">正在读取实体治理信息</p>
            {:else if detail}
              <EntityGovernance {detail} onChanged={refreshKnowledge} onOpenRelation={openRelation} />
            {:else}
              <div class="inspector-state" aria-live="polite">
                <p>{detailError || "实体治理信息暂不可用。"}</p>
                <button type="button" onclick={() => selected && loadDetail(selected)}>重试读取实体信息</button>
              </div>
            {/if}
          {/if}
        </aside>
      {/if}
    </div>
  {/if}
</div>

{#if ctxMenu}
  <!-- Pointer convenience layer. The same rename/governance entry stays visible in EntityGovernance. -->
  <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
  <div class="menu-overlay" onclick={closeCtxMenu} oncontextmenu={(event) => { event.preventDefault(); closeCtxMenu(); }}></div>
  <div class="ctx-menu" style:left={`${ctxMenu.x}px`} style:top={`${ctxMenu.y}px`} role="menu" aria-label={`治理 ${ctxMenu.name}`}>
    <p>{ctxMenu.name}</p>
    <button class="ctx-item" role="menuitem" onclick={openContextGovernance}>改名</button>
    <button class="ctx-item" role="menuitem" onclick={openContextGovernance}>打开治理面板</button>
  </div>
{/if}

<style>
  .graph-main, .entity-stage, .canvas-shell { height: 100%; min-width: 0; min-height: 0; }
  .graph-main { overflow: hidden; }
  .entity-stage { display: flex; background: var(--canvas); }
  .canvas-shell { flex: 1 1 auto; position: relative; }
  .edge-inspector {
    flex: 0 0 clamp(340px, 34%, 440px);
    box-sizing: border-box;
    min-width: 0;
    overflow-y: auto;
    padding: 22px 22px 0;
    border-left: 1px solid var(--hairline);
    background: var(--surface);
  }
  .inspector-close {
    position: sticky;
    top: 0;
    z-index: 1;
    float: right;
    width: 36px;
    height: 36px;
    padding: 0;
    border: 0;
    border-radius: var(--radius-full);
    background: var(--surface);
    color: var(--ink-secondary);
    font: inherit;
    font-size: 1.3rem;
    cursor: pointer;
  }
  .inspector-close:hover { background: var(--surface-soft); color: var(--ink); }
  .inspector-state { margin: 56px 0 0; color: var(--ink-secondary); font-size: 0.86rem; line-height: 1.6; }
  .inspector-state button, .empty-cta {
    min-height: 36px;
    padding: 7px 13px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-md);
    background: transparent;
    color: var(--ink-secondary);
    font: inherit;
    font-size: 0.82rem;
    cursor: pointer;
  }
  .inspector-state button:hover, .empty-cta:hover { background: var(--surface-soft); color: var(--ink); }
  .placeholder { max-width: 440px; margin: 18vh auto 0; padding: 0 20px; text-align: center; }
  .ph-title { margin: 0 0 8px; color: var(--ink); font-size: 1.05rem; font-weight: 500; }
  .ph-desc { margin: 0 0 18px; color: var(--ink-secondary); font-size: 0.82rem; line-height: 1.65; }
  button:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  .menu-overlay { position: fixed; inset: 0; z-index: 40; }
  .ctx-menu {
    position: fixed;
    z-index: 41;
    display: flex;
    flex-direction: column;
    min-width: 10rem;
    padding: 4px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--surface-press);
    box-shadow: var(--shadow-popover);
  }
  .ctx-menu p { margin: 3px 7px 5px; color: var(--ink-faint); font-size: 0.7rem; overflow-wrap: anywhere; }
  .ctx-item { min-height: 34px; padding: 6px 9px; border: 0; border-radius: var(--radius-md); background: transparent; color: var(--ink); font: inherit; font-size: 0.84rem; text-align: left; cursor: pointer; }
  .ctx-item:hover { background: var(--surface-soft); }
  @media (max-width: 880px) {
    .edge-inspector {
      position: fixed;
      inset: 0 0 0 auto;
      z-index: 30;
      width: min(420px, 100vw);
      padding: max(20px, env(safe-area-inset-top)) 20px max(12px, env(safe-area-inset-bottom));
      border: 0;
      border-left: 1px solid var(--hairline-strong);
      border-radius: 0;
    }
  }
  @media (pointer: coarse) {
    button, .ctx-item { min-height: 44px; }
    .inspector-close { width: 44px; }
  }
  @media (prefers-reduced-motion: reduce) {
    *, *::before, *::after { transition-duration: 0.01ms !important; animation-duration: 0.01ms !important; }
  }
</style>
