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
  import { formatDate, formatDuration, speakerColor, speakerInk } from "$lib/notes";

  let people = $state<PersonSummary[]>([]);
  let error = $state("");

  // 同屏只允许一个操作态:开一个就收起其它(改名/合并菜单/删除确认/合并确认)。
  let mergeMenuId = $state<string | null>(null);
  let pendingMerge = $state<{ loser: string; winner: string } | null>(null);
  let confirmDeleteId = $state<string | null>(null);

  const sourceLabel = (s: string) => (s === "mic" ? "麦克风" : s === "system" ? "系统声音" : s);

  /** 合并菜单里的展示名(未命名人也要能被当成合并目标指认)。 */
  function displayName(p: PersonSummary): string {
    return p.name || `未命名 · 最近 ${formatDate(p.last_seen)}`;
  }

  /** 头像粉彩底/文字:统一走 $lib/notes 的 speakerColor/speakerInk(与说话人徽章同一套
      soft 公式)。注意 id 是 P<n> 形态(本地曾按 "^P" 剥前缀数值循环),而 notes.ts 的
      索引逻辑剥的是 "^S" 前缀——对 P<n> 不命中数值分支,统一后退化为字符串散列兜底
      (仍确定性、每人色不变,但不再是 P1/P2/P3.. 顺序循环取色;差异已在任务报告中记录)。
      source 参数在此页无意义(id 恒真值),固定传 "mic"。 */
  const avatarTint = (id: string) => speakerColor(id, "mic");
  const avatarInk = (id: string) => speakerInk(id, "mic");

  /** 最近出现的人排前面(BTreeMap 原序是 P1..Pn,对使用者没有意义)。 */
  const sorted = $derived(
    [...people].sort((a, b) => (b.last_seen || "").localeCompare(a.last_seen || "")),
  );
  /** 分组:未命名的是"待处理项"排上面,已命名的是稳定资产。 */
  const unnamed = $derived(sorted.filter((p) => !p.name));
  const named = $derived(sorted.filter((p) => p.name));

  async function refresh() {
    try {
      people = await listPeople();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }

  onMount(refresh);

  // 显式编辑态:未命名人给显眼的「命名」按钮,已命名人点名字改;点击换成真输入框。
  let editingId = $state<string | null>(null);
  let editingName = $state("");

  function closeAllOps() {
    mergeMenuId = null;
    pendingMerge = null;
    confirmDeleteId = null;
  }

  function beginRename(p: PersonSummary) {
    editingId = p.id;
    editingName = p.name;
    closeAllOps();
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
    mergeMenuId = null;
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
    录到的说话人会自动登记。给"未命名"的人<strong>命名</strong>后,之后的录制会自动认出他并直接显示名字;
    认错拆重了就用<strong>合并</strong>归到同一个人。点<strong>试听</strong>可以听一段他的原声确认是谁。
  </p>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if people.length === 0}
    <div class="empty">
      <p>还没有说话人。</p>
      <p class="hint">录一场会议(单人说话累计满 10 秒),停止后会自动出现在这里。</p>
    </div>
  {:else}
    {#if unnamed.length > 0}
      <div class="section-head">
        <h2 class="section-title">待命名<span class="count">{unnamed.length}</span></h2>
        <span class="section-hint">命名后,之后的录制会自动认出并直接显示名字</span>
      </div>
      <ul class="list">
        {#each unnamed as p (p.id)}
          {@render personRow(p)}
        {/each}
      </ul>
    {/if}

    {#if named.length > 0}
      <div class="section-head">
        <h2 class="section-title">已命名<span class="count">{named.length}</span></h2>
      </div>
      <ul class="list">
        {#each named as p (p.id)}
          {@render personRow(p)}
        {/each}
      </ul>
    {/if}
  {/if}

  {#snippet personRow(p: PersonSummary)}
        <li class="item" class:active-row={samplePlayingId === p.id}>
          <div class="avatar" style="background: {avatarTint(p.id)}; color: {avatarInk(p.id)}">
            {#if p.name}
              <span class="initial">{p.name.slice(0, 1)}</span>
            {:else}
              <!-- 人形轮廓:16px 线性 SVG(DESIGN 禁用 👤 等 emoji) -->
              <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" aria-hidden="true">
                <circle cx="8" cy="5.2" r="2.7" />
                <path d="M2.8 13.6c.8-2.6 2.8-3.9 5.2-3.9s4.4 1.3 5.2 3.9" />
              </svg>
            {/if}
          </div>

          <div class="info">
            <div class="name-line">
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
                  {p.name}
                  <svg class="pencil" width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                    <path d="M11.3 2.4l2.3 2.3L5.3 13l-3 .7.7-3z" />
                  </svg>
                </button>
              {:else}
                <span class="unnamed">未命名</span>
                <button class="name-cta" onclick={() => beginRename(p)}>命名</button>
              {/if}
            </div>
            <div class="meta-line">
              最近 {formatDate(p.last_seen)} · 累计发声 {formatDuration(Math.floor(p.total_ms / 1000))}
              {#each p.sources as s (s)}
                <span class="badge">{sourceLabel(s)}</span>
              {/each}
            </div>
          </div>

          <div class="actions" class:pinned={samplePlayingId === p.id || mergeMenuId === p.id || pendingMerge?.loser === p.id || confirmDeleteId === p.id}>
            {#if p.sample_path}
              <button
                class="icon-btn"
                class:playing={samplePlayingId === p.id}
                title={samplePlayingId === p.id ? "停止" : "试听本人原声"}
                aria-label={samplePlayingId === p.id ? "停止" : "试听"}
                onclick={() => toggleSample(p)}
              >
                {#if samplePlayingId === p.id}
                  <span class="bars" aria-hidden="true"><span></span><span></span><span></span></span>
                {:else}
                  <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
                    <path d="M5 2.9v10.2c0 .7.8 1.2 1.4.8l7.4-5.1c.6-.4.6-1.2 0-1.6L6.4 2.1c-.6-.4-1.4.1-1.4.8z" fill="currentColor" />
                  </svg>
                {/if}
              </button>
            {:else}
              <span class="icon-btn ghost" title="暂无录音样本:下次录到这个人并停止录制后会自动补上">
                <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" aria-hidden="true">
                  <path d="M5 2.9v10.2c0 .7.8 1.2 1.4.8l7.4-5.1c.6-.4.6-1.2 0-1.6L6.4 2.1c-.6-.4-1.4.1-1.4.8z" />
                  <path d="M2 2l12 12" />
                </svg>
              </span>
            {/if}

            <div class="merge-anchor">
              <button
                class="icon-btn"
                title="合并到另一个人"
                aria-label="合并"
                disabled={people.length < 2}
                onclick={() => {
                  const opening = mergeMenuId !== p.id;
                  closeAllOps();
                  editingId = null;
                  if (opening) mergeMenuId = p.id;
                }}
              >
                <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <path d="M3 4h3.2L10 12h3" />
                  <path d="M3 12h3.2" />
                  <path d="M10 4h3" />
                  <path d="M11.4 2.4L13 4l-1.6 1.6M11.4 10.4L13 12l-1.6 1.6" />
                </svg>
              </button>
              {#if mergeMenuId === p.id && !pendingMerge}
                <div class="menu">
                  <div class="menu-title">把「{displayName(p)}」并入…</div>
                  {#each sorted.filter((o) => o.id !== p.id) as o (o.id)}
                    <button class="menu-item" onclick={() => (pendingMerge = { loser: p.id, winner: o.id })}>
                      <span class="menu-dot" style="background: {avatarTint(o.id)}"></span>
                      {displayName(o)}
                    </button>
                  {/each}
                </div>
              {:else if pendingMerge && pendingMerge.loser === p.id}
                {@const target = people.find((o) => o.id === pendingMerge?.winner)}
                <div class="menu confirm">
                  <div class="menu-title">
                    并入「{target ? displayName(target) : "?"}」?合并后这个人历史笔记都显示对方的名字,不可撤销。
                  </div>
                  <div class="confirm-row">
                    <button class="mini danger" onclick={doMerge}>确认合并</button>
                    <button class="mini" onclick={() => { pendingMerge = null; mergeMenuId = null; }}>取消</button>
                  </div>
                </div>
              {/if}
            </div>

            {#if confirmDeleteId === p.id}
              <div class="confirm-inline">
                <button class="mini danger" onclick={() => doDelete(p.id)}>确认删除</button>
                <button class="mini" onclick={() => (confirmDeleteId = null)}>取消</button>
              </div>
            {:else}
              <button
                class="icon-btn danger-hover"
                title="从声纹库删除(不影响已有笔记文字)"
                aria-label="删除"
                onclick={() => {
                  closeAllOps();
                  confirmDeleteId = p.id;
                }}
              >
                <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <path d="M2.5 4.2h11M6.2 4V2.8c0-.4.3-.8.8-.8h2c.5 0 .8.4.8.8V4M4 4.2l.7 9c0 .5.4.8.9.8h4.8c.5 0 .9-.3.9-.8l.7-9" />
                  <path d="M6.6 7v4.4M9.4 7v4.4" />
                </svg>
              </button>
            {/if}
          </div>
        </li>
  {/snippet}
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 52rem;
  }
  h1 {
    margin: 0 0 0.75rem;
  }
  .desc {
    color: var(--ink-secondary);
    font-size: 0.85rem;
    line-height: 1.5;
    margin: 0 0 1.25rem;
    max-width: 46rem;
  }
  /* 分区标题:小号加粗 + 计数胶囊,待命名区带引导文案 */
  .section-head {
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    margin: 1.1rem 0 0.45rem;
  }
  .section-head:first-of-type {
    margin-top: 0;
  }
  .section-title {
    font-size: 0.82rem;
    font-weight: 500;
    color: var(--ink-secondary);
    margin: 0;
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
  }
  .count {
    font-size: 0.72rem;
    font-weight: 500;
    color: var(--ink-faint);
    background: var(--surface-press);
    border-radius: var(--radius-full);
    padding: 0 0.5em;
    line-height: 1.5;
  }
  .section-hint {
    font-size: 0.78rem;
    color: var(--ink-faint);
  }
  /* list-row 容器:surface 卡片承载各行 */
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    background: var(--surface);
    border-radius: var(--radius-lg);
  }
  /* list-row:行间 hairline 分隔,hover surface-soft;试听中整行微亮 */
  .item {
    display: flex;
    align-items: center;
    gap: 0.9rem;
    padding: 0.7rem 1rem;
    border-bottom: 1px solid var(--hairline);
    transition: background 120ms ease;
  }
  .item:first-child {
    border-top-left-radius: var(--radius-lg);
    border-top-right-radius: var(--radius-lg);
  }
  .item:last-child {
    border-bottom: none;
    border-bottom-left-radius: var(--radius-lg);
    border-bottom-right-radius: var(--radius-lg);
  }
  .item:hover,
  .item.active-row {
    background: var(--surface-soft);
  }
  /* 头像:36px 圆形粉彩底,名字首字或人形轮廓 */
  .avatar {
    width: 36px;
    height: 36px;
    border-radius: 50%;
    flex: none;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--ink);
  }
  .initial {
    font-size: 0.95rem;
    font-weight: 500;
  }
  .info {
    flex: 1;
    min-width: 0;
  }
  .name-line {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    min-height: 1.6em;
  }
  /* editable-text(名字):静态无边,hover accent-tint 底;✎ 改线性 SVG 角标 */
  .name-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.35em;
    background: none;
    border: none;
    font: inherit;
    font-weight: 500;
    font-size: 1rem;
    color: inherit;
    cursor: pointer;
    border-radius: var(--radius-sm);
    padding: 0.1em 0.35em;
    margin: -0.1em -0.35em;
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
    font-weight: 500;
    font-size: 1rem;
    font-style: italic;
    color: var(--ink-faint);
  }
  /* button-primary:命名是本行唯一主动作 */
  .name-cta {
    background: var(--primary);
    color: var(--on-primary);
    border: none;
    border-radius: var(--radius-full);
    padding: 0.15em 0.8em;
    font-size: 0.85rem;
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
    font-size: 1rem;
    border: 1px solid var(--accent);
    border-radius: var(--radius-md);
    background: var(--canvas);
    color: var(--ink);
    padding: 0.1em 0.4em;
    min-width: 12rem;
  }
  .meta-line {
    color: var(--ink-faint);
    font-size: 0.82rem;
    margin-top: 0.15rem;
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
  /* 行级操作:默认隐身,悬停显影;有活动操作态(播放/菜单/确认)时钉住 */
  .actions {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    flex: none;
    visibility: hidden;
  }
  .item:hover .actions,
  .actions.pinned {
    visibility: visible;
  }
  .icon-btn {
    width: 1.9rem;
    height: 1.9rem;
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-md);
    border: 1px solid transparent;
    background: transparent;
    color: var(--ink-secondary);
    cursor: pointer;
    transition: background 120ms ease, color 120ms ease;
  }
  .icon-btn:hover {
    background: var(--surface-press);
    color: var(--ink);
  }
  .icon-btn:disabled {
    color: var(--ink-faint);
    cursor: default;
    background: transparent;
  }
  .icon-btn.playing {
    color: var(--accent);
  }
  .icon-btn.danger-hover:hover {
    color: var(--danger);
  }
  /* 无样本占位:极淡、不可点,title 解释何时会有 */
  .icon-btn.ghost {
    color: var(--ink-faint);
    opacity: 0.45;
    cursor: default;
  }
  /* 试听中的跳动条(纯 CSS,无符号字符) */
  .bars {
    display: inline-flex;
    align-items: flex-end;
    gap: 2.5px;
    height: 13px;
  }
  .bars span {
    width: 2.5px;
    border-radius: 1px;
    background: currentColor;
    animation: eq 0.9s ease-in-out infinite;
  }
  .bars span:nth-child(1) {
    height: 60%;
    animation-delay: 0s;
  }
  .bars span:nth-child(2) {
    height: 100%;
    animation-delay: 0.25s;
  }
  .bars span:nth-child(3) {
    height: 75%;
    animation-delay: 0.5s;
  }
  @keyframes eq {
    0%,
    100% {
      transform: scaleY(0.5);
    }
    50% {
      transform: scaleY(1);
    }
  }
  /* menu/popover(合并目标):canvas 底、hairline 边、rounded-lg、shadow-popover */
  .merge-anchor {
    position: relative;
  }
  .menu {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    z-index: 10;
    min-width: 15rem;
    max-height: 16rem;
    overflow-y: auto;
    background: var(--canvas);
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
  /* mini 按钮:确认条里的小实体按钮 */
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
    color: var(--on-accent);
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
    font-weight: 400;
  }
</style>
