import { describe, expect, it } from "vitest";
import type { EntitySummary } from "./graph";
import type { KnowledgePath, KnowledgeFilter, SemanticEdge, SemanticGraphData } from "./knowledge";
import {
  DEFAULT_KNOWLEDGE_FILTER,
  defaultBackbone,
  filterSemanticGraph,
  nextExpandedIds,
  pathEmphasis,
  relationLabel,
  viewEdges,
} from "./knowledgeView";

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
});
