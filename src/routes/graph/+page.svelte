<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { goto, replaceState } from "$app/navigation";
  import { page } from "$app/stores";
  import { graphData, kindLabel, type GraphData, type RenderEdge } from "$lib/graph";
  import {
    semanticEntityDetail,
    semanticGraph,
    semanticGraphDebugFixture,
    semanticGraphDebugRelease,
    semanticGraphDebugRelationDetail,
    type KnowledgeFilter,
    type SemanticEntityDetail,
    type SemanticGraphData,
  } from "$lib/knowledge";
  import {
    DEFAULT_KNOWLEDGE_FILTER,
    GLOBAL_SEMANTIC_PRESENCE_FILTER,
    defaultBackbone,
    createDebugFixtureReleaseOnce,
    debugKnowledgeRoutePolicy,
    ensureBackboneEdge,
    filterSemanticGraph,
    legacyFallbackGraph,
    nextExpandedIds,
    relationLabel,
    resolveGraphRefreshState,
    sanitizeDebugGraphUrl,
    searchAdmissionIds,
    semanticRequestFailureMessage,
    shouldUseLegacyFallback,
    viewEdges,
    type GlobalSemanticPresence,
  } from "$lib/knowledgeView";
  import { KNOWLEDGE_CHANGED_EVENT } from "$lib/knowledgeGovernance";
  import { graphFilter } from "$lib/graphFilter.svelte";
  import { noteGraphState } from "$lib/noteGraph.svelte";
  import ForceGraph from "$lib/ForceGraph.svelte";
  import KnowledgeGraphToolbar from "$lib/KnowledgeGraphToolbar.svelte";
  import EntityGovernance from "$lib/EntityGovernance.svelte";
  import RelationDrawer from "$lib/RelationDrawer.svelte";
  import RelationBackfillDialog from "$lib/RelationBackfillDialog.svelte";

  const emptySemantic = (): SemanticGraphData => ({
    nodes: [],
    semantic_edges: [],
    cooccurrence_edges: [],
    degraded: false,
    message: null,
  });
  const BACKBONE_NODE_LIMIT = 80;

  let graph = $state<GraphData>({ nodes: [], edges: [] });
  let semantic = $state<SemanticGraphData>(emptySemantic());
  let knowledgeFilter = $state<KnowledgeFilter>({
    ...DEFAULT_KNOWLEDGE_FILTER,
    entity_kinds: graphFilter.kind === "all" ? [] : [graphFilter.kind],
  });
  let visibleIds = $state<Set<string>>(new Set());
  let expansionDepth = $state<Map<string, number>>(new Map());
  let loaded = $state(false);
  let semanticLoading = $state(false);
  let semanticError = $state("");
  let semanticRequestFailed = $state(false);
  let predicateCatalog = $state<Map<string, string>>(new Map());
  let globalSemanticPresence = $state<GlobalSemanticPresence>("unknown");
  let detail = $state<SemanticEntityDetail | null>(null);
  let detailLoading = $state(false);
  let detailError = $state("");
  let detailGeneration = 0;
  let graphGeneration = 0;
  let graphViewResetKey = $state(0);
  let backfillOpen = $state(false);
  let debugFixtureSession = $state<string | null>(null);
  let debugRelationEnabled = $state(false);
  let ctxMenu = $state<{ id: string; name: string; isPerson: boolean; x: number; y: number } | null>(null);
  let debugFixtureDisposed = false;
  const releaseDebugFixtureOnce = createDebugFixtureReleaseOnce(semanticGraphDebugRelease);
  let lastSidebarKind = graphFilter.kind;

  const debugFixtureRequested = $derived(
    import.meta.env.DEV && $page.url.searchParams.get("debugFixture") === "semantic-large",
  );
  const routePolicy = $derived(
    debugKnowledgeRoutePolicy(
      $page.url,
      import.meta.env.DEV,
      Boolean(debugFixtureSession),
      debugRelationEnabled,
    ),
  );
  const selected = $derived(routePolicy.selected);
  const relationId = $derived(routePolicy.relationId);
  const manageOpen = $derived($page.url.searchParams.get("manage") === "1");
  const inspectorOpen = $derived(Boolean(selected || relationId));
  const detailFilter = { ...DEFAULT_KNOWLEDGE_FILTER, include_history: true };
  const effectiveGraphFilter = $derived<KnowledgeFilter>(knowledgeFilter);

  const usableSemantic = $derived.by((): SemanticGraphData => {
    if (!shouldUseLegacyFallback(globalSemanticPresence, semantic, semanticRequestFailed)) {
      return semantic;
    }
    return legacyFallbackGraph(semantic, graph);
  });
  const filteredSemantic = $derived(filterSemanticGraph(usableSemantic, effectiveGraphFilter));
  const renderedNodes = $derived(filteredSemantic.nodes.filter((node) => visibleIds.has(node.id)));
  const renderedEdges = $derived(
    viewEdges(usableSemantic, effectiveGraphFilter).filter(
      (edge) => visibleIds.has(edge.a) && visibleIds.has(edge.b),
    ),
  );
  const semanticFallback = $derived(
    loaded &&
      !semanticRequestFailed &&
      globalSemanticPresence === "absent" &&
      filteredSemantic.semantic_edges.length === 0 &&
      filteredSemantic.cooccurrence_edges.length > 0,
  );
  const filteredSemanticEmpty = $derived(
    loaded &&
      !semanticLoading &&
      !semanticRequestFailed &&
      globalSemanticPresence === "present" &&
      semantic.semantic_edges.length === 0,
  );
  const semanticFailureHasLegacy = $derived(
    viewEdges(legacyFallbackGraph(semantic, graph), effectiveGraphFilter).length > 0,
  );
  const semanticStatusMessage = $derived(
    semanticRequestFailed
      ? semanticRequestFailureMessage(semanticFailureHasLegacy)
      : semanticError,
  );
  const entityNames = $derived(
    new Map([...graph.nodes, ...semantic.nodes].map((node) => [node.id, node.name])),
  );
  const availableKinds = $derived.by(() => {
    const values = new Set([...graph.nodes, ...semantic.nodes].map((node) => node.kind));
    return [...values].sort().map((value) => ({ value, label: kindLabel(value) }));
  });
  const availablePredicates = $derived.by(() => {
    return [...predicateCatalog].sort(([left], [right]) => left.localeCompare(right)).map(([value, label]) => ({ value, label }));
  });

  function initialIds(data: SemanticGraphData): Set<string> {
    const filtered = filterSemanticGraph(data, knowledgeFilter);
    const semanticIds = ensureBackboneEdge(
      defaultBackbone(filtered, BACKBONE_NODE_LIMIT, 3),
      filtered,
      BACKBONE_NODE_LIMIT,
    );
    if (semanticIds.size > 0) return semanticIds;
    return new Set(
      [...filtered.nodes]
        .sort((left, right) => right.note_count - left.note_count || left.id.localeCompare(right.id))
        .slice(0, BACKBONE_NODE_LIMIT)
        .map((node) => node.id),
    );
  }

  async function loadGraph() {
    if (debugFixtureRequested) return;
    try {
      graph = await graphData();
      if ((semantic.nodes.length === 0 || semanticRequestFailed) && visibleIds.size === 0) {
        visibleIds = initialIds(usableSemantic);
      }
    } catch {
      graph = { nodes: [], edges: [] };
    } finally {
      loaded = true;
    }
  }

  type SemanticLoadMode = "reset-view" | "preserve-view";

  async function loadSemantic(
    filter: KnowledgeFilter,
    mode: SemanticLoadMode = "reset-view",
  ) {
    if (debugFixtureRequested) return;
    const generation = ++graphGeneration;
    semanticLoading = true;
    semanticError = "";
    try {
      const value = await semanticGraph(filter);
      if (generation !== graphGeneration) return;
      semanticRequestFailed = false;
      semantic = value;
      const nextPredicates = new Map(predicateCatalog);
      for (const edge of value.semantic_edges) nextPredicates.set(edge.predicate_type, relationLabel(edge));
      predicateCatalog = nextPredicates;
      if (value.semantic_edges.length > 0) globalSemanticPresence = "present";
      if (value.degraded && value.message) console.warn("semantic graph degraded", value.message);
      semanticError = value.degraded ? "语义关系服务暂时降级，当前显示可用结果。" : "";
      const filtered = filterSemanticGraph(value, filter);
      if (mode === "preserve-view") {
        const refreshed = resolveGraphRefreshState(
          filtered,
          visibleIds,
          expansionDepth,
          [],
          initialIds(value),
        );
        visibleIds = refreshed.visibleIds;
        expansionDepth = refreshed.expansionDepth;
        if (refreshed.shouldResetView) graphViewResetKey += 1;
      } else {
        visibleIds = initialIds(value);
        expansionDepth = new Map();
        graphViewResetKey += 1;
      }
    } catch (cause) {
      if (generation !== graphGeneration) return;
      console.warn("semantic graph request failed", cause);
      semanticRequestFailed = true;
      const fallback = legacyFallbackGraph(semantic, graph);
      if (mode === "preserve-view") {
        const filteredFallback = filterSemanticGraph(fallback, filter);
        const refreshed = resolveGraphRefreshState(
          filteredFallback,
          visibleIds,
          expansionDepth,
          [],
          initialIds(fallback),
        );
        visibleIds = refreshed.visibleIds;
        expansionDepth = refreshed.expansionDepth;
        if (refreshed.shouldResetView) graphViewResetKey += 1;
      } else {
        visibleIds = initialIds(fallback);
        expansionDepth = new Map();
        graphViewResetKey += 1;
      }
    } finally {
      if (generation === graphGeneration) {
        semanticLoading = false;
        loaded = true;
      }
    }
  }

  async function probeGlobalSemanticPresence() {
    if (debugFixtureRequested) return;
    try {
      const value = await semanticGraph(GLOBAL_SEMANTIC_PRESENCE_FILTER);
      const observedPresence = value.semantic_edges.length > 0 ? "present" : "absent";
      if (globalSemanticPresence !== "present" || observedPresence === "present") {
        globalSemanticPresence = observedPresence;
      }
      if (globalSemanticPresence === "absent" && visibleIds.size === 0) {
        visibleIds = initialIds(usableSemantic);
      }
    } catch {
      if (globalSemanticPresence !== "present") globalSemanticPresence = "unknown";
    }
  }

  async function loadDetail(id: string) {
    if (debugFixtureRequested) return;
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

  async function loadDebugFixture() {
    semanticLoading = true;
    semanticError = "";
    try {
      const fixture = await semanticGraphDebugFixture();
      if (debugFixtureDisposed) {
        try {
          await releaseDebugFixtureOnce(fixture.session_id);
        } catch (cause) {
          console.warn("isolated semantic graph fixture cleanup failed", cause);
        }
        return;
      }
      debugFixtureSession = fixture.session_id;
      semantic = fixture.graph;
      graph = { nodes: [], edges: [] };
      globalSemanticPresence = "present";
      semanticRequestFailed = false;
      visibleIds = initialIds(fixture.graph);
      loaded = true;
    } catch (cause) {
      if (debugFixtureDisposed) return;
      console.warn("isolated semantic graph fixture failed", cause);
      semanticRequestFailed = true;
      semanticError = "隔离调试夹具加载失败。请重新打开调试地址。";
      loaded = true;
    } finally {
      if (!debugFixtureDisposed) semanticLoading = false;
    }
  }

  onMount(() => {
    if (debugFixtureRequested) {
      const sanitized = sanitizeDebugGraphUrl($page.url);
      debugRelationEnabled = false;
      backfillOpen = false;
      if (sanitized.search !== $page.url.search) {
        replaceState(sanitized.pathname + sanitized.search, {});
      }
      void loadDebugFixture();
      return;
    }
    void Promise.all([
      loadGraph(),
      loadSemantic(knowledgeFilter),
      probeGlobalSemanticPresence(),
    ]);
  });

  onDestroy(() => {
    debugFixtureDisposed = true;
    const session = debugFixtureSession;
    debugFixtureSession = null;
    if (session) {
      void releaseDebugFixtureOnce(session).catch((cause) => {
        console.warn("isolated semantic graph fixture cleanup failed", cause);
      });
    }
  });

  $effect(() => {
    const sidebarKind = graphFilter.kind;
    if (sidebarKind === lastSidebarKind) return;
    lastSidebarKind = sidebarKind;
    applyKnowledgeFilter({
      ...knowledgeFilter,
      entity_kinds: sidebarKind === "all" ? [] : [sidebarKind],
    });
  });

  $effect(() => {
    if (graphFilter.mode === "note" && noteGraphState.status === "idle") void noteGraphState.load();
  });

  $effect(() => {
    const query = graphFilter.query.trim().toLowerCase();
    if (!query) return;
    const matches = [...searchAdmissionIds(filteredSemantic.nodes, filteredSemantic.semantic_edges, query)];
    if (matches.some((id) => !visibleIds.has(id))) visibleIds = new Set([...visibleIds, ...matches]);
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
    if (debugFixtureRequested) return;
    const id = selected;
    const generation = ++detailGeneration;
    // Install this refresh's co-occurrence graph before semantic loading. If the
    // semantic request fails, its preserve-view fallback must rebase against the
    // same generation rather than IDs from the previous graph.
    const nextGraph = await graphData();
    graph = nextGraph;
    const [nextDetail] = await Promise.all([
      id ? semanticEntityDetail(id, detailFilter) : Promise.resolve(null),
      loadSemantic(effectiveGraphFilter, "preserve-view"),
    ]);
    loaded = true;
    if (generation === detailGeneration) {
      detail = nextDetail;
      detailLoading = false;
      detailError = id && !nextDetail ? "该实体还未进入当前语义索引，请稍后刷新。" : "";
    }
    window.dispatchEvent(new CustomEvent(KNOWLEDGE_CHANGED_EVENT));
  }

  async function refreshAfterBackfill() {
    if (debugFixtureRequested) return;
    await loadGraph();
    await Promise.all([
      loadSemantic(effectiveGraphFilter, "preserve-view"),
      probeGlobalSemanticPresence(),
    ]);
    if (selected) await loadDetail(selected);
  }

  function updateQuery(change: (params: URLSearchParams) => void) {
    const url = new URL($page.url);
    change(url.searchParams);
    goto(url.pathname + url.search);
  }

  function revealFrom(id: string) {
    const depth = (expansionDepth.get(id) ?? 0) + 1;
    expansionDepth = new Map(expansionDepth).set(id, depth);
    const revealed = nextExpandedIds(new Set([id]), filteredSemantic.semantic_edges, depth, 8);
    visibleIds = new Set([...visibleIds, ...revealed]);
  }

  function pickNode(id: string, _isPerson: boolean) {
    revealFrom(id);
    if (debugFixtureRequested) return;
    updateQuery((params) => { params.set("e", id); params.delete("manage"); params.delete("review"); params.delete("r"); });
  }

  function pickNoteNode(id: string) {
    goto("/notes/" + encodeURIComponent(id));
  }

  function openRelation(id: string) {
    if (debugFixtureRequested) {
      if (!debugFixtureSession) return;
      debugRelationEnabled = true;
    }
    updateQuery((params) => { params.set("r", id); });
  }

  function pickEdge(id: string, layer: RenderEdge["layer"]) {
    if (layer === "semantic") openRelation(id);
  }

  function closeRelation() {
    debugRelationEnabled = false;
    updateQuery((params) => { params.delete("r"); });
  }

  function closeEntity() {
    updateQuery((params) => { params.delete("e"); params.delete("manage"); });
  }

  function collapseGraph() {
    expansionDepth = new Map();
    visibleIds = initialIds(usableSemantic);
    graphViewResetKey += 1;
  }

  function openCtxMenu(id: string, name: string, isPerson: boolean, clientX: number, clientY: number) {
    if (debugFixtureRequested) return;
    ctxMenu = {
      id,
      name,
      isPerson,
      x: Math.max(8, Math.min(clientX, window.innerWidth - 196)),
      y: Math.max(8, Math.min(clientY, window.innerHeight - 126)),
    };
  }

  function closeCtxMenu() {
    ctxMenu = null;
  }

  function openContextDetail() {
    if (!ctxMenu) return;
    const id = ctxMenu.id;
    closeCtxMenu();
    updateQuery((params) => { params.set("e", id); params.delete("manage"); params.delete("r"); });
  }

  function openContextGovernance() {
    if (!ctxMenu) return;
    const id = ctxMenu.id;
    closeCtxMenu();
    updateQuery((params) => { params.set("e", id); params.set("manage", "1"); params.delete("r"); });
  }

  function applyKnowledgeFilter(next: KnowledgeFilter) {
    knowledgeFilter = {
      ...next,
      entity_kinds: [...next.entity_kinds],
      predicate_types: [...next.predicate_types],
    };
    const sidebarKind = next.entity_kinds.length === 1 ? next.entity_kinds[0]! : "all";
    lastSidebarKind = sidebarKind;
    graphFilter.kind = sidebarKind;
    if (debugFixtureRequested) {
      visibleIds = initialIds(semantic);
      return;
    }
    void loadSemantic(knowledgeFilter);
  }

  async function loadDebugRelationDetail(id: string) {
    if (!debugFixtureRequested || !debugFixtureSession) return null;
    return semanticGraphDebugRelationDetail(debugFixtureSession, id);
  }

  function handleEscape(event: KeyboardEvent) {
    if (event.key !== "Escape") return;
    if (ctxMenu) {
      closeCtxMenu();
      return;
    }
    if (document.querySelector("dialog[open]")) return;
    if (relationId) closeRelation();
    else if (selected) closeEntity();
  }
</script>

<svelte:window onkeydown={handleEscape} />

<div class="graph-main">
  {#if graphFilter.mode === "note" && !debugFixtureRequested}
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
        <p class="ph-desc">当两篇笔记提到同一个实体时，它们会在这里连成一条边。</p>
      </div>
    {:else if noteGraphState.status === "loading" || noteGraphState.status === "idle"}
      <div class="placeholder"><p class="ph-title">文章视角</p><p class="ph-desc">正在按共享实体加载笔记关系。</p></div>
    {:else if noteGraphState.status === "error"}
      <div class="placeholder">
        <p class="ph-title">文章图谱加载失败</p>
        <p class="ph-desc">图谱索引失败不会影响笔记内容。</p>
        <button class="empty-cta" onclick={() => noteGraphState.load()}>重新加载文章图谱</button>
      </div>
    {:else}
      <div class="placeholder"><p class="ph-title">还没有进入图谱的笔记</p><p class="ph-desc">对笔记重新 Aing 后会按共享实体建立连接。</p></div>
    {/if}
  {:else}
    <!-- 点击实体或关系只打开附着式详情，不会重挂载画布。 -->
    <div class="entity-stage" class:with-inspector={inspectorOpen}>
      <div class="map-column">
        <KnowledgeGraphToolbar
          filter={knowledgeFilter}
          kinds={availableKinds}
          predicates={availablePredicates}
          visibleCount={renderedNodes.length}
          totalCount={filteredSemantic.nodes.length}
          loading={semanticLoading}
          onChange={applyKnowledgeFilter}
          onCollapse={collapseGraph}
        />
        <div class="canvas-shell" aria-label="知识图谱画布">
          {#if debugFixtureRequested}
            <div class="map-message debug-fixture" role="status">
              <span>隔离调试模式 · 仅创建并读取临时夹具，不会读取或修改真实资料库</span>
              {#if debugFixtureSession}<small>1,000 个实体 / 5,000 条语义关系</small>{/if}
              {#if semanticError}<small>{semanticError}</small>{/if}
            </div>
          {/if}
          {#if !debugFixtureRequested && semanticStatusMessage}
            <div class="map-message degraded" role="status">
              <span>{semanticStatusMessage}</span>
              {#if semanticRequestFailed}
                <button type="button" onclick={() => loadSemantic(effectiveGraphFilter)}>重新读取</button>
              {/if}
              <button type="button" onclick={() => (backfillOpen = true)}>补建语义关系</button>
            </div>
          {/if}
          {#if !debugFixtureRequested && semanticFallback}
            <div class="map-message fallback">
              <span>尚未补建语义关系，当前保留共现弱连接供继续探索。</span>
              <button type="button" onclick={() => (backfillOpen = true)}>补建语义关系</button>
            </div>
          {/if}
          {#if !debugFixtureRequested && filteredSemanticEmpty}
            <div class="map-message" role="status">
              <span>当前筛选下没有语义关系，图谱没有切换为旧版共现结果。</span>
              <button type="button" onclick={() => applyKnowledgeFilter(DEFAULT_KNOWLEDGE_FILTER)}>重置图谱筛选</button>
            </div>
          {/if}

          {#if loaded && usableSemantic.nodes.length === 0 && graph.nodes.length === 0}
            <div class="placeholder">
              <p class="ph-title">还没有知识图谱</p>
              <p class="ph-desc">配置大模型并对笔记「重新 Aing」后，人物、组织、项目等实体会汇入这里。</p>
              <button class="empty-cta" onclick={() => goto("/ai")}>前往 AI 设置</button>
            </div>
          {:else if renderedNodes.length >= 2 && renderedEdges.length > 0}
            <ForceGraph
              nodes={renderedNodes}
              edges={renderedEdges}
              onPick={pickNode}
              onEdgePick={pickEdge}
              onContextMenu={openCtxMenu}
              query={graphFilter.query}
              showLegend={false}
              resetKey={graphViewResetKey}
              maxNodes={2000}
              minEdgeWeight={0}
              backboneK={2000}
            />
          {:else if loaded && filteredSemantic.nodes.length >= 2}
            <div class="placeholder">
              <p class="ph-title">没有匹配的实体关系</p>
              <p class="ph-desc">当前类型下没有可连接的关系。选择全部类型，或从侧栏搜索其他实体。</p>
              <button class="empty-cta" onclick={() => applyKnowledgeFilter(DEFAULT_KNOWLEDGE_FILTER)}>显示全部类型</button>
            </div>
          {:else}
            <div class="placeholder">
              <p class="ph-title">知识图谱</p>
              <p class="ph-desc">从实体名称开始，沿完整关系逐层探索会议上下文。</p>
            </div>
          {/if}

        </div>
      </div>

      {#if inspectorOpen}
        <aside class="edge-inspector" aria-label="知识治理检查器">
          {#if debugFixtureRequested && debugFixtureSession && relationId}
            <RelationDrawer
              {relationId}
              onClose={closeRelation}
              onChanged={async () => {}}
              relationLoader={loadDebugRelationDetail}
              resolveEntityName={(id) => entityNames.get(id)}
              readOnly={true}
            />
          {:else if !debugFixtureRequested && relationId}
            <RelationDrawer
              {relationId}
              onClose={closeRelation}
              onChanged={refreshKnowledge}
              simple={true}
            />
          {:else if !debugFixtureRequested && selected}
            <button class="inspector-close" type="button" aria-label="关闭实体治理检查器" onclick={closeEntity}>×</button>
            {#if detailLoading && !detail}
              <p class="inspector-state" aria-live="polite">正在读取实体治理信息</p>
            {:else if detail}
              <EntityGovernance
                {detail}
                onChanged={refreshKnowledge}
                onOpenRelation={openRelation}
                resolveEntityName={(id) => entityNames.get(id)}
                simple={!manageOpen}
              />
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

{#if !debugFixtureRequested}
  <RelationBackfillDialog
    open={backfillOpen}
    onClose={() => (backfillOpen = false)}
    onCompleted={refreshAfterBackfill}
  />
{/if}

{#if ctxMenu}
  <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
  <div class="menu-overlay" onclick={closeCtxMenu} oncontextmenu={(event) => { event.preventDefault(); closeCtxMenu(); }}></div>
  <div class="ctx-menu" style:left={`${ctxMenu.x}px`} style:top={`${ctxMenu.y}px`} role="menu" aria-label={`治理 ${ctxMenu.name}`}>
    <p>{ctxMenu.name}</p>
    <button class="ctx-item" role="menuitem" onclick={openContextDetail}>查看实体详情</button>
    <button class="ctx-item" role="menuitem" onclick={openContextGovernance}>管理实体</button>
  </div>
{/if}

<style>
  .graph-main, .entity-stage { height: 100%; min-width: 0; min-height: 0; }
  .graph-main { overflow: hidden; }
  .entity-stage { display: flex; background: var(--canvas); }
  .map-column { display: flex; flex: 1 1 auto; flex-direction: column; min-width: 0; min-height: 0; }
  .canvas-shell { flex: 1 1 auto; position: relative; min-width: 0; min-height: 0; }
  .menu-overlay { position: fixed; inset: 0; z-index: 48; }
  .ctx-menu {
    position: fixed;
    z-index: 49;
    display: grid;
    min-width: 180px;
    padding: 6px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-lg);
    background: var(--surface-press);
    box-shadow: var(--shadow-popover);
  }
  .ctx-menu p { margin: 3px 8px 7px; color: var(--ink-faint); font-size: 0.7rem; }
  .ctx-item {
    min-height: 36px;
    padding: 7px 9px;
    border: 0;
    border-radius: var(--radius-md);
    background: transparent;
    color: var(--ink);
    font: inherit;
    font-size: 0.82rem;
    text-align: left;
    cursor: pointer;
  }
  .ctx-item:hover { background: var(--surface-soft); }
  .map-message {
    position: absolute;
    z-index: 9;
    top: 10px;
    left: 12px;
    display: flex;
    align-items: center;
    gap: 10px;
    max-width: min(620px, calc(100% - 24px));
    box-sizing: border-box;
    padding: 7px 10px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-md);
    background: var(--surface-press);
    color: var(--ink-secondary);
    font-size: 0.74rem;
    line-height: 1.45;
  }
  .map-message.degraded + .map-message { top: 52px; }
  .map-message button {
    flex: none;
    min-height: 32px;
    padding: 4px 8px;
    border: 0;
    border-radius: var(--radius-md);
    background: var(--accent-tint);
    color: var(--accent);
    font: inherit;
    cursor: pointer;
  }
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
    button { min-height: 44px; }
    .inspector-close { width: 44px; }
  }
  @media (prefers-reduced-motion: reduce) {
    *, *::before, *::after { transition-duration: 0.01ms !important; animation-duration: 0.01ms !important; }
  }
</style>
