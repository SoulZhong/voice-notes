<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import { graphData, entityDetail, kindLabel, type EntityDetail, type GraphData } from "$lib/graph";
  import { speakerInk, formatDate } from "$lib/notes";
  import ForceGraph from "$lib/ForceGraph.svelte";

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
    <div class="detail">
      <button class="back" onclick={() => goto("/graph")}>← 返回图谱</button>
      <div class="d-head">
        <span class="d-name">{detail.name}</span>
        <span class="kind">{kindLabel(detail.kind)}</span>
      </div>
      {#if detail.aliases.length}
        <p class="d-aliases">
          别名:{detail.aliases.slice(0, 6).join("、")}{detail.aliases.length > 6
            ? ` 等 ${detail.aliases.length} 个`
            : ""}
        </p>
      {/if}
      <p class="d-stat">出现在 {detail.note_count} 篇 · {detail.mention_total} 次提及</p>

      {#if detail.notes.length}
        <section class="d-section">
          <h3>出现的笔记</h3>
          <ul>
            {#each detail.notes as n (n.id)}
              <!-- svelte-ignore a11y_no_noninteractive_element_interactions, a11y_click_events_have_key_events -->
              <li class="d-note" onclick={() => goto("/notes/" + n.id)}>
                <span class="d-note-title">{n.title}</span>
                <span class="d-note-meta">{formatDate(n.started_at)} · {n.mention_count} 提及</span>
              </li>
            {/each}
          </ul>
        </section>
      {/if}

      {#if detail.related.length}
        <section class="d-section">
          <h3>相关实体</h3>
          <ul>
            {#each detail.related as r (r.id)}
              <!-- svelte-ignore a11y_no_noninteractive_element_interactions, a11y_click_events_have_key_events -->
              <li class="d-rel" onclick={() => pickRelated(r.id)}>
                <span class="dot" style={isPersonId(r.id) ? `background:${speakerInk(r.id, "mic")}` : ""}></span>
                <span class="d-rel-name">{r.name}</span>
                <span class="kind">{kindLabel(r.kind)}</span>
                <span class="d-rel-shared">{r.shared_notes} 篇共现</span>
              </li>
            {/each}
          </ul>
        </section>
      {/if}
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
  .graph-main { height: 100%; overflow-y: auto; }

  /* 单列居中,舒适可读宽度 */
  .detail { max-width: 640px; margin: 0 auto; padding: 28px 36px 48px; }
  .back {
    display: block; margin: 20px 0 18px 36px;
    background: none; border: 0; padding: 0; cursor: pointer;
    font-size: 12.5px; font-weight: 500; color: var(--ink-secondary); font-family: inherit;
  }
  .back:hover { color: var(--ink); }
  .detail .back { margin-left: 0; }
  .d-head { display: flex; align-items: baseline; gap: 10px; margin-bottom: 10px; }
  .d-name { font-size: 20px; font-weight: 500; color: var(--ink); }
  .kind {
    font-size: 11px; color: var(--ink-secondary);
    padding: 1px 7px; border-radius: 5px; background: var(--surface-soft);
  }
  .d-aliases { font-size: 13px; color: var(--ink-secondary); margin: 0 0 6px; line-height: 1.6; }
  .d-stat { font-size: 13px; color: var(--ink-faint); margin: 0 0 26px; }

  .d-section { margin-bottom: 22px; }
  .d-section h3 { font-size: 12px; font-weight: 500; color: var(--ink-secondary); margin: 0 0 8px; }
  .d-section ul { list-style: none; margin: 0; padding: 0; }
  .d-note, .d-rel {
    display: flex; align-items: center; gap: 8px;
    padding: 8px 10px; border-radius: 8px; cursor: pointer;
  }
  .d-note { flex-direction: column; align-items: flex-start; gap: 2px; }
  .d-note:hover, .d-rel:hover { background: var(--surface-soft); }
  .d-note-title { font-size: 13px; color: var(--ink); font-weight: 500; }
  .d-note-meta { font-size: 11px; color: var(--ink-faint); }
  .dot { width: 8px; height: 8px; border-radius: 50%; background: var(--hairline-strong); flex: none; }
  .d-rel-name { font-size: 13px; color: var(--ink); font-weight: 500; }
  .d-rel-shared { margin-left: auto; font-size: 11px; color: var(--ink-faint); }

  .placeholder, .empty { max-width: 420px; margin: 18vh auto 0; text-align: center; padding: 0 20px; }
  .ph-title, .empty-title { font-weight: 500; color: var(--ink); font-size: 17px; margin: 0 0 8px; }
  .ph-desc, .empty-desc { color: var(--ink-secondary); font-size: 13px; line-height: 1.65; margin: 0 0 18px; }
  .empty-cta {
    background: var(--primary); color: var(--surface); border: 0; border-radius: 8px;
    padding: 7px 15px; font-weight: 500; font-size: 13px; cursor: pointer; font-family: inherit;
  }
  .hint { color: var(--ink-faint); font-size: 13px; text-align: center; margin: 18vh auto 0; }
</style>
