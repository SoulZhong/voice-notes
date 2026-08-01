<script lang="ts">
  import { onMount } from "svelte";
  import { convertFileSrc } from "@tauri-apps/api/core";
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
  import { buildTidyQueue, mergeDuplicatePeople, tidyItemKey, type TidyItem } from "$lib/tidyQueue";
  import { createAudition, type PlayerLike } from "$lib/tidyAudio";
  import { recording } from "$lib/recording.svelte";

  // 主从结构的落地页:人物索引在侧栏,本页概览引导之下常驻「分析说话人」区——
  // 队列由共享 store + 人物表现算,全量渲染;处理完的项随重算自然从列表消失。
  let people = $state<PersonSummary[]>([]);
  let error = $state("");
  let busy = $state(false);
  /** 手动合并后的页内撤销条(最近一次;同名组连并多条时只能撤最后一条——撤销后
      其前一条虽在合并日志里复活,但 manual 条目不进回执队列,UI 上无法继续撤;
      同名组卡会重新出现,留给用户对复活的那一对重新拍板)。 */
  let lastManual = $state<{ journalId: string; label: string } | null>(null);
  /** 一键清理无样本条目的二段确认:单一状态,多张 nosample 卡共享(简单为先)。 */
  let confirmClean = $state(false);

  const named = $derived(people.filter((p) => p.name).length);
  const unnamed = $derived(people.length - named);

  const personById = $derived(new Map(people.map((p) => [p.id, p])));
  const queue = $derived(buildTidyQueue(people, tidy.visible, tidy.receipts, tidy.dismissed));
  const pendingN = $derived(queue.filter((i) => i.kind !== "receipt").length);
  const receiptsN = $derived(queue.filter((i) => i.kind === "receipt").length);
  const invalidReceiptsN = $derived(
    queue.filter((i) => i.kind === "receipt" && i.receipt.invalid_reason !== null).length,
  );
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
  /** 某条目涉及的人物(回执卡的 loser 已并入 winner,看 winner 即可)。 */
  function itemIds(item: TidyItem): string[] {
    if (item.kind === "suggestion") return [item.suggestion.loser, item.suggestion.winner];
    if (item.kind === "receipt") return [item.receipt.winner];
    if (item.kind === "dup") return item.people.map((p) => p.id);
    return [item.person.id];
  }
  // 首屏可见范围懒加载:queue 前 12 张卡涉及的人物才主动拉会议上下文,滚动到
  // 深处的卡按需可再点进详情页看。
  $effect(() => {
    for (const item of queue.slice(0, 12)) {
      for (const id of itemIds(item)) void loadNotes(id);
    }
  });

  // ── 试听:单实例互斥,离开页面即停(列表页同屏多卡,切卡不再意味着换焦点)──
  let playingKey = $state<string | null>(null);
  const audition = createAudition(
    (src) => new Audio(convertFileSrc(src)) as unknown as PlayerLike,
    (k) => (playingKey = k),
    (msg) => (error = `试听失败:${msg}`),
  );
  $effect(() => () => audition.stop());

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
  onMount(() => {
    void tidy.refresh();
    void refreshPeople();
  });
  // 详情页改名/合并/删除、录制停止、自动归并后统计同步。
  $effect(() => {
    void recording.peopleVersion;
    refreshPeople();
  });

  // ── 动作:busy 只覆盖后端操作本身;重算(tidy.refresh/refreshPeople)放后台,
  // layout 对 peopleVersion 的 effect + tidy 单飞 refresh 会兜底同步。失败卡片
  // 留在原地,错误横幅透出后端文案 ──
  async function act(fn: () => Promise<void>, optimistic?: () => void) {
    if (busy) return;
    busy = true;
    error = "";
    audition.stop();
    confirmClean = false;
    try {
      await fn();
      optimistic?.(); // 后端成功即本地收起,不等整轮重算
      recording.bumpPeople(); // 驱动 layout 后台 tidy.refresh(单飞)与各处同步
      void refreshPeople();
    } catch (e) {
      error = `${e}`;
    }
    busy = false;
  }

  async function doMergeSuggestion(s: PersonMergeSuggestion) {
    // 无 optimistic:这张建议卡随后台 refreshPeople/tidy.refresh 自然消失。
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
  }
  async function doMergeDup(name: string, g: PersonSummary[]) {
    // 无 optimistic:同名组卡随后台 refreshPeople 自然消失。
    await act(async () => {
      const winner = dupPrimaryId(name, g);
      await mergeDuplicatePeople(g, winner, mergePerson, (journalId) => {
        lastManual = { journalId, label: `「${name}」并成一条` };
      });
    });
  }
  async function doDeleteNoSample(p: PersonSummary) {
    // 无 optimistic:无样本卡随后台 refreshPeople 自然消失。
    await act(async () => {
      await deletePerson(p.id);
    });
  }
  /** 一键清理剩余全部无样本条目(二段确认后)。 */
  async function doCleanAll() {
    const rest = queue.filter((i) => i.kind === "nosample");
    // 无 optimistic:这批卡随后台 refreshPeople 自然消失。
    await act(async () => {
      for (const i of rest) {
        if (i.kind === "nosample") await deletePerson(i.person.id);
      }
    });
  }
  function doDismiss(item: TidyItem) {
    tidy.dismiss(tidyItemKey(item));
  }
  async function doAck(r: MergeReceipt) {
    await act(
      async () => {
        await acknowledgeMerge(r.journal_id);
      },
      () => tidy.removeReceipt(r.journal_id),
    );
  }
  /** 不可撤销的回执只剩回看价值,批量确认清掉,不必逐张点。 */
  async function ackAllInvalid() {
    const invalid = queue.filter(
      (i) => i.kind === "receipt" && i.receipt.invalid_reason !== null,
    );
    await act(
      async () => {
        for (const i of invalid) {
          if (i.kind === "receipt") await acknowledgeMerge(i.receipt.journal_id);
        }
      },
      () => {
        for (const i of invalid) {
          if (i.kind === "receipt") tidy.removeReceipt(i.receipt.journal_id);
        }
      },
    );
  }
  async function doUndo(journalId: string) {
    // removeReceipt 对不存在的 id 是 no-op:doUndo 既用于回执撤销卡也用于手动
    // 合并后的页内撤销条,两处共用同一 optimistic 安全。
    await act(
      async () => {
        await undoMerge(journalId);
        lastManual = null;
      },
      () => tidy.removeReceipt(journalId),
    );
  }
</script>

{#snippet personPane(pid: string, name: string)}
  {@const p = personById.get(pid)}
  <div class="pane">
    <div class="pane-head">
      <span class="dot" style="background: {speakerInk(pid, 'mic')}"></span>
      <a class="pname" href="/speakers/{pid}">{plabel(pid, name)}</a>
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
  <h1>会议搭子</h1>
  <p class="desc">
    录到的说话人会自动登记。给"未命名"的人<strong>命名</strong>后,之后的录制会自动认出他并直接显示名字;
    声纹足够相似的会自动归并,拿不准的进「分析说话人」等你拍板。从左侧选择一个人查看详情、试听原声或管理。
  </p>

  {#if people.length === 0}
    <div class="empty">
      <p>还没有说话人。</p>
      <p class="hint">录一场会议(单人说话累计满 30 秒),停止后会自动出现在左侧。</p>
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

    <section class="tidy">
      <div class="tidy-head">
        <span class="tidy-title">分析说话人</span>
        <span class="summary">
          {#if pendingN === 0 && receiptsN === 0}
            没有要整理的
          {:else}
            {#if pendingN > 0}{pendingN} 件待处理{/if}{#if pendingN > 0 && receiptsN > 0} · {/if}{#if receiptsN > 0}{receiptsN} 条已自动归并{/if}
          {/if}
        </span>
        {#if tidy.loading}<span class="refreshing">正在比对声纹…</span>{/if}
      </div>

      {#if invalidReceiptsN > 1}
        <div class="tools">
          <button class="mini plain" disabled={busy || live} onclick={ackAllInvalid}>
            一键确认全部 {invalidReceiptsN} 条不可撤销回执
          </button>
        </div>
      {/if}

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

      {#if queue.length === 0}
        {#if tidy.loading}
          <p class="hint">正在比对声纹…</p>
        {:else}
          <p class="hint">都整理完了——新的建议会随录制自动出现;高置信的会自动归并并在这里留回执。</p>
        {/if}
      {:else}
        <div class="stack">
          {#each queue as item (tidyItemKey(item))}
            {#if item.kind === "receipt"}
              {@const r = item.receipt}
              <section class="card" class:archived={r.invalid_reason}>
                <div class="card-tag">
                  {#if r.invalid_reason}
                    已自动归并 · 仅存档 · {r.invalid_reason}
                  {:else}
                    已自动归并
                  {/if}
                </div>
                <div class="card-title">
                  {plabel(r.loser, r.loser_name)} → {plabel(r.winner, r.winner_name)}
                  {#if r.similarity !== null}
                    <span class="sim strong">相似度 {Math.round(r.similarity * 100)}%</span>
                  {/if}
                </div>
                <div class="panes">
                  <div class="pane">
                    <div class="pane-head">
                      <span class="dot" style="background: {speakerInk(r.loser, 'mic')}"></span>
                      <span class="pname">{plabel(r.loser, r.loser_name)}</span>
                      <span class="pane-tag">已并入</span>
                    </div>
                    <div class="samples">
                      {#each r.loser_sample_paths as path, i (path)}
                        <button
                          class="chip"
                          class:playing={playingKey === path}
                          title={playingKey === path ? "停止" : "试听合并前的原声"}
                          onclick={() => audition.toggle(path, path)}
                        >
                          {playingKey === path ? "◼" : "▶"} 快照 {i + 1}
                        </button>
                      {:else}
                        <span class="hint">无可试听的快照</span>
                      {/each}
                    </div>
                  </div>
                  <svg class="arrow" width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                    <path d="M2.5 8h10M9 4.5L13.5 8 9 11.5" />
                  </svg>
                  <div class="pane-col">
                    {@render personPane(r.winner, r.winner_name)}
                    {#if r.winner_sample_paths.length > 0}
                      <div class="snap-listen">
                        <span class="snap-label">合并时的原声</span>
                        {#each r.winner_sample_paths as path, i (path)}
                          <button
                            class="chip"
                            class:playing={playingKey === path}
                            title={playingKey === path ? "停止" : "试听合并时刻的样本"}
                            onclick={() => audition.toggle(path, path)}
                          >
                            {playingKey === path ? "◼" : "▶"} 快照 {i + 1}
                          </button>
                        {/each}
                      </div>
                    {/if}
                  </div>
                </div>
                <p class="hint">声纹足够相似已自动并入。听一下不对劲就撤销;没问题点「好」。</p>
                <div class="acts">
                  {#if r.invalid_reason}
                    <button class="mini" disabled={busy || live} onclick={() => doAck(r)}>知道了</button>
                  {:else}
                    <button class="mini accent" disabled={busy || live} onclick={() => doAck(r)}>好</button>
                    <button class="mini" disabled={busy || live} onclick={() => doUndo(r.journal_id)}>撤销</button>
                  {/if}
                </div>
                {#if r.invalid_reason}
                  <p class="hint">不能撤销时:先听「合并时的原声」核对;确认并错也不必慌——后续录制会把不同的声音重新分开建档,不会将错就错。</p>
                {/if}
              </section>
            {:else if item.kind === "suggestion"}
              {@const s = item.suggestion}
              <section class="card">
                <div class="card-tag">归属建议</div>
                <div class="card-title">
                  这两条像同一个人吗?
                  <span class="sim" class:strong={isStrong(s)}>
                    相似度 {Math.round(s.similarity * 100)}%{isStrong(s) ? " · 很可能" : ""}
                  </span>
                </div>
                <div class="panes">
                  {@render personPane(s.loser, s.loser_name)}
                  <svg class="arrow" width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                    <path d="M2.5 8h10M9 4.5L13.5 8 9 11.5" />
                  </svg>
                  {@render personPane(s.winner, s.winner_name)}
                </div>
                <p class="hint">两边各听一段原声,确认是同一个人再合并;合并保留双方声纹,认得更准。</p>
                <div class="acts">
                  <button class="mini accent" disabled={busy || live} onclick={() => doMergeSuggestion(s)}>合并</button>
                  <button class="mini" disabled={busy || live} onclick={() => doIgnoreSuggestion(s)}>忽略</button>
                </div>
              </section>
            {:else if item.kind === "dup"}
              {@const g = item.people}
              {@const primary = dupPrimaryId(item.name, g)}
              <section class="card">
                <div class="card-tag">同名重复</div>
                <div class="card-title">「{item.name}」有 {g.length} 条,多半是同一个人被拆开了</div>
                <div class="panes wrap">
                  {#each g as p (p.id)}
                    <div class="dup-item" class:primary={p.id === primary}>
                      {@render personPane(p.id, p.name)}
                      <label class="pick">
                        <input
                          type="radio"
                          name={"dup-" + item.name}
                          checked={p.id === primary}
                          onchange={() => (dupPrimary = { ...dupPrimary, [item.name]: p.id })}
                        />
                        作为主条目
                      </label>
                    </div>
                  {/each}
                </div>
                <p class="hint">其余条目将并入主条目(默认最近活跃的);逐条试听核对。</p>
                <div class="acts">
                  <button class="mini accent" disabled={busy || live} onclick={() => doMergeDup(item.name, g)}>
                    全部并入主条目
                  </button>
                  <button class="mini" disabled={busy || live} onclick={() => doDismiss(item)}>忽略</button>
                </div>
              </section>
            {:else}
              {@const p = item.person}
              <section class="card">
                <div class="card-tag">无样本条目</div>
                <div class="card-title">{plabel(p.id, p.name)}——没有原声可核对</div>
                <div class="panes">
                  {@render personPane(p.id, p.name)}
                </div>
                <p class="hint warn-text">
                  删除后历史笔记中这个说话人恢复显示为编号,不可恢复。认不出是谁就删,拿不准就保留。
                </p>
                <div class="acts">
                  <button class="mini danger" disabled={busy || live} onclick={() => doDeleteNoSample(p)}>删除</button>
                  <button class="mini" disabled={busy || live} onclick={() => doDismiss(item)}>保留</button>
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
          {/each}
        </div>
      {/if}
    </section>

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
  /* 分析区:常驻展示,不再是独立收件箱入口——标题+同行摘要,下接工具条/横幅/
     撤销条/全量卡列表 */
  .tidy {
    margin-bottom: 1.5rem;
  }
  .tidy-head {
    display: flex;
    align-items: baseline;
    gap: 0.8rem;
    margin-bottom: 1rem;
  }
  .tidy-title {
    font-size: 1.05rem;
    font-weight: 600;
    color: var(--ink);
  }
  .summary {
    color: var(--ink-faint);
    font-size: 0.82rem;
  }
  .refreshing {
    color: var(--ink-faint);
    font-size: 0.78rem;
  }
  .pick-hint {
    color: var(--ink-faint);
    font-size: 0.85rem;
  }
  .stack {
    display: flex;
    flex-direction: column;
    gap: 0.8rem;
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
  .tools {
    display: flex;
    justify-content: flex-end;
    margin-bottom: 0.8rem;
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
  .card.archived {
    opacity: 0.75;
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
  .pane-col {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
  }
  .snap-listen {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
    margin-top: 0.45rem;
    align-items: center;
  }
  .snap-label {
    color: var(--ink-faint);
    font-size: 0.76rem;
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
  .pane-tag {
    color: var(--ink-faint);
    font-size: 0.72rem;
  }
  .samples {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
  }
  .pane-head + .samples {
    margin-top: 0.4rem;
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
</style>
