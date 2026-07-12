<script lang="ts">
  // AI 调用日志独立页:可浏览(类别过滤+分页加载+详情展开),可导出 JSONL,
  // 可在访达中打开日志目录。数据真值源是 data_root/ai_logs/,现查现示。
  import { onMount } from "svelte";
  import { aiLogsQuery, aiLogsExport, aiLogsOpenDir, AI_LOG_KIND_LABELS, type AiLogEntry } from "$lib/ailog";

  const PAGE_SIZE = 50;
  const KIND_TABS = [
    { key: "", label: "全部" },
    ...Object.entries(AI_LOG_KIND_LABELS).map(([key, label]) => ({ key, label })),
  ];

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
      notice = `已导出 ${r.count} 条 → ${r.path}`;
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
    <h1>AI 调用日志</h1>
    <div class="topbar-actions">
      <button class="btn-secondary" onclick={openDir}>打开日志目录</button>
      <button class="btn-secondary" disabled={busy || total === 0} onclick={doExport}>导出 JSONL</button>
    </div>
  </header>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  <div class="toolbar">
    <div class="seg">
      {#each KIND_TABS as t (t.key)}
        <label class="seg-item">
          <input type="radio" name="log-kind" value={t.key} checked={kind === t.key} onchange={() => setKind(t.key)} />
          {t.label}
        </label>
      {/each}
    </div>
    <span class="toolbar-hint">
      {#if notice}{notice}{:else}共 {total} 条,已加载 {entries.length} 条{/if}
    </span>
  </div>

  <div class="rows">
    {#if entries.length === 0}
      <p class="empty">暂无记录。精修与标题生成的每次对外 AI 调用(请求与响应全量)都会在这里留痕。</p>
    {:else}
      {#each entries as e (e.id)}
        <div class="row">
          <div class="row-info">
            <span class="row-label-line">
              <span class="row-label">{AI_LOG_KIND_LABELS[e.kind] ?? e.kind}</span>
              {#if e.status !== "ok"}<span class="pill warn">失败</span>{/if}
              {#if e.truncated}<span class="pill">超长已截断</span>{/if}
            </span>
            <span class="row-desc">
              {fmtTs(e.ts)} · {e.provider}{e.model ? ` · ${e.model}` : ""} · {e.duration_ms}ms{e.note_id
                ? ` · ${e.note_id}`
                : ""}{#if e.status !== "ok" && e.error}<span class="desc-warn"> · {e.error.slice(0, 80)}</span>{/if}
            </span>
          </div>
          <button class="link" onclick={() => (expandedId = expandedId === e.id ? null : e.id)}>
            {expandedId === e.id ? "收起" : "详情"}
          </button>
        </div>
        {#if expandedId === e.id}
          <pre class="snippet log-detail">{JSON.stringify(e, null, 2)}</pre>
        {/if}
      {/each}
    {/if}
    {#if entries.length < total}
      <div class="row load-more">
        <button class="btn-secondary" disabled={busy} onclick={() => load(false)}>
          加载更多(剩 {total - entries.length} 条)
        </button>
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

  /* 以下控件形态与 /ai 页同一语言(seg/rows/pill/snippet) */
  .seg {
    display: flex;
    gap: 2px;
    flex: none;
    background: var(--surface-press);
    border-radius: var(--radius-md);
    padding: 2px;
  }
  .seg-item {
    position: relative;
    padding: 0.26em 0.7em;
    font-size: 0.85rem;
    font-weight: 500;
    color: var(--ink-secondary);
    border-radius: calc(var(--radius-md) - 2px);
    cursor: pointer;
    white-space: nowrap;
  }
  .seg-item:hover { color: var(--ink); }
  .seg-item:has(input:checked) {
    background: var(--canvas);
    color: var(--ink);
    box-shadow: var(--shadow-btn);
  }
  .seg-item input { position: absolute; opacity: 0; pointer-events: none; }

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

  .pill {
    flex: none;
    font-size: 0.78rem;
    font-weight: 500;
    border-radius: var(--radius-sm);
    padding: 0.1em 0.5em;
    background: var(--surface-soft);
    color: var(--ink-secondary);
    border: 1px solid var(--hairline);
    white-space: nowrap;
  }
  .pill.warn {
    background: var(--warning-tint);
    border-color: var(--warning-line);
    color: var(--warning-ink);
  }

  .link {
    flex: none;
    border: none;
    background: none;
    color: var(--accent);
    font-size: 0.85rem;
    cursor: pointer;
    padding: 0;
  }
  .link:hover { text-decoration: underline; }

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

  .btn-secondary {
    flex: none;
    border-radius: var(--radius-md);
    border: 1px solid var(--hairline-strong);
    padding: 0.35em 0.9em;
    font-size: 0.85rem;
    font-weight: 500;
    cursor: pointer;
    background: transparent;
    color: var(--ink);
  }
  .btn-secondary:hover { background: var(--surface-soft); }
  .btn-secondary:disabled { opacity: 0.5; cursor: default; background: transparent; }
</style>
