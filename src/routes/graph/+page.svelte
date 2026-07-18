<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import { graphData, entityDetail, renameEntity, kindLabel, kindInk, kindSoft, type EntityDetail, type GraphData, type EntitySummary, type EdgeRow } from "$lib/graph";
  import { formatDate } from "$lib/notes";
  import { graphFilter } from "$lib/graphFilter.svelte";
  import ForceGraph from "$lib/ForceGraph.svelte";

  // 改名(纠 ASR 提取错的实体名)。头部改名入口(针对当前选中实体)+ 图上右键菜单
  // (针对任意节点,不必先选中/导航过去)共用这一套提交逻辑。
  let renaming = $state(false);
  let renameValue = $state("");
  let renameErr = $state("");
  let renameBusy = $state(false);
  function startRename(d: EntityDetail) {
    renameValue = d.name;
    renameErr = "";
    renaming = true;
  }
  /** 改名成功后的收尾:图数据整体刷新(反映新名字/合并),若改的正是当前选中实体则
      同步刷新详情面板或跳到新 id。 */
  async function afterRename(oldId: string, newId: string) {
    graph = await graphData().catch(() => graph);
    if (selected === oldId) {
      if (newId === oldId) detail = await entityDetail(newId).catch(() => detail);
      else goto("/graph?e=" + encodeURIComponent(newId));
    }
  }
  async function submitRename(oldId: string) {
    const name = renameValue.trim();
    if (!name || renameBusy) return;
    renameBusy = true;
    try {
      const r = await renameEntity(oldId, name);
      renaming = false;
      await afterRename(oldId, r.new_id);
    } catch (e) {
      renameErr = `改名失败: ${e}`;
    } finally {
      renameBusy = false;
    }
  }

  // 图上右键菜单:任意节点(不管是不是当前选中实体)右键→改名。
  let ctxMenu = $state<{ id: string; name: string; isPerson: boolean; x: number; y: number } | null>(null);
  let ctxRenaming = $state(false);
  let ctxRenameValue = $state("");
  let ctxRenameErr = $state("");
  let ctxRenameBusy = $state(false);
  function openCtxMenu(id: string, name: string, isPerson: boolean, clientX: number, clientY: number) {
    ctxMenu = { id, name, isPerson, x: clientX, y: clientY };
    ctxRenaming = false;
    ctxRenameErr = "";
  }
  function closeCtxMenu() {
    ctxMenu = null;
    ctxRenaming = false;
  }
  function startCtxRename() {
    if (!ctxMenu) return;
    ctxRenameValue = ctxMenu.name;
    ctxRenameErr = "";
    ctxRenaming = true;
  }
  async function submitCtxRename() {
    if (!ctxMenu || ctxRenameBusy) return;
    const name = ctxRenameValue.trim();
    if (!name) return;
    ctxRenameBusy = true;
    try {
      const r = await renameEntity(ctxMenu.id, name);
      await afterRename(ctxMenu.id, r.new_id);
      closeCtxMenu();
    } catch (e) {
      ctxRenameErr = `改名失败: ${e}`;
    } finally {
      ctxRenameBusy = false;
    }
  }

  /** 详情页小型关系图的节点容量——比全局图(60)小得多,一个实体的最强关系够看清。 */
  const EGO_MAX_RELATED = 30;

  let graph = $state<GraphData>({ nodes: [], edges: [] });
  let loaded = $state(false);
  let detail = $state<EntityDetail | null>(null);
  let detailLoading = $state(false);

  /** 选中实体的全局 id(经 ?e= 深链 / 侧栏点击);person 全局 id 不以 e: 开头。 */
  const selected = $derived($page.url.searchParams.get("e"));
  const isPersonId = (id: string) => !id.startsWith("e:");

  onMount(async () => {
    try {
      graph = await graphData();
    } catch {
      graph = { nodes: [], edges: [] };
    }
    loaded = true;
  });

  function pickNode(id: string, isPerson: boolean) {
    if (isPerson) goto("/speakers/" + id);
    else goto("/graph?e=" + encodeURIComponent(id));
  }

  // 选中变化 → 拉详情。侧栏点人实体已直接跳 /speakers,这里只处理非人。
  $effect(() => {
    const id = selected;
    renaming = false;
    renameErr = "";
    if (!id) {
      detail = null;
      return;
    }
    detailLoading = true;
    entityDetail(id)
      .then((d) => {
        detail = d;
        detailLoading = false;
      })
      .catch(() => {
        detail = null;
        detailLoading = false;
      });
  });

  function pickRelated(id: string) {
    if (isPersonId(id)) goto("/speakers/" + id);
    else goto("/graph?e=" + encodeURIComponent(id));
  }

  /** 以当前实体为中心的小型关系图数据:中心 + 最相关的 N 个(后端已按共享笔记数降序),
      节点优先取自已加载的全局图(真实 note_count/kind/aliases,渲染更准确),取不到就用
      detail/related 自带的字段兜底合成,保证图谱数据未就绪时也能渲染。边 = 中心↔各相关
      实体(权重=共享笔记数,来自 detail,始终可靠)+ 相关实体彼此之间的共现(取自全局图,
      展示邻居间的聚簇——非必需但更有信息量,全局图未加载时自然为空)。 */
  const ego = $derived.by(() => {
    const d = detail;
    if (!d) return { nodes: [] as EntitySummary[], edges: [] as EdgeRow[] };
    const relatedTop = d.related.slice(0, EGO_MAX_RELATED);
    const idSet = new Set<string>([d.id, ...relatedTop.map((r) => r.id)]);
    const byId = new Map(graph.nodes.map((n) => [n.id, n]));
    const center: EntitySummary = byId.get(d.id) ?? {
      id: d.id, kind: d.kind, name: d.name, aliases: d.aliases,
      is_person: d.is_person, note_count: d.note_count, mention_total: d.mention_total,
    };
    const relatedNodes: EntitySummary[] = relatedTop.map(
      (r) =>
        byId.get(r.id) ?? {
          id: r.id, kind: r.kind, name: r.name, aliases: [],
          is_person: !r.id.startsWith("e:"), note_count: r.shared_notes, mention_total: 0,
        },
    );
    const centerEdges: EdgeRow[] = relatedTop.map((r) => ({ a: d.id, b: r.id, weight: r.shared_notes }));
    const neighborEdges = graph.edges.filter(
      (e) => idSet.has(e.a) && idSet.has(e.b) && e.a !== d.id && e.b !== d.id,
    );
    return { nodes: [center, ...relatedNodes], edges: [...centerEdges, ...neighborEdges] };
  });

  /** 主区未选实体态(全局力导图)按侧栏 kind 药丸过滤——只保留匹配 kind 的节点,
      边要求两端都保留(否则会有指向已隐藏节点的悬空边)。搜索关键词不在这里过滤掉
      节点,而是整份原样传给 ForceGraph 走高亮+聚焦镜头(见 query prop),这样搜索
      命中的节点即使不在 top-N 骨架里也可能通过力导图自身的封顶规则显示不出来——
      这是已知取舍,搜不到就点"显示全部"。 */
  const filteredGraph = $derived.by(() => {
    if (graphFilter.kind === "all") return graph;
    const keep = new Set(graph.nodes.filter((n) => n.kind === graphFilter.kind).map((n) => n.id));
    return {
      nodes: graph.nodes.filter((n) => keep.has(n.id)),
      edges: graph.edges.filter((e) => keep.has(e.a) && keep.has(e.b)),
    };
  });
</script>

<div class="graph-main">
  {#if loaded && graph.nodes.length === 0}
    <div class="empty">
      <p class="empty-title">还没有知识图谱</p>
      <p class="empty-desc">配置大模型并对笔记「重新 Aing」后,人物、组织、项目等实体会自动汇入这里,按共享的笔记彼此关联。</p>
      <button class="empty-cta" onclick={() => goto("/ai")}>去 AI 设置</button>
    </div>
  {:else if selected && detailLoading && !detail}
    <button class="back" onclick={() => goto("/graph")}>← 返回图谱</button>
    <p class="hint">加载中…</p>
  {:else if selected && detail}
    {@const d = detail}
    <div class="detail">
      <button class="back" onclick={() => goto("/graph")}>← 返回图谱</button>
      <div class="d-head">
        {#if renaming}
          <!-- svelte-ignore a11y_autofocus -->
          <input
            class="d-rename-input"
            autofocus
            bind:value={renameValue}
            disabled={renameBusy}
            onkeydown={(e) => {
              if (e.key === "Enter") submitRename(d.id);
              if (e.key === "Escape") renaming = false;
            }}
            onblur={() => submitRename(d.id)}
          />
        {:else}
          <span class="d-name">{detail.name}</span>
        {/if}
        {#if !renaming}
          <!-- 紧跟在名字右边(冒烟反馈:原先 margin-left:auto 甩到整行最右端,
               视觉上跟名字脱节,看不出改的是谁) -->
          <button class="d-rename-btn" onclick={() => startRename(d)} title="改名(纠正提取错误)">改名</button>
        {/if}
        <span class="kind" style="background:{kindSoft(detail.kind)}; color:{kindInk(detail.kind)}">{kindLabel(detail.kind)}</span>
      </div>
      {#if renameErr}
        <p class="d-rename-err">{renameErr}</p>
      {/if}
      {#if detail.aliases.length}
        <p class="d-aliases">
          别名:{detail.aliases.slice(0, 6).join("、")}{detail.aliases.length > 6
            ? ` 等 ${detail.aliases.length} 个`
            : ""}
        </p>
      {/if}
      <p class="d-stat">出现在 {detail.note_count} 篇 · {detail.mention_total} 次提及</p>

      <div class="d-cols">
        <section class="d-section notes-col">
          <h3>出现的笔记 <span class="d-count">{detail.notes.length}</span></h3>
          {#if detail.notes.length}
            <ul class="d-scroll">
              {#each detail.notes as n (n.id)}
                <!-- svelte-ignore a11y_no_noninteractive_element_interactions, a11y_click_events_have_key_events -->
                <li class="d-note" onclick={() => goto("/notes/" + n.id)}>
                  <span class="d-note-title">{n.title}</span>
                  <span class="d-note-meta">{formatDate(n.started_at)} · {n.mention_count} 提及</span>
                </li>
              {/each}
            </ul>
          {/if}
        </section>

        <section class="d-section graph-col">
          <h3>
            相关实体 <span class="d-count">{detail.related.length}</span>
            {#if detail.related.length > EGO_MAX_RELATED}
              <span class="d-cap">只画最相关的 {EGO_MAX_RELATED} 个</span>
            {/if}
          </h3>
          {#if ego.nodes.length >= 2}
            <div class="ego-wrap">
              <ForceGraph
                nodes={ego.nodes}
                edges={ego.edges}
                onPick={pickRelated}
                onContextMenu={openCtxMenu}
                maxNodes={EGO_MAX_RELATED + 1}
                minEdgeWeight={1}
                backboneK={999}
              />
            </div>
          {/if}
        </section>
      </div>
    </div>
  {:else if selected}
    <button class="back" onclick={() => goto("/graph")}>← 返回图谱</button>
    <p class="hint">该实体还未进入图谱(可能刚 Aing 完,稍后重试)。</p>
  {:else if graph.edges.length > 0 && graph.nodes.length >= 2}
    {#if filteredGraph.nodes.length >= 2}
      <ForceGraph
        nodes={filteredGraph.nodes}
        edges={filteredGraph.edges}
        onPick={pickNode}
        onContextMenu={openCtxMenu}
        query={graphFilter.query}
      />
    {:else}
      <div class="placeholder">
        <p class="ph-title">没有匹配的实体</p>
        <p class="ph-desc">当前类型筛选下画布里没有足够的关系可画,换一个类型试试。</p>
        <button class="empty-cta" onclick={() => (graphFilter.kind = "all")}>清除筛选</button>
      </div>
    {/if}
  {:else}
    <div class="placeholder">
      <p class="ph-title">知识图谱</p>
      <p class="ph-desc">从左侧选择一个实体,查看它出现在哪些笔记、和谁一起被提到。</p>
    </div>
  {/if}
</div>

{#if ctxMenu}
  <!-- 点击任意处关闭;遮罩是纯指针便利层,与 Sidebar.svelte 的行右键菜单同一惯例 -->
  <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
  <div
    class="menu-overlay"
    onclick={closeCtxMenu}
    oncontextmenu={(e) => {
      e.preventDefault();
      closeCtxMenu();
    }}
  ></div>
  <div class="ctx-menu" style="left: {ctxMenu.x}px; top: {ctxMenu.y}px">
    {#if ctxRenaming}
      <!-- svelte-ignore a11y_autofocus -->
      <input
        class="ctx-rename-input"
        autofocus
        bind:value={ctxRenameValue}
        disabled={ctxRenameBusy}
        onkeydown={(e) => {
          if (e.key === "Enter") submitCtxRename();
          if (e.key === "Escape") closeCtxMenu();
        }}
      />
      {#if ctxRenameErr}
        <p class="ctx-rename-err">{ctxRenameErr}</p>
      {/if}
    {:else}
      <button class="ctx-item" onclick={startCtxRename}>改名</button>
    {/if}
  </div>
{/if}

<style>
  .graph-main { height: 100%; overflow: hidden; }

  /* 撑满可用高度(不再是纵向长滚的文档流):居中但拓宽上限,让相关实体力导图有
     足够画布——纯文字列表挤在窄列里既浪费横向空间、又逼出大片纵向留白。 */
  .detail {
    display: flex; flex-direction: column;
    height: 100%; max-width: 1400px; margin: 0 auto;
    padding: 24px 36px 28px; box-sizing: border-box;
  }
  /* 醒目一些:从纯文字链接改药丸按钮(冒烟反馈"太不起眼"),字号/内边距放大到
     与其他主要操作按钮同一量级,才配得上"退回全局图谱"这个高频动作。 */
  .back {
    display: inline-flex; align-items: center; gap: 6px; flex: none; align-self: flex-start; margin: 0 0 20px;
    background: var(--surface-soft); border: 1px solid var(--hairline); border-radius: var(--radius-full);
    padding: 8px 18px; cursor: pointer;
    font-size: 14px; font-weight: 500; color: var(--ink-secondary); font-family: inherit;
  }
  .back:hover { color: var(--ink); background: var(--surface-press); border-color: var(--hairline-strong); }
  .d-head { flex: none; display: flex; align-items: baseline; gap: 10px; margin-bottom: 10px; }
  .d-rename-input {
    font-size: 20px; font-weight: 500; color: var(--ink); font-family: inherit;
    background: var(--surface-soft); border: 1px solid var(--accent); border-radius: 6px;
    padding: 1px 6px; min-width: 8em;
  }
  .d-rename-btn {
    background: none; border: 0; padding: 0; cursor: pointer;
    font-size: 12px; font-weight: 500; color: var(--ink-faint); font-family: inherit;
  }
  .d-rename-btn:hover { color: var(--accent); }
  .d-rename-err { font-size: 12px; color: var(--danger-ink); margin: -4px 0 8px; }
  .d-name { font-size: 20px; font-weight: 500; color: var(--ink); }
  .kind {
    font-size: 11px; color: var(--ink-secondary);
    padding: 1px 7px; border-radius: 5px; background: var(--surface-soft);
  }
  .d-aliases { flex: none; font-size: 13px; color: var(--ink-secondary); margin: 0 0 6px; line-height: 1.6; }
  .d-stat { flex: none; font-size: 13px; color: var(--ink-faint); margin: 0 0 20px; }

  /* 两栏并排(笔记 / 相关实体关系图):填满剩余高度,而非按内容撑开——
     笔记列表内部滚(与会议搭子详情页「出现过的会议」同一惯例),关系图直接
     用满这块画布把「大片留白」变成有效可视化。 */
  .d-cols { flex: 1; min-height: 0; display: flex; gap: 28px; }
  @media (max-width: 760px) {
    .d-cols { flex-direction: column; }
  }
  .d-section {
    display: flex; flex-direction: column; min-height: 0;
  }
  .notes-col { flex: 0 0 340px; }
  .graph-col { flex: 1; min-width: 0; }
  @media (max-width: 760px) {
    .notes-col { flex: 0 0 auto; max-height: 40%; }
    .graph-col { flex: 1; }
  }
  .d-section h3 {
    flex: none; display: flex; align-items: center; gap: 6px; flex-wrap: wrap;
    font-size: 12px; font-weight: 500; color: var(--ink-secondary); margin: 0 0 8px;
  }
  .d-count {
    font-size: 10.5px; color: var(--ink-faint);
    background: var(--surface-soft); border-radius: 999px; padding: 0 6px;
  }
  .d-cap { font-size: 11px; color: var(--ink-faint); }
  .d-scroll {
    flex: 1; min-height: 0;
    list-style: none; margin: 0; padding: 0; overflow-y: auto;
  }
  .d-note {
    display: flex; flex-direction: column; align-items: flex-start; gap: 2px;
    padding: 8px 10px; border-radius: 8px; cursor: pointer;
  }
  .d-note:hover { background: var(--surface-soft); }
  .d-note-title { font-size: 13px; color: var(--ink); font-weight: 500; }
  .d-note-meta { font-size: 11px; color: var(--ink-faint); }

  /* 相关实体关系图:填满 graph-col 剩余空间的画布 */
  .ego-wrap {
    flex: 1; min-height: 0;
    border: 1px solid var(--hairline); border-radius: 12px;
    background: var(--surface); overflow: hidden;
  }

  .placeholder, .empty { max-width: 420px; margin: 18vh auto 0; text-align: center; padding: 0 20px; }
  .ph-title, .empty-title { font-weight: 500; color: var(--ink); font-size: 17px; margin: 0 0 8px; }
  .ph-desc, .empty-desc { color: var(--ink-secondary); font-size: 13px; line-height: 1.65; margin: 0 0 18px; }
  .empty-cta {
    background: var(--primary); color: var(--surface); border: 0; border-radius: 8px;
    padding: 7px 15px; font-weight: 500; font-size: 13px; cursor: pointer; font-family: inherit;
  }
  .hint { color: var(--ink-faint); font-size: 13px; text-align: center; margin: 18vh auto 0; }

  /* 图上节点右键菜单:复用 Sidebar.svelte 笔记行右键菜单同一套视觉语言
     (popover 规范 = surface-press 底 + hairline + shadow-popover)。 */
  .menu-overlay {
    position: fixed;
    inset: 0;
    z-index: 40;
  }
  .ctx-menu {
    position: fixed;
    z-index: 41;
    min-width: 9rem;
    background: var(--surface-press);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    padding: 4px;
    display: flex;
    flex-direction: column;
  }
  .ctx-item {
    background: none;
    border: none;
    text-align: left;
    color: var(--ink);
    cursor: pointer;
    padding: 0.4em 0.7em;
    border-radius: var(--radius-md);
    font-size: 0.88rem;
    font-family: inherit;
  }
  .ctx-item:hover {
    background: var(--surface-soft);
  }
  .ctx-rename-input {
    font-size: 0.88rem; font-family: inherit; color: var(--ink);
    background: var(--surface); border: 1px solid var(--accent); border-radius: var(--radius-md);
    padding: 0.4em 0.7em; margin: 0; width: 100%; box-sizing: border-box;
  }
  .ctx-rename-err { font-size: 11px; color: var(--danger-ink); margin: 4px 4px 0; }
</style>
