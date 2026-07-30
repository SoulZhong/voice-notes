<script lang="ts">
  import { onMount } from "svelte";
  import { convertFileSrc } from "@tauri-apps/api/core";
  import { goto } from "$app/navigation";
  import {
    acknowledgeMerge,
    deletePerson,
    listPeople,
    mergePerson,
    personNotes,
    undoMerge,
    type MergeReceipt,
    type PersonMergeSuggestion,
    type PersonSummary,
  } from "$lib/people";
  import { formatDate, formatDuration, speakerInk, type NoteSummary } from "$lib/notes";
  import { isStrong, tidy } from "$lib/tidy.svelte";
  import {
    buildTidyQueue,
    keyCommand,
    orderWithSkips,
    tidyItemKey,
    type TidyItem,
  } from "$lib/tidyQueue";
  import { createAudition } from "$lib/tidyAudio";
  import { recording } from "$lib/recording.svelte";

  // ── 数据:队列由共享 store + 人物表现算;处理完的项随重算自然消失,始终看队首 ──
  let people = $state<PersonSummary[]>([]);
  let error = $state("");
  let busy = $state(false);
  let skipped = $state<string[]>([]);
  let done = $state(0);
  /** 手动合并后的页内撤销条(最近一次;同名组连并多条时只能撤最后一条——撤销后
      其前一条虽在合并日志里复活,但 manual 条目不进回执队列,UI 上无法继续撤;
      同名组卡会重新出现,留给用户对复活的那一对重新拍板)。 */
  let lastManual = $state<{ journalId: string; label: string } | null>(null);
  let confirmClean = $state(false);

  const personById = $derived(new Map(people.map((p) => [p.id, p])));
  const queue = $derived(
    orderWithSkips(buildTidyQueue(people, tidy.visible, tidy.receipts, tidy.dismissed), skipped),
  );
  const current = $derived(queue[0] ?? null);
  const total = $derived(done + queue.length);
  const live = $derived(recording.isLive);

  const plabel = (id: string, name: string) => name || `说话人 ${id.replace(/^P/, "")}`;

  // ── 会议上下文(拍板信息):每人最近 3 场,懒加载缓存 ──
  let notesCache = $state<Record<string, NoteSummary[]>>({});
  async function loadNotes(pid: string) {
    if (notesCache[pid]) return;
    try {
      notesCache[pid] = (await personNotes(pid)).slice(0, 3);
    } catch {
      notesCache[pid] = [];
    }
  }
  /** 当前卡涉及的人物(回执卡的 loser 已并入 winner,看 winner 即可)。 */
  const currentIds = $derived.by(() => {
    if (!current) return [] as string[];
    if (current.kind === "suggestion") return [current.suggestion.loser, current.suggestion.winner];
    if (current.kind === "receipt") return [current.receipt.winner];
    if (current.kind === "dup") return current.people.map((p) => p.id);
    return [current.person.id];
  });
  $effect(() => {
    for (const id of currentIds) void loadNotes(id);
  });

  // ── 试听:单实例互斥,切卡/离开即停 ──
  let playingKey = $state<string | null>(null);
  const audition = createAudition(
    (src) => {
      const audio = new Audio(convertFileSrc(src));
      return {
        play: () => audio.play(),
        pause: () => audio.pause(),
        set onended(cb: (() => void) | null) {
          audio.onended = cb ? () => cb() : null;
        },
        get onended() {
          return null;
        },
      } as any;
    },
    (k) => (playingKey = k),
  );
  $effect(() => {
    void current;
    audition.stop();
  });
  $effect(() => () => audition.stop());

  /** 播某人最新一份样本(键盘数字键)。无样本静默不动。 */
  function playLatest(pid: string) {
    const p = personById.get(pid);
    const path = p?.sample_paths[p.sample_paths.length - 1];
    if (path) audition.toggle(path, path);
  }

  // ── 同名组主条目(可切换,默认最近活跃=组首) ──
  let dupPrimary = $state<Record<string, string>>({});
  const dupPrimaryId = (name: string, g: PersonSummary[]) => dupPrimary[name] ?? g[0].id;

  async function refreshPeople() {
    try {
      people = await listPeople();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }
  onMount(async () => {
    await tidy.refresh();
    await refreshPeople();
  });
  $effect(() => {
    void recording.peopleVersion;
    refreshPeople();
  });

  // ── 动作:每个动作后重算队列;失败卡片留在原地,错误横幅透出后端文案 ──
  async function act(fn: () => Promise<void>) {
    if (busy) return;
    busy = true;
    error = "";
    audition.stop();
    confirmClean = false;
    try {
      await fn();
      done++;
      recording.bumpPeople();
      await tidy.refresh();
      await refreshPeople();
    } catch (e) {
      error = `${e}`;
    }
    busy = false;
  }

  async function doMergeSuggestion(s: PersonMergeSuggestion) {
    await act(async () => {
      const jid = await mergePerson(s.loser, s.winner);
      lastManual = {
        journalId: jid,
        label: `${plabel(s.loser, s.loser_name)} → ${plabel(s.winner, s.winner_name)}`,
      };
    });
  }
  function doIgnoreSuggestion(s: PersonMergeSuggestion) {
    tidy.ignore(s);
    done++;
  }
  async function doMergeDup(name: string, g: PersonSummary[]) {
    await act(async () => {
      const winner = dupPrimaryId(name, g);
      let jid = "";
      for (const p of g) {
        if (p.id !== winner) jid = await mergePerson(p.id, winner);
      }
      lastManual = { journalId: jid, label: `「${name}」并成一条` };
    });
  }
  async function doDeleteNoSample(p: PersonSummary) {
    await act(async () => {
      await deletePerson(p.id);
    });
  }
  /** 一键清理剩余全部无样本条目(二段确认后)。 */
  async function doCleanAll() {
    const rest = queue.filter((i) => i.kind === "nosample");
    await act(async () => {
      for (const i of rest) {
        if (i.kind === "nosample") await deletePerson(i.person.id);
      }
    });
  }
  function doDismiss(item: TidyItem) {
    tidy.dismiss(tidyItemKey(item));
    done++;
  }
  function doSkip() {
    if (current) skipped = [...skipped, tidyItemKey(current)];
  }
  async function doAck(r: MergeReceipt) {
    await act(async () => {
      await acknowledgeMerge(r.journal_id);
    });
  }
  async function doUndo(journalId: string) {
    await act(async () => {
      await undoMerge(journalId);
      lastManual = null;
    });
  }

  // ── 键盘:Enter 主动作 / X 忽略保留 / S 跳过 / 1-9 试听 / Esc 返回 ──
  function onKeydown(e: KeyboardEvent) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.key === "Escape") {
      e.preventDefault();
      goto("/speakers");
      return;
    }
    if (!current || busy) return;
    const cmd = keyCommand(e.key, current.kind);
    if (!cmd) return;
    e.preventDefault();
    if (typeof cmd === "object") {
      playLatest(currentIds[cmd.play] ?? "");
      return;
    }
    if (cmd === "skip") {
      doSkip();
      return;
    }
    if (live) return; // 录制中只许试听/跳过
    const it = current;
    if (cmd === "primary") {
      if (it.kind === "suggestion") void doMergeSuggestion(it.suggestion);
      else if (it.kind === "dup") void doMergeDup(it.name, it.people);
      else if (it.kind === "nosample") void doDeleteNoSample(it.person);
      else void doAck(it.receipt);
    } else if (cmd === "dismiss") {
      if (it.kind === "suggestion") doIgnoreSuggestion(it.suggestion);
      else if (it.kind !== "receipt") doDismiss(it);
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#snippet personPane(pid: string, name: string, digit: number | null)}
  {@const p = personById.get(pid)}
  <div class="pane">
    <div class="pane-head">
      <span class="dot" style="background: {speakerInk(pid, 'mic')}"></span>
      <a class="pname" href="/speakers/{pid}">{plabel(pid, name)}</a>
      {#if digit !== null}<kbd class="kbd">{digit}</kbd>{/if}
    </div>
    {#if p}
      <div class="pane-meta">
        最近 {formatDate(p.last_seen)} · 累计 {formatDuration(Math.floor(p.total_ms / 1000))}
      </div>
      <div class="samples">
        {#each p.sample_paths as path, i (path)}
          <button
            class="chip"
            class:playing={playingKey === path}
            title={playingKey === path ? "停止" : "试听这份原声"}
            onclick={() => audition.toggle(path, path)}
          >
            {playingKey === path ? "◼" : "▶"}
            {p.sample_dates[i] ? formatDate(p.sample_dates[i]).slice(5, 10) : `样本 ${i + 1}`}
          </button>
        {:else}
          <span class="hint">无录音样本</span>
        {/each}
      </div>
      {#if (notesCache[pid] ?? []).length > 0}
        <div class="meets">
          {#each notesCache[pid] as n (n.id)}
            <a class="meet" href="/notes/{n.id}">{n.title || formatDate(n.started_at)}</a>
          {/each}
        </div>
      {/if}
    {:else}
      <div class="pane-meta">已并入,记录随合并转移</div>
    {/if}
  </div>
{/snippet}

<main class="container">
  <header class="head">
    <a class="back" href="/speakers">← 概览</a>
    <h1>整理收件箱</h1>
    {#if total > 0 && queue.length > 0}
      <span class="progress">第 {done + 1} / {total} 件</span>
    {/if}
  </header>

  {#if live}
    <div class="banner warn">录制中不能整理——可以浏览和试听,合并/删除/撤销等停止录制后再做。</div>
  {/if}
  {#if error}
    <div class="banner">{error}</div>
  {/if}
  {#if lastManual}
    <div class="undo-strip">
      已合并:{lastManual.label}
      <button class="mini" disabled={busy || live} onclick={() => doUndo(lastManual!.journalId)}>撤销</button>
      <button class="mini plain" onclick={() => (lastManual = null)}>好</button>
    </div>
  {/if}

  {#if !current}
    {#if tidy.loading}
      <div class="empty">
        <p class="hint">正在比对声纹…</p>
      </div>
    {:else}
      <div class="empty">
        <p>都整理完了</p>
        <p class="hint">新的建议会随录制自动出现;高置信的会自动归并并在这里留回执。</p>
        <a class="mini as-link" href="/speakers">返回概览</a>
      </div>
    {/if}
  {:else if current.kind === "receipt"}
    {@const r = current.receipt}
    <section class="card">
      <div class="card-tag">已自动归并</div>
      <div class="card-title">
        {plabel(r.loser, r.loser_name)} → {plabel(r.winner, r.winner_name)}
        {#if r.similarity !== null}
          <span class="sim strong">相似度 {Math.round(r.similarity * 100)}%</span>
        {/if}
      </div>
      <div class="panes">
        {@render personPane(r.winner, r.winner_name, 1)}
      </div>
      <p class="hint">声纹足够相似已自动并入。听一下不对劲就撤销;没问题点「好」。</p>
      <div class="acts">
        <button class="mini accent" disabled={busy || live} onclick={() => doAck(r)}>好 <kbd class="kbd">⏎</kbd></button>
        {#if r.invalid_reason}
          <button class="mini" disabled title={r.invalid_reason}>撤销(不可用)</button>
          <span class="hint">{r.invalid_reason}</span>
        {:else}
          <button class="mini" disabled={busy || live} onclick={() => doUndo(r.journal_id)}>撤销</button>
        {/if}
        <button class="mini plain" onclick={doSkip}>跳过 <kbd class="kbd">S</kbd></button>
      </div>
    </section>
  {:else if current.kind === "suggestion"}
    {@const s = current.suggestion}
    <section class="card">
      <div class="card-tag">归属建议</div>
      <div class="card-title">
        这两条像同一个人吗?
        <span class="sim" class:strong={isStrong(s)}>
          相似度 {Math.round(s.similarity * 100)}%{isStrong(s) ? " · 很可能" : ""}
        </span>
      </div>
      <div class="panes">
        {@render personPane(s.loser, s.loser_name, 1)}
        <svg class="arrow" width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M2.5 8h10M9 4.5L13.5 8 9 11.5" />
        </svg>
        {@render personPane(s.winner, s.winner_name, 2)}
      </div>
      <p class="hint">两边各听一段原声(数字键 1/2 播最新样本),确认是同一个人再合并;合并保留双方声纹,认得更准。</p>
      <div class="acts">
        <button class="mini accent" disabled={busy || live} onclick={() => doMergeSuggestion(s)}>合并 <kbd class="kbd">⏎</kbd></button>
        <button class="mini" disabled={busy || live} onclick={() => doIgnoreSuggestion(s)}>忽略 <kbd class="kbd">X</kbd></button>
        <button class="mini plain" onclick={doSkip}>跳过 <kbd class="kbd">S</kbd></button>
      </div>
    </section>
  {:else if current.kind === "dup"}
    {@const g = current.people}
    {@const primary = dupPrimaryId(current.name, g)}
    <section class="card">
      <div class="card-tag">同名重复</div>
      <div class="card-title">「{current.name}」有 {g.length} 条,多半是同一个人被拆开了</div>
      <div class="panes wrap">
        {#each g as p, i (p.id)}
          <div class="dup-item" class:primary={p.id === primary}>
            {@render personPane(p.id, p.name, i < 9 ? i + 1 : null)}
            <label class="pick">
              <input
                type="radio"
                name="dup-primary"
                checked={p.id === primary}
                onchange={() => (dupPrimary = { ...dupPrimary, [current.name]: p.id })}
              />
              作为主条目
            </label>
          </div>
        {/each}
      </div>
      <p class="hint">其余条目将并入主条目(默认最近活跃的);数字键逐条试听核对。</p>
      <div class="acts">
        <button class="mini accent" disabled={busy || live} onclick={() => doMergeDup(current.name, g)}>
          全部并入主条目 <kbd class="kbd">⏎</kbd>
        </button>
        <button class="mini" disabled={busy || live} onclick={() => doDismiss(current)}>忽略 <kbd class="kbd">X</kbd></button>
        <button class="mini plain" onclick={doSkip}>跳过 <kbd class="kbd">S</kbd></button>
      </div>
    </section>
  {:else}
    {@const p = current.person}
    <section class="card">
      <div class="card-tag">无样本条目</div>
      <div class="card-title">{plabel(p.id, p.name)}——没有原声可核对</div>
      <div class="panes">
        {@render personPane(p.id, p.name, null)}
      </div>
      <p class="hint warn-text">
        删除后历史笔记中这个说话人恢复显示为编号,不可恢复。认不出是谁就删,拿不准就保留。
      </p>
      <div class="acts">
        <button class="mini danger" disabled={busy || live} onclick={() => doDeleteNoSample(p)}>删除 <kbd class="kbd">⏎</kbd></button>
        <button class="mini" disabled={busy || live} onclick={() => doDismiss(current)}>保留 <kbd class="kbd">X</kbd></button>
        <button class="mini plain" onclick={doSkip}>跳过 <kbd class="kbd">S</kbd></button>
        {#if queue.filter((i) => i.kind === "nosample").length > 1}
          <span class="spacer"></span>
          {#if confirmClean}
            <span class="warn-text">共 {queue.filter((i) => i.kind === "nosample").length} 条,删除不可恢复。</span>
            <button class="mini danger" disabled={busy || live} onclick={doCleanAll}>确认清理</button>
            <button class="mini plain" onclick={() => (confirmClean = false)}>取消</button>
          {:else}
            <button class="mini plain" disabled={busy || live} onclick={() => (confirmClean = true)}>
              剩余 {queue.filter((i) => i.kind === "nosample").length} 条无样本条目一键清理
            </button>
          {/if}
        {/if}
      </div>
    </section>
  {/if}

  <footer class="keys">
    <span><kbd class="kbd">⏎</kbd> 主动作</span>
    <span><kbd class="kbd">X</kbd> 忽略/保留</span>
    <span><kbd class="kbd">S</kbd> 跳过</span>
    <span><kbd class="kbd">1-9</kbd> 试听</span>
    <span><kbd class="kbd">Esc</kbd> 返回</span>
  </footer>
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 44rem;
  }
  .head {
    display: flex;
    align-items: baseline;
    gap: 0.8rem;
    margin-bottom: 1rem;
  }
  .head h1 {
    margin: 0;
    font-size: 1.3rem;
  }
  .back {
    color: var(--ink-secondary);
    text-decoration: none;
    font-size: 0.85rem;
  }
  .back:hover {
    color: var(--accent);
  }
  .progress {
    color: var(--ink-faint);
    font-size: 0.82rem;
  }
  .banner {
    background: var(--danger-tint);
    border: 1px solid var(--danger-line);
    color: var(--danger-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin-bottom: 0.8rem;
    font-size: 0.9rem;
  }
  .banner.warn {
    background: var(--warning-tint);
    border-color: var(--warning-line);
    color: var(--warning-ink);
  }
  .undo-strip {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.5rem 0.8rem;
    margin-bottom: 0.8rem;
    font-size: 0.85rem;
    color: var(--ink-secondary);
  }
  .card {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 1rem 1.1rem;
  }
  .card-tag {
    font-size: 0.75rem;
    color: var(--ink-faint);
    margin-bottom: 0.25rem;
  }
  .card-title {
    font-size: 1rem;
    font-weight: 500;
    color: var(--ink);
    margin-bottom: 0.8rem;
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    flex-wrap: wrap;
  }
  .sim {
    color: var(--ink-faint);
    font-size: 0.8rem;
    font-weight: 400;
  }
  .sim.strong {
    color: var(--accent);
  }
  .panes {
    display: flex;
    align-items: flex-start;
    gap: 0.8rem;
  }
  .panes.wrap {
    flex-wrap: wrap;
  }
  .arrow {
    color: var(--ink-faint);
    flex: none;
    align-self: center;
  }
  .pane {
    flex: 1;
    min-width: 0;
    background: var(--surface-soft);
    border-radius: var(--radius-md);
    padding: 0.6rem 0.7rem;
  }
  .dup-item {
    flex: 1 1 16rem;
    border: 1px solid transparent;
    border-radius: var(--radius-md);
  }
  .dup-item.primary {
    border-color: var(--accent);
  }
  .pane-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .dot {
    width: 9px;
    height: 9px;
    border-radius: var(--radius-full);
    flex: none;
  }
  .pname {
    color: var(--ink);
    font-weight: 500;
    text-decoration: none;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .pname:hover {
    color: var(--accent);
    text-decoration: underline;
  }
  .pane-meta {
    color: var(--ink-faint);
    font-size: 0.76rem;
    margin: 0.25rem 0 0.4rem;
  }
  .samples {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
  }
  .chip {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink-secondary);
    border-radius: var(--radius-full);
    font-size: 0.75rem;
    padding: 0.15em 0.6em;
    cursor: pointer;
  }
  .chip:hover {
    background: var(--surface);
    color: var(--ink);
  }
  .chip.playing {
    border-color: var(--accent);
    color: var(--accent);
  }
  .meets {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    margin-top: 0.45rem;
  }
  .meet {
    color: var(--ink-secondary);
    font-size: 0.78rem;
    text-decoration: none;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .meet:hover {
    color: var(--accent);
  }
  .pick {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    font-size: 0.78rem;
    color: var(--ink-secondary);
    padding: 0.3rem 0.7rem 0.5rem;
    cursor: pointer;
  }
  .pick input {
    accent-color: var(--accent);
    margin: 0;
  }
  .acts {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-top: 0.8rem;
    flex-wrap: wrap;
  }
  .spacer {
    flex: 1;
  }
  .mini {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    border-radius: var(--radius-md);
    font-size: 0.82rem;
    padding: 0.25em 0.75em;
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
  .mini.danger {
    border-color: var(--danger);
    color: var(--danger);
    font-weight: 500;
  }
  .mini.danger:hover:not(:disabled) {
    background: var(--danger);
    color: var(--on-record);
  }
  .mini.plain {
    border-color: transparent;
    color: var(--ink-secondary);
  }
  .mini.as-link {
    display: inline-block;
    text-decoration: none;
    margin-top: 0.6rem;
  }
  .kbd {
    font-family: inherit;
    font-size: 0.68rem;
    color: var(--ink-faint);
    border: 1px solid var(--hairline);
    border-radius: 3px;
    padding: 0 0.25em;
    margin-left: 0.15em;
  }
  .hint {
    color: var(--ink-faint);
    font-size: 0.8rem;
  }
  p.hint {
    margin: 0.7rem 0 0;
  }
  .warn-text {
    color: var(--warning-ink);
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
  .keys {
    display: flex;
    gap: 1rem;
    margin-top: 0.9rem;
    color: var(--ink-faint);
    font-size: 0.75rem;
  }
</style>
