<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { forceSimulation, forceManyBody, forceLink, forceCenter, forceCollide, forceX, forceY, type Simulation } from "d3-force";
  import { kindInk, kindLabel, type EntitySummary, type EdgeRow, type RenderEdge } from "$lib/graph";
  import { stableEdgeLanes } from "$lib/knowledgeView";
  import { speakerInk } from "$lib/notes";

  let {
    nodes: allNodes,
    edges: allEdges,
    onPick,
    onEdgePick,
    onContextMenu,
    maxNodes = 60,
    minEdgeWeight = 2,
    backboneK = 3,
    query,
    showLegend = true,
    focusedNodeIds = new Set<string>(),
    focusedEdgeIds = new Set<string>(),
    reducedMotion,
  }: {
    nodes: EntitySummary[];
    edges: RenderEdge[] | EdgeRow[];
    onPick: (id: string, isPerson: boolean) => void;
    onEdgePick?: (id: string, layer: "semantic" | "cooccurrence") => void;
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
    /** kind 图例开关;文章视角(节点全是笔记、单一类型)传 false,图例无信息量。 */
    showLegend?: boolean;
    focusedNodeIds?: Set<string>;
    focusedEdgeIds?: Set<string>;
    reducedMotion?: boolean;
  } = $props();

  interface SimNode extends EntitySummary {
    x?: number;
    y?: number;
    fx?: number | null;
    fy?: number | null;
    r?: number;
    labelLines?: string[];
    labelW?: number;
    labelH?: number;
    labelRank?: number;
    showLabelByDefault?: boolean;
    collisionR?: number;
  }
  type SimLink = Omit<RenderEdge, "a" | "b"> & {
    a: string;
    b: string;
    source: SimNode;
    target: SimNode;
    lane: number;
  };

  function normalizeEdges(edges: RenderEdge[] | EdgeRow[]): RenderEdge[] {
    return edges.map((edge) => {
      if ("layer" in edge) return { ...edge };
      const [a, b] = edge.a <= edge.b ? [edge.a, edge.b] : [edge.b, edge.a];
      return {
        id: `co:${a}:${b}`,
        a,
        b,
        weight: edge.weight,
        layer: "cooccurrence" as const,
        label: `共同出现（${edge.weight} 篇）`,
        directed: false,
        confidence: null,
        status: null,
      };
    });
  }

  const normalizedEdges = $derived(normalizeEdges(allEdges));

  let container = $state<HTMLDivElement>();
  let width = $state(800);
  let height = $state(560);
  let hovered = $state<string | null>(null);
  let hoveredEdge = $state<string | null>(null);
  let focusedEdge = $state<string | null>(null);
  let systemReducedMotion = $state(false);
  const effectiveReducedMotion = $derived(reducedMotion ?? systemReducedMotion);
  let truncated = $state(0);
  /** 用户点了「显示全部」:build() 里忽略 maxNodes/minEdgeWeight/backboneK 的封顶,
      画出这份数据里全部有共现边的实体。 */
  let expanded = $state(false);
  /** 用户点了「展开一层」的次数:以当前骨架图为种子,逐层把邻居(不看边权重,弱连接
      也算)并进来,一步步长大而不是从 60 个直接跳到全部——避免"一下爆炸"。 */
  let expandHops = $state(0);

  // 渲染快照(每 tick 刷新;与 d3 mutate 的节点对象解耦,保证 Svelte 反应式更新)。
  let snap = $state<{
    nodes: {
      id: string;
      name: string;
      kind: string;
      is_person: boolean;
      aliases: string[];
      x: number;
      y: number;
      r: number;
      labelLines: string[];
      labelW: number;
      labelH: number;
      labelRank: number;
      showLabelByDefault: boolean;
    }[];
    links: (RenderEdge & { lane: number; x1: number; y1: number; x2: number; y2: number; r1: number; r2: number })[];
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
  const LABEL_FONT = 11;
  const LABEL_LINE = 14;
  const ALL_LABELS_LIMIT = 30;
  const DEFAULT_LARGE_LABELS = 32;
  const EDGE_LANE_GAP = 26;

  // 人实体=个人身份色(与会议搭子同一套,跨页一致认人);非人=kind 分类色
  // (`kindInk` 来自 $lib/graph,是全应用唯一真值源——侧栏 kind 过滤药丸/实体列表
  // 色点/详情面板 kind 徽章都从那取,保证跟这里的圆圈同色)。
  const nodeColor = (id: string, kind: string, isPerson: boolean) =>
    isPerson ? speakerInk(id, "mic") : kindInk(kind);

  /** 半径=重要度(主信号,相对当前渲染集合归一化);完整名称直接锚定圆心,
      圆点和文字共同构成一个节点,不再用额外卡片拆成两个视觉对象。
      线性比例(非 sqrt)——sqrt 会把低值往上拉、高值往下压,大多数低 note_count 节点
      挤在一个窄区间里看不出差别;线性 + 拉宽 MIN_R..MAX_R 让差距在视觉上真正显著。 */
  // 逐字估宽:CJK/全角 ≈ 1em、ASCII ≈ 0.55em。用于 SVG 标签换行与碰撞范围,
  // 避免依赖 foreignObject(桌面 WebView 兼容性和测量时序都更不稳定)。
  const isWide = (ch: string) => /[⺀-鿿＀-￯　-〿]/.test(ch);
  function estWidth(str: string, fontSize: number): number {
    let w = 0;
    for (const ch of str) w += isWide(ch) ? fontSize : fontSize * 0.55;
    return w;
  }

  /** 按词/CJK 字符贪心换行。每个字符都进入某一行,只换行不截断。 */
  function wrapLabel(name: string, maxWidth: number): string[] {
    const normalized = name.trim() || "未命名";
    const tokens: string[] = [];
    let word = "";
    const flushWord = () => {
      if (word) tokens.push(word);
      word = "";
    };
    for (const ch of normalized) {
      if (isWide(ch)) {
        flushWord();
        tokens.push(ch);
      } else if (/\s/.test(ch)) {
        flushWord();
        if (tokens.at(-1) !== " ") tokens.push(" ");
      } else {
        word += ch;
      }
    }
    flushWord();

    const lines: string[] = [];
    let line = "";
    const pushLine = () => {
      const clean = line.trimEnd();
      if (clean) lines.push(clean);
      line = "";
    };
    for (const token of tokens) {
      const candidate = line + token;
      if (!line || estWidth(candidate, LABEL_FONT) <= maxWidth) {
        line = candidate;
        continue;
      }
      pushLine();
      const cleanToken = token.trimStart();
      if (estWidth(cleanToken, LABEL_FONT) <= maxWidth) {
        line = cleanToken;
        continue;
      }
      // 极长的英文单词/URL 没有自然断点,逐字拆行仍保留全部内容。
      for (const ch of cleanToken) {
        if (line && estWidth(line + ch, LABEL_FONT) > maxWidth) pushLine();
        line += ch;
      }
    }
    pushLine();
    return lines.length > 0 ? lines : ["未命名"];
  }

  function layoutFor(
    name: string,
    kind: string,
    noteCount: number,
    maxNoteCount: number,
    labelRank: number,
    showAllLabels: boolean,
  ) {
    const t = maxNoteCount > 0 ? noteCount / maxNoteCount : 0;
    const r = MIN_R + t * (MAX_R - MIN_R);
    const maxLabelW = kind === "note" ? 176 : 136;
    const labelLines = wrapLabel(name, maxLabelW);
    const textW = Math.max(...labelLines.map((line) => estWidth(line, LABEL_FONT)));
    const labelW = Math.max(36, Math.min(maxLabelW, textW));
    const labelH = labelLines.length * LABEL_LINE;
    const showLabelByDefault = showAllLabels || labelRank < DEFAULT_LARGE_LABELS;
    // 标签与圆点同心:用文字包围盒的外接圆做碰撞半径,既能避免名称互相压住,
    // 又不再为「圆点 + 下方方框」重复占两份纵向空间。
    const collisionR = showLabelByDefault
      ? Math.max(r + 8, Math.hypot(labelW / 2, labelH / 2) + 8)
      : r + 6;
    return { r, labelLines, labelW, labelH, labelRank, showLabelByDefault, collisionR };
  }

  function assignStableLanes(links: SimLink[]): SimLink[] {
    const lanes = stableEdgeLanes(links);
    return links.map((edge) => ({ ...edge, lane: lanes.get(edge.id) ?? 0 }));
  }

  // 只画有共现边的实体,按 note_count 降序取前 N;边只留两端都在集里的。
  // expanded(显示全部)/expandHops(展开一层,可重复点)都会放宽这份封顶——
  // 前者一步到位画出全部,后者以当前骨架图为种子逐层并入邻居,一步步长大,不用
  // 一下从 60 跳到全部。
  // **backbone 稀疏化即便放宽也不能真的解除**:实测撞过真实卡死——900+ 实体数据集
  // 全量共现边有几万条,forceLink 每 tick 都要过一遍全部边,配合每 tick 全量重渲染
  // 上千个 DOM 节点,浏览器直接卡死。放宽到 6(默认 3)已经比骨架图丰富得多,边数量级
  // 仍被按节点数线性封顶,不会随原始边数暴涨。
  function build() {
    const growing = expanded || expandHops > 0;
    const effMinWeight = expanded ? 1 : minEdgeWeight;
    const effMaxNodes = expanded ? 2000 : maxNodes;
    const effBackboneK = growing ? Math.max(6, backboneK) : backboneK;
    // 只用「权重≥effMinWeight」的强边定种子:度数、选点都基于强边,骨架图不被
    // 大量弱共现噪声撑大。
    const strong = normalizedEdges.filter(
      (edge) => edge.layer === "semantic" || edge.weight >= effMinWeight,
    );
    const deg = new Set<string>();
    for (const e of strong) {
      deg.add(e.a);
      deg.add(e.b);
    }
    const normalizedQuery = (query ?? "").trim().toLowerCase();
    const searchNodeIds = new Set(
      normalizedQuery
        ? allNodes
            .filter(
              (node) =>
                node.name.toLowerCase().includes(normalizedQuery) ||
                node.aliases.some((alias) => alias.toLowerCase().includes(normalizedQuery)),
            )
            .map((node) => node.id)
        : [],
    );
    const candidates = allNodes
      .filter((node) => deg.has(node.id) || searchNodeIds.has(node.id))
      .sort((a, b) => b.note_count - a.note_count || a.id.localeCompare(b.id));

    let idset: Set<string>;
    if (expanded) {
      truncated = Math.max(0, candidates.length - effMaxNodes);
      idset = new Set(candidates.slice(0, effMaxNodes).map((n) => n.id));
    } else {
      const seed = candidates.slice(0, maxNodes);
      idset = new Set(seed.map((n) => n.id));
      // 展开一层:以当前 idset 为种子,逐层并入邻居——看全部边不局限于强边,
      // 展开就是要看到弱连接,不然跟骨架图没区别。BFS 一圈圈往外长,2000 是硬上限
      // (与「显示全部」共用的安全兜底)。
      // **每个节点每层最多带入 NEIGHBOR_CAP 个新邻居(取权重最高的几个),不能来者
      // 不拒**:实测种子里的枢纽实体(如「AI」共现过几百个不同实体)一层就能把
      // 几乎全图(959/975)拖进来,跟直接「显示全部」没区别,完全违背"避免一下
      // 爆炸、一层层看"的初衷。按权重排序取前几个,既控制了每层的膨胀速度,又
      // 优先带出关系最紧的邻居。
      if (expandHops > 0) {
        const NEIGHBOR_CAP = 8;
        let frontier = idset;
        for (let h = 0; h < expandHops && idset.size < 2000; h++) {
          const byNode = new Map<string, { id: string; w: number }[]>();
          for (const e of normalizedEdges) {
            if (frontier.has(e.a) && !idset.has(e.b)) {
              const arr = byNode.get(e.a) ?? [];
              arr.push({ id: e.b, w: e.weight });
              byNode.set(e.a, arr);
            }
            if (frontier.has(e.b) && !idset.has(e.a)) {
              const arr = byNode.get(e.b) ?? [];
              arr.push({ id: e.a, w: e.weight });
              byNode.set(e.b, arr);
            }
          }
          const next = new Set<string>();
          for (const [, arr] of byNode) {
            arr.sort((a, b) => b.w - a.w).slice(0, NEIGHBOR_CAP).forEach((x) => next.add(x.id));
          }
          if (next.size === 0) break; // 已经长到头,没有更多邻居可扩了
          for (const id of next) idset.add(id);
          frontier = next;
        }
      }
      if (expandHops === 0) {
        truncated = Math.max(0, candidates.length - maxNodes);
      } else {
        // 展开态的"还有多少"改按全量边(不局限强边)的度数论域算——种子阶段的
        // strong 论域跟这里不是一回事。
        const universeDeg = new Set<string>();
        for (const e of normalizedEdges) {
          universeDeg.add(e.a);
          universeDeg.add(e.b);
        }
        const universe = allNodes.filter((n) => universeDeg.has(n.id)).length;
        truncated = Math.max(0, universe - idset.size);
      }
    }

    // 搜索命中是用户明确请求的焦点，即便它没有达到默认骨架的边权阈值或完全孤立，
    // 也必须进入画布。否则侧栏能搜到、镜头却没有可聚焦的节点。
    for (const id of searchNodeIds) idset.add(id);

    // 重要节点最后绘制,它们的标记和标签自然位于较弱节点之上。
    const chosen = allNodes
      .filter((n) => idset.has(n.id))
      .sort((a, b) => a.note_count - b.note_count);
    const maxNoteCount = chosen.reduce((m, n) => Math.max(m, n.note_count), 0);
    const rankById = new Map(
      [...chosen]
        .sort((a, b) => b.note_count - a.note_count || a.name.localeCompare(b.name))
        .map((n, rank) => [n.id, rank]),
    );
    const showAllLabels = chosen.length <= ALL_LABELS_LIMIT;
    dNodes = chosen.map((n) => {
      const layout = layoutFor(
        n.name,
        n.kind,
        n.note_count,
        maxNoteCount,
        rankById.get(n.id) ?? chosen.length,
        showAllLabels,
      );
      return { ...n, ...layout };
    });
    const byId = new Map(dNodes.map((n) => [n.id, n]));
    // 已经放宽/展开了就不再局限于强边——节点都决定要画出来了,哪怕只共享 1 篇笔记
    // 也该连起来,不然图会显得比实际更散(也是本节点跟主团断联的一个常见成因)。
    const edgePool = growing ? normalizedEdges : strong;
    let candLinks = edgePool
      .filter((e) => idset.has(e.a) && idset.has(e.b))
      .map((edge) => ({ ...edge, source: byId.get(edge.a)!, target: byId.get(edge.b)!, lane: 0 }));
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
    dLinks = assignStableLanes(
      candLinks
        .filter((link) => keep.has(link))
        .sort(
          (left, right) =>
            (left.layer === right.layer ? 0 : left.layer === "cooccurrence" ? -1 : 1) ||
            left.id.localeCompare(right.id),
        ),
    );
  }

  function refreshSnap() {
    snap = {
      nodes: dNodes.map((n) => ({
        id: n.id,
        name: n.name,
        kind: n.kind,
        is_person: n.is_person,
        aliases: n.aliases,
        x: n.x ?? width / 2,
        y: n.y ?? height / 2,
        r: n.r ?? MIN_R,
        labelLines: n.labelLines ?? [n.name],
        labelW: n.labelW ?? 42,
        labelH: n.labelH ?? LABEL_LINE,
        labelRank: n.labelRank ?? Number.MAX_SAFE_INTEGER,
        showLabelByDefault: n.showLabelByDefault ?? true,
      })),
      links: dLinks.map((l) => ({
        id: l.id,
        a: l.a,
        b: l.b,
        layer: l.layer,
        label: l.label,
        directed: l.directed,
        confidence: l.confidence,
        status: l.status,
        weight: l.weight,
        lane: l.lane,
        x1: l.source.x ?? 0,
        y1: l.source.y ?? 0,
        x2: l.target.x ?? 0,
        y2: l.target.y ?? 0,
        r1: l.source.r ?? MIN_R,
        r2: l.target.r ?? MIN_R,
      })),
    };
  }

  /** heavy(>150 节点/>800 边):只影响物理参数(弱斥力/强向心力,把孤岛拉近、填满
      画布),不等于要放弃逐帧动画——backbone 已经把边数按节点线性封顶了,几百节点
      的「展开一层」逐帧渲染完全跑得动,直接冻结定格反而让用户看不出发生了什么
      (冒烟反馈"为什么没有动效")。
      skipAnimation(阈值高得多,>450 节点):真正巨大的图(「显示全部」常见的
      900+ 节点)才需要跳过逐帧动画直接冷启动定格,不然照样卡死。拖拽/resize 等
      交互要查的是这个标记,而不是 heavy——178/335 节点的展开态该有动画,也该在
      拖拽时正常重热仿真。 */
  let heavy = false;
  let skipAnimation = false;

  function settleSimulation(iterations = 120) {
    if (!sim) return;
    sim.stop();
    sim.alpha(Math.max(sim.alpha(), 0.3));
    sim.tick(iterations);
    sim.stop();
    refreshSnap();
  }

  function updateSimulationCenter() {
    const gravityStrength = heavy ? 0.4 : 0.08;
    sim?.force("center", forceCenter(width / 2, height / 2));
    sim?.force("gravityX", forceX<SimNode>(width / 2).strength(gravityStrength));
    sim?.force("gravityY", forceY<SimNode>(height / 2).strength(gravityStrength));
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
    heavy = dNodes.length > 150 || dLinks.length > 800;
    // 独立于共现关系的小簇(比如几个只互相共现、跟主图毫无连接的「日期」实体)光靠
    // forceCenter 拉不住——那只是把全体节点的平均位置摆回中心,不会单独管束某个
    // 跟主团完全没有边连接的孤岛,互斥力(charge)会把它们越推越远,越推越飘。
    // forceX/forceY 是逐节点的独立向心力,才能真正把孤岛拽回可见范围内。
    // **heavy 图斥力调弱、向心力调强很多**:默认 60 节点视图的参数是照顾"少数节点
    // 撑满容器"调的,直接套到几百上千节点的展开图上,孤岛会被推得离主团很远、画布
    // 大片黑边空着(冒烟反馈"不利于充分利用空间"——单靠 fit 排除离群点只解决了缩放
    // 框选,没解决节点实际位置分散、空间没用满的问题)。
    const chargeStrength = heavy ? -55 : -190;
    const gravityStrength = heavy ? 0.4 : 0.08;
    sim = forceSimulation<SimNode>(dNodes)
      .force("charge", forceManyBody().strength(chargeStrength))
      .force(
        "link",
        forceLink<SimNode, SimLink>(dLinks)
          .id((d) => d.id)
          .distance(heavy ? 36 : 54)
          .strength(0.35),
      )
      .force("center", forceCenter(width / 2, height / 2))
      .force("gravityX", forceX<SimNode>(width / 2).strength(gravityStrength))
      .force("gravityY", forceY<SimNode>(height / 2).strength(gravityStrength))
      // heavy 图碰撞力加大迭代次数(默认 1→3):快衰减(见下)只给约 40 tick,
      // 碰撞力每 tick 只推一次的话根本来不及把重叠分开、仿真就冷却停了(冒烟反馈
      // "位置没好就停止了")。加迭代=每 tick 多推几轮,少 tick 也能分干净——用
      // "推得更狠"换"跑得更快",而不是靠拖时间。
      .force("collide", forceCollide<SimNode>((d) => d.collisionR ?? (d.r ?? MIN_R) + 6).iterations(4))
      // heavy 图默认衰减(alphaDecay≈0.0228,~300 tick、5 秒+)配合弱化过的斥力,
      // 动画拖得很长、过程飘来飘去看着乱(冒烟反馈"动的时间太长了很混乱")。
      // 调快衰减(~40 tick、1 秒内)+ 加大阻尼(velocityDecay,抑制过冲/来回摆动),
      // 让"展开一层"的生长动画短平快、不慌乱;碰撞迭代兜住布局质量不因快而糊。
      .alphaDecay(heavy ? 0.26 : 0.0228)
      .velocityDecay(heavy ? 0.45 : 0.4)
      .on("tick", refreshSnap);
    skipAnimation = dNodes.length > 450 || dLinks.length > 2500;
    if (skipAnimation) {
      sim.stop();
      // 巨图(900+ 节点)一次性冷启动定格,不逐帧动画(逐 tick 全量重渲染上千 DOM
      // 会真卡死)。这条路用户看不到动画过程、只看最终结果,时间不敏感但布局必须
      // 干净。**这里必须用慢衰减跑足够多的 tick**:上面为动画调的快衰减(0.26)会
      // 让下面这个 alpha 门控循环几十 tick 就退出,几百上千节点根本没被碰撞力分开、
      // 挤成一坨(冒烟反馈"位置没好就停止了")。临时把衰减调回慢档(0.02≈300 tick
      // 预算)专门给冷启动用,配合 collide 迭代把重叠彻底清干净,400 tick 安全上限。
      sim.alphaDecay(0.05).alpha(1);
      let iterations = 0;
      while (sim.alpha() > sim.alphaMin() && iterations < 400) {
        sim.tick();
        iterations++;
      }
      refreshSnap();
    } else {
      if (effectiveReducedMotion) settleSimulation();
    }
  }

  onMount(() => {
    const reducedMotionQuery = window.matchMedia("(prefers-reduced-motion: reduce)");
    systemReducedMotion = reducedMotionQuery.matches;
    const updateReducedMotion = (event: MediaQueryListEvent) => {
      systemReducedMotion = event.matches;
    };
    reducedMotionQuery.addEventListener("change", updateReducedMotion);
    if (container) {
      width = container.clientWidth || 800;
      height = container.clientHeight || 560;
    }
    const ro = new ResizeObserver(() => {
      if (!container) return;
      width = container.clientWidth || width;
      height = container.clientHeight || height;
      updateSimulationCenter();
      // 巨图 resize 不走 alphaTarget 持续重热(会重新触发逐 tick 全量重渲染),
      // 冷启动重定格一次即可——容器大小变化不需要重新物理仿真,只是换个中心点。
      if (effectiveReducedMotion) {
        settleSimulation(20);
      } else if (skipAnimation) {
        settleSimulation(20);
      } else {
        sim?.alpha(0.3).restart();
      }
    });
    if (container) ro.observe(container);
    return () => {
      ro.disconnect();
      reducedMotionQuery.removeEventListener("change", updateReducedMotion);
    };
  });

  $effect(() => {
    // 读取这些依赖建立追踪:任一变化(换了中心实体/规模参数/显示全部开关)即重建。
    // query 需要参与建图：搜索命中的孤立节点不在默认边骨架里，只有查询时才准入。
    void allNodes;
    void allEdges;
    void maxNodes;
    void minEdgeWeight;
    void backboneK;
    void query;
    void effectiveReducedMotion;
    void expanded;
    void expandHops;
    if (container) rebuild();
  });

  onDestroy(() => sim?.stop());

  // hover:高亮邻居,其余淡化。
  const neighbors = $derived.by(() => {
    if (!hovered) return null;
    const s = new Set<string>([hovered]);
    for (const l of snap.links) {
      if (l.a === hovered) s.add(l.b);
      if (l.b === hovered) s.add(l.a);
    }
    return s;
  });
  const dimNode = (id: string) => neighbors !== null && !neighbors.has(id);
  const dimLink = (aid: string, bid: string) =>
    hovered !== null && aid !== hovered && bid !== hovered;

  const visibleSemanticCount = $derived(
    snap.links.filter((edge) => edge.layer === "semantic").length,
  );

  function edgePathId(id: string, label = false): string {
    const encoded = Array.from(id)
      .map((character) => character.codePointAt(0)!.toString(16))
      .join("-");
    return `${label ? "edge-label" : "edge-path"}-${encoded}`;
  }

  function edgePath(edge: (typeof snap.links)[number]): string {
    const dx = edge.x2 - edge.x1;
    const dy = edge.y2 - edge.y1;
    const length = Math.max(1, Math.hypot(dx, dy));
    const startInset = Math.min(edge.r1 + 2, length * 0.3);
    const endInset = Math.min(edge.r2 + (edge.directed ? 8 : 2), length * 0.34);
    const canonicalDirection = edge.a.localeCompare(edge.b) <= 0 ? 1 : -1;
    const canonicalDx = dx * canonicalDirection;
    const canonicalDy = dy * canonicalDirection;
    const offset = edge.lane * EDGE_LANE_GAP;
    const controlX = (edge.x1 + edge.x2) / 2 - (canonicalDy / length) * offset;
    const controlY = (edge.y1 + edge.y2) / 2 + (canonicalDx / length) * offset;
    return `M ${edge.x1 + (dx / length) * startInset} ${edge.y1 + (dy / length) * startInset} Q ${controlX} ${controlY} ${edge.x2 - (dx / length) * endInset} ${edge.y2 - (dy / length) * endInset}`;
  }

  function edgeLabelPath(edge: (typeof snap.links)[number]): string {
    const dx = edge.x2 - edge.x1;
    const dy = edge.y2 - edge.y1;
    const length = Math.max(1, Math.hypot(dx, dy));
    const canonicalDirection = edge.a.localeCompare(edge.b) <= 0 ? 1 : -1;
    const offset = edge.lane * EDGE_LANE_GAP;
    const controlX = (edge.x1 + edge.x2) / 2 - ((dy * canonicalDirection) / length) * offset;
    const controlY = (edge.y1 + edge.y2) / 2 + ((dx * canonicalDirection) / length) * offset;
    const readsForward = edge.x1 < edge.x2 || (edge.x1 === edge.x2 && edge.y1 <= edge.y2);
    return readsForward
      ? `M ${edge.x1} ${edge.y1} Q ${controlX} ${controlY} ${edge.x2} ${edge.y2}`
      : `M ${edge.x2} ${edge.y2} Q ${controlX} ${controlY} ${edge.x1} ${edge.y1}`;
  }

  function edgeLabelVisible(edge: (typeof snap.links)[number]): boolean {
    if (edge.layer !== "semantic") return false;
    return (
      visibleSemanticCount <= 30 ||
      viewZoom >= 1.35 ||
      hoveredEdge === edge.id ||
      focusedEdge === edge.id ||
      focusedEdgeIds.has(edge.id)
    );
  }

  function edgeOpacity(edge: (typeof snap.links)[number]): number {
    if (focusedEdgeIds.size > 0) {
      if (focusedEdgeIds.has(edge.id)) return 0.94;
      return 0.15;
    }
    if (hoveredEdge && hoveredEdge !== edge.id) return 0.16;
    if (dimLink(edge.a, edge.b)) return 0.12;
    return edge.layer === "semantic" ? 0.72 : 0.2;
  }

  function nodeOpacity(id: string): number {
    if (focusedNodeIds.size > 0) {
      if (focusedNodeIds.has(id)) return 1;
      return 0.15;
    }
    return dimNode(id) || dimForSearch(id) ? 0.22 : 1;
  }

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
  /** 语义缩放:大型图先显示最重要的一组完整标签,放大后逐级揭示更多。悬停时把
      一阶邻居的名字一起展开,让用户沿着关系读图;搜索命中永远可见。 */
  const labelVisible = (n: (typeof snap.nodes)[number]) =>
    n.showLabelByDefault ||
    matchedIds.has(n.id) ||
    (neighbors?.has(n.id) ?? false) ||
    viewZoom >= 2.2 ||
    (viewZoom >= 1.45 && n.labelRank < 180);

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
      缩放+居中变换,让图形永远撑满容器,而不是死守仿真给的绝对像素间距。
      **孤岛/留白问题不该靠这里的"排除离群点"来治**:早先试过「去掉离质心最远 10%」
      的核心包围盒,结果适得其反——排除的那 10% 依然会被画出来,只是不再参与居中
      计算,分布一旦不对称(不是均匀往四周散,而是明显偏一侧),排除后算出的"核心
      中心"就会偏离全体节点的真实视觉中心,导致整体构图被推向一边、留出大片不对称
      黑边(实测反馈"变成这样了"——比不排除还难看)。真正的解法在 rebuild() 里调
      物理参数(heavy 图弱斥力/强向心力,把点群本身聚拢紧凑),这里老老实实用全部
      节点的真实包围盒即可。 */
  const fit = $derived.by(() => {
    const ns = snap.nodes;
    if (ns.length === 0) return { scale: 1, tx: 0, ty: 0 };
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const n of ns) {
      // 标签使用逆缩放保持屏幕字号稳定,不属于仿真坐标系的几何尺寸。fit 只看标记,
      // 再用更宽的画布边距给同心文字留空间,避免文字反过来把节点缩成小点。
      minX = Math.min(minX, n.x - n.r);
      minY = Math.min(minY, n.y - n.r);
      maxX = Math.max(maxX, n.x + n.r);
      maxY = Math.max(maxY, n.y + n.r);
    }
    const bw = Math.max(1, maxX - minX);
    const bh = Math.max(1, maxY - minY);
    // 封顶 2.5x:防止节点很少时被放大到失真;留 14% 边距给屏幕定宽标签。
    const scale = Math.min((width / bw) * 0.86, (height / bh) * 0.86, 2.5);
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
      // 标签在屏幕坐标中保持定宽,这里不能再乘 fit.scale。
      const labelHalfW = n.labelW / 2;
      const labelHalfH = n.labelH / 2;
      minX = Math.min(minX, px - Math.max(pr, labelHalfW));
      minY = Math.min(minY, py - Math.max(pr, labelHalfH));
      maxX = Math.max(maxX, px + Math.max(pr, labelHalfW));
      maxY = Math.max(maxY, py + Math.max(pr, labelHalfH));
    }
    const bw = Math.max(1, maxX - minX);
    const bh = Math.max(1, maxY - minY);
    const pad = 70;
    const bboxZoom = Math.max(1, Math.min((width - pad) / bw, (height - pad) / bh));
    // 单个命中时 bbox 几乎就是节点自身,直接套 3.5x 上限会把已经经过 fit 放大的
    // 圆再次放到占满画布、甚至被裁切。除了相对倍率上限,再给最终节点半径一个画布
    // 尺寸相关的绝对上限:聚焦后仍保留邻居上下文,也不因窗口大小写死像素。
    const maxRenderedRadius = Math.max(...pts.map((n) => n.r * fit.scale));
    const radiusZoom = (Math.min(width, height) * 0.12) / Math.max(1, maxRenderedRadius);
    const targetZoom = Math.min(3.5, bboxZoom, Math.max(1, radiusZoom));
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
      // heavy 图不重热仿真(alphaTarget 持续重启=逐 tick 全量重渲染,卡死元凶之一),
      // 只把这一个节点钉在指针位置、手动单帧刷新——其余节点保持冷却定格不陪着抖。
      if (!heavy && !effectiveReducedMotion) sim?.alphaTarget(0.3).restart();
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
      if (heavy || effectiveReducedMotion) refreshSnap();
    }
  }
  function onUp(id: string, isPerson: boolean) {
    const n = dNodes.find((d) => d.id === id);
    if (n) {
      n.fx = null;
      n.fy = null;
    }
    if (!heavy && !effectiveReducedMotion) sim?.alphaTarget(0);
    // dragId 只在左键 onDown 时被置位(见上方注释);右键释放时 dragId 仍是上次左键交互
    // 遗留值(通常是 null),不等于当前 id,故不会误触发导航。
    if (dragId === id && !moved) onPick(id, isPerson);
    dragId = null;
  }
</script>

<div class="fg" class:reduced={effectiveReducedMotion} bind:this={container}>
  <svg {width} {height} role="img" aria-label="知识图谱力导向图" onwheel={onWheel}>
    <defs>
      <marker
        id="semantic-arrow"
        viewBox="0 0 10 10"
        refX="8"
        refY="5"
        markerWidth="7"
        markerHeight="7"
        orient="auto-start-reverse"
        markerUnits="userSpaceOnUse"
      >
        <path d="M 0 1 L 9 5 L 0 9 z" fill="var(--accent)" />
      </marker>
    </defs>
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
          {#each snap.links as l (l.id)}
            <g
              class="edge"
              role="group"
              aria-label={l.label}
              class:semantic={l.layer === "semantic"}
              class:cooccurrence={l.layer === "cooccurrence"}
              opacity={edgeOpacity(l)}
              onmouseenter={() => (hoveredEdge = l.id)}
              onmouseleave={() => (hoveredEdge = null)}
            >
              {#if l.layer === "semantic"}
                <path
                  id={edgePathId(l.id)}
                  class="edge-line semantic-line"
                  d={edgePath(l)}
                  marker-end="url(#semantic-arrow)"
                  vector-effect="non-scaling-stroke"
                />
              {:else}
                <path
                  id={edgePathId(l.id)}
                  class="edge-line cooccurrence-line"
                  d={edgePath(l)}
                  vector-effect="non-scaling-stroke"
                />
              {/if}
              <path
                id={edgePathId(l.id, true)}
                class="edge-label-guide"
                d={edgeLabelPath(l)}
              />
              {#if onEdgePick}
                <path
                  class="edge-hit"
                  d={edgePath(l)}
                  role="button"
                  tabindex="0"
                  aria-label={`${l.label}，${l.layer === "semantic" ? "语义关系" : "共现弱连接"}`}
                  onclick={() => onEdgePick(l.id, l.layer)}
                  onfocus={() => (focusedEdge = l.id)}
                  onblur={() => (focusedEdge = null)}
                  onkeydown={(event) => {
                    if (event.key === "Enter" || event.key === " ") {
                      event.preventDefault();
                      onEdgePick(l.id, l.layer);
                    }
                  }}
                ><title>{l.label}</title></path>
              {:else}
                <path class="edge-hover-target" d={edgePath(l)}><title>{l.label}</title></path>
              {/if}
              {#if edgeLabelVisible(l)}
                <text class="edge-label">
                  <textPath href={`#${edgePathId(l.id, true)}`} startOffset="50%">{l.label}</textPath>
                </text>
              {/if}
            </g>
          {/each}
        </g>
        <g class="nodes">
          {#each snap.nodes as n (n.id)}
            <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
            <g
              transform="translate({n.x},{n.y})"
              opacity={nodeOpacity(n.id)}
              style="cursor:pointer"
              role="button"
              tabindex="0"
              aria-label={`${n.name}，${kindLabel(n.kind)}`}
              onpointerdown={(e) => onDown(n.id, e)}
              onpointermove={onMove}
              onpointerup={() => onUp(n.id, n.is_person)}
              onmouseenter={() => (hovered = n.id)}
              onmouseleave={() => (hovered = null)}
              onkeydown={(event) => {
                if (event.key === "Enter" || event.key === " ") {
                  event.preventDefault();
                  onPick(n.id, n.is_person);
                }
              }}
              oncontextmenu={(e) => {
                if (!onContextMenu) return;
                e.preventDefault();
                onContextMenu(n.id, n.name, n.is_person, e.clientX, e.clientY);
              }}
            >
              <circle
                class="node-halo"
                r={n.r + 3}
                fill="none"
                stroke={nodeColor(n.id, n.kind, n.is_person)}
                stroke-width="1"
                vector-effect="non-scaling-stroke"
              />
              <circle class="node-marker" r={n.r} fill={nodeColor(n.id, n.kind, n.is_person)}>
                <title>{n.name}</title>
              </circle>
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
              {#if labelVisible(n)}
                <!-- 名称与圆点同心,共同构成一个节点。逆缩放让文字在缩放时保持可读字号。 -->
                {@const renderScale = Math.max(0.001, fit.scale * viewZoom)}
                <g class="node-label" transform="scale({1 / renderScale})">
                  <text>
                    {#each n.labelLines as line, i}
                      <tspan
                        x="0"
                        y={(i - (n.labelLines.length - 1) / 2) * LABEL_LINE}
                      >{line}</tspan>
                    {/each}
                  </text>
                </g>
              {/if}
            </g>
          {/each}
        </g>
      </g>
    </g>
  </svg>
  {#if snap.nodes.length > ALL_LABELS_LIMIT && viewZoom < 2.2}
    <div class="semantic-hint">滚轮放大显示更多名称 · 悬停探索相邻关系</div>
  {/if}
  {#if expanded}
    <div class="trunc-bar">
      <span class="trunc-label">已显示全部 {snap.nodes.length} 个实体</span>
      <button class="trunc-action" onclick={() => { expanded = false; expandHops = 0; }}>收起</button>
    </div>
  {:else if expandHops > 0}
    <!-- 展开一层可重复点,一圈圈往外长,不像「显示全部」一步到位——避免一下爆炸 -->
    <div class="trunc-bar">
      <span class="trunc-label">已展开 {expandHops} 层 · 共 {snap.nodes.length} 个实体</span>
      {#if truncated > 0}
        <button class="trunc-action" onclick={() => (expandHops += 1)}>继续展开</button>
        <button class="trunc-action trunc-action-strong" onclick={() => (expanded = true)}>显示全部</button>
      {/if}
      <button class="trunc-action" onclick={() => (expandHops = 0)}>收起</button>
    </div>
  {:else if truncated > 0}
    <div class="trunc-bar">
      <span class="trunc-label">显示连接最紧的 {snap.nodes.length} 个实体</span>
      <button class="trunc-action" onclick={() => (expandHops = 1)}>展开一层</button>
      <button class="trunc-action trunc-action-strong" onclick={() => (expanded = true)}>显示全部</button>
    </div>
  {/if}
  {#if showLegend && legend.length > 0}
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
  .edge { transition: opacity 240ms cubic-bezier(0.25, 1, 0.5, 1); }
  .fg.reduced .edge, .fg.reduced .nodes > g { transition: none; }
  .edge-line { fill: none; vector-effect: non-scaling-stroke; }
  .semantic-line {
    stroke: var(--accent);
    stroke-width: 1.35px;
  }
  .cooccurrence-line {
    stroke: var(--hairline-strong);
    stroke-width: 0.8px;
    stroke-dasharray: 3 5;
  }
  .edge-label-guide { fill: none; stroke: none; }
  .edge-hit, .edge-hover-target {
    fill: none;
    stroke: transparent;
    stroke-width: 12px;
    vector-effect: non-scaling-stroke;
  }
  .edge-hit {
    cursor: pointer;
  }
  .edge-hit:focus-visible {
    stroke: var(--accent-tint);
    outline: none;
  }
  .edge-label {
    fill: var(--ink-secondary);
    stroke: var(--canvas);
    stroke-width: 3px;
    paint-order: stroke fill;
    font-family: inherit;
    font-size: 10px;
    font-weight: 560;
    text-anchor: middle;
    letter-spacing: 0.015em;
    pointer-events: none;
  }
  .node-halo { opacity: 0.2; }
  .node-marker {
    stroke: var(--surface);
    stroke-width: 1.5px;
    vector-effect: non-scaling-stroke;
  }
  .node-label text {
    fill: var(--ink);
    stroke: var(--surface);
    stroke-width: 3px;
    stroke-linejoin: round;
    paint-order: stroke fill;
    font-family: inherit;
    font-size: 11px;
    font-weight: 650;
    text-anchor: middle;
    dominant-baseline: central;
    letter-spacing: 0.01em;
  }
  .nodes > g:hover .node-label text { font-weight: 750; }
  .nodes > g { transition: opacity 240ms cubic-bezier(0.25, 1, 0.5, 1); }
  .nodes > g:focus-visible { outline: none; }
  .nodes > g:focus-visible .node-halo { opacity: 1; stroke-width: 2.5px; }
  .semantic-hint {
    position: absolute;
    top: 12px;
    left: 50%;
    transform: translateX(-50%);
    padding: 6px 10px;
    border: 1px solid var(--hairline);
    border-radius: 999px;
    background: var(--surface);
    color: var(--ink-faint);
    font-size: 11px;
    line-height: 1;
    white-space: nowrap;
    pointer-events: none;
  }
  /* 规模控制条:说明文字(纯信息,不可点)+ 独立的药丸按钮群(可点),不再是一整块
     文字链接挤在一个胶囊里分不清哪段是说明哪段是按钮(冒烟反馈"很粗糙,不够精美")。
     每个按钮自成一个 hairline 描边药丸,跟侧栏「全局图谱」/「返回图谱」同一套
     视觉语言,拉开彼此间距,touch target 也更大。 */
  .trunc-bar {
    position: absolute;
    left: 16px;
    bottom: 14px;
    max-width: calc(100% - 32px);
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px;
  }
  .trunc-label {
    font-size: 11px;
    color: var(--ink-faint);
    background: var(--surface);
    padding: 5px 11px;
    border-radius: 999px;
    border: 1px solid var(--hairline);
    white-space: nowrap;
  }
  .trunc-action {
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: 999px;
    padding: 5px 12px;
    margin: 0;
    cursor: pointer;
    font: inherit;
    font-size: 11px;
    font-weight: 500;
    color: var(--ink-secondary);
    white-space: nowrap;
  }
  .trunc-action:hover {
    background: var(--surface-soft);
    border-color: var(--hairline-strong);
    color: var(--ink);
  }
  /* 「显示全部」是分量最重的一步(直接跳到底,而非逐层展开),用 accent 区分出来,
     不能跟「继续展开」「收起」这些平级操作长得一样。 */
  .trunc-action-strong {
    color: var(--accent);
    border-color: var(--accent-tint);
    background: var(--accent-tint);
  }
  .trunc-action-strong:hover {
    background: var(--accent-tint);
    border-color: var(--accent);
  }
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
    background: var(--ink-faint);
  }
  @media (pointer: coarse) {
    .edge-hit, .edge-hover-target { stroke-width: 18px; }
    .trunc-action { min-height: 44px; padding-inline: 14px; }
  }
  @media (prefers-reduced-motion: reduce) {
    .edge, .nodes > g { transition-duration: 0.01ms; }
  }
</style>
