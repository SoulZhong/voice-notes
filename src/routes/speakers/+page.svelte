<script lang="ts">
  import { onMount } from "svelte";
  import {
    listPeople,
    renamePerson,
    mergePerson,
    deletePerson,
    type PersonSummary,
  } from "$lib/people";
  import { formatDate, formatDuration } from "$lib/notes";

  let people = $state<PersonSummary[]>([]);
  let error = $state("");

  // 开始改名时顺带收起其它行内菜单/确认态，避免同屏多个操作态互相冲突。
  let mergeMenuId = $state<string | null>(null);
  let pendingMerge = $state<{ loser: string; winner: string } | null>(null);
  let confirmDeleteId = $state<string | null>(null);

  const sourceLabel = (s: string) => (s === "mic" ? "麦克风" : s === "system" ? "系统声音" : s);

  /** 名字为空时的展示态：不是真的改名，只是列表怎么显示。 */
  function displayName(p: PersonSummary): string {
    return p.name || `未命名 · 最近 ${formatDate(p.last_seen)}`;
  }

  async function refresh() {
    try {
      people = await listPeople();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }

  onMount(refresh);

  /** 聚焦时收起其它行内菜单/确认态；若当前显示的是"未命名 · 最近 …"占位文本
   *（而非真名），先清空内容再进编辑态——否则直接打字会插在占位文本前后，
   *提交成"未命名 · 最近 7月5日张三"这种把占位串当真名存进库的怪状态。 */
  function nameFocus(e: FocusEvent, p: PersonSummary) {
    mergeMenuId = null;
    pendingMerge = null;
    confirmDeleteId = null;
    if (!p.name) {
      (e.currentTarget as HTMLElement).textContent = "";
    }
  }

  /** 失焦提交：与段落编辑同模式——空文本或未变则还原展示态，不当真改名。 */
  async function nameBlur(e: FocusEvent, p: PersonSummary) {
    const el = e.currentTarget as HTMLElement;
    const text = (el.textContent ?? "").trim();
    const original = displayName(p);
    if (!text || text === original) {
      el.textContent = original;
      return;
    }
    try {
      await renamePerson(p.id, text);
      await refresh();
    } catch (err) {
      el.textContent = original;
      error = `改名失败: ${err}`;
      await refresh();
    }
  }

  async function doMerge() {
    if (!pendingMerge) return;
    const { loser, winner } = pendingMerge;
    pendingMerge = null;
    try {
      await mergePerson(loser, winner);
      await refresh();
    } catch (e) {
      // 录制中后端拒绝等错误文案原样展示。
      error = `${e}`;
    }
  }

  async function doDelete(id: string) {
    confirmDeleteId = null;
    try {
      await deletePerson(id);
      await refresh();
    } catch (e) {
      error = `删除失败: ${e}`;
    }
  }
</script>

<main class="container">
  <h1>说话人</h1>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if people.length === 0}
    <p class="hint">录一场会议,停止后本场说话人会自动出现在这里。</p>
  {:else}
    <ul class="list">
      {#each people as p (p.id)}
        <li class="item">
          <div class="main-line">
            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <span
              class="name editable"
              contenteditable="plaintext-only"
              role="textbox"
              tabindex="0"
              spellcheck="false"
              onfocus={(e) => nameFocus(e, p)}
              onblur={(e) => nameBlur(e, p)}
              onkeydown={(e) => {
                const el = e.currentTarget as HTMLElement;
                if (e.key === "Enter") {
                  e.preventDefault();
                  el.blur();
                }
                if (e.key === "Escape") {
                  el.textContent = displayName(p);
                  el.blur();
                }
              }}>{displayName(p)}</span>
            <span class="meta">
              累计发声 {formatDuration(Math.floor(p.total_ms / 1000))}
              {#each p.sources as s (s)}
                <span class="badge">{sourceLabel(s)}</span>
              {/each}
            </span>
          </div>
          <div class="actions">
            {#if pendingMerge && pendingMerge.loser === p.id}
              {@const target = people.find((o) => o.id === pendingMerge?.winner)}
              <span class="confirm-text">
                确认合并到「{target ? displayName(target) : "?"}」？合并后该人历史笔记显示目标人的名字。
              </span>
              <button class="link danger" onclick={doMerge}>确认合并</button>
              <button class="link" onclick={() => (pendingMerge = null)}>取消</button>
            {:else if mergeMenuId === p.id}
              <span class="menu">
                {#each people.filter((o) => o.id !== p.id) as o (o.id)}
                  <button class="menu-item" onclick={() => (pendingMerge = { loser: p.id, winner: o.id })}>
                    {displayName(o)}
                  </button>
                {/each}
                <button class="menu-item" onclick={() => (mergeMenuId = null)}>取消</button>
              </span>
            {:else}
              <button class="link" disabled={people.length < 2} onclick={() => (mergeMenuId = p.id)}>
                合并到…
              </button>
            {/if}
            {#if confirmDeleteId === p.id}
              <button class="link danger" onclick={() => doDelete(p.id)}>确认删除</button>
              <button class="link" onclick={() => (confirmDeleteId = null)}>取消</button>
            {:else}
              <button class="link" onclick={() => (confirmDeleteId = p.id)}>删除</button>
            {/if}
          </div>
        </li>
      {/each}
    </ul>
  {/if}
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
  }
  h1 {
    margin: 0 0 1rem;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    background: #f5f5f7;
    border-radius: 8px;
  }
  .item {
    padding: 0.75rem 1rem;
    border-bottom: 1px solid #e5e5e7;
  }
  .item:last-child {
    border-bottom: none;
  }
  .main-line {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 1rem;
    flex-wrap: wrap;
  }
  .name.editable {
    font-weight: 600;
    font-size: 1.05em;
    cursor: text;
    border-radius: 4px;
    padding: 0.1em 0.3em;
    margin: -0.1em -0.3em;
  }
  .name.editable:hover {
    background: rgba(57, 108, 216, 0.08);
  }
  .name.editable:focus {
    outline: 2px solid #396cd8;
    background: #fff;
  }
  .meta {
    color: #888;
    font-size: 0.85em;
    white-space: nowrap;
  }
  .badge {
    display: inline-block;
    font-size: 0.85em;
    border-radius: 6px;
    padding: 0.05em 0.5em;
    margin-left: 0.4em;
    background: #e5e5e7;
    color: #555;
  }
  .actions {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.4rem;
    margin-top: 0.4rem;
  }
  .link {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    padding: 0.15em 0.3em;
    font-size: 0.85em;
    box-shadow: none;
  }
  .link:disabled {
    color: #aaa;
    cursor: default;
  }
  .link.danger {
    color: #c0392b;
    font-weight: 600;
  }
  .menu {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 0.25em;
    background: #fff;
    border: 1px solid #ccc;
    border-radius: 8px;
    padding: 0.2em 0.4em;
  }
  .menu-item {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    font-size: 0.85em;
    padding: 0.15em 0.4em;
  }
  .confirm-text {
    font-size: 0.85em;
    color: #8a5a00;
  }
  .banner {
    background: #fff4e5;
    border: 1px solid #f0c98a;
    color: #8a5a00;
    border-radius: 8px;
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .hint {
    color: #aaa;
  }
  @media (prefers-color-scheme: dark) {
    .list {
      background: #2a2a2a;
    }
    .item {
      border-color: #3a3a3a;
    }
    .name.editable:focus {
      background: #2a2a2a;
    }
    .badge {
      background: #3a3a3a;
      color: #ccc;
    }
    .menu {
      background: #2a2a2a;
      border-color: #555;
    }
    .banner {
      background: #3a2e18;
      border-color: #6b5426;
      color: #e8c88a;
    }
    .hint {
      color: #555;
    }
    .confirm-text {
      color: #e8c88a;
    }
  }
</style>
