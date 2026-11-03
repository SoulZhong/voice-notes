<script lang="ts">
  // AI 调用日志独立页:可浏览(类别过滤+分页加载+详情展开),可导出 JSONL,
  // 可在访达中打开日志目录。数据真值源是 data_root/ai_logs/,现查现示。
  import { onMount } from "svelte";
  import { aiLogsQuery, aiLogsExport, aiLogsOpenDir, AI_LOG_KIND_LABELS, type AiLogEntry } from "$lib/ailog";
  import { t } from "$lib/i18n/index.svelte";
  import { Button } from "$lib/ui/components/button";
  import { Badge } from "$lib/ui/components/badge";
  import * as ToggleGroup from "$lib/ui/components/toggle-group";

  const PAGE_SIZE = 50;
  // label 存 i18n 键,渲染处 t() 取值。
  const KIND_TABS = [
    { key: "", label: "ai.logs.kind.all" },
    ...Object.entries(AI_LOG_KIND_LABELS).map(([key, label]) => ({ key, label })),
  ];

  // ToggleGroup 单选把空串当「未选中」,「全部」(kind = "")在控件层用哨兵值代表。
  const ALL_KIND = "__all__";

  let entries = $state<AiLogEntry[]>([]);
  let total = $state(0);
  let kind = $state("");
  let busy = $state(false);
  let error = $state("");
  let notice = $state(""); // 导出结果等一次性提示
  let expandedId = $state<string | null>(null);

  async function load(reset: boolean) {
    busy = true;
    error = "";
    try {
      const page = await aiLogsQuery({
        kind: kind || undefined,
        offset: reset ? 0 : entries.length,
        limit: PAGE_SIZE,
      });
      entries = reset ? page.entries : [...entries, ...page.entries];
      total = page.total;
      if (reset) expandedId = null;
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  function setKind(k: string) {
    kind = k;
    notice = "";
    load(true);
  }

  async function doExport() {
    busy = true;
    error = "";
    try {
      const r = await aiLogsExport();
      notice = t("ai.logs.exported", { count: r.count, path: r.path });
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function openDir() {
    error = "";
    try {
      await aiLogsOpenDir();
    } catch (e) {
      error = String(e);
    }
  }

  /** "2026-07-12T11:51:40.952+08:00" → "2026-07-12 11:51" */
  const fmtTs = (ts: string) => ts.slice(0, 16).replace("T", " ");

  onMount(() => {
    load(true);
  });
</script>

<div class="page">
  <header class="topbar">
    <h1>{t("ai.logs.sectionTitle")}</h1>
    <div class="topbar-actions">
      <Button variant="outline" size="sm" onclick={openDir}>{t("ai.logs.openDir")}</Button>
      <Button variant="outline" size="sm" disabled={busy || total === 0} onclick={doExport}>{t("ai.logs.export")}</Button>
    </div>
  </header>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  <div class="toolbar">
    <ToggleGroup.Root
      type="single"
      spacing={1}
      class="flex-none rounded-md bg-[var(--surface-press)] p-0.5"
      value={kind || ALL_KIND}
      onValueChange={(v) => {
        if (!v) return; // 再点已选项:保持不变
        const next = v === ALL_KIND ? "" : v;
        if (next !== kind) setKind(next);
      }}
    >
      {#each KIND_TABS as tab (tab.key)}
        <ToggleGroup.Item
          value={tab.key || ALL_KIND}
          class="h-auto min-w-0 rounded-md px-[0.7em] py-[0.26em] text-[0.85rem] text-muted-foreground hover:bg-transparent data-[state=on]:bg-background data-[state=on]:text-foreground data-[state=on]:shadow-[var(--shadow-btn)]"
        >{t(tab.label)}</ToggleGroup.Item>
      {/each}
    </ToggleGroup.Root>
    <span class="toolbar-hint">
      {#if notice}{notice}{:else}{t("ai.logs.count", { total, loaded: entries.length })}{/if}
    </span>
  </div>

  <div class="rows">
    {#if entries.length === 0}
      <p class="empty">{t("ai.logs.empty")}</p>
    {:else}
      {#each entries as e (e.id)}
        <div class="row">
          <div class="row-info">
            <span class="row-label-line">
              <span class="row-label">{AI_LOG_KIND_LABELS[e.kind] ? t(AI_LOG_KIND_LABELS[e.kind]) : e.kind}</span>
              {#if e.status !== "ok"}<Badge variant="outline" class="rounded-sm border-[var(--warning-line)] bg-[var(--warning-tint)] text-[var(--warning-ink)]">{t("ai.logs.failed")}</Badge>{/if}
              {#if e.truncated}<Badge variant="outline" class="rounded-sm bg-muted text-muted-foreground">{t("ai.logs.truncated")}</Badge>{/if}
            </span>
            <span class="row-desc">
              {fmtTs(e.ts)} · {e.provider}{e.model ? ` · ${e.model}` : ""} · {e.duration_ms}ms{e.note_id
                ? ` · ${e.note_id}`
                : ""}{#if e.status !== "ok" && e.error}<span class="desc-warn"> · {e.error.slice(0, 80)}</span>{/if}
            </span>
          </div>
          <Button variant="link" size="sm" class="h-auto shrink-0 px-0 text-ring" onclick={() => (expandedId = expandedId === e.id ? null : e.id)}>
            {expandedId === e.id ? t("ai.action.collapse") : t("ai.logs.detail")}
          </Button>
        </div>
        {#if expandedId === e.id}
          <pre class="snippet log-detail">{JSON.stringify(e, null, 2)}</pre>
        {/if}
      {/each}
    {/if}
    {#if entries.length < total}
      <div class="row load-more">
        <Button variant="outline" size="sm" disabled={busy} onclick={() => load(false)}>
          {t("ai.logs.loadMore", { n: total - entries.length })}
        </Button>
      </div>
    {/if}
  </div>
</div>

<style>
  .page { padding: 0 1.5rem 2rem; }
  .topbar {
    position: sticky;
    top: 0;
    z-index: 1;
    display: flex;
    align-items: center;
    justify-content: space-between;
    background: var(--canvas);
    padding: 1.1rem 0 0.6rem;
  }
  h1 { font-size: 1.15rem; font-weight: 600; margin: 0; }
  .topbar-actions { display: flex; gap: 0.5rem; }

  .banner {
    background: var(--danger-tint, var(--warning-tint));
    border: 1px solid var(--danger, var(--warning-line));
    color: var(--danger, var(--warning-ink));
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.9rem;
  }

  .toolbar {
    display: flex;
    align-items: center;
    gap: 0.9rem;
    flex-wrap: wrap;
    margin: 0.2rem 0 0.7rem;
  }
  .toolbar-hint {
    font-size: 0.8rem;
    color: var(--ink-faint);
    min-width: 0;
    overflow-wrap: anywhere;
  }

  /* 以下形态与 /ai 页同一语言(rows/snippet);分段、徽标、按钮已换组件 */

  .rows {
    background: var(--surface);
    border-radius: var(--radius-lg);
    overflow: hidden;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.9rem;
    padding: 0.55rem 1rem;
    border-bottom: 1px solid var(--hairline);
  }
  .rows > :last-child,
  .rows .row:last-child { border-bottom: none; }
  .row-info { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 0.1rem; }
  .row-label { font-size: 0.92rem; font-weight: 500; color: var(--ink); }
  .row-label-line { display: flex; align-items: center; gap: 0.4rem; }
  .row-desc { font-size: 0.8rem; color: var(--ink-secondary); line-height: 1.4; overflow-wrap: anywhere; }
  .desc-warn { color: var(--warning-ink); }



  .snippet {
    margin: 0 1rem 0.6rem;
    padding: 0.6rem 0.8rem;
    background: var(--surface-soft);
    border-radius: var(--radius-sm);
    font-size: 0.8rem;
    user-select: text;
  }
  .log-detail {
    max-height: 24rem;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-all;
  }

  .empty {
    margin: 0;
    padding: 1.4rem 1rem;
    font-size: 0.85rem;
    color: var(--ink-faint);
    text-align: center;
  }
  .load-more { justify-content: center; }

</style>
