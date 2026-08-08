<script lang="ts">
  import { onMount } from "svelte";
  import { convertFileSrc } from "@tauri-apps/api/core";
  import {
    acknowledgeMerge,
    acknowledgeIdentify,
    applyIdentifySuggestion,
    deletePerson,
    listPeople,
    mergePerson,
    personNotes,
    rejectIdentifySuggestion,
    undoIdentifyApply,
    renamePerson,
    restoreMergedPerson,
    undoMerge,
    type IdentifySuggestion,
    type MergeReceipt,
    type PersonMergeSuggestion,
    type PersonSummary,
  } from "$lib/people";
  import { formatDate, formatDuration, speakerInk, type NoteSummary } from "$lib/notes";
  import { isStrong, sugKey, tidy } from "$lib/tidy.svelte";
  import {
    buildTidyQueue,
    mergeDuplicatePeople,
    resolveSugTarget,
    splitArchive,
    tidyItemKey,
    type TidyItem,
  } from "$lib/tidyQueue";
  import { createAudition, type PlayerLike } from "$lib/tidyAudio";
  import { keyedOnce } from "$lib/keyedOnce";
  import { recording } from "$lib/recording.svelte";
  import PersonPickList from "$lib/PersonPickList.svelte";
  import { t } from "$lib/i18n/index.svelte";

  // 主从结构的落地页:人物索引在侧栏,本页概览引导之下常驻「分析说话人」区——
  // 队列由共享 store + 人物表现算,全量渲染;处理完的项随重算自然从列表消失。
  let people = $state<PersonSummary[]>([]);
  let error = $state("");
  let busy = $state(false);
  /** 卡片级动作失败:挂在出错的那张卡下面,用户在哪儿点的就在哪儿看到原因
      (顶部横幅在长队列底部操作时完全在视野外,表现为"点了没反应")。 */
  let actionError = $state<{ key: string; msg: string } | null>(null);
  /** 一键清理无样本条目的二段确认(控件只渲染在第一张无样本卡上,单处出现)。 */
  let confirmClean = $state(false);
  /** 存档折叠组的展开态(默认收起,失效回执只剩回看价值,不必占屏)。 */
  let archiveOpen = $state(false);
  /** 打开选人 popover 的建议卡(sugKey);同屏至多一个。 */
  let sugPickFor = $state<string | null>(null);
  let sugPickQuery = $state("");

  const named = $derived(people.filter((p) => p.name).length);
  const unnamed = $derived(people.length - named);

  const personById = $derived(new Map(people.map((p) => [p.id, p])));
  const queueParts = $derived(
    splitArchive(buildTidyQueue(people, tidy.visible, tidy.receipts, tidy.dismissed, tidy.visibleIdentify)),
  );
  const queue = $derived(queueParts.pending);
  const archived = $derived(queueParts.archived);
  const pendingN = $derived(queue.filter((i) => i.kind !== "receipt").length);
  const receiptsN = $derived(queue.filter((i) => i.kind === "receipt").length);
  const nosampleN = $derived(queue.filter((i) => i.kind === "nosample").length);
  /** 一键清理控件只挂在第一张无样本卡:共享确认态若逐卡渲染,会同屏出现多组
      重复的破坏性确认按钮。 */
  const firstNosampleKey = $derived(
    (() => {
      const first = queue.find((i) => i.kind === "nosample");
      return first ? tidyItemKey(first) : null;
    })(),
  );
  const live = $derived(recording.isLive);

  const plabel = (id: string, name: string) =>
    name || t("speakers.personFallback", { n: id.replace(/^P/, "") });

  // ── 会议上下文(拍板信息):每人最近 3 场,懒加载缓存 ──
  let notesCache = $state<Record<string, NoteSummary[]>>({});
  // keyedOnce 去重:effect 因 notesCache 写入而重跑时,同一人不再重复发
  // person_notes(此前 8 人放大成 87 次调用的 IPC 风暴)。
  const loadNotes = keyedOnce(async (pid: string) => {
    try {
      notesCache[pid] = (await personNotes(pid)).slice(0, 3);
    } catch {
      notesCache[pid] = [];
    }
  });
  /** 某条目涉及的人物(回执卡的 loser 已并入 winner,看 winner 即可)。 */
  function itemIds(item: TidyItem): string[] {
    if (item.kind === "suggestion") return [item.suggestion.loser, item.suggestion.winner];
    if (item.kind === "receipt") return [item.receipt.winner];
    if (item.kind === "dup") return item.people.map((p) => p.id);
    if (item.kind === "identify") return item.suggestion.person_id ? [item.suggestion.person_id] : [];
    return [item.person.id];
  }
  // 首屏可见范围懒加载:queue 前 12 张卡涉及的人物才主动拉会议上下文,滚动到
  // 深处的卡按需可再点进详情页看。
  $effect(() => {
    for (const item of queue.slice(0, 12)) {
      for (const id of itemIds(item)) loadNotes(id);
    }
  });

  // ── 试听:单实例互斥,离开页面即停(列表页同屏多卡,切卡不再意味着换焦点)──
  let playingKey = $state<string | null>(null);
  const audition = createAudition(
    (src) => new Audio(convertFileSrc(src)) as unknown as PlayerLike,
    (k) => (playingKey = k),
    (msg) => (error = t("speakers.auditionFailed", { msg })),
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
      error = t("common.loadFailed", { e });
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
  // 留在原地,错误就地(errKey 对应卡)或顶部横幅透出后端文案 ──
  async function act(fn: () => Promise<void>, optimistic?: () => void, errKey?: string) {
    if (busy) return;
    busy = true;
    error = "";
    actionError = null;
    audition.stop();
    confirmClean = false;
    sugPickFor = null;
    try {
      await fn();
      optimistic?.(); // 后端成功即本地收起,不等整轮重算
    } catch (e) {
      if (errKey) actionError = { key: errKey, msg: `${e}` };
      else error = `${e}`;
    }
    // 成败都对账:同名组连并是逐条落库的,部分成功后本地视图若不回到与库一致,
    // 重试会拿已并走的旧 id 报「人物不存在」;回执撤销被拒同理(条目已失效要
    // 重拉才会转「仅存档」形态)。
    recording.bumpPeople(); // 驱动 layout 后台 tidy.refresh(单飞)与各处同步
    void refreshPeople();
    busy = false;
  }

  async function doMergeSuggestion(s: PersonMergeSuggestion, targetId: string, targetName: string) {
    // 无 optimistic:这张建议卡随后台 refreshPeople/tidy.refresh 自然消失。
    await act(
      async () => {
        const jid = await mergePerson(s.loser, targetId);
        tidy.lastManual = {
          journalId: jid,
          label: `${plabel(s.loser, s.loser_name)} → ${plabel(targetId, targetName)}`,
        };
      },
      undefined,
      `s:${s.loser}>${s.winner}`,
    );
  }
  /** 换人弹层「新建」:库里没这个人=左侧说话人就是他,就地命名(声纹/样本/笔记
      原地不动,无合并日志);已命名的人不再进归属建议,这张卡随重算消失——乐观
      先移除,不写 dismissed(命名后建议本就不再生成,不该占一条落盘忽略)。
      在途 refresh 覆写 suggestions 可能让卡短暂闪回,下一轮自愈——tidy.doRefresh
      注释里已明文接受的单飞刷新权衡,此处不另设防。 */
  async function doNameAsNew(s: PersonMergeSuggestion, name: string) {
    await act(
      async () => {
        await renamePerson(s.loser, name);
      },
      () => {
        tidy.suggestions = tidy.suggestions.filter((x) => sugKey(x) !== sugKey(s));
      },
      `s:${s.loser}>${s.winner}`,
    );
  }
  function doIgnoreSuggestion(s: PersonMergeSuggestion) {
    tidy.ignore(s);
  }
  async function doMergeDup(name: string, g: PersonSummary[]) {
    // 无 optimistic:同名组卡随后台 refreshPeople 自然消失。
    await act(
      async () => {
        const winner = dupPrimaryId(name, g);
        await mergeDuplicatePeople(g, winner, mergePerson, (journalId) => {
          tidy.lastManual = { journalId, label: t("speakers.dupMergedLabel", { name }) };
        });
      },
      undefined,
      `d:${name}`,
    );
  }
  async function doDeleteNoSample(p: PersonSummary) {
    // 无 optimistic:无样本卡随后台 refreshPeople 自然消失。
    await act(
      async () => {
        await deletePerson(p.id);
      },
      undefined,
      `n:${p.id}`,
    );
  }
  /** 一键清理剩余全部无样本条目(二段确认后)。 */
  async function doCleanAll() {
    const rest = queue.filter((i) => i.kind === "nosample");
    // 无 optimistic:这批卡随后台 refreshPeople 自然消失。错误挂在承载清理控件
    // 的第一张无样本卡上。
    const errKey = firstNosampleKey ?? undefined;
    await act(
      async () => {
        for (const i of rest) {
          if (i.kind === "nosample") await deletePerson(i.person.id);
        }
      },
      undefined,
      errKey,
    );
  }
  /** 证据类型 → 展示文案(词典键强类型,不走模板串键)。 */
  function evidenceLabel(ty: string): string {
    if (ty === "self_intro") return t("speakers.identifyEvSelfIntro");
    if (ty === "addressed_reply") return t("speakers.identifyEvReply");
    if (ty === "third_person_exclusion") return t("speakers.identifyEvExclusion");
    return t("speakers.identifyEvTopic");
  }
  /** P2b 自动回执「好」:确认自动认人。 */
  async function doAckIdentify(s: IdentifySuggestion) {
    if (!s.op_id) return;
    const opId = s.op_id;
    await act(
      async () => {
        await acknowledgeIdentify(s.note_id, opId);
      },
      () => tidy.removeIdentify(s.note_id, s.fingerprint),
      `i:${s.note_id}:${opId}`,
    );
  }
  /** P2b 自动回执「撤销」:解除关联+还原质心;质心未还原时横幅如实提示。 */
  async function doUndoIdentify(s: IdentifySuggestion) {
    if (!s.op_id) return;
    const opId = s.op_id;
    await act(
      async () => {
        const restored = await undoIdentifyApply(s.note_id, opId);
        if (!restored) error = t("speakers.identifyUndoPartial");
      },
      () => tidy.removeIdentify(s.note_id, s.fingerprint),
      `i:${s.note_id}:${opId}`,
    );
  }

  /** 身份建议「就是 TA」:后端确认(关联+回灌+建档),成功即乐观收起。 */
  async function doApplyIdentify(s: IdentifySuggestion) {
    await act(
      async () => {
        await applyIdentifySuggestion(s.note_id, s.fingerprint);
      },
      () => tidy.removeIdentify(s.note_id, s.fingerprint),
      `i:${s.note_id}:${s.fingerprint}`,
    );
  }
  /** 身份建议「不是」:后端拒绝(同目标永久静默),成功即乐观收起。 */
  async function doRejectIdentify(s: IdentifySuggestion) {
    await act(
      async () => {
        await rejectIdentifySuggestion(s.note_id, s.fingerprint);
      },
      () => tidy.removeIdentify(s.note_id, s.fingerprint),
      `i:${s.note_id}:${s.fingerprint}`,
    );
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
      `r:${r.journal_id}`,
    );
  }
  /** 失效回执「拆回独立说话人」:按合并时快照恢复原编号,历史笔记段落重新归他。 */
  async function doRestore(r: MergeReceipt) {
    await act(
      async () => {
        await restoreMergedPerson(r.journal_id);
      },
      () => tidy.removeReceipt(r.journal_id),
      `r:${r.journal_id}`,
    );
  }
  /** 不可撤销的回执只剩回看价值,批量确认清掉,不必逐张点。 */
  async function ackAllInvalid() {
    const invalid = archived;
    await act(
      async () => {
        for (const i of invalid) {
          await acknowledgeMerge(i.receipt.journal_id);
        }
      },
      () => {
        for (const i of invalid) {
          tidy.removeReceipt(i.receipt.journal_id);
        }
      },
    );
  }
  async function doUndo(journalId: string, errKey?: string) {
    // removeReceipt 对不存在的 id 是 no-op:doUndo 既用于回执撤销卡也用于手动
    // 合并后的撤销条,两处共用同一 optimistic 安全。
    await act(
      async () => {
        await undoMerge(journalId);
        tidy.lastManual = null;
      },
      () => tidy.removeReceipt(journalId),
      errKey,
    );
  }
</script>

{#snippet cardError(key: string)}
  {#if actionError?.key === key}
    <p class="card-error">{actionError.msg}</p>
  {/if}
{/snippet}

{#snippet personPane(pid: string, name: string)}
  {@const p = personById.get(pid)}
  <div class="pane">
    <div class="pane-head">
      <span class="dot" style="background: {speakerInk(pid, 'mic')}"></span>
      <a class="pname" href="/speakers/{pid}">{plabel(pid, name)}</a>
    </div>
    {#if p}
      <div class="pane-meta">
        {t("speakers.paneMeta", {
          date: formatDate(p.last_seen),
          dur: formatDuration(Math.floor(p.total_ms / 1000)),
        })}
      </div>
      <div class="samples">
        {#each p.sample_paths as path, i (path)}
          <button
            class="chip"
            class:playing={playingKey === path}
            title={playingKey === path ? t("speakers.stop") : t("speakers.auditionSample")}
            onclick={() => audition.toggle(path, path)}
          >
            {playingKey === path ? "◼" : "▶"}
            {p.sample_dates[i]
              ? formatDate(p.sample_dates[i]).slice(5, 10)
              : t("speakers.sampleN", { n: i + 1 })}
          </button>
        {:else}
          <span class="hint">{t("speakers.noSamples")}</span>
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
      <div class="pane-meta">{t("speakers.mergedAway")}</div>
    {/if}
  </div>
{/snippet}

{#snippet receiptCard(r: MergeReceipt)}
  <section class="card" class:archived={r.invalid_reason}>
    <div class="card-tag">
      {#if r.invalid_reason}
        {t("speakers.autoMergedArchived", { reason: r.invalid_reason })}
      {:else}
        {t("speakers.autoMerged")}
      {/if}
    </div>
    <div class="card-title">
      {plabel(r.loser, r.loser_name)} → {plabel(r.winner, r.winner_name)}
      {#if r.similarity !== null}
        <span class="sim strong">
          {t("speakers.similarity", { pct: Math.round(r.similarity * 100) })}
        </span>
      {/if}
    </div>
    <div class="panes">
      <div class="pane">
        <div class="pane-head">
          <span class="dot" style="background: {speakerInk(r.loser, 'mic')}"></span>
          <span class="pname">{plabel(r.loser, r.loser_name)}</span>
          <span class="pane-tag">{t("speakers.mergedTag")}</span>
        </div>
        <div class="samples">
          {#each r.loser_sample_paths as path, i (path)}
            <button
              class="chip"
              class:playing={playingKey === path}
              title={playingKey === path ? t("speakers.stop") : t("speakers.auditionPreMerge")}
              onclick={() => audition.toggle(path, path)}
            >
              {playingKey === path ? "◼" : "▶"}
              {t("speakers.snapshotN", { n: i + 1 })}
            </button>
          {:else}
            <span class="hint">{t("speakers.noSnapshots")}</span>
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
            <span class="snap-label">{t("speakers.snapshotAtMerge")}</span>
            {#each r.winner_sample_paths as path, i (path)}
              <button
                class="chip"
                class:playing={playingKey === path}
                title={playingKey === path ? t("speakers.stop") : t("speakers.auditionMergeSample")}
                onclick={() => audition.toggle(path, path)}
              >
                {playingKey === path ? "◼" : "▶"}
                {t("speakers.snapshotN", { n: i + 1 })}
              </button>
            {/each}
          </div>
        {/if}
      </div>
    </div>
    {#if !r.invalid_reason}
      <p class="hint">{t("speakers.autoMergedHint")}</p>
    {/if}
    <div class="acts">
      {#if r.invalid_reason}
        <button class="mini accent" disabled={busy || live} onclick={() => doRestore(r)}>{t("speakers.restoreSplit")}</button>
        <button class="mini" disabled={busy || live} onclick={() => doAck(r)}>{t("speakers.gotIt")}</button>
      {:else}
        <button class="mini accent" disabled={busy || live} onclick={() => doAck(r)}>{t("speakers.ok")}</button>
        <button class="mini" disabled={busy || live} onclick={() => doUndo(r.journal_id, `r:${r.journal_id}`)}>{t("speakers.undo")}</button>
      {/if}
    </div>
    {#if r.invalid_reason}
      <p class="hint">{t("speakers.restoreHint")}</p>
    {/if}
    {@render cardError(`r:${r.journal_id}`)}
  </section>
{/snippet}

<main class="container">
  <h1>{t("speakers.title")}</h1>
  <p class="desc">
    {t("speakers.desc1")}<strong>{t("speakers.descName")}</strong>{t("speakers.desc2")}
  </p>

  {#if people.length === 0}
    <div class="empty">
      <p>{t("speakers.emptyTitle")}</p>
      <p class="hint">{t("speakers.emptyHint")}</p>
    </div>
  {:else}
    <div class="stats">
      <div class="stat">
        <span class="num">{people.length}</span>
        <span class="label">{t("speakers.statTotal")}</span>
      </div>
      <div class="stat">
        <span class="num">{named}</span>
        <span class="label">{t("speakers.statNamed")}</span>
      </div>
      {#if unnamed > 0}
        <div class="stat todo">
          <span class="num">{unnamed}</span>
          <span class="label">{t("speakers.statUnnamed")}</span>
        </div>
      {/if}
    </div>

    <section class="tidy">
      <div class="tidy-head">
        <span class="tidy-title">{t("speakers.tidyTitle")}</span>
        <span class="summary">
          {#if pendingN === 0 && receiptsN === 0}
            {t("speakers.tidyNone")}
          {:else}
            {#if pendingN > 0}{t("speakers.pendingCount", { n: pendingN })}{/if}{#if pendingN > 0 && receiptsN > 0} · {/if}{#if receiptsN > 0}{t("speakers.receiptsCount", { n: receiptsN })}{/if}
          {/if}
        </span>
        {#if tidy.loading}<span class="refreshing">{t("speakers.comparing")}</span>{/if}
        <span class="head-spacer"></span>
        <button class="mini plain" disabled={tidy.loading} onclick={() => void tidy.refresh()}>{t("speakers.reTidy")}</button>
      </div>

      {#if live}
        <div class="banner warn">{t("speakers.liveBanner")}</div>
      {/if}
      {#if error}
        <div class="banner">{error}</div>
      {/if}
      {#if tidy.lastManual}
        <div class="undo-strip">
          {t("speakers.mergedLabel", { label: tidy.lastManual.label })}
          <button class="mini" disabled={busy || live} onclick={() => doUndo(tidy.lastManual!.journalId)}>{t("speakers.undo")}</button>
          <button class="mini plain" onclick={() => (tidy.lastManual = null)}>{t("speakers.ok")}</button>
        </div>
      {/if}

      {#if queue.length === 0 && archived.length === 0}
        {#if tidy.loading}
          <p class="hint">{t("speakers.comparing")}</p>
        {:else}
          <p class="hint">{t("speakers.allDone")}</p>
        {/if}
      {:else}
        {#if queue.length > 0}
        <div class="stack">
          {#each queue as item (tidyItemKey(item))}
            {#if item.kind === "receipt"}
              {@render receiptCard(item.receipt)}
            {:else if item.kind === "identify"}
              {@const s = item.suggestion}
              {#if s.status === "auto_applied"}
                <section class="card">
                  <div class="card-tag">{t("speakers.tagIdentifyAuto")}</div>
                  <div class="card-title">{t("speakers.identifyAutoTitle", { cluster: s.cluster, name: s.person_name })}</div>
                  <p class="hint">{t("speakers.identifyMeta", { title: s.note_title, cluster: s.cluster })}</p>
                  {#if s.quote}
                    <p class="hint">{t("speakers.identifyQuote", { quote: s.quote, kind: evidenceLabel(s.evidence_type) })}</p>
                  {/if}
                  {#if !s.revertible}
                    <p class="hint">{t("speakers.identifyAutoConflict")}</p>
                  {/if}
                  <div class="acts">
                    <button class="mini accent" disabled={busy} onclick={() => doAckIdentify(s)}>{t("speakers.ok")}</button>
                    {#if s.revertible}
                      <button class="mini" disabled={busy || live} onclick={() => doUndoIdentify(s)}>{t("speakers.undo")}</button>
                    {/if}
                  </div>
                  {@render cardError(tidyItemKey(item))}
                </section>
              {:else}
              <section class="card">
                <div class="card-tag">{t("speakers.tagIdentify")}</div>
                <div class="card-title">
                  {s.is_new
                    ? t("speakers.identifyTitleNew", { name: s.person_name })
                    : t("speakers.identifyTitle", { name: s.person_name })}
                  {#if s.tier === "high"}
                    <span class="sim strong">{t("speakers.identifyTierHigh")}</span>
                  {/if}
                </div>
                <p class="hint">
                  {t("speakers.identifyMeta", { title: s.note_title, cluster: s.cluster })}
                </p>
                {#if s.quote}
                  <p class="hint">
                    {t("speakers.identifyQuote", { quote: s.quote, kind: evidenceLabel(s.evidence_type) })}
                  </p>
                {/if}
                <div class="acts">
                  <button class="mini accent" disabled={busy || live} onclick={() => doApplyIdentify(s)}>
                    {s.is_new ? t("speakers.identifyApplyNew") : t("speakers.identifyApply")}
                  </button>
                  <button class="mini" disabled={busy} onclick={() => doRejectIdentify(s)}>{t("speakers.identifyReject")}</button>
                </div>
                {@render cardError(tidyItemKey(item))}
              </section>
              {/if}
            {:else if item.kind === "suggestion"}
              {@const s = item.suggestion}
              {@const skey = `${s.loser}>${s.winner}`}
              {@const target = resolveSugTarget(s, tidy.sugOverride, personById)}
              <section class="card">
                <div class="card-tag">{t("speakers.tagSuggestion")}</div>
                <div class="card-title">
                  {t("speakers.sugTitle")}
                  {#if target.overridden}
                    <!-- 相似度只对系统建议对成立,换了人再挂着就是误导 -->
                    <span class="sim">{t("speakers.manualOverride")}</span>
                    <button
                      class="mini plain"
                      onclick={() => {
                        const { [skey]: _, ...rest } = tidy.sugOverride;
                        tidy.sugOverride = rest;
                      }}>{t("speakers.restoreSuggestion")}</button>
                  {:else}
                    <span class="sim" class:strong={isStrong(s)}>
                      {t("speakers.similarity", {
                        pct: Math.round(s.similarity * 100),
                      })}{isStrong(s) ? t("speakers.likelySuffix") : ""}
                    </span>
                  {/if}
                </div>
                <div class="panes">
                  {@render personPane(s.loser, s.loser_name)}
                  <svg class="arrow" width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                    <path d="M2.5 8h10M9 4.5L13.5 8 9 11.5" />
                  </svg>
                  <div class="switch-anchor">
                    {@render personPane(target.id, target.name)}
                    <!-- 猜错人时就地换目标,不必忽略后绕去详情页「合并到…」 -->
                    <button
                      class="switch-btn"
                      title={t("speakers.switchTitle")}
                      onclick={() => {
                        confirmClean = false;
                        sugPickQuery = "";
                        sugPickFor = sugPickFor === skey ? null : skey;
                      }}>{t("speakers.switchPerson")}</button>
                    {#if sugPickFor === skey}
                      {@const q = sugPickQuery.trim()}
                      <button class="menu-scrim" aria-label={t("speakers.closeMenu")} onclick={() => (sugPickFor = null)}></button>
                      <div class="menu">
                        <div class="menu-title">
                          {t("speakers.mergeInto", { name: plabel(s.loser, s.loser_name) })}
                        </div>
                        <!-- svelte-ignore a11y_autofocus -->
                        <input class="pick-input" autofocus placeholder={t("speakers.searchPlaceholder")} bind:value={sugPickQuery} />
                        <PersonPickList
                          people={people.filter((p) => p.id !== s.loser && p.id !== target.id)}
                          query={sugPickQuery}
                          onpick={(p) => {
                            tidy.sugOverride = { ...tidy.sugOverride, [skey]: p.id };
                            loadNotes(p.id);
                            sugPickFor = null;
                          }}
                        />
                        {#if q && !people.some((p) => p.name === q)}
                          <!-- 库里没这个人:就地命名,不必出弹层绕去详情页 -->
                          <button class="create-row" disabled={busy} onclick={() => doNameAsNew(s, q)}>
                            {t("speakers.createAndName", { name: q })}
                          </button>
                        {/if}
                      </div>
                    {/if}
                  </div>
                </div>
                <p class="hint">{t("speakers.sugHint")}</p>
                <div class="acts">
                  <button class="mini accent" disabled={busy || live} onclick={() => doMergeSuggestion(s, target.id, target.name)}>{t("speakers.merge")}</button>
                  <!-- 忽略是本地处置(dismissed 元数据),后端录制中也放行,不必陪绑置灰 -->
                  <button class="mini" disabled={busy} onclick={() => doIgnoreSuggestion(s)}>{t("speakers.ignore")}</button>
                </div>
                {@render cardError(tidyItemKey(item))}
              </section>
            {:else if item.kind === "dup"}
              {@const g = item.people}
              {@const primary = dupPrimaryId(item.name, g)}
              <section class="card">
                <div class="card-tag">{t("speakers.tagDup")}</div>
                <div class="card-title">{t("speakers.dupTitle", { name: item.name, n: g.length })}</div>
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
                        {t("speakers.asPrimary")}
                      </label>
                    </div>
                  {/each}
                </div>
                <p class="hint">{t("speakers.dupHint")}</p>
                <div class="acts">
                  <button class="mini accent" disabled={busy || live} onclick={() => doMergeDup(item.name, g)}>
                    {t("speakers.mergeAllPrimary")}
                  </button>
                  <button class="mini" disabled={busy} onclick={() => doDismiss(item)}>{t("speakers.ignore")}</button>
                </div>
                {@render cardError(tidyItemKey(item))}
              </section>
            {:else}
              {@const p = item.person}
              <section class="card">
                <div class="card-tag">{t("speakers.tagNosample")}</div>
                <div class="card-title">{t("speakers.nosampleTitle", { name: plabel(p.id, p.name) })}</div>
                <div class="panes">
                  {@render personPane(p.id, p.name)}
                </div>
                <p class="hint warn-text">{t("speakers.nosampleHint")}</p>
                <div class="acts">
                  <button class="mini danger" disabled={busy || live} onclick={() => doDeleteNoSample(p)}>{t("speakers.delete")}</button>
                  <button class="mini" disabled={busy} onclick={() => doDismiss(item)}>{t("speakers.keep")}</button>
                  {#if nosampleN > 1 && tidyItemKey(item) === firstNosampleKey}
                    <span class="spacer"></span>
                    {#if confirmClean}
                      <span class="warn-text">{t("speakers.cleanConfirmWarn", { n: nosampleN })}</span>
                      <button class="mini danger" disabled={busy || live} onclick={doCleanAll}>{t("speakers.confirmClean")}</button>
                      <button class="mini plain" onclick={() => (confirmClean = false)}>{t("speakers.cancel")}</button>
                    {:else}
                      <button class="mini plain" disabled={busy || live} onclick={() => (confirmClean = true)}>
                        {t("speakers.cleanAll", { n: nosampleN })}
                      </button>
                    {/if}
                  {/if}
                </div>
                {@render cardError(tidyItemKey(item))}
              </section>
            {/if}
          {/each}
        </div>
        {/if}

        {#if archived.length > 0}
          <div class="archive">
            <div class="archive-head">
              <button class="archive-toggle" onclick={() => (archiveOpen = !archiveOpen)}>
                {archiveOpen ? t("speakers.collapse") : t("speakers.expand")}
                {t("speakers.archivedCount", { n: archived.length })}
              </button>
              <span class="hint">{t("speakers.archiveHint")}</span>
              <span class="spacer"></span>
              <button class="mini plain" disabled={busy || live} onclick={ackAllInvalid}>{t("speakers.ackAll")}</button>
            </div>
            {#if archiveOpen}
              <div class="stack">
                {#each archived as item (tidyItemKey(item))}
                  {@render receiptCard(item.receipt)}
                {/each}
              </div>
            {/if}
          </div>
        {/if}
      {/if}
    </section>

    <p class="pick-hint">
      {t("speakers.pickHint")}
      {#if unnamed > 0}{t("speakers.pickHintUnnamed")}{/if}
    </p>
  {/if}
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
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
  .head-spacer {
    flex: 1;
  }
  .pick-hint {
    color: var(--ink-faint);
    font-size: 0.85rem;
  }
  /* 自适应多列:宽窗 2-3 列一屏多卡,窄窗自动退单列;卡片顶对齐各保己高,
     DOM 序即队列序(逐行阅读)。存档组/横幅/撤销条在 .stack 外,天然整行。 */
  .stack {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(26rem, 1fr));
    gap: 0.8rem;
    align-items: start;
  }
  .archive {
    margin-top: 1rem;
  }
  .archive-head {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    margin-bottom: 0.8rem;
  }
  .archive-toggle {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    border-radius: var(--radius-md);
    font-size: 0.82rem;
    padding: 0.25em 0.75em;
    cursor: pointer;
  }
  .archive-toggle:hover {
    background: var(--surface-soft);
  }
  /* 粘性:长队列滚到深处时顶部通知(试听失败/批量操作错误/录制中提示)仍在
     视野内——否则底部操作失败表现为"点了没反应"。tint 在暗色下半透明,叠一层
     canvas 垫底,悬浮时不透出下方卡片。 */
  .banner {
    position: sticky;
    top: 0;
    z-index: 5;
    background:
      linear-gradient(var(--danger-tint), var(--danger-tint)),
      var(--canvas);
    border: 1px solid var(--danger-line);
    color: var(--danger-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin-bottom: 0.8rem;
    font-size: 0.9rem;
  }
  .banner.warn {
    background:
      linear-gradient(var(--warning-tint), var(--warning-tint)),
      var(--canvas);
    border-color: var(--warning-line);
    color: var(--warning-ink);
  }
  /* 卡片级错误:失败原因贴在出错的那张卡下,与顶部横幅同色系 */
  .card-error {
    background: var(--danger-tint);
    border: 1px solid var(--danger-line);
    color: var(--danger-ink);
    border-radius: var(--radius-md);
    padding: 0.45rem 0.6rem;
    font-size: 0.82rem;
    margin: 0.6rem 0 0;
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
  /* 换目标:锚定 winner 面板,按钮悬浮右上角(faint 小字,hover 显色) */
  .switch-anchor {
    position: relative;
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  .switch-btn {
    position: absolute;
    top: 0.45rem;
    right: 0.55rem;
    border: none;
    background: none;
    color: var(--ink-faint);
    font-size: 0.75rem;
    padding: 0.1em 0.3em;
    border-radius: var(--radius-sm);
    cursor: pointer;
  }
  .switch-btn:hover {
    color: var(--accent);
    background: var(--accent-tint);
  }
  .menu-scrim {
    position: fixed;
    inset: 0;
    z-index: 9;
    border: 0;
    padding: 0;
    background: transparent;
    cursor: default;
  }
  .menu {
    position: absolute;
    top: 2rem;
    right: 0.4rem;
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
  .pick-input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.35rem 0.5rem;
    margin-bottom: 0.25rem;
    background: transparent;
    border: none;
    border-bottom: 1px solid var(--hairline);
    outline: none;
    font: inherit;
    font-size: 0.85rem;
    color: var(--ink);
  }
  .pick-input::placeholder {
    color: var(--ink-faint);
  }
  /* 新建行:menu 行形态,accent 色标识"这是创建动作"而非候选 */
  .create-row {
    display: block;
    width: 100%;
    padding: 0.38rem 0.55rem;
    background: none;
    border: none;
    border-top: 1px solid var(--hairline);
    border-radius: 0 0 var(--radius-md) var(--radius-md);
    color: var(--accent);
    font: inherit;
    font-size: 0.85rem;
    text-align: left;
    cursor: pointer;
  }
  .create-row:hover:not(:disabled) {
    background: var(--surface-soft);
  }
  .create-row:disabled {
    color: var(--ink-faint);
    cursor: default;
  }
</style>

<svelte:window
  onkeydown={(e) => {
    if (e.key !== "Escape") return;
    if (sugPickFor) sugPickFor = null;
    else if (confirmClean) confirmClean = false;
  }}
/>
