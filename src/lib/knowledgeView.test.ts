import { describe, expect, it, vi } from "vitest";
import type { EntitySummary } from "./graph";
import type { KnowledgePath, KnowledgeFilter, KnowledgePathStep, SemanticEdge, SemanticGraphData } from "./knowledge";
import {
  DEFAULT_KNOWLEDGE_FILTER,
  GLOBAL_SEMANTIC_PRESENCE_FILTER,
  NORMAL_GRAPH_ALPHA_DECAY,
  canonicalPathEdgeId,
  createDebugFixtureReleaseOnce,
  defaultBackbone,
  debugKnowledgeRoutePolicy,
  ensureBackboneEdge,
  filterSemanticGraph,
  graphDragPosition,
  graphSimulationTickBudget,
  hasPathEndpoints,
  legacyFallbackGraph,
  nextExpandedIds,
  pathEmphasis,
  preserveGraphExplorationState,
  preserveGraphNodePositions,
  resolveGraphRefreshState,
  runGuardedPathRefresh,
  relationLabel,
  searchAdmissionIds,
  semanticRequestFailureMessage,
  shouldUseLegacyFallback,
  stableEdgeLanes,
  sanitizeDebugGraphUrl,
  viewEdges,
} from "./knowledgeView";

describe("debug fixture route isolation", () => {
  it("blocks production effects before the fixture session exists and clears unsafe inspectors", () => {
    const unsafe = new URL(
      "http://localhost/graph?debugFixture=semantic-large&e=real-entity&r=real-relation&review=1",
    );
    const initial = debugKnowledgeRoutePolicy(unsafe, true, false, false);
    expect(initial).toEqual({
      debugFixtureRequested: true,
      productionEffectsAllowed: false,
      selected: null,
      relationId: null,
      reviewOpen: false,
    });

    const production = {
      semanticGraph: vi.fn(),
      entityDetail: vi.fn(),
      relationDetail: vi.fn(),
      pendingReview: vi.fn(),
      governance: vi.fn(),
      backfill: vi.fn(),
    };
    if (initial.productionEffectsAllowed) {
      for (const effect of Object.values(production)) effect();
    }
    for (const effect of Object.values(production)) expect(effect).not.toHaveBeenCalled();

    const delayed = debugKnowledgeRoutePolicy(unsafe, true, true, false);
    expect(delayed.relationId).toBeNull();
    const cleaned = sanitizeDebugGraphUrl(unsafe);
    expect(cleaned.searchParams.get("debugFixture")).toBe("semantic-large");
    expect(cleaned.searchParams.has("e")).toBe(false);
    expect(cleaned.searchParams.has("r")).toBe(false);
    expect(cleaned.searchParams.has("review")).toBe(false);

    const fixtureClick = new URL("http://localhost/graph?debugFixture=semantic-large&r=fixture-relation");
    expect(debugKnowledgeRoutePolicy(fixtureClick, true, true, true).relationId).toBe(
      "fixture-relation",
    );
  });

  it("releases an opaque fixture session at most once", async () => {
    const release = vi.fn(async (_sessionId: string) => {});
    const releaseOnce = createDebugFixtureReleaseOnce(release);

    await Promise.all([
      releaseOnce("fixture-session"),
      releaseOnce("fixture-session"),
      releaseOnce(null),
    ]);

    expect(release).toHaveBeenCalledTimes(1);
    expect(release).toHaveBeenCalledWith("fixture-session");
  });
});

describe("graph refresh continuity", () => {
  it("keeps surviving visible and expanded entities while pruning removed IDs", () => {
    const refreshed = graph([node("kg_keep"), node("kg_new")], []);
    const result = preserveGraphExplorationState(
      refreshed,
      new Set(["kg_keep", "kg_removed"]),
      new Map([["kg_keep", 2], ["kg_removed", 1]]),
      ["kg_keep", "kg_removed"],
    );

    expect(result.visibleIds).toEqual(new Set(["kg_keep"]));
    expect(result.expansionDepth).toEqual(new Map([["kg_keep", 2]]));
  });

  it("carries coordinates only for nodes that survive a data refresh", () => {
    const previous = new Map([
      ["kg_keep", { name: "Stale name", x: 120, y: 240, vx: 1, vy: -1 }],
      ["kg_removed", { name: "Removed", x: 20, y: 40, vx: 0, vy: 0 }],
    ]);
    const result = preserveGraphNodePositions(
      [{ id: "kg_keep", name: "Keep" }, { id: "kg_new", name: "New" }],
      previous,
    );

    expect(result).toEqual([
      { id: "kg_keep", name: "Keep", x: 120, y: 240, vx: 1, vy: -1 },
      { id: "kg_new", name: "New" },
    ]);
    expect(result.some((item) => item.id === "kg_removed")).toBe(false);
  });

  it("rebases a successful refresh when no visible entity survives", () => {
    const result = resolveGraphRefreshState(
      graph([node("kg_new")], []),
      new Set(["kg_removed"]),
      new Map([["kg_removed", 2]]),
      [],
      new Set(["kg_new"]),
    );

    expect(result).toEqual({
      visibleIds: new Set(["kg_new"]),
      expansionDepth: new Map(),
      shouldResetView: true,
    });
  });

  it("rebases a failed refresh onto a non-overlapping legacy fallback", () => {
    const fallback = graph([node("legacy_new")], []);
    const result = resolveGraphRefreshState(
      fallback,
      new Set(["semantic_removed"]),
      new Map([["semantic_removed", 1]]),
      [],
      new Set(["legacy_new"]),
    );

    expect(result.visibleIds).toEqual(new Set(["legacy_new"]));
    expect(result.expansionDepth.size).toBe(0);
    expect(result.shouldResetView).toBe(true);
  });
});

const node = (id: string, kind = "project", noteCount = 1): EntitySummary => ({
  id,
  kind,
  name: id,
  aliases: [],
  is_person: kind === "person",
  note_count: noteCount,
  mention_total: noteCount,
});

const edge = (
  id: string,
  subjectId: string,
  objectId: string,
  overrides: Partial<SemanticEdge> = {},
): SemanticEdge => ({
  id,
  subject_id: subjectId,
  object_id: objectId,
  predicate_type: "responsible_for",
  predicate_label: null,
  status: "current",
  confidence: 0.9,
  origin: "model",
  evidence_count: 1,
  note_count: 1,
  valid_from: null,
  valid_to: null,
  ...overrides,
});

const graph = (
  nodes: EntitySummary[],
  semanticEdges: SemanticEdge[],
  cooccurrenceEdges: SemanticGraphData["cooccurrence_edges"] = [],
): SemanticGraphData => ({
  nodes,
  semantic_edges: semanticEdges,
  cooccurrence_edges: cooccurrenceEdges,
  degraded: false,
  message: null,
});

const filter = (overrides: Partial<KnowledgeFilter> = {}): KnowledgeFilter => ({
  ...DEFAULT_KNOWLEDGE_FILTER,
  ...overrides,
});

describe("relationLabel", () => {
  it("uses stable Chinese labels for core predicates and full custom labels", () => {
    expect(
      [
        "participates_in",
        "responsible_for",
        "belongs_to",
        "uses",
        "depends_on",
        "produces",
        "assigned_to",
        "occurs_at",
      ].map((predicate_type) => relationLabel({ predicate_type, predicate_label: null })),
    ).toEqual(["参与", "负责", "属于", "使用", "依赖", "产生", "指派给", "发生于"]);
    expect(relationLabel({ predicate_type: "custom", predicate_label: "推动" })).toBe("推动");
    expect(
      relationLabel({
        predicate_type: "custom",
        predicate_label: "推动跨团队的长期知识治理计划",
      }),
    ).toBe("推动跨团队的长期知识治理计划");
  });

  it("falls back deterministically to the predicate code without inventing wording", () => {
    expect(relationLabel({ predicate_type: "unmapped_relation", predicate_label: "误导标签" })).toBe(
      "unmapped_relation",
    );
    expect(relationLabel({ predicate_type: "custom", predicate_label: null })).toBe("custom");
    expect(relationLabel({ predicate_type: "custom", predicate_label: "   " })).toBe("custom");
  });
});

describe("filterSemanticGraph", () => {
  const nodes = [node("kg_person", "person"), node("kg_peer", "person"), node("kg_project")];
  const edges = [
    edge("r_open", "kg_person", "kg_project"),
    edge("r_people", "kg_person", "kg_peer", { predicate_type: "uses" }),
    edge("r_old", "kg_person", "kg_project", {
      status: "historical",
      valid_from: "2026-01-01T00:00:00Z",
      valid_to: "2026-01-31T00:00:00Z",
    }),
    edge("r_ends_at", "kg_person", "kg_project", {
      status: "historical",
      valid_from: null,
      valid_to: "2026-02-01T00:00:00Z",
    }),
    edge("r_starts_at", "kg_person", "kg_project", {
      valid_from: "2026-02-01T00:00:00Z",
      valid_to: null,
    }),
    edge("r_future", "kg_person", "kg_project", {
      valid_from: "2026-02-02T00:00:00Z",
      valid_to: null,
    }),
  ];
  const data = graph(nodes, edges, [
    { a: "kg_person", b: "kg_project", weight: 2 },
    { a: "kg_person", b: "missing", weight: 7 },
  ]);

  it("filters current/history, predicates, and both entity endpoints", () => {
    expect(filterSemanticGraph(data, filter()).semantic_edges.map((item) => item.id)).toEqual([
      "r_future",
      "r_open",
      "r_people",
      "r_starts_at",
    ]);

    const peopleOnly = filterSemanticGraph(
      data,
      filter({ entity_kinds: ["person"], predicate_types: ["uses"] }),
    );
    expect(peopleOnly.nodes.map((item) => item.id)).toEqual(["kg_peer", "kg_person"]);
    expect(peopleOnly.semantic_edges.map((item) => item.id)).toEqual(["r_people"]);
    expect(peopleOnly.cooccurrence_edges).toEqual([]);
  });

  it("uses inclusive overlap boundaries and treats missing endpoints as open", () => {
    const atBoundary = filterSemanticGraph(
      data,
      filter({
        include_history: true,
        from: "2026-02-01T00:00:00Z",
        to: "2026-02-01T00:00:00Z",
      }),
    );
    expect(atBoundary.semantic_edges.map((item) => item.id)).toEqual([
      "r_ends_at",
      "r_open",
      "r_people",
      "r_starts_at",
    ]);
  });

  it("returns fresh arrays and never mutates IPC data or filter state", () => {
    const request = filter({ include_history: true, include_cooccurrence: true });
    const beforeData = structuredClone(data);
    const beforeFilter = structuredClone(request);
    const result = filterSemanticGraph(data, request);

    expect(data).toEqual(beforeData);
    expect(request).toEqual(beforeFilter);
    expect(result.nodes).not.toBe(data.nodes);
    expect(result.semantic_edges).not.toBe(data.semantic_edges);
    expect(result.cooccurrence_edges).not.toBe(data.cooccurrence_edges);
  });
});

describe("viewEdges", () => {
  const data = graph(
    [node("kg_a"), node("kg_b"), node("kg_c")],
    [
      edge("r_z", "kg_b", "kg_c", { predicate_type: "custom", predicate_label: "完整关系标签" }),
      edge("r_a", "kg_a", "kg_b", { confidence: 0.8 }),
    ],
    [
      { a: "kg_b", b: "kg_c", weight: 1 },
      { a: "kg_a", b: "kg_c", weight: 3 },
    ],
  );

  it("keeps semantic edges in the foreground and co-occurrence opt-in", () => {
    const semanticOnly = viewEdges(data, filter({ include_cooccurrence: false }));
    expect(semanticOnly.every((item) => item.layer === "semantic")).toBe(true);
    expect(semanticOnly.map((item) => item.id)).toEqual(["r_a", "r_z"]);

    const combined = viewEdges(data, filter({ include_cooccurrence: true }));
    expect(combined.map((item) => item.layer)).toEqual([
      "semantic",
      "semantic",
      "cooccurrence",
      "cooccurrence",
    ]);
    expect(combined[1]?.label).toBe("完整关系标签");
  });

  it("falls back to co-occurrence when the filtered semantic graph is empty", () => {
    const fallback = viewEdges(
      graph(data.nodes, [], data.cooccurrence_edges),
      filter({ include_cooccurrence: false }),
    );
    expect(fallback).toHaveLength(2);
    expect(fallback.every((item) => item.layer === "cooccurrence")).toBe(true);
    expect(viewEdges(data, filter({ predicate_types: ["does_not_exist"] }))).toEqual([]);
  });

  it("preserves empty-semantic fallback under history, date, and predicate filters", () => {
    const emptySemantic = graph(
      [node("kg_a", "project"), node("kg_b", "term")],
      [],
      [{ a: "kg_a", b: "kg_b", weight: 4 }],
    );
    const semanticFilters: Partial<KnowledgeFilter>[] = [
      { include_history: true },
      { predicate_types: ["uses"] },
      { from: "2026-01-01T00:00:00Z", to: "2026-12-31T23:59:59Z" },
      {
        include_history: true,
        predicate_types: ["uses"],
        from: "2026-01-01T00:00:00Z",
      },
    ];

    for (const overrides of semanticFilters) {
      expect(viewEdges(emptySemantic, filter(overrides))).toEqual([
        {
          id: "co:kg_a:kg_b",
          a: "kg_a",
          b: "kg_b",
          weight: 4,
          layer: "cooccurrence",
          label: "共同出现（4 篇）",
          directed: false,
          confidence: null,
          status: null,
        },
      ]);
    }
    expect(
      viewEdges(emptySemantic, filter({ include_history: true, entity_kinds: ["project"] })),
    ).toEqual([]);
  });

  it("is independent of input ordering and never shortens a label", () => {
    const reversed = graph(
      [...data.nodes].reverse(),
      [...data.semantic_edges].reverse(),
      [...data.cooccurrence_edges].reverse(),
    );
    expect(viewEdges(reversed, filter({ include_cooccurrence: true }))).toEqual(
      viewEdges(data, filter({ include_cooccurrence: true })),
    );
    expect(
      viewEdges(data, filter({ include_cooccurrence: true })).every(
        (item) => !item.label.includes("…") && !item.label.endsWith("..."),
      ),
    ).toBe(true);
  });
});

describe("nextExpandedIds", () => {
  const edges = [
    edge("r_ac", "kg_a", "kg_c", { confidence: 0.9, note_count: 1 }),
    edge("r_ab", "kg_a", "kg_b", { confidence: 0.9, note_count: 2 }),
    edge("r_ad", "kg_a", "kg_d", { confidence: 0.8, note_count: 9 }),
    edge("r_be", "kg_b", "kg_e", { confidence: 1, note_count: 1 }),
    edge("r_ca", "kg_c", "kg_a", { confidence: 1, note_count: 8 }),
    edge("r_cf", "kg_c", "kg_f", { confidence: 0.7, note_count: 1 }),
    edge("r_missing", "", "kg_ignored", { confidence: 1, note_count: 99 }),
  ];

  it("does deterministic one-hop expansion with an independent per-node cap", () => {
    const seed = new Set(["kg_a"]);
    expect(nextExpandedIds(seed, edges, 1, 2)).toEqual(new Set(["kg_a", "kg_b", "kg_c"]));
    expect(seed).toEqual(new Set(["kg_a"]));
    expect(nextExpandedIds(seed, [...edges].reverse(), 1, 2)).toEqual(
      nextExpandedIds(seed, edges, 1, 2),
    );
  });

  it("handles multiple hops, cycles, missing endpoints, and zero hops", () => {
    expect(nextExpandedIds(new Set(["kg_a"]), edges, 2, 2)).toEqual(
      new Set(["kg_a", "kg_b", "kg_c", "kg_e", "kg_f"]),
    );
    expect(nextExpandedIds(new Set(["missing_seed"]), edges, 2)).toEqual(
      new Set(["missing_seed"]),
    );
    expect(nextExpandedIds(new Set(["kg_a"]), edges, 0)).toEqual(new Set(["kg_a"]));
  });

  it("defaults to eight neighbors per expanded node", () => {
    const manyEdges = Array.from({ length: 10 }, (_, index) =>
      edge(`r_${String(index).padStart(2, "0")}`, "root", `neighbor_${index}`),
    );
    const result = nextExpandedIds(new Set(["root"]), manyEdges, 1);
    expect(result.size).toBe(9);
    expect(result.has("neighbor_8")).toBe(false);
    expect(result.has("neighbor_9")).toBe(false);
  });
});

describe("defaultBackbone", () => {
  const data = graph(
    [node("a", "project", 9), node("b", "project", 8), node("c"), node("x", "term", 7), node("y", "term", 6), node("isolated", "term", 99)],
    [
      edge("r_ab", "a", "b", { confidence: 0.95, note_count: 8 }),
      edge("r_bc", "b", "c", { confidence: 0.8, note_count: 2 }),
      edge("r_xy", "x", "y", { confidence: 0.9, note_count: 6 }),
    ],
  );

  it("is bounded, deterministic, and represents disconnected semantic components", () => {
    const original = structuredClone(data);
    const backbone = defaultBackbone(data, 4, 2);
    expect(backbone.size).toBeLessThanOrEqual(4);
    expect([...backbone].some((id) => id === "a" || id === "b" || id === "c")).toBe(true);
    expect([...backbone].some((id) => id === "x" || id === "y")).toBe(true);
    expect(defaultBackbone(graph([...data.nodes].reverse(), [...data.semantic_edges].reverse()), 4, 2)).toEqual(
      backbone,
    );
    expect(data).toEqual(original);
  });

  it("collapses an expanded set back to the same default backbone", () => {
    const baseline = defaultBackbone(data, 3, 2);
    const expanded = nextExpandedIds(baseline, data.semantic_edges, 2);
    expect(expanded.size).toBeGreaterThanOrEqual(baseline.size);
    expect(defaultBackbone(data, 3, 2)).toEqual(baseline);
  });

  it("never fills past a capped bridge with a disconnected high-degree node", () => {
    const nodes = [node("A", "project", 20), node("B", "project", 10), node("C"), node("D")];
    const edges = [
      edge("r_ab", "A", "B", { confidence: 1, note_count: 20 }),
      edge("r_ac", "A", "C", { confidence: 0.1, note_count: 1 }),
      edge("r_cd", "C", "D", { confidence: 0.1, note_count: 1 }),
    ];
    for (let index = 0; index < 8; index += 1) {
      const leaf = `A_leaf_${index}`;
      nodes.push(node(leaf));
      edges.push(edge(`r_a_leaf_${index}`, "A", leaf, { confidence: 0.5, note_count: 2 }));
    }
    for (let index = 0; index < 6; index += 1) {
      const leaf = `D_leaf_${index}`;
      nodes.push(node(leaf));
      edges.push(edge(`r_d_leaf_${index}`, "D", leaf, { confidence: 0.9, note_count: 9 }));
    }

    const backbone = defaultBackbone(graph(nodes, edges), 3, 1);
    expect(backbone).toEqual(new Set(["A", "B"]));

    const adjacency = new Map<string, Set<string>>();
    for (const item of edges) {
      if (!backbone.has(item.subject_id) || !backbone.has(item.object_id)) continue;
      adjacency.set(
        item.subject_id,
        new Set([...(adjacency.get(item.subject_id) ?? []), item.object_id]),
      );
      adjacency.set(
        item.object_id,
        new Set([...(adjacency.get(item.object_id) ?? []), item.subject_id]),
      );
    }
    const reachable = new Set(["A"]);
    const pending = ["A"];
    while (pending.length > 0) {
      for (const neighbor of adjacency.get(pending.shift()!) ?? []) {
        if (!reachable.has(neighbor)) {
          reachable.add(neighbor);
          pending.push(neighbor);
        }
      }
    }
    expect([...backbone].every((id) => reachable.has(id))).toBe(true);
  });
});

describe("pathEmphasis", () => {
  it("returns exactly the path node and edge IDs", () => {
    const path: KnowledgePath = {
      entity_ids: ["kg_a", "kg_b", "kg_c"],
      steps: [
        {
          id: "r_ab",
          from_id: "kg_a",
          to_id: "kg_b",
          subject_id: "kg_a",
          object_id: "kg_b",
          predicate_type: "uses",
          predicate_label: null,
          direction: "forward",
          origin: "confirmed",
          confidence: 0.9,
          evidence_count: 1,
          note_count: 1,
        },
        {
          id: "r_bc",
          from_id: "kg_b",
          to_id: "kg_c",
          subject_id: "kg_c",
          object_id: "kg_b",
          predicate_type: "custom",
          predicate_label: "协作",
          direction: "reverse",
          origin: "model",
          confidence: 0.8,
          evidence_count: 2,
          note_count: 1,
        },
      ],
      total_cost: 2.4,
    };
    expect(pathEmphasis(path)).toEqual({
      nodeIds: new Set(["kg_a", "kg_b", "kg_c"]),
      edgeIds: new Set(["r_ab", "r_bc"]),
    });
    expect(pathEmphasis(null)).toEqual({ nodeIds: new Set(), edgeIds: new Set() });
  });

  it("keeps a newly selected path when a deferred backfill refresh finishes", async () => {
    let resolveRefresh!: () => void;
    const refresh = new Promise<void>((resolve) => {
      resolveRefresh = resolve;
    });
    let dialogOpen = true;
    let current = { generation: 11, start: "kg_a", end: "kg_b" };
    let active = "old-path";
    let semanticRefreshes = 0;
    const rerun = async () => {
      active = "stale-rerun";
    };
    const rerunSpy = { call: rerun };
    const task = runGuardedPathRefresh(
      { ...current },
      () => current,
      [async () => {
        await refresh;
        semanticRefreshes += 1;
      }],
      rerunSpy.call,
    );

    dialogOpen = false;
    current = { generation: 12, start: "kg_new_a", end: "kg_new_b" };
    active = "new-path";
    resolveRefresh();

    expect(await task).toBe(false);
    expect(semanticRefreshes).toBe(1);
    expect(dialogOpen).toBe(false);
    expect(active).toBe("new-path");
  });
});

describe("exploratory graph UI source contract", () => {
  const sources = import.meta.glob(
    ["./ForceGraph.svelte", "./KnowledgeGraphToolbar.svelte", "./KnowledgePathPanel.svelte", "./RelationDrawer.svelte", "./knowledge.ts", "./knowledgeView.ts", "../routes/graph/+page.svelte", "./Sidebar.svelte", "../../DESIGN.md"],
    { eager: true, query: "?raw", import: "default" },
  ) as Record<string, string>;
  const source = (name: string) => {
    const value = sources[name];
    if (value === undefined) throw new Error(`Missing source fixture: ${name}`);
    return value;
  };

  it("renders semantic foreground edges with stable directed paths and whole labels", () => {
    const forceGraph = source("./ForceGraph.svelte");
    expect(forceGraph).toContain('marker-end="url(#semantic-arrow)"');
    expect(forceGraph).toContain("<textPath");
    expect(forceGraph).toContain("edgePathId(");
    expect(forceGraph).toContain("character.codePointAt(0)!.toString(16)");
    expect(forceGraph).toContain("edge.x1 === edge.x2 && edge.y1 <= edge.y2");
    expect(forceGraph).toContain("edgeLabelVisible(");
    expect(forceGraph).toContain("visibleSemanticCount <= 30");
    expect(forceGraph).toContain("viewZoom >= 1.35");
    expect(forceGraph).toContain('class:cooccurrence={l.layer === "cooccurrence"}');
    expect(forceGraph).not.toContain("text-overflow: ellipsis");
    expect(forceGraph).not.toContain("line-clamp");
  });

  it("keeps complete centered wrapped node names on the vertex", () => {
    const forceGraph = source("./ForceGraph.svelte");
    expect(forceGraph).toContain('class="node-label"');
    expect(forceGraph).toContain('text-anchor: middle');
    expect(forceGraph).toContain("wrapLabel(name");
    expect(forceGraph).toContain("n.labelLines as line");
    expect(forceGraph).not.toMatch(/slice\([^\n]*name|substring\([^\n]*name/);
    expect(forceGraph).not.toContain('class="node-label-box"');
  });

  it("preserves legacy callers and exposes focus/edge/reduced-motion props", () => {
    const forceGraph = source("./ForceGraph.svelte");
    expect(forceGraph).toContain("RenderEdge[] | EdgeRow[]");
    expect(forceGraph).toContain("onEdgePick?:");
    expect(forceGraph).toContain("focusedNodeIds?: Set<string>");
    expect(forceGraph).toContain("focusedEdgeIds?: Set<string>");
    expect(forceGraph).toContain("reducedMotion?: boolean");
    expect(forceGraph).toContain("normalizeEdges(");
  });

  it("retains unrelated path context at fifteen percent instead of deleting it", () => {
    const forceGraph = source("./ForceGraph.svelte");
    expect(forceGraph).toContain("focusedEdgeIds.size > 0");
    expect(forceGraph).toContain("focusedNodeIds.size > 0");
    expect(forceGraph).toMatch(/return\s+0\.15/);
  });

  it("offers complete filters, deterministic reveal controls, and honest fallback", () => {
    const toolbar = source("./KnowledgeGraphToolbar.svelte");
    const route = source("../routes/graph/+page.svelte");
    for (const label of ["实体类型", "关系类型", "开始日期", "结束日期", "包含历史关系", "显示共现弱连接", "收起到主干", "显示全部"]) {
      expect(toolbar).toContain(label);
    }
    expect(route).toContain("semanticGraph(");
    expect(route).toContain("defaultBackbone(");
    expect(route).toContain("nextExpandedIds(");
    expect(route).toContain("visibleIds = new Set([...visibleIds, ...matches])");
    expect(route).toContain("尚未补建语义关系");
    expect(route).toContain("补建语义关系");
    expect(route).toContain('class="canvas-shell"');
    expect(route).toContain("<EntityGovernance");
  });

  it("guards two-point paths against stale responses and exposes accessible evidence steps", () => {
    const route = source("../routes/graph/+page.svelte");
    const panel = source("./KnowledgePathPanel.svelte");
    expect(route).toContain("pathGeneration");
    expect(route).toContain("generation !== pathGeneration");
    expect(route).toContain("knowledgePath(");
    expect(route).toContain("include_cooccurrence: includeWeak");
    expect(route).toContain('class="accessible-network"');
    for (const label of ["设为路径起点", "包含共现弱连接", "查看关系证据", "未找到可连接两点的路径"]) {
      expect(route + panel).toContain(label);
    }
    expect(panel).not.toContain("…");
    expect(panel).not.toMatch(/\.\.\.(?=["'`<])/);
  });

  it("clears stale path emphasis during backfill refresh and reruns only valid endpoints", () => {
    const route = source("../routes/graph/+page.svelte");
    const refreshed = graph([node("kg_a"), node("kg_b")], [edge("rel_new", "kg_a", "kg_b")]);

    expect(hasPathEndpoints(refreshed, "kg_a", "kg_b")).toBe(true);
    expect(hasPathEndpoints(refreshed, "kg_a", "kg_removed")).toBe(false);
    expect(route).toMatch(/async function refreshAfterBackfill\(\)[\s\S]{0,260}activePath = null/);
    expect(route).toContain("runGuardedPathRefresh(");
    expect(route).toContain("hasPathEndpoints(semantic, snapshot.start, snapshot.end)");
    expect(route).toContain("snapshot.generation");
    expect(route).toContain("expectedGeneration ?? ++pathGeneration");
    expect(route).toContain("关系补建后路径端点已变化，原路径已清除。请重新选择两点。");
  });

  it("preserves graph exploration, positions, and camera only for data refreshes", () => {
    const route = source("../routes/graph/+page.svelte");
    const forceGraph = source("./ForceGraph.svelte");
    expect(route).toContain('type SemanticLoadMode = "reset-view" | "preserve-view"');
    expect(route).toContain('loadSemantic(effectiveGraphFilter, "preserve-view")');
    expect(route).toContain("resolveGraphRefreshState(");
    expect(route).toContain("resetKey={graphViewResetKey}");
    expect(forceGraph).toContain("resetKey?: string | number");
    expect(forceGraph).toContain("preserveGraphNodePositions(");
    expect(forceGraph).toContain("if (shouldReset) resetViewport()");
    expect(forceGraph).toContain("const previousPositions = shouldReset");
    expect(forceGraph).not.toMatch(/function rebuild\([^)]*\)\s*\{[\s\S]{0,180}viewZoom = 1/);
  });

  it("installs the same-generation fallback graph before preserve-view semantic loads", () => {
    const route = source("../routes/graph/+page.svelte");
    const governanceRefresh = route.slice(
      route.indexOf("async function refreshKnowledge()"),
      route.indexOf("async function refreshAfterBackfill()"),
    );
    expect(governanceRefresh.indexOf("graph = nextGraph")).toBeGreaterThan(-1);
    expect(governanceRefresh.indexOf('loadSemantic(effectiveGraphFilter, "preserve-view")')).toBeGreaterThan(
      governanceRefresh.indexOf("graph = nextGraph"),
    );

    const backfillRefresh = route.slice(
      route.indexOf("async function refreshAfterBackfill()"),
      route.indexOf("function updateQuery("),
    );
    expect(backfillRefresh.indexOf("await loadGraph()")).toBeGreaterThan(-1);
    expect(backfillRefresh.indexOf('loadSemantic(effectiveGraphFilter, "preserve-view")')).toBeGreaterThan(
      backfillRefresh.indexOf("await loadGraph()"),
    );
  });

  it("documents the shipped semantic zoom layers without promising detail overlays", () => {
    const design = source("../../DESIGN.md");
    expect(design).toContain("缩放时逐级显示更多完整的顶点名称和关系标签");
    expect(design).toContain("别名继续用于搜索");
    expect(design).toContain("证据与治理状态通过详情面板查看");
    expect(design).not.toContain("逐步显露别名、关系、证据数量和治理状态");
  });

  it("keeps the large graph and relation evidence behind one opaque debug session", () => {
    const route = source("../routes/graph/+page.svelte");
    const knowledge = source("./knowledge.ts");
    const drawer = source("./RelationDrawer.svelte");
    expect(route).toContain("const debugFixtureRequested = $derived(");
    expect(route).toContain('import.meta.env.DEV && $page.url.searchParams.get("debugFixture") === "semantic-large"');
    expect(route).toContain("sanitizeDebugGraphUrl");
    expect(route).toContain("const fixture = await semanticGraphDebugFixture()");
    expect(route).toContain("debugFixtureSession = fixture.session_id");
    expect(route).toContain("semanticGraphDebugRelationDetail");
    expect(route).toContain('import { onDestroy, onMount } from "svelte"');
    expect(route).toContain("const releaseDebugFixtureOnce = createDebugFixtureReleaseOnce(");
    expect(route).toContain("onDestroy(() => {");
    expect(route).toContain("debugFixtureDisposed = true");
    expect(route).toContain("debugFixtureSession = null");
    expect(route).toContain("void releaseDebugFixtureOnce(session)");
    expect(route).toContain("if (debugFixtureDisposed) {");
    expect(route).toContain("await releaseDebugFixtureOnce(fixture.session_id)");
    expect(route).toContain("debugFixtureSession && relationId");
    expect(route).toContain("relationLoader={loadDebugRelationDetail}");
    expect(route).toContain("readOnly={true}");
    expect(route).toContain("!debugFixtureRequested && reviewOpen");
    expect(route).toContain("!debugFixtureRequested && selected");
    expect(route).toContain("{#if !debugFixtureRequested}\n  <RelationBackfillDialog");
    expect(route).toContain("仅创建并读取临时夹具，不会读取或修改真实资料库");
    expect(knowledge).toContain('invoke<SemanticGraphDebugFixture>("semantic_graph_debug_fixture")');
    expect(knowledge).toContain('invoke<RelationDetail | null>("semantic_graph_debug_relation_detail"');
    expect(knowledge).toContain('invoke<void>("semantic_graph_debug_release", { sessionId })');
    expect(knowledge).not.toMatch(/semanticGraphDebugFixture\s*=\s*\([^)]*(root|path)/);
    expect(knowledge).not.toMatch(/semanticGraphDebugRelease\s*=\s*\([^)]*(root|path)/);
    expect(knowledge).not.toContain("fixture_root");
    expect(drawer).toContain("relationLoader");
    expect(drawer).toContain("resolveEntityName");
    expect(drawer).toContain("readOnly");
    expect(drawer).toContain("await relationLoader(id)");
  });

  it("canonicalizes Rust-style cooccurrence path step IDs to rendered weak-edge IDs", () => {
    const route = source("../routes/graph/+page.svelte");
    const policy = source("./knowledgeView.ts");
    const rustStyleStep = {
      id: "co_91d3e0e29ac24fb6",
      subject_id: "kg_zeta",
      object_id: "kg_alpha",
      origin: "cooccurrence",
    };
    const [a, b] = [rustStyleStep.subject_id, rustStyleStep.object_id].sort();
    expect(`co:${a}:${b}`).toBe("co:kg_alpha:kg_zeta");
    expect(rustStyleStep.id).not.toBe(`co:${a}:${b}`);
    expect(canonicalPathEdgeId(rustStyleStep as KnowledgePathStep)).toBe("co:kg_alpha:kg_zeta");
    expect(
      pathEmphasis({ entity_ids: [a, b], steps: [rustStyleStep as KnowledgePathStep], total_cost: 1 }),
    ).toEqual({ nodeIds: new Set([a, b]), edgeIds: new Set(["co:kg_alpha:kg_zeta"]) });
    expect(policy).toContain("function canonicalPathEdgeId(");
    expect(policy).toContain('step.origin !== "cooccurrence"');
    expect(policy).toContain('return `co:${a}:${b}`');
    expect(route).toContain("pathEmphasis(activePath)");
  });

  it("admits and focuses an off-backbone search match even when it is isolated", () => {
    const forceGraph = source("./ForceGraph.svelte");
    const route = source("../routes/graph/+page.svelte");
    const policy = source("./knowledgeView.ts");
    expect(forceGraph).toContain("searchNodeIds");
    expect(forceGraph).toContain("deg.has(node.id) || searchNodeIds.has(node.id)");
    expect(forceGraph).toContain("void query");
    expect(policy).toContain("function searchAdmissionIds(");
    expect(route).toContain("visibleIds = new Set([...visibleIds, ...matches])");
    expect(route).not.toMatch(/graphFilter\.query[\s\S]{0,300}renderedNodes\s*=/);
    expect(
      searchAdmissionIds(
        [node("hidden"), node("neighbor"), node("isolated", "term"), node("unrelated")],
        [edge("r_hidden", "hidden", "neighbor")],
        "isolated",
      ),
    ).toEqual(new Set(["isolated"]));
    expect(
      searchAdmissionIds(
        [node("hidden"), node("neighbor"), node("isolated"), node("unrelated")],
        [edge("r_hidden", "hidden", "neighbor")],
        "hidden",
      ),
    ).toEqual(new Set(["hidden", "neighbor"]));
  });

  it("uses legacy fallback only when the global semantic index is truly absent", () => {
    const route = source("../routes/graph/+page.svelte");
    const policy = source("./knowledgeView.ts");
    expect(route).toContain('globalSemanticPresence === "absent"');
    expect(route).toContain("probeGlobalSemanticPresence(");
    expect(route).toContain('globalSemanticPresence === "present"');
    expect(route).toContain("当前筛选下没有语义关系");
    expect(policy).toContain("function shouldUseLegacyFallback(");
    expect(route).not.toMatch(/semantic\.semantic_edges\.length === 0[\s\S]{0,180}cooccurrence_edges: graph\.edges/);
    const filteredEmpty = graph([node("a"), node("b")], []);
    expect(shouldUseLegacyFallback("present", filteredEmpty)).toBe(false);
    expect(shouldUseLegacyFallback("unknown", filteredEmpty)).toBe(false);
    expect(shouldUseLegacyFallback("absent", filteredEmpty)).toBe(true);
  });

  it("detects a historical-only backend graph without changing the current-only view", () => {
    const route = source("../routes/graph/+page.svelte");
    const historicalOnly = graph(
      [node("kg_person", "person"), node("kg_project")],
      [
        edge("r_historical", "kg_person", "kg_project", {
          status: "historical",
          valid_from: "2025-01-01T00:00:00Z",
          valid_to: "2025-12-31T23:59:59Z",
        }),
      ],
    );

    const currentOnly = filterSemanticGraph(historicalOnly, DEFAULT_KNOWLEDGE_FILTER);
    const globalProbe = filterSemanticGraph(historicalOnly, GLOBAL_SEMANTIC_PRESENCE_FILTER);

    expect(DEFAULT_KNOWLEDGE_FILTER.include_history).toBe(false);
    expect(currentOnly.semantic_edges).toEqual([]);
    expect(GLOBAL_SEMANTIC_PRESENCE_FILTER).toEqual({
      ...DEFAULT_KNOWLEDGE_FILTER,
      include_history: true,
    });
    expect(globalProbe.semantic_edges.map((item) => item.id)).toEqual(["r_historical"]);
    expect(shouldUseLegacyFallback("present", currentOnly)).toBe(false);
    expect(route).toContain("semanticGraph(GLOBAL_SEMANTIC_PRESENCE_FILTER)");
    expect(route).toContain("loadSemantic(knowledgeFilter)");
  });

  it("gives parallel relations stable signed Bezier lanes and independent label paths", () => {
    const forceGraph = source("./ForceGraph.svelte");
    const policy = source("./knowledgeView.ts");
    expect(forceGraph).toContain("assignStableLanes(");
    expect(policy).toContain("left.id.localeCompare(right.id)");
    expect(policy).toContain("index - (group.length - 1) / 2");
    expect(forceGraph).toContain(" Q ");
    expect(forceGraph).toContain("edge.lane * EDGE_LANE_GAP");
    expect(forceGraph).toContain("edgePathId(l.id, true)");
    expect(policy).toContain("function stableEdgeLanes(");
    const parallel = [
      { id: "rel_c", a: "z", b: "a" },
      { id: "rel_a", a: "a", b: "z" },
      { id: "rel_b", a: "a", b: "z" },
    ];
    expect(stableEdgeLanes(parallel)).toEqual(new Map([["rel_a", -1], ["rel_b", 0], ["rel_c", 1]]));
    expect(stableEdgeLanes([...parallel].reverse())).toEqual(stableEdgeLanes(parallel));
  });

  it("keeps reduced-motion settled through resize and drag, with honest SVG semantics", () => {
    const forceGraph = source("./ForceGraph.svelte");
    expect(forceGraph).toContain("effectiveReducedMotion");
    expect(forceGraph).toContain("settleSimulation(");
    expect(forceGraph).toContain("if (effectiveReducedMotion)");
    expect(forceGraph).toContain('class:reduced={effectiveReducedMotion}');
    expect(forceGraph).toContain("focusedEdge === edge.id");
    expect(forceGraph).toContain("{#if onEdgePick}");
    expect(forceGraph).toContain('class="edge-hover-target"');
    expect(forceGraph).not.toContain("tabindex={onEdgePick ? 0 : -1}");
  });

  it("ensures a root-heavy default view still contains at least one useful edge", () => {
    const route = source("../routes/graph/+page.svelte");
    const policy = source("./knowledgeView.ts");
    expect(route).toContain("ensureBackboneEdge(");
    expect(route).toContain("BACKBONE_NODE_LIMIT");
    expect(policy).toContain("selected.has(edge.subject_id) && selected.has(edge.object_id)");
    const roots = Array.from({ length: 81 }, (_, index) => node(`root_${index}`));
    const rootEdges = Array.from({ length: 40 }, (_, index) =>
      edge(`rel_${index}`, `root_${index * 2}`, `root_${index * 2 + 1}`, {
        confidence: 1 - index / 100,
      }),
    );
    const data = graph(roots, rootEdges);
    const selected = defaultBackbone(data, 20, 1);
    expect([...selected].some((id) => rootEdges.some((item) => item.subject_id === id && selected.has(item.object_id)))).toBe(false);
    const ensured = ensureBackboneEdge(selected, data, 20);
    expect(ensured.size).toBeLessThanOrEqual(20);
    expect(rootEdges.some((item) => ensured.has(item.subject_id) && ensured.has(item.object_id))).toBe(true);
  });

  it("preserves a usable graph canvas at 800, 500, and 390 pixel viewport widths", () => {
    const sidebar = source("./Sidebar.svelte");
    const toolbar = source("./KnowledgeGraphToolbar.svelte");
    expect(sidebar).toContain('class:graph-mode={tab === "graph"}');
    expect(sidebar).toContain('class="graph-drawer-toggle"');
    expect(sidebar).toContain("@media (max-width: 700px)");
    expect(sidebar).toMatch(/\.sidebar\.graph-mode\s*\{[^}]*width:\s*44px/s);
    expect(sidebar).toMatch(/\.sidebar\s*\{[^}]*width:\s*300px/s);
    expect(sidebar).toContain(".panel.drawer-open");
    expect(toolbar).toMatch(/\.map-toolbar\s*\{[^}]*overflow-x:\s*auto/s);
    expect(toolbar).toMatch(/\.filter-run\s*\{[^}]*flex-wrap:\s*nowrap/s);
    expect(toolbar).toContain("function positionMenu(");
    expect(toolbar).toMatch(/fieldset, \.date-fields\s*\{[^}]*position:\s*fixed/s);
    expect(toolbar).not.toMatch(/@media \(max-width: 980px\)[\s\S]*flex-direction:\s*column/);

    const sidebarWidth = (viewport: number) => (viewport <= 700 ? 44 : 300);
    expect([800, 500, 390].map((viewport) => [viewport, sidebarWidth(viewport), viewport - sidebarWidth(viewport)])).toEqual([
      [800, 300, 500],
      [500, 44, 456],
      [390, 44, 346],
    ]);
  });

  it("gives every node a transparent coarse-pointer target of at least 44 pixels", () => {
    const forceGraph = source("./ForceGraph.svelte");
    expect(forceGraph).toContain('class="node-hit-target"');
    expect(forceGraph).toContain("interactionScale = Math.max(0.001, fit.scale * viewZoom)");
    expect(forceGraph).toContain("r={Math.max(n.r, 22 / interactionScale)}");
    expect(forceGraph).toMatch(/\.node-hit-target\s*\{[^}]*fill:\s*transparent/s);
    expect(forceGraph).toMatch(/\.node-hit-target\s*\{[^}]*pointer-events:\s*all/s);
    expect(forceGraph).not.toContain('class="node-hit-box"');
  });

  it("moves reduced-motion and giant-graph drags directly while suppressing clicks only after travel", () => {
    const forceGraph = source("./ForceGraph.svelte");
    const policy = source("./knowledgeView.ts");
    expect(policy).toContain("function graphDragPosition(");
    expect(forceGraph).toContain("effectiveReducedMotion || skipAnimation");
    expect(forceGraph).toContain("graphDragPosition(");
    expect(forceGraph).toContain("Object.assign(n, position)");
    expect(forceGraph).toContain("DRAG_MOVE_THRESHOLD");
    expect(forceGraph).toMatch(/Math\.hypot\([\s\S]{0,180}< DRAG_MOVE_THRESHOLD\s*\)\s*return/);
    expect(forceGraph).toMatch(/< DRAG_MOVE_THRESHOLD\s*\)\s*return;[\s\S]{0,100}moved = true/);
    expect(graphDragPosition(120, 240, true)).toEqual({
      x: 120,
      y: 240,
      fx: 120,
      fy: 240,
    });
    expect(graphDragPosition(120, 240, false)).toEqual({ fx: 120, fy: 240 });
  });

  it("bounds normal graph motion to a short settling window without continuous drag reheating", () => {
    const forceGraph = source("./ForceGraph.svelte");
    const policy = source("./knowledgeView.ts");
    expect(policy).toContain("NORMAL_GRAPH_ALPHA_DECAY = 0.23");
    expect(policy).toContain("function graphSimulationTickBudget(");
    expect(forceGraph).toContain(".alphaDecay(NORMAL_GRAPH_ALPHA_DECAY)");
    expect(forceGraph).toContain('.on("end"');
    expect(forceGraph).not.toContain("alphaTarget(0.3)");
    expect(forceGraph).toContain("sim?.alpha(0.3).restart()");
    expect(forceGraph).toContain("refreshSnap();");
    expect(NORMAL_GRAPH_ALPHA_DECAY).toBe(0.23);
    const tickBudget = graphSimulationTickBudget();
    expect(tickBudget).toBeGreaterThanOrEqual(25);
    expect(tickBudget).toBeLessThanOrEqual(30);
  });

  it("renders legacy graph data on semantic request failure without false backfill or raw errors", () => {
    const route = source("../routes/graph/+page.svelte");
    const policy = source("./knowledgeView.ts");
    expect(route).toContain("semanticRequestFailed");
    expect(route).toContain("legacyFallbackGraph(");
    expect(route).toContain("visibleIds = showingAll");
    expect(route).toContain(">重新读取</button>");
    expect(policy).toContain("requestFailed ||");
    expect(route).toMatch(/semanticFallback[\s\S]{0,240}!semanticRequestFailed/);
    expect(route).toMatch(/filteredSemanticEmpty[\s\S]{0,220}!semanticRequestFailed/);
    expect(policy).toContain("function semanticRequestFailureMessage(");
    expect(route).toContain("semanticRequestFailureMessage(");
    expect(policy).toContain("语义关系暂时无法读取，已显示可用的共现关系。请稍后重试。");
    expect(policy).toContain("语义关系暂时无法读取，当前没有可用的备用关系图。请稍后重试。");
    expect(semanticRequestFailureMessage(true)).toBe(
      "语义关系暂时无法读取，已显示可用的共现关系。请稍后重试。",
    );
    expect(semanticRequestFailureMessage(false)).toBe(
      "语义关系暂时无法读取，当前没有可用的备用关系图。请稍后重试。",
    );
    expect(route).not.toMatch(/semanticError\s*=\s*`[^`]*\$\{cause/);
    expect(route).not.toMatch(/semanticError\s*=\s*value\.degraded\s*\?\s*value\.message/);

    const semanticBeforeFailure = graph(
      [node("stale_a"), node("stale_b")],
      [edge("stale_relation", "stale_a", "stale_b")],
    );
    const legacy = {
      nodes: [node("legacy_a"), node("legacy_b")],
      edges: [{ a: "legacy_a", b: "legacy_b", weight: 3 }],
    };
    expect(shouldUseLegacyFallback("unknown", semanticBeforeFailure, true)).toBe(true);
    const failedFallback = legacyFallbackGraph(semanticBeforeFailure, legacy);
    expect(failedFallback.nodes.map((item) => item.id)).toEqual(["legacy_a", "legacy_b"]);
    expect(failedFallback.semantic_edges).toEqual([]);
    expect(viewEdges(failedFallback, DEFAULT_KNOWLEDGE_FILTER)).toMatchObject([
      { id: "co:legacy_a:legacy_b", layer: "cooccurrence", weight: 3 },
    ]);
    expect(shouldUseLegacyFallback("present", graph(legacy.nodes, []), false)).toBe(false);
  });

  it("uses full coarse targets and restrained drawer motion without changing fine-pointer density", () => {
    const sidebar = source("./Sidebar.svelte");
    const finePointerStyles = sidebar.split("@media (pointer: coarse)")[0];
    expect(sidebar).toMatch(/\.graph-drawer-toggle\s*\{[^}]*width:\s*44px[^}]*height:\s*44px/s);
    expect(sidebar).toContain("transform 240ms cubic-bezier(0.16, 1, 0.3, 1)");
    expect(sidebar).not.toContain("transform 180ms ease");
    expect(sidebar).toMatch(/@media \(pointer: coarse\)[\s\S]*\.sidebar\.graph-mode \.panel button[\s\S]*min-height:\s*44px/);
    expect(sidebar).toMatch(/@media \(pointer: coarse\)[\s\S]*\.sidebar\.graph-mode \.panel button[\s\S]*min-inline-size:\s*44px/);
    expect(sidebar).toMatch(/@media \(pointer: coarse\)[\s\S]*\.gchip[\s\S]*min-inline-size:\s*44px/);
    expect(sidebar).toMatch(/@media \(pointer: coarse\)[\s\S]*\.sidebar\.graph-mode \.panel input[\s\S]*min-height:\s*44px/);
    expect(finePointerStyles).not.toMatch(/\.graph-global\s*\{[^}]*min-height:\s*44px/s);
    expect(finePointerStyles).not.toMatch(/\.gchip\s*\{[^}]*min-inline-size:\s*44px/s);
  });
});
