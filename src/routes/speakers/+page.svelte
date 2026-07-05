<script lang="ts">
  import { onMount } from "svelte";
  import { convertFileSrc } from "@tauri-apps/api/core";
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

  /** 合并菜单里的展示名(未命名人也要能被当成合并目标指认)。 */
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

  // 显式编辑态(冒烟反馈:占位文本 contenteditable 让"能改名"完全不可发现)——
  // 未命名人给显眼的「命名」按钮,已命名人名字带 ✎ 角标;点击换成真输入框。
  let editingId = $state<string | null>(null);
  let editingName = $state("");

  function beginRename(p: PersonSummary) {
    editingId = p.id;
    editingName = p.name;
    mergeMenuId = null;
    pendingMerge = null;
    confirmDeleteId = null;
  }

  async function commitRename(p: PersonSummary) {
    if (editingId !== p.id) return;
    const text = editingName.trim();
    editingId = null;
    if (!text || text === p.name) return; // 空/未变:静默还原,不当真改名
    try {
      await renamePerson(p.id, text);
      await refresh();
    } catch (err) {
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
    if (samplePlayingId === id) stopSample();
    try {
      await deletePerson(id);
      await refresh();
    } catch (e) {
      error = `删除失败: ${e}`;
    }
  }

  // 录音样本试听:全页共享一个 Audio 实例,点击切换;换人先停上一个。
  let sampleAudio: HTMLAudioElement | null = null;
  let samplePlayingId = $state<string | null>(null);

  function stopSample() {
    sampleAudio?.pause();
    sampleAudio = null;
    samplePlayingId = null;
  }

  function toggleSample(p: PersonSummary) {
    if (samplePlayingId === p.id) {
      stopSample();
      return;
    }
    stopSample();
    if (!p.sample_path) return;
    const a = new Audio(convertFileSrc(p.sample_path));
    a.onended = () => {
      if (samplePlayingId === p.id) stopSample();
    };
    sampleAudio = a;
    samplePlayingId = p.id;
    // 迟到的 play() 拒绝(文件已删/加载失败)只清自己:无身份守卫会把用户随后
    // 点播的另一个人的样本一并停掉。
    void a.play().catch(() => {
      if (samplePlayingId === p.id) stopSample();
    });
  }

  // 离开页面停播,不留幽灵声音。
  $effect(() => stopSample);
</script>

<main class="container">
  <h1>声纹库</h1>
  <p class="desc">
    这里是声纹库:录到的说话人会自动登记。给"未命名"的人<strong>命名</strong>后,
    之后的录制会自动认出他并直接显示名字;认错拆重了就用<strong>合并</strong>归到同一个人。
  </p>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if people.length === 0}
    <p class="hint">还没有说话人。录一场会议(单人说话累计满 10 秒),停止后会自动出现在这里。</p>
  {:else}
    <ul class="list">
      {#each people as p (p.id)}
        <li class="item">
          <div class="main-line">
            {#if editingId === p.id}
              <!-- svelte-ignore a11y_autofocus -->
              <input
                class="name-input"
                autofocus
                placeholder="输入名字,如 张三"
                bind:value={editingName}
                onkeydown={(e) => {
                  if (e.key === "Enter") commitRename(p);
                  if (e.key === "Escape") editingId = null;
                }}
                onblur={() => commitRename(p)}
              />
            {:else if p.name}
              <button class="name-btn" title="点击改名" onclick={() => beginRename(p)}>
                {p.name}<span class="pencil">✎</span>
              </button>
            {:else}
              <span class="unnamed">未命名</span>
              <button class="name-cta" onclick={() => beginRename(p)}>命名</button>
            {/if}
            <span class="meta">
              {#if !p.name}最近 {formatDate(p.last_seen)} · {/if}累计发声 {formatDuration(Math.floor(p.total_ms / 1000))}
              {#each p.sources as s (s)}
                <span class="badge">{sourceLabel(s)}</span>
              {/each}
            </span>
          </div>
          <div class="actions">
            <!-- 纯文字标签:DESIGN.md 禁用 ▶/⏸ 等 Unicode 符号字符 -->
            {#if p.sample_path}
              <button class="link" onclick={() => toggleSample(p)}>
                {samplePlayingId === p.id ? "停止" : "试听"}
              </button>
            {/if}
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
  /* list-row 容器：以 surface 卡片形式承载各行，行内分隔见 .item */
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    background: var(--surface);
    border-radius: var(--radius-lg);
  }
  /* list-row：透明底 + 行间 hairline 分隔，hover surface-soft */
  .item {
    padding: 0.75rem 1rem;
    border-bottom: 1px solid var(--hairline);
  }
  .item:last-child {
    border-bottom: none;
  }
  .item:hover {
    background: var(--surface-soft);
  }
  .main-line {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 1rem;
    flex-wrap: wrap;
  }
  /* desc 用 caption 色阶（次要说明文字） */
  .desc {
    color: var(--ink-secondary);
    font-size: 0.85rem;
    line-height: 1.45;
    margin: -0.5rem 0 1rem;
    max-width: 46rem;
  }
  /* editable-text（名字）：静态无边，hover accent-tint 底 + rounded-sm */
  .name-btn {
    background: none;
    border: none;
    font: inherit;
    font-weight: 600;
    font-size: 1.05em;
    color: inherit;
    cursor: pointer;
    border-radius: var(--radius-sm);
    padding: 0.1em 0.3em;
    margin: -0.1em -0.3em;
  }
  .name-btn:hover {
    background: var(--accent-tint);
  }
  /* 已命名说话人的 ✎ 角标：ink-faint，hover 变 accent */
  .pencil {
    color: var(--ink-faint);
    font-size: 0.8em;
    margin-left: 0.35em;
  }
  .name-btn:hover .pencil {
    color: var(--accent);
  }
  .unnamed {
    font-weight: 600;
    font-size: 1.05em;
    font-style: italic;
    color: var(--ink-faint);
  }
  /* button-primary：命名是本行唯一主动作 */
  .name-cta {
    background: var(--accent);
    color: var(--on-accent);
    border: none;
    border-radius: var(--radius-md);
    padding: 0.15em 0.8em;
    font-size: 0.9rem; /* button 字级 token,与全局按钮对齐 */
    font-weight: 500;
    cursor: pointer;
    margin-left: 0.5em;
  }
  .name-cta:hover {
    background: var(--accent-pressed);
  }
  .name-input {
    font: inherit;
    font-weight: 600;
    font-size: 1.05em;
    border: 1px solid var(--accent);
    border-radius: var(--radius-md);
    background: var(--canvas);
    color: var(--ink);
    padding: 0.1em 0.4em;
    min-width: 12rem;
  }
  .meta {
    color: var(--ink-faint);
    font-size: 0.85em;
    white-space: nowrap;
  }
  .badge {
    display: inline-block;
    font-size: 0.85em;
    border-radius: var(--radius-md);
    padding: 0.05em 0.5em;
    margin-left: 0.4em;
    background: var(--surface-press);
    color: var(--ink-secondary);
  }
  .actions {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.4rem;
    margin-top: 0.4rem;
  }
  /* button-link：无底无边，accent 字，悬停加下划线 */
  .link {
    background: none;
    border: none;
    color: var(--accent);
    cursor: pointer;
    padding: 0.15em 0.3em;
    font-size: 0.85em;
  }
  .link:hover {
    text-decoration: underline;
  }
  .link:disabled {
    color: var(--ink-faint);
    cursor: default;
  }
  .link.danger {
    color: var(--danger);
    font-weight: 600;
  }
  /* menu/popover（合并目标下拉）：canvas 底、hairline 边、rounded-lg、shadow-popover */
  .menu {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 0.25em;
    background: var(--canvas);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    padding: 0.2em 0.4em;
  }
  .menu-item {
    background: none;
    border: none;
    color: var(--accent);
    cursor: pointer;
    font-size: 0.85em;
    padding: 0.15em 0.4em;
  }
  .confirm-text {
    font-size: 0.85em;
    color: var(--warning-ink);
  }
  /* 此处 banner 只用于加载/改名/合并/删除失败，用 danger 色系 */
  .banner {
    background: var(--danger-tint);
    border: 1px solid var(--danger-line);
    color: var(--danger-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .hint {
    color: var(--ink-faint);
  }
</style>
