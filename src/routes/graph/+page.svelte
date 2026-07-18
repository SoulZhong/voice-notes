<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import { graphData, entityDetail, renameEntity, kindLabel, type EntityDetail, type GraphData, type EntitySummary, type EdgeRow } from "$lib/graph";
  import { formatDate } from "$lib/notes";
  import ForceGraph from "$lib/ForceGraph.svelte";

  // 改名(纠 ASR 提取错的实体名)。
  let renaming = $state(false);
  let renameValue = $state("");
  let renameErr = $state("");
  let renameBusy = $state(false);
  function startRename(d: EntityDetail) {
    renameValue = d.name;
    renameErr = "";
    renaming = true;
  }
  async function submitRename(oldId: string) {
    const name = renameValue.trim();
    if (!name || renameBusy) return;
    renameBusy = true;
    try {
      const r = await renameEntity(oldId, name);
      renaming = false;
      if (r.new_id === oldId) {
        // id 没变(人实体改名 / 纯大小写归一后相同):URL 没变,goto 不会重新拉取,手动刷新。
        detail = await entityDetail(r.new_id).catch(() => detail);
      } else {
        goto("/graph?e=" + encodeURIComponent(r.new_id));
      }
    } catch (e) {
      renameErr = `改名失败: ${e}`;
    } finally {
      renameBusy = false;
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
        <span class="kind">{kindLabel(detail.kind)}</span>
        {#if !renaming}
          <button class="d-rename-btn" onclick={() => startRename(d)} title="改名(纠正提取错误)">改名</button>
        {/if}
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
    <ForceGraph nodes={graph.nodes} edges={graph.edges} onPick={pickNode} />
  {:else}
    <div class="placeholder">
      <p class="ph-title">知识图谱</p>
      <p class="ph-desc">从左侧选择一个实体,查看它出现在哪些笔记、和谁一起被提到。</p>
    </div>
  {/if}
</div>

<style>
  .graph-main { height: 100%; overflow: hidden; }

  /* 撑满可用高度(不再是纵向长滚的文档流):居中但拓宽上限,让相关实体力导图有
     足够画布——纯文字列表挤在窄列里既浪费横向空间、又逼出大片纵向留白。 */
  .detail {
    display: flex; flex-direction: column;
    height: 100%; max-width: 1400px; margin: 0 auto;
    padding: 24px 36px 28px; box-sizing: border-box;
  }
  .back {
    display: block; flex: none; margin: 0 0 16px;
    background: none; border: 0; padding: 0; cursor: pointer;
    font-size: 12.5px; font-weight: 500; color: var(--ink-secondary); font-family: inherit;
  }
  .back:hover { color: var(--ink); }
  .d-head { flex: none; display: flex; align-items: baseline; gap: 10px; margin-bottom: 10px; }
  .d-rename-input {
    font-size: 20px; font-weight: 500; color: var(--ink); font-family: inherit;
    background: var(--surface-soft); border: 1px solid var(--accent); border-radius: 6px;
    padding: 1px 6px; min-width: 8em;
  }
  .d-rename-btn {
    background: none; border: 0; padding: 0; cursor: pointer; margin-left: auto;
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
</style>
