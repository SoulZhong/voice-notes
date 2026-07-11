<script lang="ts">
  import { onMount } from "svelte";
  import { convertFileSrc } from "@tauri-apps/api/core";
  import {
    listPeople,
    mergePerson,
    deletePerson,
    type PersonSummary,
    type PersonMergeSuggestion,
  } from "$lib/people";
  import { tidy, sugKey } from "$lib/tidy.svelte";
  import { formatDate, formatDuration, speakerInk } from "$lib/notes";
  import { recording } from "$lib/recording.svelte";

  // 主从结构的落地页:人物索引在侧栏,本页只做概览引导——不再重复列一遍名单。
  let people = $state<PersonSummary[]>([]);
  let error = $state("");

  const named = $derived(people.filter((p) => p.name).length);
  const unnamed = $derived(people.length - named);

  /** 同名分组(疑似重复:多半是同一个人被声纹拆成了多条)。people 已按 last_seen 降序。 */
  const dupGroups = $derived.by(() => {
    const by = new Map<string, PersonSummary[]>();
    for (const p of people) {
      if (!p.name) continue;
      by.set(p.name, [...(by.get(p.name) ?? []), p]);
    }
    return [...by.values()].filter((g) => g.length > 1);
  });

  const recent = (p: PersonSummary) => {
    const d = formatDate(p.last_seen);
    return d === "—" ? p.id : d.slice(5, 10);
  };

  // ── 整理:①未命名者再辨认(声纹比对给合并建议);②清理无样本条目 ──
  const plabel = (id: string, name: string) => name || `说话人 ${id.replace(/^P/, "")}`;
  const noSample = $derived(people.filter((p) => p.sample_paths.length === 0));
  /** 有可整理内容才出卡:无样本条目或存在待辨认(未命名)者。 */
  const tidyAvailable = $derived(noSample.length > 0 || unnamed > 0);

  let tidyOpen = $state(false);
  let checked = $state<Record<string, boolean>>({});
  let confirmCleanup = $state(false);
  let cleaning = $state(false);
  let tidyErr = $state("");

  // 建议与忽略集走共享 tidy store:侧栏徽标、详情页上下文提示与本卡三处同源。
  const visibleSuggestions = $derived(tidy.visible);
  /** 出现在任一建议里的人:清理区默认不勾(有归属先合并,合并保数据,删除丢数据)。 */
  const suggestedIds = $derived(new Set(tidy.suggestions.flatMap((s) => [s.loser, s.winner])));
  const checkedIds = $derived(noSample.filter((p) => checked[p.id]).map((p) => p.id));

  // ── 建议行内试听(单实例):不听原声没法拍板该不该合,双方名字旁都给 ▶。
  //    播的是该人第一份录音样本;无样本的不出钮(点名字进详情页也没得听)。 ──
  const personById = $derived(new Map(people.map((p) => [p.id, p])));
  let sugAudio: HTMLAudioElement | null = null;
  let sugPlayingId = $state<string | null>(null);

  function stopSug() {
    sugAudio?.pause();
    sugAudio = null;
    sugPlayingId = null;
  }

  function toggleSugSample(pid: string) {
    if (sugPlayingId === pid) {
      stopSug();
      return;
    }
    stopSug();
    const path = personById.get(pid)?.sample_paths[0];
    if (!path) return;
    const a = new Audio(convertFileSrc(path));
    a.onended = () => {
      if (sugPlayingId === pid) stopSug();
    };
    sugAudio = a;
    sugPlayingId = pid;
    void a.play().catch(() => stopSug());
  }

  // 离开页面/收起整理即停,别让声音悬在半空
  $effect(() => stopSug);

  /** 重算建议 + 清理区默认勾选(未命名且无归属建议的无样本条目)。 */
  async function recomputeTidy() {
    await tidy.refresh();
    const c: Record<string, boolean> = {};
    const suggested = new Set(tidy.suggestions.flatMap((s) => [s.loser, s.winner]));
    for (const p of noSample) c[p.id] = !p.name && !suggested.has(p.id);
    checked = c;
  }

  async function toggleTidy() {
    tidyOpen = !tidyOpen;
    if (!tidyOpen) {
      stopSug();
      return;
    }
    tidyErr = "";
    confirmCleanup = false;
    await recomputeTidy();
  }

  /** 采纳建议:并入推荐归属。合并会改变库(质心/样本迁移),其余建议随之重算。 */
  async function applySuggestion(s: PersonMergeSuggestion) {
    tidyErr = "";
    stopSug();
    try {
      await mergePerson(s.loser, s.winner);
      recording.bumpPeople();
      await refresh();
      await recomputeTidy();
    } catch (e) {
      tidyErr = `${e}`; // 录制中后端拒绝等文案原样透出
    }
  }

  async function doCleanup() {
    confirmCleanup = false;
    cleaning = true;
    tidyErr = "";
    stopSug();
    let failed = 0;
    for (const id of checkedIds) {
      try {
        await deletePerson(id);
      } catch {
        failed++;
      }
    }
    cleaning = false;
    if (failed > 0) tidyErr = `${failed} 项删除失败(录制中不能删除)`;
    recording.bumpPeople();
    await refresh();
    await recomputeTidy();
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
  // 详情页改名/合并/删除后统计同步。
  $effect(() => {
    void recording.peopleVersion;
    refresh();
  });
</script>

{#snippet listenBtn(pid: string)}
  <!-- 行内试听小钮:播该人第一份样本;无样本不出钮。播放中换成停止方块+accent -->
  {#if (personById.get(pid)?.sample_paths.length ?? 0) > 0}
    <button
      class="listen-mini"
      class:playing={sugPlayingId === pid}
      title={sugPlayingId === pid ? "停止" : "试听原声"}
      aria-label={sugPlayingId === pid ? "停止" : "试听原声"}
      onclick={() => toggleSugSample(pid)}
    >
      {#if sugPlayingId === pid}
        <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true">
          <rect x="3.5" y="3.5" width="9" height="9" rx="1.5" fill="currentColor" />
        </svg>
      {:else}
        <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true">
          <path d="M5 2.9v10.2c0 .7.8 1.2 1.4.8l7.4-5.1c.6-.4.6-1.2 0-1.6L6.4 2.1c-.6-.4-1.4.1-1.4.8z" fill="currentColor" />
        </svg>
      {/if}
    </button>
  {/if}
{/snippet}

<main class="container">
  <h1>会议搭子</h1>
  <p class="desc">
    录到的说话人会自动登记。给"未命名"的人<strong>命名</strong>后,之后的录制会自动认出他并直接显示名字;
    认错拆重了就用<strong>合并</strong>归到同一个人。从左侧选择一个人查看详情、试听原声或管理。
  </p>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if people.length === 0}
    <div class="empty">
      <p>还没有说话人。</p>
      <p class="hint">录一场会议(单人说话累计满 10 秒),停止后会自动出现在左侧。</p>
    </div>
  {:else}
    <div class="stats">
      <div class="stat">
        <span class="num">{people.length}</span>
        <span class="label">位说话人</span>
      </div>
      <div class="stat">
        <span class="num">{named}</span>
        <span class="label">已命名</span>
      </div>
      {#if unnamed > 0}
        <div class="stat todo">
          <span class="num">{unnamed}</span>
          <span class="label">待命名</span>
        </div>
      {/if}
    </div>
    {#if dupGroups.length > 0}
      <!-- 疑似重复:同名多条多半是同一个人被声纹拆重,引导去详情页合并 -->
      <div class="dup-card">
        <div class="dup-head">
          有 {dupGroups.length} 组搭子同名,可能是同一个人被拆成了多条——进入任意一条的详情页,用「合并到…」归成一个人。
        </div>
        {#each dupGroups as g (g[0].name)}
          <div class="dup-row">
            <span class="dup-name">「{g[0].name}」× {g.length}</span>
            {#each g as p (p.id)}
              <a class="dup-link" href="/speakers/{p.id}">最近 {recent(p)}</a>
            {/each}
          </div>
        {/each}
      </div>
    {/if}
    {#if tidyAvailable}
      <!-- 整理:再辨认(声纹比对给合并建议)+ 清理无样本条目。surface 卡,展开式 -->
      <section class="tidy">
        <div class="tidy-head">
          <div>
            <div class="tidy-title">整理</div>
            <div class="tidy-desc">
              {#if unnamed > 0}{unnamed} 个待辨认的说话人可尝试自动归属{/if}{#if unnamed > 0 && noSample.length > 0} · {/if}{#if noSample.length > 0}{noSample.length} 个条目没有录音样本{/if}
            </div>
          </div>
          <button class="tidy-toggle" onclick={toggleTidy}>{tidyOpen ? "收起" : "开始整理"}</button>
        </div>

        {#if tidyOpen}
          {#if tidyErr}<div class="banner tidy-banner">{tidyErr}</div>{/if}

          <div class="tidy-sec">
            <div class="tidy-sec-title">可归属建议</div>
            {#if tidy.loading}
              <p class="hint">正在比对声纹…</p>
            {:else if visibleSuggestions.length === 0}
              <p class="hint">没有比对出可归属的说话人。声纹够相似才会出建议,拿不准的不猜。</p>
            {:else}
              {#each visibleSuggestions as s (sugKey(s))}
                <div class="sug-row">
                  <span class="dot" style="background: {speakerInk(s.loser, 'mic')}"></span>
                  <a class="sug-name" href="/speakers/{s.loser}">{plabel(s.loser, s.loser_name)}</a>
                  {@render listenBtn(s.loser)}
                  <svg class="arrow" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                    <path d="M2.5 8h10M9 4.5L13.5 8 9 11.5" />
                  </svg>
                  <span class="dot" style="background: {speakerInk(s.winner, 'mic')}"></span>
                  <a class="sug-name" href="/speakers/{s.winner}">{plabel(s.winner, s.winner_name)}</a>
                  {@render listenBtn(s.winner)}
                  <span class="sim" class:strong={s.similarity >= 0.74}>
                    相似度 {Math.round(s.similarity * 100)}%{s.similarity >= 0.74 ? " · 很可能" : ""}
                  </span>
                  <span class="sug-spacer"></span>
                  <button
                    class="mini accent"
                    disabled={recording.isLive}
                    title={recording.isLive ? "录制中不能合并" : "并入推荐归属(先点两边的试听核对)"}
                    onclick={() => applySuggestion(s)}>合并</button
                  >
                  <button class="mini" onclick={() => tidy.ignore(s)}>忽略</button>
                </div>
              {/each}
              <p class="hint">先点两边的播放键听原声,确认是同一个人再合并;合并会保留双方声纹数据,认得更准。</p>
            {/if}
          </div>

          <div class="tidy-sec">
            <div class="tidy-sec-title">无样本条目</div>
            {#if noSample.length === 0}
              <p class="hint">每个条目都有录音样本。</p>
            {:else}
              {#each noSample as p (p.id)}
                <label class="clean-row">
                  <input type="checkbox" bind:checked={checked[p.id]} disabled={cleaning} />
                  <span class="dot" style="background: {speakerInk(p.id, 'mic')}"></span>
                  <a class="sug-name" href="/speakers/{p.id}">{plabel(p.id, p.name)}</a>
                  <span class="row-meta">最近 {recent(p)} · 累计 {formatDuration(Math.floor(p.total_ms / 1000))}</span>
                  {#if suggestedIds.has(p.id)}
                    <span class="row-flag">有归属建议,先合并更好</span>
                  {/if}
                </label>
              {/each}
              <div class="clean-act">
                {#if confirmCleanup}
                  <span class="warn-text">删除后历史笔记中这些说话人恢复显示为编号,不可恢复。</span>
                  <button class="mini danger" onclick={doCleanup}>确认清理 {checkedIds.length} 项</button>
                  <button class="mini" onclick={() => (confirmCleanup = false)}>取消</button>
                {:else}
                  <button
                    class="mini"
                    disabled={checkedIds.length === 0 || cleaning || recording.isLive}
                    title={recording.isLive ? "录制中不能删除" : ""}
                    onclick={() => (confirmCleanup = true)}
                  >
                    {cleaning ? "正在清理…" : `清理选中 ${checkedIds.length} 项`}
                  </button>
                  <span class="hint-inline">没有原声可核对、也认不出是谁的条目,占着列表不如清掉。</span>
                {/if}
              </div>
            {/if}
          </div>
        {/if}
      </section>
    {/if}
    <p class="pick-hint">
      从左侧列表选择一个人查看详情。
      {#if unnamed > 0}「待命名」的人命名后,之后的录制会自动显示名字。{/if}
    </p>
  {/if}
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 44rem;
  }
  h1 {
    margin: 0 0 0.75rem;
  }
  .desc {
    color: var(--ink-secondary);
    font-size: 0.85rem;
    line-height: 1.5;
    margin: 0 0 1.25rem;
    max-width: 40rem;
  }
  /* 统计卡:surface 底并排三块,数字大字 500 权重(层级靠亮度不靠重字重) */
  .stats {
    display: flex;
    gap: 0.75rem;
    margin-bottom: 1rem;
  }
  .stat {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.9rem 1.3rem;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    min-width: 6.5rem;
  }
  .num {
    font-size: 1.5rem;
    font-weight: 500;
    color: var(--ink);
    line-height: 1.2;
  }
  .label {
    font-size: 0.8rem;
    color: var(--ink-secondary);
  }
  /* 待命名是待处理项:warning 色系点亮数字提示还有活没干 */
  .stat.todo .num {
    color: var(--warning-ink);
  }
  .pick-hint {
    color: var(--ink-faint);
    font-size: 0.85rem;
  }
  /* 疑似重复卡:warning 色系横幅形态(banner 家族),行内链接直达各同名条目 */
  .dup-card {
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    border-radius: var(--radius-lg);
    padding: 0.7rem 0.9rem;
    margin-bottom: 1rem;
    font-size: 0.85rem;
  }
  .dup-head {
    color: var(--warning-ink);
    line-height: 1.5;
    margin-bottom: 0.35rem;
  }
  .dup-row {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    flex-wrap: wrap;
    padding: 0.15rem 0;
  }
  .dup-name {
    color: var(--ink);
    font-weight: 500;
  }
  .dup-link {
    color: var(--accent);
    font-size: 0.82rem;
  }
  /* 整理卡:surface 底、rounded-lg,头部一行(标题+摘要 | 展开钮),展开分两节 */
  .tidy {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.85rem 1rem;
    margin-bottom: 1rem;
  }
  .tidy-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  .tidy-title {
    font-weight: 500;
    font-size: 0.92rem;
    color: var(--ink);
  }
  .tidy-desc {
    color: var(--ink-secondary);
    font-size: 0.8rem;
    margin-top: 0.15rem;
  }
  .tidy-toggle {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    border-radius: var(--radius-md);
    font-size: 0.85rem;
    font-weight: 500;
    padding: 0.35em 1em;
    cursor: pointer;
    flex: none;
  }
  .tidy-toggle:hover {
    background: var(--surface-soft);
  }
  .tidy-banner {
    margin: 0.6rem 0 0;
  }
  .tidy-sec {
    margin-top: 0.8rem;
  }
  .tidy-sec-title {
    font-size: 0.78rem;
    font-weight: 500;
    color: var(--ink-secondary);
    margin-bottom: 0.3rem;
  }
  .sug-row,
  .clean-row {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    padding: 0.3rem 0.2rem;
    font-size: 0.88rem;
  }
  .clean-row {
    cursor: pointer;
  }
  .clean-row input[type="checkbox"] {
    accent-color: var(--accent);
    margin: 0;
  }
  .dot {
    width: 9px;
    height: 9px;
    border-radius: var(--radius-full);
    flex: none;
  }
  .sug-name {
    color: var(--ink);
    text-decoration: none;
    max-width: 11rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sug-name:hover {
    color: var(--accent);
    text-decoration: underline;
  }
  .arrow {
    color: var(--ink-faint);
    flex: none;
  }
  /* 行内试听:圆形小图标钮,hairline 边;播放中 accent 描边+字色 */
  .listen-mini {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 1.35rem;
    height: 1.35rem;
    padding: 0;
    flex: none;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-full);
    background: transparent;
    color: var(--ink-secondary);
    cursor: pointer;
  }
  .listen-mini:hover {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .listen-mini.playing {
    border-color: var(--accent);
    color: var(--accent);
  }
  .sim {
    color: var(--ink-faint);
    font-size: 0.78rem;
  }
  .sim.strong {
    color: var(--accent);
  }
  .sug-spacer {
    flex: 1;
  }
  .row-meta {
    color: var(--ink-faint);
    font-size: 0.78rem;
  }
  .row-flag {
    color: var(--warning-ink);
    font-size: 0.75rem;
  }
  .clean-act {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-top: 0.4rem;
    flex-wrap: wrap;
  }
  .warn-text {
    color: var(--warning-ink);
    font-size: 0.78rem;
  }
  .hint-inline {
    color: var(--ink-faint);
    font-size: 0.78rem;
  }
  /* mini 按钮族(与详情页同款):secondary 形态,accent 主推,danger 破坏 */
  .mini {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    border-radius: var(--radius-md);
    font-size: 0.8rem;
    padding: 0.2em 0.7em;
    cursor: pointer;
  }
  .mini:hover:not(:disabled) {
    background: var(--surface-soft);
  }
  .mini:disabled {
    color: var(--ink-faint);
    cursor: default;
  }
  .mini.accent {
    border-color: var(--accent);
    color: var(--accent);
    font-weight: 500;
  }
  .mini.accent:hover:not(:disabled) {
    background: var(--accent-tint);
  }
  .mini.accent:disabled {
    border-color: var(--hairline-strong);
    color: var(--ink-faint);
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
