<script lang="ts">
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { convertFileSrc } from "@tauri-apps/api/core";
  import {
    listPeople,
    renamePerson,
    mergePerson,
    deletePerson,
    type PersonSummary,
  } from "$lib/people";
  import { formatDate, formatDuration, speakerColor, speakerInk } from "$lib/notes";
  import { recording } from "$lib/recording.svelte";

  // 主从结构的"从":本页只呈现/操作一个人;人物索引在侧栏声纹库页签。
  const personId = $derived($page.params.id);

  let people = $state<PersonSummary[]>([]);
  let loaded = $state(false);
  let error = $state("");

  // 详情数据从全量列表 find:合并菜单本就需要其他人物做目标,一次拉取两用。
  const person = $derived(people.find((p) => p.id === personId) ?? null);
  const others = $derived(
    people
      .filter((p) => p.id !== personId)
      .sort((a, b) => (b.last_seen || "").localeCompare(a.last_seen || "")),
  );

  const sourceLabel = (s: string) => (s === "mic" ? "麦克风" : s === "system" ? "系统声音" : s);

  /** 合并菜单里的展示名(未命名人用全局编号 + 最近出现指认)。 */
  function displayName(p: PersonSummary): string {
    return p.name || `说话人 ${p.id.replace(/^P/, "")} · 最近 ${formatDate(p.last_seen)}`;
  }

  async function refresh() {
    try {
      people = await listPeople();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
    loaded = true;
  }

  // 路由参数变化(侧栏点选另一人)时重载;同页操作后手动 refresh。
  $effect(() => {
    void personId;
    stopSample();
    closeAllOps();
    editingId = null;
    refresh();
  });

  // ── 改名(沿旧管理页语义:未命名给显眼「命名」,已命名点名字改) ──
  let editingId = $state<string | null>(null);
  let editingName = $state("");

  function beginRename() {
    if (!person) return;
    editingId = person.id;
    editingName = person.name;
    closeAllOps();
  }

  async function commitRename() {
    const p = person;
    if (!p || editingId !== p.id) return;
    const text = editingName.trim();
    editingId = null;
    if (!text || text === p.name) return; // 空/未变:静默还原,不当真改名
    try {
      await renamePerson(p.id, text);
      await refresh();
      recording.bumpPeople(); // 侧栏索引同步新名
    } catch (err) {
      error = `改名失败: ${err}`;
      await refresh();
    }
  }

  // ── 合并/删除(同屏只开一个操作态) ──
  let mergeOpen = $state(false);
  let pendingMergeWinner = $state<string | null>(null);
  let confirmDelete = $state(false);

  function closeAllOps() {
    mergeOpen = false;
    pendingMergeWinner = null;
    confirmDelete = false;
  }

  async function doMerge() {
    const winner = pendingMergeWinner;
    if (!person || !winner) return;
    const loser = person.id;
    closeAllOps();
    try {
      await mergePerson(loser, winner);
      recording.bumpPeople();
      // 本人已并入对方:跳到对方详情,让"这个人现在是谁"立即可见。
      goto(`/speakers/${winner}`);
    } catch (e) {
      // 录制中后端拒绝等错误文案原样展示。
      error = `${e}`;
    }
  }

  async function doDelete() {
    if (!person) return;
    confirmDelete = false;
    stopSample();
    try {
      await deletePerson(person.id);
      recording.bumpPeople();
      goto("/speakers"); // 人没了,回概览
    } catch (e) {
      error = `删除失败: ${e}`;
    }
  }

  // ── 录音样本试听(单实例:同一时刻只放一份;换页/离开即停) ──
  let sampleAudio: HTMLAudioElement | null = null;
  /** 正在播放的样本下标;null = 未在播放。多样本时点另一份 = 停旧起新。 */
  let playingIdx = $state<number | null>(null);

  function stopSample() {
    sampleAudio?.pause();
    sampleAudio = null;
    playingIdx = null;
  }

  function toggleSample(idx: number) {
    if (playingIdx === idx) {
      stopSample();
      return;
    }
    stopSample(); // 换份样本:先停当前
    const path = person?.sample_paths[idx];
    if (!path) return;
    const id = person!.id;
    const a = new Audio(convertFileSrc(path));
    a.onended = () => {
      if (playingIdx === idx && personId === id) stopSample();
    };
    sampleAudio = a;
    playingIdx = idx;
    void a.play().catch(() => {
      if (personId === id) stopSample();
    });
  }

  $effect(() => stopSample);
</script>

<main class="container">
  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if !loaded}
    <!-- 首拉中不闪"不存在" -->
  {:else if !person}
    <div class="empty">
      <p>这个人不在「会议搭子」里。</p>
      <p class="hint">可能已被合并或删除。<a href="/speakers">回到会议搭子</a></p>
    </div>
  {:else}
    <header class="head">
      <div class="avatar" style="background: {speakerColor(person.id, 'mic')}; color: {speakerInk(person.id, 'mic')}">
        {#if person.name}
          <span class="initial">{person.name.slice(0, 1)}</span>
        {:else}
          <!-- 人形轮廓:线性 SVG(DESIGN 禁用 emoji) -->
          <svg width="26" height="26" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" aria-hidden="true">
            <circle cx="8" cy="5.2" r="2.7" />
            <path d="M2.8 13.6c.8-2.6 2.8-3.9 5.2-3.9s4.4 1.3 5.2 3.9" />
          </svg>
        {/if}
      </div>

      <div class="head-info">
        <div class="name-line">
          {#if editingId === person.id}
            <!-- svelte-ignore a11y_autofocus -->
            <input
              class="name-input"
              autofocus
              placeholder="输入名字,如 张三"
              bind:value={editingName}
              onkeydown={(e) => {
                if (e.key === "Enter") commitRename();
                if (e.key === "Escape") editingId = null;
              }}
              onblur={commitRename}
            />
          {:else if person.name}
            <button class="name-btn" title="点击改名" onclick={beginRename}>
              <h1>{person.name}</h1>
              <svg class="pencil" width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M11.3 2.4l2.3 2.3L5.3 13l-3 .7.7-3z" />
              </svg>
            </button>
          {:else}
            <h1 class="unnamed">说话人 {person.id.replace(/^P/, "")}</h1>
            <button class="name-cta" onclick={beginRename}>命名</button>
          {/if}
        </div>
        <div class="meta-line">
          最近出现 {formatDate(person.last_seen)} · 累计发声 {formatDuration(Math.floor(person.total_ms / 1000))}
          {#each person.sources as s (s)}
            <span class="badge">{sourceLabel(s)}</span>
          {/each}
        </div>
        {#if !person.name}
          <p class="naming-hint">命名后,之后的录制会自动认出他并直接显示名字。</p>
        {/if}
      </div>
    </header>

    <!-- 试听:确认"这个声纹是谁"的主要手段,给成块的卡而非藏在角标里 -->
    <section class="card">
      <div class="card-title">原声试听</div>
      {#if person.sample_paths.length > 0}
        <div class="listen-row">
          {#each person.sample_paths as _, i (i)}
            <button class="listen" class:playing={playingIdx === i} onclick={() => toggleSample(i)}>
              {#if playingIdx === i}
                <span class="bars" aria-hidden="true"><span></span><span></span><span></span></span>
                停止
              {:else}
                <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
                  <path d="M5 2.9v10.2c0 .7.8 1.2 1.4.8l7.4-5.1c.6-.4.6-1.2 0-1.6L6.4 2.1c-.6-.4-1.4.1-1.4.8z" fill="currentColor" />
                </svg>
                {person.sample_paths.length === 1 ? "播放样本" : `样本 ${i + 1}`}
              {/if}
            </button>
          {/each}
        </div>
        <span class="card-hint">
          听一段这个人的原声,确认声纹认的是谁。{#if person.sample_paths.length > 1}多份样本来自合并带入的不同条目,可逐份核对。{/if}
        </span>
      {:else}
        <span class="card-hint">暂无录音样本:下次录到这个人并停止录制后会自动补上。</span>
      {/if}
    </section>

    <!-- 管理动作:合并/删除,两段确认;录制中后端会拒,前端同步禁用 -->
    <section class="card">
      <div class="card-title">管理</div>
      <div class="ops">
        <div class="merge-anchor">
          <button
            class="op-btn"
            disabled={others.length === 0 || recording.isLive}
            title={recording.isLive ? "录制中不能合并" : others.length === 0 ? "库里没有其他人可并入" : "认错拆重了,把这个人归并到另一个人"}
            onclick={() => {
              const opening = !mergeOpen;
              closeAllOps();
              mergeOpen = opening;
            }}
          >
            合并到…
          </button>
          {#if mergeOpen && !pendingMergeWinner}
            <div class="menu">
              <div class="menu-title">把「{displayName(person)}」并入…</div>
              {#each others as o (o.id)}
                <button class="menu-item" onclick={() => (pendingMergeWinner = o.id)}>
                  <span class="menu-dot" style="background: {speakerColor(o.id, 'mic')}"></span>
                  {displayName(o)}
                </button>
              {/each}
            </div>
          {:else if pendingMergeWinner}
            {@const target = people.find((o) => o.id === pendingMergeWinner)}
            <div class="menu confirm">
              <div class="menu-title">
                并入「{target ? displayName(target) : "?"}」?合并后这个人历史笔记都显示对方的名字,不可撤销。
              </div>
              <div class="confirm-row">
                <button class="mini danger" onclick={doMerge}>确认合并</button>
                <button class="mini" onclick={closeAllOps}>取消</button>
              </div>
            </div>
          {/if}
        </div>

        {#if confirmDelete}
          <div class="confirm-inline">
            <button class="mini danger" onclick={doDelete}>确认删除</button>
            <button class="mini" onclick={() => (confirmDelete = false)}>取消</button>
          </div>
        {:else}
          <button
            class="op-btn danger-hover"
            disabled={recording.isLive}
            title={recording.isLive ? "录制中不能删除" : "从搭子中删除(不影响已有笔记文字)"}
            onclick={() => {
              closeAllOps();
              confirmDelete = true;
            }}
          >
            从搭子中删除
          </button>
        {/if}
      </div>
    </section>
  {/if}
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 44rem;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 1.1rem;
    margin-bottom: 1.25rem;
  }
  /* 大头像:56px,soft 底 + 配对文字色(与侧栏色点/徽章同色源) */
  .avatar {
    width: 56px;
    height: 56px;
    border-radius: 50%;
    flex: none;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .initial {
    font-size: 1.5rem;
    font-weight: 500;
  }
  .head-info {
    min-width: 0;
  }
  .name-line {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    min-height: 2rem;
  }
  h1 {
    margin: 0;
    display: inline;
  }
  /* editable-text:静态无边,hover accent-tint 底 */
  .name-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.4em;
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    border-radius: var(--radius-sm);
    padding: 0.05em 0.35em;
    margin: -0.05em -0.35em;
  }
  .name-btn:hover {
    background: var(--accent-tint);
  }
  .pencil {
    color: var(--ink-faint);
  }
  .name-btn:hover .pencil {
    color: var(--accent);
  }
  .unnamed {
    font-style: italic;
    color: var(--ink-faint);
  }
  .name-cta {
    background: var(--primary);
    color: var(--on-primary);
    border: none;
    border-radius: var(--radius-full);
    padding: 0.2em 0.9em;
    font-size: 0.88rem;
    font-weight: 500;
    cursor: pointer;
    box-shadow: var(--shadow-btn);
  }
  .name-cta:hover {
    background: var(--primary-pressed);
  }
  .name-input {
    font: inherit;
    font-weight: 500;
    font-size: 1.2rem;
    border: 1px solid var(--accent);
    border-radius: var(--radius-md);
    background: var(--canvas);
    color: var(--ink);
    padding: 0.1em 0.4em;
    min-width: 13rem;
  }
  .meta-line {
    color: var(--ink-faint);
    font-size: 0.85rem;
    margin-top: 0.3rem;
    display: flex;
    align-items: center;
    gap: 0.4rem;
    flex-wrap: wrap;
  }
  .badge {
    font-size: 0.78rem;
    border-radius: var(--radius-md);
    padding: 0.02em 0.5em;
    background: var(--surface-press);
    color: var(--ink-secondary);
  }
  .naming-hint {
    color: var(--ink-secondary);
    font-size: 0.82rem;
    margin: 0.4rem 0 0;
  }
  /* 卡片区块:surface 底 + rounded-lg,与全应用卡片规范一致 */
  .card {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.9rem 1rem;
    margin-bottom: 0.9rem;
    display: flex;
    align-items: center;
    gap: 0.8rem;
    flex-wrap: wrap;
  }
  .card-title {
    font-size: 0.82rem;
    font-weight: 500;
    color: var(--ink-secondary);
    flex: none;
    min-width: 4.5rem;
  }
  .card-hint {
    color: var(--ink-faint);
    font-size: 0.82rem;
  }
  /* 多份样本按钮排一行,窄卡自动换行 */
  .listen-row {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    margin-bottom: 0.4rem;
  }
  /* 试听按钮:secondary 形态,播放中 accent 文字 + 跳动条 */
  .listen {
    display: inline-flex;
    align-items: center;
    gap: 0.45em;
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    border-radius: var(--radius-md);
    font-size: 0.85rem;
    font-weight: 500;
    padding: 0.3em 0.9em;
    cursor: pointer;
  }
  .listen:hover {
    background: var(--surface-soft);
  }
  .listen.playing {
    color: var(--accent);
    border-color: var(--accent);
  }
  .bars {
    display: inline-flex;
    align-items: flex-end;
    gap: 2.5px;
    height: 12px;
  }
  .bars span {
    width: 2.5px;
    border-radius: 1px;
    background: currentColor;
    animation: eq 0.9s ease-in-out infinite;
  }
  .bars span:nth-child(1) { height: 60%; animation-delay: 0s; }
  .bars span:nth-child(2) { height: 100%; animation-delay: 0.25s; }
  .bars span:nth-child(3) { height: 75%; animation-delay: 0.5s; }
  @keyframes eq {
    0%, 100% { transform: scaleY(0.5); }
    50% { transform: scaleY(1); }
  }
  .ops {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    flex-wrap: wrap;
  }
  /* 操作按钮:secondary 形态;删除 hover 变 danger */
  .op-btn {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    border-radius: var(--radius-md);
    font-size: 0.85rem;
    font-weight: 500;
    padding: 0.3em 0.9em;
    cursor: pointer;
  }
  .op-btn:hover:not(:disabled) {
    background: var(--surface-soft);
  }
  .op-btn:disabled {
    color: var(--ink-faint);
    cursor: default;
  }
  .op-btn.danger-hover:hover:not(:disabled) {
    color: var(--danger);
    border-color: var(--danger);
    background: transparent;
  }
  /* menu/popover:surface-press 底(暗色下 canvas 会成"洞") */
  .merge-anchor {
    position: relative;
  }
  .menu {
    position: absolute;
    top: calc(100% + 4px);
    left: 0;
    z-index: 10;
    min-width: 16rem;
    max-height: 16rem;
    overflow-y: auto;
    background: var(--surface-press);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    padding: 0.35rem;
  }
  .menu-title {
    color: var(--ink-secondary);
    font-size: 0.78rem;
    line-height: 1.45;
    padding: 0.25rem 0.5rem 0.35rem;
  }
  .menu-item {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    color: var(--ink);
    font-size: 0.85rem;
    padding: 0.35rem 0.5rem;
    border-radius: var(--radius-md);
    cursor: pointer;
  }
  .menu-item:hover {
    background: var(--surface-soft);
  }
  .menu-dot {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    flex: none;
  }
  .menu.confirm .menu-title {
    color: var(--warning-ink);
  }
  .confirm-row {
    display: flex;
    gap: 0.4rem;
    padding: 0.15rem 0.5rem 0.25rem;
  }
  .confirm-inline {
    display: flex;
    gap: 0.4rem;
    align-items: center;
  }
  .mini {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    border-radius: var(--radius-md);
    font-size: 0.8rem;
    padding: 0.2em 0.7em;
    cursor: pointer;
  }
  .mini:hover {
    background: var(--surface-soft);
  }
  .mini.danger {
    border-color: var(--danger);
    color: var(--danger);
    font-weight: 500;
  }
  .mini.danger:hover {
    background: var(--danger);
    color: var(--on-record);
  }
  .empty {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 2rem 1.5rem;
    text-align: center;
  }
  .empty p {
    margin: 0 0 0.4rem;
    font-weight: 500;
  }
  .empty a {
    color: var(--accent);
  }
  .banner {
    background: var(--danger-tint);
    border: 1px solid var(--danger-line);
    color: var(--danger-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin: 0 0 1rem;
    font-size: 0.95rem;
  }
  .hint {
    color: var(--ink-faint);
    font-weight: 400;
  }
</style>
