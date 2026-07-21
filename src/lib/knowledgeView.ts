import type { EntitySummary, GraphData, RenderEdge } from "./graph";
import type {
  KnowledgeFilter,
  KnowledgePath,
  KnowledgePathStep,
  SemanticEdge,
  SemanticGraphData,
} from "./knowledge";

export const DEFAULT_KNOWLEDGE_FILTER: KnowledgeFilter = {
  entity_kinds: [],
  predicate_types: [],
  from: null,
  to: null,
  include_history: false,
  include_cooccurrence: false,
};

/** 全局语义存在性探测必须覆盖历史关系；用户画布仍使用自己的实际筛选。 */
export const GLOBAL_SEMANTIC_PRESENCE_FILTER: KnowledgeFilter = {
  ...DEFAULT_KNOWLEDGE_FILTER,
  entity_kinds: [],
  predicate_types: [],
  include_history: true,
};

/** D3 默认约 300 帧；0.23 在 alphaMin=0.001 时约 27 帧，约 450ms 后自动冻结。 */
export const NORMAL_GRAPH_ALPHA_DECAY = 0.23;

export interface GraphNodePosition {
  x?: number;
  y?: number;
  vx?: number;
  vy?: number;
  fx?: number | null;
  fy?: number | null;
}

/** Keep only exploration state that still points at entities in the refreshed graph. */
export function preserveGraphExplorationState(
  graph: Pick<SemanticGraphData, "nodes">,
  visibleIds: ReadonlySet<string>,
  expansionDepth: ReadonlyMap<string, number>,
  additionallyVisibleIds: readonly string[] = [],
): { visibleIds: Set<string>; expansionDepth: Map<string, number> } {
  const available = new Set(graph.nodes.map((node) => node.id));
  const nextVisible = new Set(
    [...visibleIds, ...additionallyVisibleIds].filter((id) => available.has(id)),
  );
  const nextDepth = new Map(
    [...expansionDepth].filter(([id]) => available.has(id)),
  );
  return { visibleIds: nextVisible, expansionDepth: nextDepth };
}

/** Preserve a refresh only while at least one visible entity can anchor the old camera. */
export function resolveGraphRefreshState(
  graph: Pick<SemanticGraphData, "nodes">,
  visibleIds: ReadonlySet<string>,
  expansionDepth: ReadonlyMap<string, number>,
  additionallyVisibleIds: readonly string[],
  resetVisibleIds: ReadonlySet<string>,
): {
  visibleIds: Set<string>;
  expansionDepth: Map<string, number>;
  shouldResetView: boolean;
} {
  const preserved = preserveGraphExplorationState(
    graph,
    visibleIds,
    expansionDepth,
    additionallyVisibleIds,
  );
  if (preserved.visibleIds.size > 0 || graph.nodes.length === 0) {
    return { ...preserved, shouldResetView: false };
  }
  return {
    visibleIds: new Set(resetVisibleIds),
    expansionDepth: new Map(),
    shouldResetView: true,
  };
}

/** Re-attach the last simulation coordinates to nodes that survived a data refresh. */
export function preserveGraphNodePositions<T extends { id: string }>(
  nodes: readonly T[],
  previous: ReadonlyMap<string, GraphNodePosition>,
): Array<T & GraphNodePosition> {
  return nodes.map((node) => {
    const position = previous.get(node.id);
    if (!position) return { ...node };
    const preserved: GraphNodePosition = {};
    if (position.x !== undefined) preserved.x = position.x;
    if (position.y !== undefined) preserved.y = position.y;
    if (position.vx !== undefined) preserved.vx = position.vx;
    if (position.vy !== undefined) preserved.vy = position.vy;
    if (position.fx !== undefined) preserved.fx = position.fx;
    if (position.fy !== undefined) preserved.fy = position.fy;
    return { ...node, ...preserved };
  });
}

export interface DebugKnowledgeRoutePolicy {
  debugFixtureRequested: boolean;
  productionEffectsAllowed: boolean;
  selected: string | null;
  relationId: string | null;
  reviewOpen: boolean;
}

/** Debug isolation is decided synchronously from the URL, never from an async fixture result. */
export function debugKnowledgeRoutePolicy(
  url: URL,
  dev: boolean,
  fixtureSessionReady: boolean,
  debugRelationEnabled: boolean,
): DebugKnowledgeRoutePolicy {
  const debugFixtureRequested =
    dev && url.searchParams.get("debugFixture") === "semantic-large";
  if (debugFixtureRequested) {
    return {
      debugFixtureRequested,
      productionEffectsAllowed: false,
      selected: null,
      relationId:
        fixtureSessionReady && debugRelationEnabled ? url.searchParams.get("r") : null,
      reviewOpen: false,
    };
  }
  return {
    debugFixtureRequested,
    productionEffectsAllowed: true,
    selected: url.searchParams.get("e"),
    relationId: url.searchParams.get("r"),
    reviewOpen: url.searchParams.get("review") === "1",
  };
}

export function sanitizeDebugGraphUrl(url: URL): URL {
  const sanitized = new URL(url);
  sanitized.searchParams.delete("e");
  sanitized.searchParams.delete("r");
  sanitized.searchParams.delete("review");
  return sanitized;
}

export function createDebugFixtureReleaseOnce(
  release: (sessionId: string) => Promise<void>,
): (sessionId: string | null) => Promise<void> {
  const released = new Set<string>();
  return (sessionId) => {
    if (!sessionId || released.has(sessionId)) return Promise.resolve();
    released.add(sessionId);
    return release(sessionId);
  };
}

export function graphSimulationTickBudget(
  alphaDecay = NORMAL_GRAPH_ALPHA_DECAY,
  alphaMin = 0.001,
): number {
  if (!(alphaDecay > 0 && alphaDecay < 1) || !(alphaMin > 0 && alphaMin < 1)) return 0;
  return Math.ceil(Math.log(alphaMin) / Math.log(1 - alphaDecay));
}

export function graphDragPosition(
  x: number,
  y: number,
  direct: boolean,
): { x?: number; y?: number; fx: number; fy: number } {
  return direct ? { x, y, fx: x, fy: y } : { fx: x, fy: y };
}

const CORE_PREDICATE_LABELS: Readonly<Record<string, string>> = {
  participates_in: "参与",
  responsible_for: "负责",
  belongs_to: "属于",
  uses: "使用",
  depends_on: "依赖",
  produces: "产生",
  assigned_to: "指派给",
  occurs_at: "发生于",
};

export function relationLabel(
  edge: Pick<SemanticEdge, "predicate_type" | "predicate_label">,
): string {
  const coreLabel = CORE_PREDICATE_LABELS[edge.predicate_type];
  if (coreLabel !== undefined) return coreLabel;
  if (edge.predicate_type === "custom" && edge.predicate_label?.trim()) {
    return edge.predicate_label;
  }
  return edge.predicate_type;
}

function canonicalValues(values: string[]): Set<string> {
  return new Set(values.map((value) => value.trim()).filter(Boolean));
}

function timestamp(value: string | null): number | null {
  if (value === null) return null;
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? null : parsed;
}

function overlaps(edge: SemanticEdge, filter: KnowledgeFilter): boolean {
  const requestedFrom = timestamp(filter.from);
  const requestedTo = timestamp(filter.to);
  const validFrom = timestamp(edge.valid_from);
  const validTo = timestamp(edge.valid_to);

  // These match the backend SQL exactly: both boundaries are inclusive and a missing
  // validity endpoint is an open interval.
  if (requestedFrom !== null && validTo !== null && validTo < requestedFrom) return false;
  if (requestedTo !== null && validFrom !== null && validFrom > requestedTo) return false;
  return true;
}

function compareStrings(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

function compareSemanticIds(left: SemanticEdge, right: SemanticEdge): number {
  return (
    compareStrings(left.id, right.id) ||
    compareStrings(left.subject_id, right.subject_id) ||
    compareStrings(left.object_id, right.object_id)
  );
}

export function filterSemanticGraph(
  data: SemanticGraphData,
  filter: KnowledgeFilter,
): SemanticGraphData {
  const kinds = canonicalValues(filter.entity_kinds);
  const predicates = canonicalValues(filter.predicate_types);
  let nodes = data.nodes
    .filter((node) => kinds.size === 0 || kinds.has(node.kind))
    .map((node) => ({ ...node, aliases: [...node.aliases] }))
    .sort((left, right) => compareStrings(left.id, right.id));
  const kindFilteredNodeIds = new Set(nodes.map((node) => node.id));
  const semanticEdges = data.semantic_edges
    .filter((edge) => filter.include_history || edge.status === "current")
    .filter((edge) => predicates.size === 0 || predicates.has(edge.predicate_type))
    .filter(
      (edge) =>
        kindFilteredNodeIds.has(edge.subject_id) && kindFilteredNodeIds.has(edge.object_id),
    )
    .filter((edge) => overlaps(edge, filter))
    .map((edge) => ({ ...edge }))
    .sort(compareSemanticIds);
  if (
    data.semantic_edges.length > 0 &&
    (filter.include_history ||
      predicates.size > 0 ||
      filter.from !== null ||
      filter.to !== null)
  ) {
    const admittedEndpoints = new Set(
      semanticEdges.flatMap((edge) => [edge.subject_id, edge.object_id]),
    );
    nodes = nodes.filter((node) => admittedEndpoints.has(node.id));
  }
  const nodeIds = new Set(nodes.map((node) => node.id));
  const shouldShowCooccurrence =
    filter.include_cooccurrence || data.semantic_edges.length === 0;
  const cooccurrenceEdges = shouldShowCooccurrence
    ? data.cooccurrence_edges
        .filter((edge) => nodeIds.has(edge.a) && nodeIds.has(edge.b))
        .map((edge) => ({ ...edge }))
        .sort(
          (left, right) =>
            compareStrings(left.a, right.a) ||
            compareStrings(left.b, right.b) ||
            left.weight - right.weight,
        )
    : [];

  return {
    nodes,
    semantic_edges: semanticEdges,
    cooccurrence_edges: cooccurrenceEdges,
    degraded: data.degraded,
    message: data.message,
  };
}

interface NeighborCandidate {
  entityId: string;
  edge: SemanticEdge;
}

function compareNeighbors(left: NeighborCandidate, right: NeighborCandidate): number {
  return (
    right.edge.confidence - left.edge.confidence ||
    right.edge.note_count - left.edge.note_count ||
    compareStrings(left.edge.id, right.edge.id) ||
    compareStrings(left.entityId, right.entityId)
  );
}

function rankedNeighbors(
  entityId: string,
  edges: SemanticEdge[],
  excluded: ReadonlySet<string>,
): NeighborCandidate[] {
  const candidates = new Map<string, NeighborCandidate>();
  for (const edge of edges) {
    if (!edge.subject_id || !edge.object_id || edge.subject_id === edge.object_id) continue;
    const neighbor =
      edge.subject_id === entityId
        ? edge.object_id
        : edge.object_id === entityId
          ? edge.subject_id
          : null;
    if (neighbor === null || excluded.has(neighbor)) continue;
    const candidate = { entityId: neighbor, edge };
    const previous = candidates.get(neighbor);
    if (previous === undefined || compareNeighbors(candidate, previous) < 0) {
      candidates.set(neighbor, candidate);
    }
  }
  return [...candidates.values()].sort(compareNeighbors);
}

export function nextExpandedIds(
  seed: Set<string>,
  edges: SemanticEdge[],
  hops: number,
  capPerNode = 8,
): Set<string> {
  const expanded = new Set(seed);
  if (hops <= 0 || capPerNode <= 0) return expanded;

  let frontier = [...seed].sort(compareStrings);
  for (let depth = 0; depth < Math.floor(hops) && frontier.length > 0; depth += 1) {
    const next = new Set<string>();
    for (const entityId of frontier) {
      const neighbors = rankedNeighbors(entityId, edges, expanded).slice(0, Math.floor(capPerNode));
      for (const neighbor of neighbors) {
        expanded.add(neighbor.entityId);
        next.add(neighbor.entityId);
      }
    }
    frontier = [...next].sort(compareStrings);
  }
  return expanded;
}

interface Component {
  nodeIds: string[];
  edgeIds: Set<string>;
  root: string;
}

function compareNodesByMeaning(
  left: EntitySummary,
  right: EntitySummary,
  incident: ReadonlyMap<string, SemanticEdge[]>,
): number {
  const leftEdges = incident.get(left.id) ?? [];
  const rightEdges = incident.get(right.id) ?? [];
  const leftConfidence = leftEdges.reduce((sum, edge) => sum + edge.confidence, 0);
  const rightConfidence = rightEdges.reduce((sum, edge) => sum + edge.confidence, 0);
  return (
    rightEdges.length - leftEdges.length ||
    rightConfidence - leftConfidence ||
    right.note_count - left.note_count ||
    right.mention_total - left.mention_total ||
    compareStrings(left.id, right.id)
  );
}

function semanticComponents(
  nodes: EntitySummary[],
  edges: SemanticEdge[],
  incident: ReadonlyMap<string, SemanticEdge[]>,
): Component[] {
  const nodeById = new Map(nodes.map((node) => [node.id, node]));
  const seen = new Set<string>();
  const components: Component[] = [];

  for (const first of [...nodeById.keys()].sort(compareStrings)) {
    if (seen.has(first) || (incident.get(first)?.length ?? 0) === 0) continue;
    const pending = [first];
    const nodeIds: string[] = [];
    const edgeIds = new Set<string>();
    seen.add(first);
    while (pending.length > 0) {
      const current = pending.shift()!;
      nodeIds.push(current);
      for (const edge of incident.get(current) ?? []) {
        edgeIds.add(edge.id);
        const neighbor = edge.subject_id === current ? edge.object_id : edge.subject_id;
        if (!seen.has(neighbor)) {
          seen.add(neighbor);
          pending.push(neighbor);
          pending.sort(compareStrings);
        }
      }
    }
    nodeIds.sort((left, right) =>
      compareNodesByMeaning(nodeById.get(left)!, nodeById.get(right)!, incident),
    );
    components.push({ nodeIds, edgeIds, root: nodeIds[0]! });
  }

  return components.sort((left, right) => {
    const leftConfidence = edges
      .filter((edge) => left.edgeIds.has(edge.id))
      .reduce((sum, edge) => sum + edge.confidence, 0);
    const rightConfidence = edges
      .filter((edge) => right.edgeIds.has(edge.id))
      .reduce((sum, edge) => sum + edge.confidence, 0);
    return (
      right.edgeIds.size - left.edgeIds.size ||
      rightConfidence - leftConfidence ||
      compareStrings(left.root, right.root)
    );
  });
}

export function defaultBackbone(
  data: SemanticGraphData,
  maxNodes = 80,
  perNode = 3,
): Set<string> {
  const limit = Math.max(0, Math.floor(maxNodes));
  if (limit === 0) return new Set();
  const nodeById = new Map(data.nodes.map((node) => [node.id, node]));
  const edges = data.semantic_edges
    .filter(
      (edge) =>
        edge.subject_id !== edge.object_id &&
        nodeById.has(edge.subject_id) &&
        nodeById.has(edge.object_id),
    )
    .map((edge) => ({ ...edge }))
    .sort(compareSemanticIds);
  const incident = new Map<string, SemanticEdge[]>();
  for (const edge of edges) {
    incident.set(edge.subject_id, [...(incident.get(edge.subject_id) ?? []), edge]);
    incident.set(edge.object_id, [...(incident.get(edge.object_id) ?? []), edge]);
  }
  const components = semanticComponents(data.nodes, edges, incident);
  const selected = new Set<string>();
  const admitted: Component[] = [];

  // Give every connected semantic component a stable representative before using the
  // remaining budget to deepen any one component.
  for (const component of components) {
    if (selected.size >= limit) break;
    selected.add(component.root);
    admitted.push(component);
  }

  const queues = admitted.map((component) => [component.root]);
  const neighborLimit = Math.max(0, Math.floor(perNode));
  // `perNode` is a strict expansion-degree budget. A selected node may admit only its
  // top N previously unseen neighbors; lower-ranked neighbors cannot be smuggled in by
  // a later global fill, because that can jump a bridge and disconnect the induced view.
  let madeProgress = true;
  while (selected.size < limit && madeProgress && neighborLimit > 0) {
    madeProgress = false;
    for (let index = 0; index < admitted.length && selected.size < limit; index += 1) {
      const current = queues[index]!.shift();
      if (current === undefined) continue;
      const neighbors = rankedNeighbors(current, incident.get(current) ?? [], selected).slice(
        0,
        neighborLimit,
      );
      for (const neighbor of neighbors) {
        if (selected.size >= limit) break;
        if (!selected.has(neighbor.entityId)) {
          selected.add(neighbor.entityId);
          queues[index]!.push(neighbor.entityId);
          madeProgress = true;
        }
      }
      if (queues[index]!.length > 0) madeProgress = true;
    }
  }

  return selected;
}

export function ensureBackboneEdge(
  initial: ReadonlySet<string>,
  data: SemanticGraphData,
  maxNodes: number,
): Set<string> {
  const limit = Math.max(0, Math.floor(maxNodes));
  if (limit < 2) return new Set([...initial].slice(0, limit));
  const nodeIds = new Set(data.nodes.map((node) => node.id));
  const selected = new Set([...initial].filter((id) => nodeIds.has(id)).slice(0, limit));
  if (
    data.semantic_edges.some(
      (edge) => selected.has(edge.subject_id) && selected.has(edge.object_id),
    )
  ) {
    return selected;
  }

  const bestEdge = [...data.semantic_edges]
    .filter(
      (edge) =>
        edge.subject_id !== edge.object_id &&
        nodeIds.has(edge.subject_id) &&
        nodeIds.has(edge.object_id),
    )
    .sort(
      (left, right) =>
        right.confidence - left.confidence ||
        right.note_count - left.note_count ||
        compareStrings(left.id, right.id),
    )[0];
  if (!bestEdge) return selected;

  const required = new Set([bestEdge.subject_id, bestEdge.object_id]);
  const missing = [...required].filter((id) => !selected.has(id));
  for (const id of [...selected].reverse()) {
    if (selected.size + missing.length <= limit) break;
    if (!required.has(id)) selected.delete(id);
  }
  for (const id of missing) selected.add(id);
  return selected;
}

function cooccurrenceId(a: string, b: string): string {
  return compareStrings(a, b) <= 0 ? `co:${a}:${b}` : `co:${b}:${a}`;
}

export function canonicalPathEdgeId(
  step: Pick<KnowledgePathStep, "id" | "subject_id" | "object_id" | "origin">,
): string {
  if (step.origin !== "cooccurrence") return step.id;
  const [a, b] =
    compareStrings(step.subject_id, step.object_id) <= 0
      ? [step.subject_id, step.object_id]
      : [step.object_id, step.subject_id];
  return `co:${a}:${b}`;
}

export function stableEdgeLanes(
  edges: readonly Pick<RenderEdge, "id" | "a" | "b">[],
): Map<string, number> {
  const groups = new Map<string, Pick<RenderEdge, "id" | "a" | "b">[]>();
  for (const edge of edges) {
    const [a, b] = compareStrings(edge.a, edge.b) <= 0 ? [edge.a, edge.b] : [edge.b, edge.a];
    const key = `${a.length}:${a}|${b.length}:${b}`;
    groups.set(key, [...(groups.get(key) ?? []), edge]);
  }
  const lanes = new Map<string, number>();
  for (const key of [...groups.keys()].sort(compareStrings)) {
    const group = groups.get(key)!.sort((left, right) => left.id.localeCompare(right.id));
    group.forEach((edge, index) => lanes.set(edge.id, index - (group.length - 1) / 2));
  }
  return lanes;
}

export function searchAdmissionIds(
  nodes: EntitySummary[],
  edges: SemanticEdge[],
  query: string,
): Set<string> {
  const normalized = query.trim().toLowerCase();
  if (!normalized) return new Set();
  const matched = nodes
    .filter(
      (node) =>
        node.name.toLowerCase().includes(normalized) ||
        node.aliases.some((alias) => alias.toLowerCase().includes(normalized)),
    )
    .map((node) => node.id)
    .sort(compareStrings);
  const admitted = new Set(matched);
  const neighbors = new Set<string>();
  for (const edge of edges) {
    if (admitted.has(edge.subject_id)) neighbors.add(edge.object_id);
    if (admitted.has(edge.object_id)) neighbors.add(edge.subject_id);
  }
  for (const id of [...neighbors].sort(compareStrings)) admitted.add(id);
  return admitted;
}

export type GlobalSemanticPresence = "unknown" | "present" | "absent";

export function shouldUseLegacyFallback(
  presence: GlobalSemanticPresence,
  filtered: SemanticGraphData,
  requestFailed = false,
): boolean {
  return requestFailed || (presence === "absent" && filtered.semantic_edges.length === 0);
}

export function legacyFallbackGraph(
  semantic: SemanticGraphData,
  legacy: Pick<GraphData, "nodes" | "edges">,
): SemanticGraphData {
  return {
    ...semantic,
    nodes: legacy.nodes,
    semantic_edges: [],
    cooccurrence_edges: legacy.edges,
  };
}

export function semanticRequestFailureMessage(hasLegacyFallback: boolean): string {
  return hasLegacyFallback
    ? "语义关系暂时无法读取，已显示可用的共现关系。请稍后重试。"
    : "语义关系暂时无法读取，当前没有可用的备用关系图。请稍后重试。";
}

export function viewEdges(data: SemanticGraphData, filter: KnowledgeFilter): RenderEdge[] {
  const filtered = filterSemanticGraph(data, filter);
  const semantic: RenderEdge[] = filtered.semantic_edges.map((edge) => ({
    id: edge.id,
    a: edge.subject_id,
    b: edge.object_id,
    weight: Math.max(1, edge.note_count),
    layer: "semantic",
    label: relationLabel(edge),
    directed: true,
    confidence: edge.confidence,
    status: edge.status,
  }));
  const cooccurrence: RenderEdge[] = filtered.cooccurrence_edges.map((edge) => ({
    id: cooccurrenceId(edge.a, edge.b),
    a: compareStrings(edge.a, edge.b) <= 0 ? edge.a : edge.b,
    b: compareStrings(edge.a, edge.b) <= 0 ? edge.b : edge.a,
    weight: edge.weight,
    layer: "cooccurrence",
    label: `共同出现（${edge.weight} 篇）`,
    directed: false,
    confidence: null,
    status: null,
  }));
  return [...semantic, ...cooccurrence].sort(
    (left, right) =>
      (left.layer === right.layer ? 0 : left.layer === "semantic" ? -1 : 1) ||
      compareStrings(left.id, right.id),
  );
}

export function pathEmphasis(
  path: KnowledgePath | null,
): { nodeIds: Set<string>; edgeIds: Set<string> } {
  return {
    nodeIds: new Set(path?.entity_ids ?? []),
    edgeIds: new Set(path?.steps.map(canonicalPathEdgeId) ?? []),
  };
}

/** A saved path may only be queried again after both endpoints exist in the newly published graph. */
export function hasPathEndpoints(
  data: Pick<SemanticGraphData, "nodes">,
  start: string,
  end: string,
): boolean {
  const ids = new Set(data.nodes.map((node) => node.id));
  return ids.has(start) && ids.has(end);
}

export interface PathRefreshSnapshot {
  generation: number;
  start: string | null;
  end: string | null;
}

function samePathRefresh(
  expected: PathRefreshSnapshot,
  current: PathRefreshSnapshot,
): boolean {
  return (
    expected.generation === current.generation &&
    expected.start === current.start &&
    expected.end === current.end
  );
}

/** Run each refresh stage only while the captured path still owns the UI. */
export async function runGuardedPathRefresh(
  expected: PathRefreshSnapshot,
  current: () => PathRefreshSnapshot,
  refreshStages: Array<() => Promise<void>>,
  rerun: () => Promise<void>,
): Promise<boolean> {
  for (const refresh of refreshStages) {
    if (!samePathRefresh(expected, current())) return false;
    await refresh();
    if (!samePathRefresh(expected, current())) return false;
  }
  await rerun();
  return samePathRefresh(expected, current());
}
