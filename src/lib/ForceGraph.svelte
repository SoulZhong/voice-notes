<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { forceSimulation, forceManyBody, forceLink, forceCenter, forceCollide, type Simulation } from "d3-force";
  import type { EntitySummary, EdgeRow } from "$lib/graph";
  import { speakerColor } from "$lib/notes";

  let {
    nodes: allNodes,
    edges: allEdges,
    onPick,
  }: {
    nodes: EntitySummary[];
    edges: EdgeRow[];
    onPick: (id: string, isPerson: boolean) => void;
  } = $props();

  const MAX_NODES = 60;
  const MIN_EDGE_WEIGHT = 2; // 只连「共享≥2篇」的实体——共享 1 篇太泛,会糊成一团

  interface SimNode extends EntitySummary {
    x?: number;
    y?: number;
    fx?: number | null;
    fy?: number | null;
    r?: number;
    label?: string;
  }
  type SimLink = { source: SimNode; target: SimNode; weight: number };

  let container = $state<HTMLDivElement>();
  let width = $state(800);
  let height = $state(560);
  let hovered = $state<string | null>(null);
  let truncated = $state(0);

  // 渲染快照(每 tick 刷新;与 d3 mutate 的节点对象解耦,保证 Svelte 反应式更新)。
  let snap = $state<{
    nodes: { id: string; label: string; is_person: boolean; x: number; y: number; r: number }[];
    links: { aid: string; bid: string; x1: number; y1: number; x2: number; y2: number; w: number }[];
  }>({ nodes: [], links: [] });

  let sim: Simulation<SimNode, undefined> | null = null;
  let dNodes: SimNode[] = [];
  let dLinks: SimLink[] = [];

  const MIN_R = 16;
  const MAX_R = 30;
  const CHAR_PX = 8; // 中英混排近似字宽(10px 字号)
  const nodeColor = (id: string, isPerson: boolean) => (isPerson ? speakerColor(id, "mic") : "var(--ink-faint)");

  /** 半径按「装得下文字」算,note_count 只在文字很短时略微加权;文字太长则截断保 MAX_R。 */
  function sizeFor(name: string, noteCount: number): { r: number; label: string } {
    const noteBased = Math.min(MAX_R, MIN_R + Math.sqrt(noteCount) * 2.2);
    const fullTextR = Math.min(MAX_R, name.length * CHAR_PX * 0.5 + 8);
    const r = Math.max(MIN_R, noteBased, Math.min(fullTextR, MAX_R));
    const maxChars = Math.max(2, Math.floor(((r - 8) * 2) / CHAR_PX));
    const label = name.length > maxChars ? name.slice(0, maxChars - 1) + "…" : name;
    return { r, label };
  }

  // 只画有共现边的实体,按 note_count 降序取前 N;边只留两端都在集里的。
  function build() {
    // 只用「共享≥2篇」的强边:度数、选点、连线都基于强边,去掉共享 1 篇的噪声连接。
    const strong = allEdges.filter((e) => e.weight >= MIN_EDGE_WEIGHT);
    const deg = new Set<string>();
    for (const e of strong) {
      deg.add(e.a);
      deg.add(e.b);
    }
    const candidates = allNodes.filter((n) => deg.has(n.id)).sort((a, b) => b.note_count - a.note_count);
    truncated = Math.max(0, candidates.length - MAX_NODES);
    const chosen = candidates.slice(0, MAX_NODES);
    const idset = new Set(chosen.map((n) => n.id));
    dNodes = chosen.map((n) => {
      const { r, label } = sizeFor(n.name, n.note_count);
      return { ...n, r, label };
    });
    const byId = new Map(dNodes.map((n) => [n.id, n]));
    let candLinks = strong
      .filter((e) => idset.has(e.a) && idset.has(e.b))
      .map((e) => ({ source: byId.get(e.a)!, target: byId.get(e.b)!, weight: e.weight }));
    // 稀疏化 backbone:每个节点只保留最强的 K 条边(union),把超密共现图收成可读骨架。
    const K = 3;
    const perNode = new Map<string, { l: SimLink; w: number }[]>();
    for (const l of candLinks) {
      for (const id of [l.source.id, l.target.id]) {
        const arr = perNode.get(id) ?? [];
        arr.push({ l, w: l.weight });
        perNode.set(id, arr);
      }
    }
    const keep = new Set<SimLink>();
    for (const [, arr] of perNode) {
      arr.sort((a, b) => b.w - a.w).slice(0, K).forEach((x) => keep.add(x.l));
    }
    dLinks = candLinks.filter((l) => keep.has(l));
  }

  function refreshSnap() {
    snap = {
      nodes: dNodes.map((n) => ({
        id: n.id,
        label: n.label ?? n.name,
        is_person: n.is_person,
        x: n.x ?? width / 2,
        y: n.y ?? height / 2,
        r: n.r ?? MIN_R,
      })),
      links: dLinks.map((l) => ({
        aid: l.source.id,
        bid: l.target.id,
        x1: l.source.x ?? 0,
        y1: l.source.y ?? 0,
        x2: l.target.x ?? 0,
        y2: l.target.y ?? 0,
        w: l.weight,
      })),
    };
  }

  onMount(() => {
    if (container) {
      width = container.clientWidth || 800;
      height = container.clientHeight || 560;
    }
    build();
    const reduce = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    sim = forceSimulation<SimNode>(dNodes)
      .force("charge", forceManyBody().strength(-190))
      .force(
        "link",
        forceLink<SimNode, SimLink>(dLinks)
          .id((d) => d.id)
          .distance(54)
          .strength(0.35),
      )
      .force("center", forceCenter(width / 2, height / 2))
      .force("collide", forceCollide<SimNode>((d) => (d.r ?? MIN_R) + 6))
      .on("tick", refreshSnap);
    if (reduce) {
      sim.stop();
      sim.tick(120);
      refreshSnap();
    }

    const ro = new ResizeObserver(() => {
      if (!container) return;
      width = container.clientWidth || width;
      height = container.clientHeight || height;
      sim?.force("center", forceCenter(width / 2, height / 2));
      sim?.alpha(0.3).restart();
    });
    if (container) ro.observe(container);
    return () => ro.disconnect();
  });

  onDestroy(() => sim?.stop());

  // hover:高亮邻居,其余淡化。
  const neighbors = $derived.by(() => {
    if (!hovered) return null;
    const s = new Set<string>([hovered]);
    for (const l of snap.links) {
      if (l.aid === hovered) s.add(l.bid);
      if (l.bid === hovered) s.add(l.aid);
    }
    return s;
  });
  const dimNode = (id: string) => neighbors !== null && !neighbors.has(id);
  const dimLink = (aid: string, bid: string) =>
    hovered !== null && aid !== hovered && bid !== hovered;

  // 拖拽 + 点击(位移小判点击)。
  let dragId: string | null = null;
  let moved = false;
  function onDown(id: string, e: PointerEvent) {
    (e.currentTarget as Element).setPointerCapture(e.pointerId);
    dragId = id;
    moved = false;
    const n = dNodes.find((d) => d.id === id);
    if (n) {
      sim?.alphaTarget(0.3).restart();
      n.fx = n.x;
      n.fy = n.y;
    }
  }
  function onMove(e: PointerEvent) {
    if (!dragId || !container) return;
    moved = true;
    const rect = container.getBoundingClientRect();
    const n = dNodes.find((d) => d.id === dragId);
    if (n) {
      n.fx = e.clientX - rect.left;
      n.fy = e.clientY - rect.top;
    }
  }
  function onUp(id: string, isPerson: boolean) {
    const n = dNodes.find((d) => d.id === id);
    if (n) {
      n.fx = null;
      n.fy = null;
    }
    sim?.alphaTarget(0);
    if (!moved) onPick(id, isPerson);
    dragId = null;
  }
</script>

<div class="fg" bind:this={container}>
  <svg {width} {height} role="img" aria-label="知识图谱力导向图">
    <g class="edges">
      {#each snap.links as l (l.aid + "-" + l.bid)}
        <line
          x1={l.x1}
          y1={l.y1}
          x2={l.x2}
          y2={l.y2}
          stroke="var(--hairline-strong)"
          stroke-width={Math.min(3, l.w)}
          opacity={dimLink(l.aid, l.bid) ? 0.06 : 0.35}
        />
      {/each}
    </g>
    <g class="nodes">
      {#each snap.nodes as n (n.id)}
        <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
        <g
          transform="translate({n.x},{n.y})"
          opacity={dimNode(n.id) ? 0.22 : 1}
          style="cursor:pointer"
          onpointerdown={(e) => onDown(n.id, e)}
          onpointermove={onMove}
          onpointerup={() => onUp(n.id, n.is_person)}
          onmouseenter={() => (hovered = n.id)}
          onmouseleave={() => (hovered = null)}
        >
          <circle r={n.r} fill={nodeColor(n.id, n.is_person)} />
          <text class="lbl" font-size={n.r >= 22 ? 11 : 9.5}>{n.label}</text>
        </g>
      {/each}
    </g>
  </svg>
  {#if truncated > 0}
    <div class="trunc">显示连接最紧的 {snap.nodes.length} 个实体 · 用左侧列表看全部</div>
  {/if}
</div>

<style>
  .fg { position: relative; height: 100%; overflow: hidden; }
  svg { display: block; }
  .lbl {
    fill: var(--surface);
    font-family: inherit;
    font-weight: 500;
    text-anchor: middle;
    dominant-baseline: central;
    pointer-events: none;
  }
  .trunc {
    position: absolute;
    left: 16px;
    bottom: 14px;
    font-size: 11px;
    color: var(--ink-faint);
    background: var(--surface);
    padding: 3px 9px;
    border-radius: 999px;
    border: 1px solid var(--hairline);
  }
</style>
