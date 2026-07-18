<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { forceSimulation, forceManyBody, forceLink, forceCenter, forceCollide, type Simulation } from "d3-force";
  import { kindInk, kindLabel, type EntitySummary, type EdgeRow } from "$lib/graph";
  import { speakerInk } from "$lib/notes";

  let {
    nodes: allNodes,
    edges: allEdges,
    onPick,
    onContextMenu,
    maxNodes = 60,
    minEdgeWeight = 2,
    backboneK = 3,
    query,
  }: {
    nodes: EntitySummary[];
    edges: EdgeRow[];
    onPick: (id: string, isPerson: boolean) => void;
    /** 右键节点(改名等次要操作入口);不传则节点不响应右键,走浏览器默认菜单。 */
    onContextMenu?: (id: string, name: string, isPerson: boolean, clientX: number, clientY: number) => void;
    /** 规模封顶(默认给全局图用);实体详情页的小型关系图会传更小的值。用户点「显示全部」
        后此值形同虚设(见 expanded)。 */
    maxNodes?: number;
    /** 只连接权重≥此值的边(默认 2,过滤共享 1 篇的噪声连接);中心实体的直连关系
        本身已被后端筛过,详情页小图应传 1,不丢真实但较弱的关联。 */
    minEdgeWeight?: number;
    /** 每节点保留最强 K 条边的 backbone 稀疏化;详情页小图数据量小,可传大值关闭稀疏化。 */
    backboneK?: number;
    /** 搜索关键词(侧栏搜索框同源):命中的节点画布上高亮+自动聚焦镜头,不传则不启用。 */
    query?: string;
  } = $props();

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
  /** 用户点了「显示全部」:build() 里忽略 maxNodes/minEdgeWeight/backboneK 的封顶,
      画出这份数据里全部有共现边的实体。 */
  let expanded = $state(false);

  // 渲染快照(每 tick 刷新;与 d3 mutate 的节点对象解耦,保证 Svelte 反应式更新)。
  let snap = $state<{
    nodes: { id: string; label: string; name: string; kind: string; is_person: boolean; aliases: string[]; x: number; y: number; r: number }[];
    links: { aid: string; bid: string; x1: number; y1: number; x2: number; y2: number; w: number }[];
  }>({ nodes: [], links: [] });

  // 镜头变换(用户手动缩放/平移,叠在 fit 自适应变换之外的第二层):滚轮缩放围绕光标、
  // 拖拽空白处平移。数据变化(rebuild)时归零回到自适应总览,不然新数据可能整个不在
  // 用户上次停留的视口范围内。
  let viewZoom = $state(1);
  let viewX = $state(0);
  let viewY = $state(0);

  let sim: Simulation<SimNode, undefined> | null = null;
  let dNodes: SimNode[] = [];
  let dLinks: SimLink[] = [];

  const MIN_R = 12;
  const MAX_R = 38;
  const CHAR_PX = 8; // 中英混排近似字宽(10px 字号)

  // 人实体=个人身份色(与会议搭子同一套,跨页一致认人);非人=kind 分类色
  // (`kindInk` 来自 $lib/graph,是全应用唯一真值源——侧栏 kind 过滤药丸/实体列表
  // 色点/详情面板 kind 徽章都从那取,保证跟这里的圆圈同色)。
  const nodeColor = (id: string, kind: string, isPerson: boolean) =>
    isPerson ? speakerInk(id, "mic") : kindInk(kind);

  /** 半径=重要度(主信号,相对当前渲染集合归一化);文字装不下就截断,不反过来撑大圆。
      线性比例(非 sqrt)——sqrt 会把低值往上拉、高值往下压,大多数低 note_count 节点
      挤在一个窄区间里看不出差别;线性 + 拉宽 MIN_R..MAX_R 让差距在视觉上真正显著。 */
  function sizeFor(name: string, noteCount: number, maxNoteCount: number): { r: number; label: string } {
    const t = maxNoteCount > 0 ? noteCount / maxNoteCount : 0;
    const r = MIN_R + t * (MAX_R - MIN_R);
    const maxChars = Math.max(2, Math.floor(((r - 6) * 2) / CHAR_PX));
    const label = name.length > maxChars ? name.slice(0, maxChars - 1) + "…" : name;
    return { r, label };
  }

  // 只画有共现边的实体,按 note_count 降序取前 N;边只留两端都在集里的。
  // expanded 时三道封顶(节点数/边权重下限/每节点 backbone)全部解除,画出这份数据里
  // 全部有共现关系的实体——用户点「显示全部」之后才走这条路径,默认仍是可读骨架图。
  function build() {
    const effMinWeight = expanded ? 1 : minEdgeWeight;
    const effMaxNodes = expanded ? Infinity : maxNodes;
    const effBackboneK = expanded ? Infinity : backboneK;
    // 只用「权重≥effMinWeight」的强边:度数、选点、连线都基于强边。
    const strong = allEdges.filter((e) => e.weight >= effMinWeight);
    const deg = new Set<string>();
    for (const e of strong) {
      deg.add(e.a);
      deg.add(e.b);
    }
    const candidates = allNodes.filter((n) => deg.has(n.id)).sort((a, b) => b.note_count - a.note_count);
    truncated = Math.max(0, candidates.length - effMaxNodes);
    const chosen = candidates.slice(0, effMaxNodes);
    const idset = new Set(chosen.map((n) => n.id));
    const maxNoteCount = chosen.reduce((m, n) => Math.max(m, n.note_count), 0);
    dNodes = chosen.map((n) => {
      const { r, label } = sizeFor(n.name, n.note_count, maxNoteCount);
      return { ...n, r, label };
    });
    const byId = new Map(dNodes.map((n) => [n.id, n]));
    let candLinks = strong
      .filter((e) => idset.has(e.a) && idset.has(e.b))
      .map((e) => ({ source: byId.get(e.a)!, target: byId.get(e.b)!, weight: e.weight }));
    // 稀疏化 backbone:每个节点只保留最强的 backboneK 条边(union),把超密共现图收成可读骨架。
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
      arr.sort((a, b) => b.w - a.w).slice(0, effBackboneK).forEach((x) => keep.add(x.l));
    }
    dLinks = candLinks.filter((l) => keep.has(l));
  }

  function refreshSnap() {
    snap = {
      nodes: dNodes.map((n) => ({
        id: n.id,
        label: n.label ?? n.name,
        name: n.name,
        kind: n.kind,
        is_person: n.is_person,
        aliases: n.aliases,
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

  /** (重)建仿真:首次挂载与之后每次 nodes/edges/规模参数变化(如详情页切换中心实体)都要
      重跑,否则组件被复用时 props 换了但内部仿真停留在旧数据,图不更新。 */
  function rebuild() {
    sim?.stop();
    // 数据集本身变了(换中心实体/kind 过滤/显示全部切换),旧的镜头位置对新数据未必
    // 还有意义,归零回自适应总览。
    viewZoom = 1;
    viewX = 0;
    viewY = 0;
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
  }

  onMount(() => {
    if (container) {
      width = container.clientWidth || 800;
      height = container.clientHeight || 560;
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

  $effect(() => {
    // 读取这些依赖建立追踪:任一变化(换了中心实体/规模参数/显示全部开关)即重建。
    // 故意不含 query——搜索只影响高亮/镜头聚焦,不该让仿真每敲一个字就重排一次。
    void allNodes;
    void allEdges;
    void maxNodes;
    void minEdgeWeight;
    void backboneK;
    void expanded;
    if (container) rebuild();
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

  // 搜索命中(名字/别名子串,大小写不敏感,与侧栏列表同一套匹配规则):命中节点画布上
  // 高亮描边+其余淡化,只在当前已渲染的节点里找——被封顶挤掉的实体点「显示全部」才够得着。
  const matchedIds = $derived.by(() => {
    const q = (query ?? "").trim().toLowerCase();
    if (!q) return new Set<string>();
    const s = new Set<string>();
    for (const n of snap.nodes) {
      if (n.name.toLowerCase().includes(q) || n.aliases.some((a) => a.toLowerCase().includes(q))) s.add(n.id);
    }
    return s;
  });
  const dimForSearch = (id: string) => matchedIds.size > 0 && !matchedIds.has(id);

  // kind 图例:按当前渲染集合里实际出现的类别生成,不是全量枚举(没出现的类别列出来
  // 也没用)。人实体在图上按个体上色(speakerInk),没有单一代表色,图例用渐变点说明
  // "这类颜色因人而异",不能随手挑一个色骗大家这就是"人"的颜色。
  // 人物聚合行的 key 特意不用字面量 "person"——数据里存在 is_person=false 但
  // kind="person" 的实体(kind 是模型抽的分类标签、is_person 是后端归并结果,两者
  // 不保证一致),字面量会跟这类非人实体的 kind 行撞 key,炸掉 keyed each。
  const legend = $derived.by(() => {
    const kinds = new Set<string>();
    let hasPerson = false;
    for (const n of snap.nodes) {
      if (n.is_person) hasPerson = true;
      else kinds.add(n.kind);
    }
    const rows: { key: string; label: string; ink: string | null }[] = [...kinds]
      .sort()
      .map((k) => ({ key: k, label: kindLabel(k), ink: kindInk(k) }));
    if (hasPerson) rows.unshift({ key: "__is_person__", label: "人 · 按个人上色", ink: null });
    return rows;
  });

  /** 自适应缩放:力导仿真的间距是固定物理单位,跟容器大小无关——图少、容器大时会挤成
      一小团、四周大片空白。每次渲染都按节点实际占据的包围盒(含半径留白)算一个整体
      缩放+居中变换,让图形永远撑满容器,而不是死守仿真给的绝对像素间距。 */
  const fit = $derived.by(() => {
    const ns = snap.nodes;
    if (ns.length === 0) return { scale: 1, tx: 0, ty: 0 };
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const n of ns) {
      minX = Math.min(minX, n.x - n.r);
      minY = Math.min(minY, n.y - n.r);
      maxX = Math.max(maxX, n.x + n.r);
      maxY = Math.max(maxY, n.y + n.r);
    }
    const bw = Math.max(1, maxX - minX);
    const bh = Math.max(1, maxY - minY);
    // 封顶 2.5x:防止节点很少时被放大到失真;留 8% 边距不贴边。
    const scale = Math.min((width / bw) * 0.92, (height / bh) * 0.92, 2.5);
    const cx = (minX + maxX) / 2;
    const cy = (minY + maxY) / 2;
    return { scale, tx: width / 2 - cx * scale, ty: height / 2 - cy * scale };
  });

  // 搜索命中后自动把镜头聚焦到命中节点(bbox 居中 + 适度放大),而不是让用户在密密麻麻
  // 的图里肉眼去找。只在有命中时动镜头;清空搜索不强制归位,不跟用户手动缩放打架。
  $effect(() => {
    const ids = matchedIds;
    if (ids.size === 0) return;
    const pts = snap.nodes.filter((n) => ids.has(n.id));
    if (pts.length === 0) return;
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const n of pts) {
      const px = n.x * fit.scale + fit.tx;
      const py = n.y * fit.scale + fit.ty;
      const pr = n.r * fit.scale;
      minX = Math.min(minX, px - pr);
      minY = Math.min(minY, py - pr);
      maxX = Math.max(maxX, px + pr);
      maxY = Math.max(maxY, py + pr);
    }
    const bw = Math.max(1, maxX - minX);
    const bh = Math.max(1, maxY - minY);
    const pad = 70;
    const targetZoom = Math.min(3.5, Math.max(1, Math.min((width - pad) / bw, (height - pad) / bh)));
    const cx = (minX + maxX) / 2;
    const cy = (minY + maxY) / 2;
    viewZoom = targetZoom;
    viewX = width / 2 - cx * targetZoom;
    viewY = height / 2 - cy * targetZoom;
  });

  // 滚轮缩放(围绕光标定位,缩放前后光标下的那个点视觉上不动)+ 拖空白处平移。
  // 两层坐标变换:screen = view(fit(sim))——先撤 view 层才能回到 fit 空间。
  function onWheel(e: WheelEvent) {
    e.preventDefault();
    if (!container) return;
    const rect = container.getBoundingClientRect();
    const mx = e.clientX - rect.left;
    const my = e.clientY - rect.top;
    const factor = Math.exp(-e.deltaY * 0.0015);
    const newZoom = Math.min(8, Math.max(0.3, viewZoom * factor));
    const fx = (mx - viewX) / viewZoom;
    const fy = (my - viewY) / viewZoom;
    viewX = mx - fx * newZoom;
    viewY = my - fy * newZoom;
    viewZoom = newZoom;
  }
  let panning = false;
  let panStart = { x: 0, y: 0, vx: 0, vy: 0 };
  function onBgDown(e: PointerEvent) {
    if (e.button !== 0) return;
    (e.currentTarget as Element).setPointerCapture(e.pointerId);
    panning = true;
    panStart = { x: e.clientX, y: e.clientY, vx: viewX, vy: viewY };
  }
  function onBgMove(e: PointerEvent) {
    if (!panning) return;
    viewX = panStart.vx + (e.clientX - panStart.x);
    viewY = panStart.vy + (e.clientY - panStart.y);
  }
  function onBgUp() {
    panning = false;
  }

  // 拖拽节点 + 点击(位移小判点击)。落点要先撤 view 层(手动缩放/平移)再撤 fit 层,
  // 两层都撤完才是仿真坐标系,否则镜头缩放/平移过后鼠标跟节点视觉位置对不上。
  let dragId: string | null = null;
  let moved = false;
  function onDown(id: string, e: PointerEvent) {
    // 只有主键(左键)才算拖拽/点击起点——右键的 pointerdown+pointerup 也会触发这对回调,
    // 不拦下的话右键点节点会在打开右键菜单的同时把页面导航走(onUp 误判为点击)。
    if (e.button !== 0) return;
    e.stopPropagation();
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
      n.fx = ((e.clientX - rect.left - viewX) / viewZoom - fit.tx) / fit.scale;
      n.fy = ((e.clientY - rect.top - viewY) / viewZoom - fit.ty) / fit.scale;
    }
  }
  function onUp(id: string, isPerson: boolean) {
    const n = dNodes.find((d) => d.id === id);
    if (n) {
      n.fx = null;
      n.fy = null;
    }
    sim?.alphaTarget(0);
    // dragId 只在左键 onDown 时被置位(见上方注释);右键释放时 dragId 仍是上次左键交互
    // 遗留值(通常是 null),不等于当前 id,故不会误触发导航。
    if (dragId === id && !moved) onPick(id, isPerson);
    dragId = null;
  }
</script>

<div class="fg" bind:this={container}>
  <svg {width} {height} role="img" aria-label="知识图谱力导向图" onwheel={onWheel}>
    <!-- 空白背景:承接拖拽平移,铺在最外层原始坐标系(不随 view/fit 变换),保证任意
         缩放级别下都能覆盖满整个视口。 -->
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <rect
      class="bg"
      {width}
      {height}
      fill="transparent"
      onpointerdown={onBgDown}
      onpointermove={onBgMove}
      onpointerup={onBgUp}
      onpointerleave={onBgUp}
    />
    <!-- 镜头层(用户手动缩放/平移)包着自适应层(fit,见其注释)——两层叠加详见
         onWheel/onMove 的坐标换算注释。 -->
    <g transform="translate({viewX},{viewY}) scale({viewZoom})">
      <g transform="translate({fit.tx},{fit.ty}) scale({fit.scale})">
        <g class="edges">
          {#each snap.links as l (l.aid + "-" + l.bid)}
            <!-- 边粗细=关系强弱(共享笔记数),连续映射而非固定档;title 给 hover 原生提示;
                 non-scaling-stroke 防 fit 缩放把线宽也跟着放大失真 -->
            <line
              x1={l.x1}
              y1={l.y1}
              x2={l.x2}
              y2={l.y2}
              stroke="var(--hairline-strong)"
              stroke-width={Math.min(5, 0.5 + l.w * 0.7)}
              opacity={dimLink(l.aid, l.bid) ? 0.06 : 0.35}
              vector-effect="non-scaling-stroke"
            ><title>共享 {l.w} 篇笔记</title></line>
          {/each}
        </g>
        <g class="nodes">
          {#each snap.nodes as n (n.id)}
            <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
            <g
              transform="translate({n.x},{n.y})"
              opacity={dimNode(n.id) || dimForSearch(n.id) ? 0.22 : 1}
              style="cursor:pointer"
              onpointerdown={(e) => onDown(n.id, e)}
              onpointermove={onMove}
              onpointerup={() => onUp(n.id, n.is_person)}
              onmouseenter={() => (hovered = n.id)}
              onmouseleave={() => (hovered = null)}
              oncontextmenu={(e) => {
                if (!onContextMenu) return;
                e.preventDefault();
                onContextMenu(n.id, n.name, n.is_person, e.clientX, e.clientY);
              }}
            >
              <circle r={n.r} fill={nodeColor(n.id, n.kind, n.is_person)}><title>{n.name}</title></circle>
              {#if matchedIds.has(n.id)}
                <!-- 搜索命中描边:比节点本身粗一圈,vector-effect 防镜头缩放把线宽带跑 -->
                <circle
                  r={n.r + 4}
                  fill="none"
                  stroke="var(--accent)"
                  stroke-width="2.5"
                  vector-effect="non-scaling-stroke"
                />
              {/if}
              <text class="lbl" font-size={Math.min(14, Math.max(8, 6 + n.r * 0.22))}>{n.label}</text>
            </g>
          {/each}
        </g>
      </g>
    </g>
  </svg>
  {#if truncated > 0}
    <div class="trunc">
      显示连接最紧的 {snap.nodes.length} 个实体
      <button class="trunc-btn" onclick={() => (expanded = true)}>显示全部</button>
    </div>
  {:else if expanded}
    <div class="trunc">已显示全部 {snap.nodes.length} 个实体 <button class="trunc-btn" onclick={() => (expanded = false)}>收起</button></div>
  {/if}
  {#if legend.length > 0}
    <div class="legend">
      {#each legend as row (row.key)}
        <div class="legend-row">
          {#if row.ink}
            <span class="legend-dot" style="background:{row.ink}"></span>
          {:else}
            <span class="legend-dot legend-dot-person"></span>
          {/if}
          {row.label}
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .fg { position: relative; height: 100%; overflow: hidden; }
  svg { display: block; touch-action: none; }
  .bg { cursor: grab; }
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
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 11px;
    color: var(--ink-faint);
    background: var(--surface);
    padding: 3px 9px;
    border-radius: 999px;
    border: 1px solid var(--hairline);
  }
  .trunc-btn {
    background: none;
    border: 0;
    padding: 0;
    margin: 0;
    cursor: pointer;
    font: inherit;
    font-weight: 500;
    color: var(--accent);
  }
  .trunc-btn:hover { text-decoration: underline; }
  /* 图例:kind→色 对照,不用记药丸圆点代表什么颜色。人物没有单一代表色(按个体上色),
     渐变点如实传达"这类颜色因人而异"而不是编一个假色骗大家。 */
  .legend {
    position: absolute;
    right: 16px;
    bottom: 14px;
    max-height: 45%;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 11px;
    color: var(--ink-secondary);
    background: var(--surface);
    padding: 7px 10px;
    border-radius: 10px;
    border: 1px solid var(--hairline);
  }
  .legend-row {
    display: flex;
    align-items: center;
    gap: 6px;
    white-space: nowrap;
  }
  .legend-dot {
    width: 8px;
    height: 8px;
    border-radius: 999px;
    flex: none;
  }
  .legend-dot-person {
    background: conic-gradient(
      var(--tint-sky-ink),
      var(--tint-mint-ink),
      var(--tint-rose-ink),
      var(--tint-lavender-ink),
      var(--tint-sky-ink)
    );
  }
</style>
