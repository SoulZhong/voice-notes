<script lang="ts">
  import { onDestroy, onMount, untrack } from "svelte";
  import { goto, replaceState } from "$app/navigation";
  import { page } from "$app/stores";
  import {
    graphData,
    graphEdgeDetail,
    graphNeighbors,
    kindLabel,
    type GraphData,
    type GraphEdgeDetailItem,
    type RenderEdge,
  } from "$lib/graph";
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
    mergeCooccurrenceEdges,
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
  import GraphEdgeInspector from "$lib/GraphEdgeInspector.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { Button } from "$lib/ui/components/button";
  import * as DropdownMenu from "$lib/ui/components/dropdown-menu";

  const emptySemantic = (): SemanticGraphData => ({
    nodes: [],
    semantic_edges: [],
    cooccurrence_edges: [],
    degraded: false,
    message: null,
  });
  const BACKBONE_NODE_LIMIT = 80;

  let graph = $state<GraphData>({ nodes: [], edges: [] });
  /** 展开时按需取回的共现边(含只共享 1 篇的)。与 `graph` 分开存:刷新重拉 `graph`
   *  时不丢已展开出来的边;收起到主干时清空(#732)。 */
  let expandedEdges = $state<GraphData["edges"]>([]);
  const cooccurrenceGraph = $derived<GraphData>({
    nodes: graph.nodes,
    edges: mergeCooccurrenceEdges(graph.edges, expandedEdges),
  });
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
  let edgeInspector = $state<{
    perspective: "note" | "entity";
    edge: RenderEdge;
    leftName: string;
    rightName: string;
    items: GraphEdgeDetailItem[];
    loading: boolean;
    error: string;
  } | null>(null);
  let edgeDetailGeneration = 0;
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
  const inspectorOpen = $derived(Boolean(selected || relationId || edgeInspector));
  const detailFilter = { ...DEFAULT_KNOWLEDGE_FILTER, include_history: true };
  const effectiveGraphFilter = $derived<KnowledgeFilter>(knowledgeFilter);

  const usableSemantic = $derived.by((): SemanticGraphData => {
    if (!shouldUseLegacyFallback(globalSemanticPresence, semantic, semanticRequestFailed)) {
      return semantic;
    }
    return legacyFallbackGraph(semantic, cooccurrenceGraph);
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
    viewEdges(legacyFallbackGraph(semantic, cooccurrenceGraph), effectiveGraphFilter).length > 0,
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
      semanticError = value.degraded ? t("graph.semantic.degraded") : "";
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
      const fallback = legacyFallbackGraph(semantic, cooccurrenceGraph);
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
      if (!value) detailError = t("graph.detail.notIndexed");
    } catch (cause) {
      if (generation !== detailGeneration) return;
      detail = null;
      detailError = t("graph.detail.loadFailed", {
        e: cause instanceof Error ? cause.message : String(cause),
      });
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
      semanticError = t("graph.debug.loadFailed");
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
    const mode = graphFilter.mode;
    if (edgeInspector && edgeInspector.perspective !== mode) closeEdgeInspector();
  });

  $effect(() => {
    const query = graphFilter.query.trim().toLowerCase();
    if (!query) return;
    const matches = [...searchAdmissionIds(filteredSemantic.nodes, filteredSemantic.semantic_edges, query)];
    if (matches.some((id) => !visibleIds.has(id))) visibleIds = new Set([...visibleIds, ...matches]);
  });

  // 从侧栏搜索点中一个不在画布上的实体:放上画布并展开一次(#732)。只读 selected 与
  // 加载状态;visibleIds 在 untrack 里读,否则展开写回 visibleIds 会让本 effect 自己重跑。
  $effect(() => {
    const id = selected;
    const ready = loaded && !semanticLoading;
    if (!id || !ready || debugFixtureRequested) return;
    untrack(() => {
      if (!visibleIds.has(id)) revealFrom(id);
    });
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
      detailError = id && !nextDetail ? t("graph.detail.notIndexed") : "";
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
    if (shouldUseLegacyFallback(globalSemanticPresence, semantic, semanticRequestFailed)) {
      // 只有共现边时:邻居不在前端(默认只下发共享 ≥2 篇的边),向后端要前 8 个,
      // 跳过画布上已有的;每点一次再往下取 8 个(#732)。
      void revealCooccurrenceNeighbors(id);
      return;
    }
    const revealed = nextExpandedIds(new Set([id]), filteredSemantic.semantic_edges, depth, 8);
    visibleIds = new Set([...visibleIds, ...revealed]);
  }

  async function revealCooccurrenceNeighbors(id: string) {
    visibleIds = new Set([...visibleIds, id]);
    try {
      const edges = await graphNeighbors(id, [...visibleIds], 8);
      expandedEdges = mergeCooccurrenceEdges(expandedEdges, edges);
      visibleIds = new Set([...visibleIds, ...edges.map((edge) => (edge.a === id ? edge.b : edge.a))]);
    } catch (cause) {
      console.warn("graph neighbors request failed", cause);
    }
  }

  function pickNode(id: string, _isPerson: boolean) {
    revealFrom(id);
    if (debugFixtureRequested) return;
    closeEdgeInspector();
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
    closeEdgeInspector();
    updateQuery((params) => { params.set("r", id); });
  }

  function edgeEndpointName(edge: RenderEdge, perspective: "note" | "entity", endpoint: "a" | "b") {
    const id = edge[endpoint];
    const nodes = perspective === "note" ? noteGraphState.data.nodes : renderedNodes;
    return nodes.find((node) => node.id === id)?.name ?? entityNames.get(id) ?? id;
  }

  async function loadEdgeInspector(edge: RenderEdge, perspective: "note" | "entity") {
    const generation = ++edgeDetailGeneration;
    edgeInspector = {
      perspective,
      edge,
      leftName: edgeEndpointName(edge, perspective, "a"),
      rightName: edgeEndpointName(edge, perspective, "b"),
      items: [],
      loading: true,
      error: "",
    };
    try {
      const result = await graphEdgeDetail(edge.a, edge.b, perspective);
      if (generation !== edgeDetailGeneration || !edgeInspector) return;
      edgeInspector = { ...edgeInspector, items: result.items, loading: false };
    } catch (cause) {
      if (generation !== edgeDetailGeneration || !edgeInspector) return;
      edgeInspector = {
        ...edgeInspector,
        loading: false,
        error: t("graph.edgeDetail.loadFailed", {
          e: cause instanceof Error ? cause.message : String(cause),
        }),
      };
    }
  }

  function pickEdge(edge: RenderEdge) {
    if (edge.layer === "semantic") openRelation(edge.id);
    else void loadEdgeInspector(edge, "entity");
  }

  function pickNoteEdge(edge: RenderEdge) {
    void loadEdgeInspector(edge, "note");
  }

  function closeEdgeInspector() {
    ++edgeDetailGeneration;
    edgeInspector = null;
  }

  function retryEdgeInspector() {
    if (edgeInspector) void loadEdgeInspector(edgeInspector.edge, edgeInspector.perspective);
  }

  function pickEdgeItem(item: GraphEdgeDetailItem) {
    if (!edgeInspector) return;
    if (edgeInspector.perspective === "entity") {
      goto("/notes/" + encodeURIComponent(item.id));
      return;
    }
    closeEdgeInspector();
    graphFilter.mode = "entity";
    updateQuery((params) => {
      params.set("e", item.id);
      params.delete("r");
      params.delete("manage");
    });
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
    expandedEdges = [];
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
    // 弹层(Select/AlertDialog/Popover…)已经处理过这次 Esc:bits-ui 在 document 冒泡阶段对原事件
    // preventDefault。不看这一位的话,关一个下拉会连带关掉抽屉、丢掉没提交的编辑(Codex P2)。
    if (event.defaultPrevented) return;
    if (document.querySelector("dialog[open]")) return;
    if (relationId) closeRelation();
    else if (selected) closeEntity();
    else if (edgeInspector) closeEdgeInspector();
  }
</script>

<!-- 菜单开着时在菜单外再右键:先关掉旧菜单(捕获阶段,早于节点自己的右键处理),节点右键随后会在
     新位置重新打开。DropdownMenu 的「点外面关闭」不认右键,原遮罩是显式处理 oncontextmenu 的(Codex P2)。 -->
<svelte:window
  onkeydown={handleEscape}
  oncontextmenucapture={(e) => {
    if (ctxMenu && !(e.target as Element | null)?.closest?.('[data-slot="dropdown-menu-content"]')) closeCtxMenu();
  }}
/>

<div class="graph-main">
  {#if graphFilter.mode === "note" && !debugFixtureRequested}
    {#if noteGraphState.data.edges.length > 0 && noteGraphState.data.nodes.length >= 2}
      <div class="entity-stage" class:with-inspector={edgeInspector?.perspective === "note"}>
        <div class="map-column">
          <ForceGraph
            nodes={noteGraphState.data.nodes}
            edges={noteGraphState.data.edges}
            onPick={(id) => pickNoteNode(id)}
            onEdgePick={pickNoteEdge}
            query={graphFilter.query}
            showLegend={false}
            cooccurrenceMeaning="shared-entities"
          />
        </div>
        {#if edgeInspector?.perspective === "note"}
          <aside class="edge-inspector" aria-label={t("graph.noteInspectorAria")}>
            <GraphEdgeInspector
              perspective="note"
              leftName={edgeInspector.leftName}
              rightName={edgeInspector.rightName}
              items={edgeInspector.items}
              loading={edgeInspector.loading}
              error={edgeInspector.error}
              onClose={closeEdgeInspector}
              onRetry={retryEdgeInspector}
              onPick={pickEdgeItem}
            />
          </aside>
        {/if}
      </div>
    {:else if noteGraphState.data.nodes.length > 0}
      <div class="placeholder">
        <p class="ph-title">{t("graph.notePh.noEdgesTitle")}</p>
        <p class="ph-desc">{t("graph.notePh.noEdgesDesc")}</p>
      </div>
    {:else if noteGraphState.status === "loading" || noteGraphState.status === "idle"}
      <div class="placeholder"><p class="ph-title">{t("graph.notePh.loadingTitle")}</p><p class="ph-desc">{t("graph.notePh.loadingDesc")}</p></div>
    {:else if noteGraphState.status === "error"}
      <div class="placeholder">
        <p class="ph-title">{t("graph.notePh.errorTitle")}</p>
        <p class="ph-desc">{t("graph.notePh.errorDesc")}</p>
        <Button variant="outline" onclick={() => noteGraphState.load()}>{t("graph.notePh.reload")}</Button>
      </div>
    {:else}
      <div class="placeholder"><p class="ph-title">{t("graph.notePh.emptyTitle")}</p><p class="ph-desc">{t("graph.notePh.emptyDesc")}</p></div>
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
        <div class="canvas-shell" aria-label={t("graph.canvasAria")}>
          {#if debugFixtureRequested}
            <div class="map-message debug-fixture" role="status">
              <span>{t("graph.debug.banner")}</span>
              {#if debugFixtureSession}<small>{t("graph.debug.fixtureSize")}</small>{/if}
              {#if semanticError}<small>{semanticError}</small>{/if}
            </div>
          {/if}
          {#if !debugFixtureRequested && semanticStatusMessage}
            <div class="map-message degraded" role="status">
              <span>{semanticStatusMessage}</span>
              {#if semanticRequestFailed}
                <Button variant="secondary" size="sm" class="shrink-0" onclick={() => loadSemantic(effectiveGraphFilter)}>{t("graph.retryRead")}</Button>
              {/if}
              <Button variant="secondary" size="sm" class="shrink-0" onclick={() => (backfillOpen = true)}>{t("graph.backfill.analyze")}</Button>
            </div>
          {/if}
          {#if !debugFixtureRequested && semanticFallback}
            <div class="map-message fallback">
              <span>{t("graph.fallback.notice")}</span>
              <Button variant="secondary" size="sm" class="shrink-0" onclick={() => (backfillOpen = true)}>{t("graph.backfill.analyze")}</Button>
            </div>
          {/if}
          {#if !debugFixtureRequested && filteredSemanticEmpty}
            <div class="map-message" role="status">
              <span>{t("graph.filteredEmpty.notice")}</span>
              <Button variant="secondary" size="sm" class="shrink-0" onclick={() => applyKnowledgeFilter(DEFAULT_KNOWLEDGE_FILTER)}>{t("graph.filteredEmpty.reset")}</Button>
            </div>
          {/if}

          {#if loaded && usableSemantic.nodes.length === 0 && graph.nodes.length === 0}
            <div class="placeholder">
              <p class="ph-title">{t("graph.emptyPh.title")}</p>
              <p class="ph-desc">{t("graph.emptyPh.desc")}</p>
              <Button variant="outline" onclick={() => goto("/ai")}>{t("graph.emptyPh.goAi")}</Button>
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
              <p class="ph-title">{t("graph.noMatchPh.title")}</p>
              <p class="ph-desc">{t("graph.noMatchPh.desc")}</p>
              <Button variant="outline" onclick={() => applyKnowledgeFilter(DEFAULT_KNOWLEDGE_FILTER)}>{t("graph.noMatchPh.showAll")}</Button>
            </div>
          {:else}
            <div class="placeholder">
              <p class="ph-title">{t("graph.introPh.title")}</p>
              <p class="ph-desc">{t("graph.introPh.desc")}</p>
            </div>
          {/if}

        </div>
      </div>

      {#if inspectorOpen}
        <aside class="edge-inspector" aria-label={t("graph.governanceInspectorAria")}>
          {#if edgeInspector?.perspective === "entity"}
            <GraphEdgeInspector
              perspective="entity"
              leftName={edgeInspector.leftName}
              rightName={edgeInspector.rightName}
              items={edgeInspector.items}
              loading={edgeInspector.loading}
              error={edgeInspector.error}
              onClose={closeEdgeInspector}
              onRetry={retryEdgeInspector}
              onPick={pickEdgeItem}
            />
          {:else if debugFixtureRequested && debugFixtureSession && relationId}
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
            <Button variant="ghost" size="icon" class="sticky top-0 z-[1] float-right rounded-full bg-card text-lg text-muted-foreground" aria-label={t("graph.closeEntityAria")} onclick={closeEntity}>×</Button>
            {#if detailLoading && !detail}
              <p class="inspector-state" aria-live="polite">{t("graph.detail.loading")}</p>
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
                <p>{detailError || t("graph.detail.unavailable")}</p>
                <Button variant="outline" onclick={() => selected && loadDetail(selected)}>{t("graph.detail.retry")}</Button>
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
  <!-- 右键菜单:锚在鼠标位置的零尺寸触发点上;Esc 仍由本页 handleEscape 统一处理(只关菜单,
       不连带关详情),所以内容层关掉 bits 自带的 Esc 行为。点外面 → onOpenChange 关;右键外面见 svelte:window。 -->
  <DropdownMenu.Root open={true} onOpenChange={(open) => { if (!open) closeCtxMenu(); }}>
    <DropdownMenu.Trigger>
      {#snippet child({ props })}
        <span {...props} class="ctx-anchor" style:left={`${ctxMenu?.x ?? 0}px`} style:top={`${ctxMenu?.y ?? 0}px`}></span>
      {/snippet}
    </DropdownMenu.Trigger>
    <DropdownMenu.Content
      align="start"
      sideOffset={0}
      class="min-w-[180px]"
      escapeKeydownBehavior="ignore"
      aria-label={t("graph.ctx.aria", { name: ctxMenu.name })}
    >
      <DropdownMenu.Label class="text-xs font-normal text-muted-foreground">{ctxMenu.name}</DropdownMenu.Label>
      <DropdownMenu.Item onSelect={openContextDetail}>{t("graph.ctx.viewDetail")}</DropdownMenu.Item>
      <DropdownMenu.Item onSelect={openContextGovernance}>{t("graph.ctx.manage")}</DropdownMenu.Item>
    </DropdownMenu.Content>
  </DropdownMenu.Root>
{/if}

<style>
  .graph-main, .entity-stage { height: 100%; min-width: 0; min-height: 0; }
  .graph-main { overflow: hidden; }
  .entity-stage { display: flex; background: var(--canvas); }
  .map-column { display: flex; flex: 1 1 auto; flex-direction: column; min-width: 0; min-height: 0; }
  .canvas-shell { flex: 1 1 auto; position: relative; min-width: 0; min-height: 0; }
  .ctx-anchor { position: fixed; width: 0; height: 0; pointer-events: none; }
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
  .edge-inspector {
    flex: 0 0 clamp(340px, 34%, 440px);
    box-sizing: border-box;
    min-width: 0;
    overflow-y: auto;
    padding: 22px 22px 0;
    border-left: 1px solid var(--hairline);
    background: var(--surface);
  }
  .inspector-state { margin: 56px 0 0; color: var(--ink-secondary); font-size: 0.86rem; line-height: 1.6; }
  .placeholder { max-width: 440px; margin: 18vh auto 0; padding: 0 20px; text-align: center; }
  .ph-title { margin: 0 0 8px; color: var(--ink); font-size: 1.05rem; font-weight: 500; }
  .ph-desc { margin: 0 0 18px; color: var(--ink-secondary); font-size: 0.82rem; line-height: 1.65; }
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
  @media (prefers-reduced-motion: reduce) {
    *, *::before, *::after { transition-duration: 0.01ms !important; animation-duration: 0.01ms !important; }
  }
</style>
