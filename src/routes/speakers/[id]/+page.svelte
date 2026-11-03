<script lang="ts">
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { convertFileSrc } from "@tauri-apps/api/core";
  import {
    listPeople,
    personNotes,
    renamePerson,
    mergePerson,
    undoMerge,
    deletePerson,
    deletePersonSample,
    rebuildPersonVoiceprint,
    type PersonSummary,
    type PersonMergeSuggestion,
    movePersonSample,
  } from "$lib/people";
  import { tidy, sugKey, isStrong } from "$lib/tidy.svelte";
  import { describePlayError } from "$lib/tidyAudio";
  import { formatDate, formatDuration, speakerColor, speakerInk, type NoteSummary } from "$lib/notes";
  import { recording } from "$lib/recording.svelte";
  import PersonPickList from "$lib/PersonPickList.svelte";
  import { onPersonSampleJob } from "$lib/events";
  import { t } from "$lib/i18n/index.svelte";
  import { Button } from "$lib/ui/components/button";
  import { Input } from "$lib/ui/components/input";
  import { Badge } from "$lib/ui/components/badge";
  import * as Popover from "$lib/ui/components/popover";
  import { cn } from "$lib/ui/utils";

  // 主从结构的"从":本页只呈现/操作一个人;人物索引在侧栏声纹库页签。
  const personId = $derived($page.params.id as string);

  let people = $state<PersonSummary[]>([]);
  let loaded = $state(false);
  let error = $state("");
  /** 撤销条重入门:双击/连点时后端还没落盘,同一条日志不能被并发撤销两次。 */
  let undoing = $state(false);

  // 详情数据从全量列表 find:合并菜单本就需要其他人物做目标,一次拉取两用。
  const person = $derived(people.find((p) => p.id === personId) ?? null);
  const others = $derived(
    people
      .filter((p) => p.id !== personId)
      .sort((a, b) => (b.last_seen || "").localeCompare(a.last_seen || "")),
  );

  const sourceLabel = (s: string) =>
    s === "mic" ? t("speakers.sourceMic") : s === "system" ? t("speakers.sourceSystem") : s;

  /** 合并菜单里的展示名(未命名人用全局编号 + 最近出现指认)。 */
  function displayName(p: PersonSummary): string {
    return (
      p.name ||
      t("speakers.personFallbackRecent", {
        n: p.id.replace(/^P/, ""),
        date: formatDate(p.last_seen),
      })
    );
  }

  // 出现过的会议(试听之外确认"认对了人"的第二手段);增值层,取失败静默按空处理。
  let appearNotes = $state<NoteSummary[]>([]);

  async function refresh() {
    const forId = personId;
    const notesPromise = personNotes(forId).catch(() => [] as NoteSummary[]);
    try {
      people = await listPeople();
      error = "";
    } catch (e) {
      error = t("common.loadFailed", { e });
    }
    const notes = await notesPromise;
    if (forId === personId) appearNotes = notes;
    loaded = true;
  }

  // 路由参数变化(侧栏点选另一人)时重载;同页操作后手动 refresh。撤销条是
  // 会话级全局(tidy.lastManual,带"谁并入谁"标签),导航不清——它是手动合并
  // 在 UI 上唯一的撤销入口,一次无意识导航不该把它永久关掉。
  $effect(() => {
    void personId;
    stopSample();
    stopCtx();
    closeAllOps();
    editingId = null;
    dupRename = null;
    appearNotes = [];
    refresh();
  });

  // ── 改名(沿旧管理页语义:未命名给显眼「命名」,已命名点名字改) ──
  let editingId = $state<string | null>(null);
  let editingName = $state("");
  /** 改名撞库中他人现名:大概率是同一个人被声纹拆重,先确认合并还是真重名。
      同名者可能不止一位,全部列出让用户挑并入哪条,而不是静默选第一个。 */
  let dupRename = $state<{ name: string; others: PersonSummary[] } | null>(null);

  function beginRename() {
    if (!person) return;
    editingId = person.id;
    editingName = person.name;
    dupRename = null;
    closeAllOps();
  }

  async function commitRename() {
    const p = person;
    if (!p || editingId !== p.id || dupRename) return;
    const text = editingName.trim();
    if (!text || text === p.name) {
      editingId = null;
      return; // 空/未变:静默还原,不当真改名
    }
    const sameNamed = others.filter((o) => o.name === text);
    if (sameNamed.length > 0) {
      dupRename = { name: text, others: sameNamed };
      return; // 输入框保留,下方出确认条
    }
    editingId = null;
    await applyRename(p.id, text);
  }

  async function applyRename(id: string, text: string) {
    try {
      await renamePerson(id, text);
      await refresh();
      recording.bumpPeople(); // 侧栏索引同步新名
    } catch (err) {
      error = t("speakers.renameFailed", { e: err });
      await refresh();
    }
  }

  /** 重名确认:就是同一个人 → 当前人并入选中的同名者,跳转对方详情。 */
  async function dupMerge(other: PersonSummary) {
    const p = person;
    if (!p || !dupRename) return;
    editingId = null;
    dupRename = null;
    try {
      const jid = await mergePerson(p.id, other.id);
      tidy.lastManual = { journalId: jid, label: `${displayName(p)} → ${displayName(other)}` };
      recording.bumpPeople();
      goto(`/speakers/${other.id}`);
    } catch (e) {
      error = `${e}`;
      await refresh(); // 失败也对账:别让本地视图停在与库不一致的状态
    }
  }

  /** 重名确认:确实是另一个人 → 照常改名,允许重名(列表以最近出现区分)。 */
  async function dupKeep() {
    const p = person;
    const d = dupRename;
    if (!p || !d) return;
    editingId = null;
    dupRename = null;
    await applyRename(p.id, d.name);
  }

  // ── 合并/删除(同屏只开一个操作态) ──
  let mergeOpen = $state(false);
  let pendingMergeWinner = $state<string | null>(null);
  let confirmDelete = $state(false);
  /** 「合并到…」菜单的检索词;与录音页选人面板共用 PersonPickList 过滤逻辑。 */
  let mergeQuery = $state("");

  /** 合并结果预览:名字继承规则(winner 名优先,winner 无名继承 loser 名)对用户是
      黑盒,确认前把结果摆出来;loser 数据比 winner 厚时提示通常反向合并更好。 */
  const mergePreview = $derived.by(() => {
    const winner = people.find((o) => o.id === pendingMergeWinner);
    if (!winner || !person) return null;
    return {
      name: winner.name || person.name,
      total: winner.total_ms + person.total_ms,
      thickerLoser: person.total_ms > winner.total_ms,
    };
  });

  function closeAllOps() {
    mergeOpen = false;
    pendingMergeWinner = null;
    mergeQuery = "";
    confirmDelete = false;
    confirmSampleIdx = null;
    moveSampleIdx = null;
    moveTarget = null;
    moveQuery = "";
  }

  // ── 样本改归属(2026-08-28 样本管理):听出来是别人的声音 → 归到对的人 ──
  /** 正在选目标的样本下标;null=未打开。 */
  let moveSampleIdx = $state<number | null>(null);
  let moveTarget = $state<string | null>(null);
  let moveQuery = $state("");
  let moving = $state(false);
  /** 样本区一次性反馈(改归属/删除完成)。 */
  let sampleMsg = $state("");
  /** 后台跟进(重建声纹 + 同步会议关联)进行中:文件操作已完成、页面已刷新,
      这里只是告诉用户"声纹还在重算";done 后再刷一次拿到重算后的会议列表。 */
  let sampleJobRunning = $state(false);
  $effect(() => {
    let unlisten: (() => void) | null = null;
    let disposed = false;
    void onPersonSampleJob((e) => {
      if (!e.person_ids.includes(personId)) return;
      if (e.state === "running") {
        sampleJobRunning = true;
      } else {
        sampleJobRunning = false;
        if (e.state === "failed") error = t("speakers.sampleJobFailed", { e: e.message });
        void refresh();
      }
    }).then((u) => {
      if (disposed) u();
      else unlisten = u;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  });
  async function doMoveSample() {
    const p = person;
    const i = moveSampleIdx;
    const to = moveTarget;
    const target = people.find((o) => o.id === to);
    if (!p || i === null || !to || !target || moving) return;
    const path = p.sample_paths[i];
    moving = true;
    sampleMsg = "";
    stopSample();
    try {
      await movePersonSample(p.id, path, to);
      closeAllOps();
      sampleMsg = t("speakers.moveSampleDone", { name: displayName(target) });
      sampleJobRunning = true;
      recording.bumpPeople();
      await refresh();
    } catch (e) {
      error = t("speakers.moveSampleFailed", { e });
      await refresh();
    } finally {
      moving = false;
    }
  }

  /** 样本删除的行内二段确认:记下标(与 sample_paths 对齐)。 */
  let confirmSampleIdx = $state<number | null>(null);

  // 按样本重建(2026-08-23 污染修复):删完坏样本一键重算,rebuilding 防双击。
  let rebuilding = $state(false);
  let rebuildMsg = $state("");
  async function doRebuildFromSamples() {
    const p = person;
    if (!p || rebuilding) return;
    rebuilding = true;
    rebuildMsg = "";
    try {
      await rebuildPersonVoiceprint(p.id);
      rebuildMsg = t("speakers.rebuildDone");
      await refresh();
    } catch (e) {
      rebuildMsg = t("speakers.rebuildFailed", { e });
    } finally {
      rebuilding = false;
    }
  }

  async function doDeleteSample(i: number) {
    const p = person;
    if (!p) return;
    const path = p.sample_paths[i];
    confirmSampleIdx = null;
    stopSample(); // 删除后下标会移位,正在播的一律先停,别让播放态指错样本
    sampleMsg = "";
    try {
      await deletePersonSample(p.id, path);
      sampleMsg = t("speakers.sampleDeleted");
      sampleJobRunning = true;
      recording.bumpPeople();
      await refresh();
    } catch (e) {
      error = t("speakers.deleteSampleFailed", { e });
      await refresh();
    }
  }

  /** 合并进行中(后端要落日志、迁样本、按样本重建声纹,数秒):按钮显"合并中…"
      并禁用,免得看起来像没反应而连点(2026-08-28 用户实报)。 */
  let merging = $state(false);
  async function doMerge() {
    const winner = pendingMergeWinner;
    const target = people.find((o) => o.id === winner);
    if (!person || !winner || !target || merging) return;
    const loser = person;
    merging = true;
    try {
      const jid = await mergePerson(loser.id, winner);
      closeAllOps();
      tidy.lastManual = { journalId: jid, label: `${displayName(loser)} → ${displayName(target)}` };
      recording.bumpPeople();
      // 本人已并入对方:跳到对方详情,让"这个人现在是谁"立即可见。
      goto(`/speakers/${winner}`);
    } catch (e) {
      // 录制中后端拒绝等错误文案原样展示。
      error = `${e}`;
      await refresh(); // 失败也对账
    } finally {
      merging = false;
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
      error = t("common.deleteFailed", { e });
    }
  }

  // ── 上下文整理提示(建议跟人走):当前这个人涉及归属建议/同名重复时,详情页
  //    头部直接给出,不必回概览找「整理」。数据走共享 tidy store(侧栏徽标同源)。 ──
  /** 上下文建议:对方必须仍在库(失效目标保险,与概览队列同口径)。 */
  const related = $derived(
    tidy
      .involving(personId)
      .filter((s) => people.some((p) => p.id === (s.loser === personId ? s.winner : s.loser))),
  );
  /** 同名的另一人(重名十有八九=同一人被拆重,给直达入口)。 */
  const sameName = $derived(person?.name ? (others.find((o) => o.name === person!.name) ?? null) : null);

  /** 建议行里"对方"的一侧(当前人可能是 loser 也可能是 winner)。 */
  const ctxOther = (s: PersonMergeSuggestion) =>
    s.loser === personId
      ? { id: s.winner, name: s.winner_name }
      : { id: s.loser, name: s.loser_name };

  /** 撤销条数据在 tidy.lastManual(会话级全局;后端日志兜底,失效时撤销报错原样透出)。 */
  async function undoLastMerge() {
    if (undoing) return;
    undoing = true;
    try {
      const last = tidy.lastManual;
      if (!last) return;
      try {
        await undoMerge(last.journalId);
        tidy.lastManual = null;
        recording.bumpPeople();
        await tidy.refresh();
        await refresh();
      } catch (e) {
        error = `${e}`;
        // 失败也对账:日志可能已失效,重拉让回执/建议回到与库一致的形态
        recording.bumpPeople();
        await refresh();
      }
    } finally {
      undoing = false;
    }
  }

  async function applyCtxSuggestion(s: PersonMergeSuggestion) {
    stopCtx();
    stopSample();
    try {
      const jid = await mergePerson(s.loser, s.winner);
      tidy.lastManual = {
        journalId: jid,
        label: `${s.loser_name || t("speakers.personFallback", { n: s.loser.replace(/^P/, "") })} → ${s.winner_name || t("speakers.personFallback", { n: s.winner.replace(/^P/, "") })}`,
      };
      recording.bumpPeople();
      await tidy.refresh();
      if (s.loser === personId) {
        goto(`/speakers/${s.winner}`); // 本人被并走:跳到归属后的人
      } else {
        await refresh();
      }
    } catch (e) {
      error = `${e}`;
      await refresh(); // 失败也对账
    }
  }

  // 提示卡内的对方试听:不听原声没法拍板该不该合。与本人样本试听单实例互斥。
  let ctxAudio: HTMLAudioElement | null = null;
  let ctxPlayingId = $state<string | null>(null);

  function stopCtx() {
    ctxAudio?.pause();
    ctxAudio = null;
    ctxPlayingId = null;
  }

  function toggleCtxSample(pid: string) {
    if (ctxPlayingId === pid) {
      stopCtx();
      return;
    }
    stopSample();
    stopCtx();
    const path = people.find((o) => o.id === pid)?.sample_paths[0];
    if (!path) return;
    const a = new Audio(convertFileSrc(path));
    a.onended = () => {
      if (ctxPlayingId === pid) stopCtx();
    };
    ctxAudio = a;
    ctxPlayingId = pid;
    // 播放失败显性化:静默吞掉=「点了没声」还不知道为什么(概览页同一语义)
    void a.play().catch((err) => {
      if (ctxPlayingId === pid) {
        stopCtx();
        error = t("speakers.auditionFailed", { msg: describePlayError(err) });
      }
    });
  }

  $effect(() => stopCtx);

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
    stopCtx(); // 提示卡的对方试听同属单实例
    const path = person?.sample_paths[idx];
    if (!path) return;
    const id = person!.id;
    const a = new Audio(convertFileSrc(path));
    a.onended = () => {
      if (playingIdx === idx && personId === id) stopSample();
    };
    sampleAudio = a;
    playingIdx = idx;
    void a.play().catch((err) => {
      if (personId === id) {
        stopSample();
        error = t("speakers.auditionFailed", { msg: describePlayError(err) });
      }
    });
  }

  $effect(() => stopSample);

  // 圆形试听小钮(上下文整理行):播放中用焦点蓝描边/文字(旧 .listen-mini.playing)。
  function listenMiniClass(playing: boolean): string {
    return cn("size-[1.35rem] rounded-full text-muted-foreground", playing && "border-ring text-ring hover:text-ring");
  }
  // 样本行的大播放钮:播放中实心焦点蓝(旧 .play.playing)。
  function playBtnClass(playing: boolean): string {
    return cn(
      "size-[2.1rem] rounded-full bg-[var(--surface-press)] text-foreground hover:border-ring hover:text-ring active:scale-[0.94] dark:bg-[var(--surface-press)]",
      playing && "border-ring bg-[var(--accent)] text-[var(--on-accent)] hover:bg-[var(--accent)] hover:text-[var(--on-accent)] dark:bg-[var(--accent)]",
    );
  }
  // 弹层(合并到…/归到别人)内容的公共外观:与旧 .menu 同尺寸。
  const menuContentClass = "max-h-64 w-auto min-w-64 gap-1 overflow-y-auto p-[0.35rem]";
</script>

<main class="container">
  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if tidy.lastManual}
    <div class="undo-strip">
      {t("speakers.mergedLabel", { label: tidy.lastManual.label })}
      <Button variant="outline" size="sm" disabled={recording.isLive || undoing} onclick={undoLastMerge}>{t("speakers.undo")}</Button>
      <Button variant="outline" size="sm" onclick={() => (tidy.lastManual = null)}>{t("speakers.ok")}</Button>
    </div>
  {/if}

  {#if !loaded}
    <!-- 首拉中不闪"不存在" -->
  {:else if !person}
    <div class="empty">
      <p>{t("speakers.notFound")}</p>
      <p class="hint">{t("speakers.notFoundHint")}<a href="/speakers">{t("speakers.backToList")}</a></p>
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
            <Input
              class="h-auto w-auto min-w-52 border-ring bg-background px-[0.4em] py-[0.1em] text-[1.2rem] font-medium md:text-[1.2rem] dark:bg-background"
              autofocus
              placeholder={t("speakers.namePlaceholder")}
              bind:value={editingName}
              oninput={() => (dupRename = null)}
              onkeydown={(e) => {
                // 中文输入法选词的 Enter 不算提交(迁移约定)
                if (e.key === "Enter" && !e.isComposing) commitRename();
                if (e.key === "Escape") {
                  editingId = null;
                  dupRename = null;
                }
              }}
              onblur={commitRename}
            />
            {#if dupRename}
              <!-- 重名确认(menu 语言):撞名十有八九=同一个人被拆重,先问合并。
                   同名者不止一位时逐条列出,让用户挑并入哪条,不静默选第一个 -->
              <div class="menu dup-menu">
                <div class="menu-title">
                  {#if dupRename.others.length === 1}
                    {t("speakers.dupOneTitle", {
                      name: dupRename.name,
                      date: formatDate(dupRename.others[0].last_seen),
                      dur: formatDuration(Math.floor(dupRename.others[0].total_ms / 1000)),
                    })}
                  {:else}
                    {t("speakers.dupManyTitle", { n: dupRename.others.length, name: dupRename.name })}
                  {/if}
                </div>
                {#if dupRename.others.length === 1}
                  <div class="confirm-row">
                    <Button variant="default" size="sm" onmousedown={(e) => e.preventDefault()} onclick={() => dupMerge(dupRename!.others[0])}>{t("speakers.dupMergeYes")}</Button>
                    <Button variant="outline" size="sm" onmousedown={(e) => e.preventDefault()} onclick={dupKeep}>{t("speakers.dupKeepNo")}</Button>
                    <Button
                      variant="ghost"
                      size="sm" class="text-[var(--ink-faint)]"
                      onmousedown={(e) => e.preventDefault()}
                      onclick={() => {
                        editingId = null;
                        dupRename = null;
                      }}>{t("speakers.cancel")}</Button>
                  </div>
                {:else}
                  {#each dupRename.others as o (o.id)}
                    <div class="confirm-row">
                      <Button variant="default" size="sm" onmousedown={(e) => e.preventDefault()} onclick={() => dupMerge(o)}>
                        {t("speakers.dupMergeThis", {
                          date: formatDate(o.last_seen),
                          dur: formatDuration(Math.floor(o.total_ms / 1000)),
                        })}
                      </Button>
                    </div>
                  {/each}
                  <div class="confirm-row">
                    <Button variant="outline" size="sm" onmousedown={(e) => e.preventDefault()} onclick={dupKeep}>{t("speakers.dupKeepNone")}</Button>
                    <Button
                      variant="ghost"
                      size="sm" class="text-[var(--ink-faint)]"
                      onmousedown={(e) => e.preventDefault()}
                      onclick={() => {
                        editingId = null;
                        dupRename = null;
                      }}>{t("speakers.cancel")}</Button>
                  </div>
                {/if}
              </div>
            {/if}
          {:else if person.name}
            <Button
              variant="ghost"
              class="group/name -mx-[0.35em] -my-[0.05em] h-auto gap-[0.4em] rounded-sm px-[0.35em] py-[0.05em] text-inherit hover:bg-[var(--accent-tint)] dark:hover:bg-[var(--accent-tint)]"
              title={t("speakers.clickRename")}
              onclick={beginRename}
            >
              <h1>{person.name}</h1>
              <svg class="text-[var(--ink-faint)] group-hover/name:text-ring" width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M11.3 2.4l2.3 2.3L5.3 13l-3 .7.7-3z" />
              </svg>
            </Button>
          {:else}
            <h1 class="unnamed">{t("speakers.personFallback", { n: person.id.replace(/^P/, "") })}</h1>
            <Button size="sm" class="rounded-full px-[0.9em] shadow-[var(--shadow-btn)]" onclick={beginRename}>{t("speakers.nameCta")}</Button>
          {/if}
        </div>
        <div class="meta-line">
          <span class="stat"><b>{person.sample_paths.length}</b>{t("speakers.statSamples")}</span>
          <span class="stat"><b>{appearNotes.length}</b>{t("speakers.statMeetings")}</span>
          <span class="stat-sep" aria-hidden="true"></span>
          <span>{t("speakers.metaLine", {
            date: formatDate(person.last_seen),
            dur: formatDuration(Math.floor(person.total_ms / 1000)),
          })}</span>
          {#each person.sources as s (s)}
            <Badge variant="secondary" class="h-auto rounded-md bg-[var(--surface-press)] px-[0.5em] py-[0.02em] text-[0.78rem] font-normal text-muted-foreground">{sourceLabel(s)}</Badge>
          {/each}
        </div>
        {#if !person.name}
          <p class="naming-hint">{t("speakers.namingHint")}</p>
        {/if}
      </div>
    </header>

    {#if related.length > 0 || sameName}
      <!-- 上下文整理提示:这个人自己的归属建议/同名重复,就地处理不必回概览 -->
      <div class="ctx-card">
        {#each related as s (sugKey(s))}
          {@const other = ctxOther(s)}
          <div class="ctx-row">
            <span class="ctx-text">
              {t("speakers.ctxSamePre")}
              <a href="/speakers/{other.id}"
                >{t("speakers.quotedName", {
                  name: other.name || t("speakers.personFallback", { n: other.id.replace(/^P/, "") }),
                })}</a
              >
              {t("speakers.ctxSamePost", { pct: Math.round(s.similarity * 100), strong: isStrong(s) })}
            </span>
            {#if (people.find((o) => o.id === other.id)?.sample_paths.length ?? 0) > 0}
              <!-- 对方原声试听:不听没法拍板该不该合 -->
              <Button
                variant="outline"
                size="icon-xs"
                class={listenMiniClass(ctxPlayingId === other.id)}
                title={ctxPlayingId === other.id ? t("speakers.stop") : t("speakers.auditionOther")}
                aria-label={ctxPlayingId === other.id ? t("speakers.stop") : t("speakers.auditionOther")}
                onclick={() => toggleCtxSample(other.id)}
              >
                {#if ctxPlayingId === other.id}
                  <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true"><rect x="3.5" y="3.5" width="9" height="9" rx="1.5" fill="currentColor" /></svg>
                {:else}
                  <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true"><path d="M5 2.9v10.2c0 .7.8 1.2 1.4.8l7.4-5.1c.6-.4.6-1.2 0-1.6L6.4 2.1c-.6-.4-1.4.1-1.4.8z" fill="currentColor" /></svg>
                {/if}
              </Button>
            {/if}
            <Button
              variant="default"
              size="sm"
              disabled={recording.isLive}
              title={recording.isLive ? t("speakers.noMergeWhileRecording") : t("speakers.ctxMergeTitle")}
              onclick={() => applyCtxSuggestion(s)}>{t("speakers.merge")}</Button>
            <Button variant="outline" size="sm" onclick={() => tidy.ignore(s)}>{t("speakers.ignore")}</Button>
          </div>
        {/each}
        {#if sameName}
          <div class="ctx-row">
            <span class="ctx-text">
              {t("speakers.sameNameNote", {
                name: person.name,
                date: formatDate(sameName.last_seen),
              })}
            </span>
            {#if sameName.sample_paths.length > 0}
              <Button
                variant="outline"
                size="icon-xs"
                class={listenMiniClass(ctxPlayingId === sameName.id)}
                title={ctxPlayingId === sameName.id ? t("speakers.stop") : t("speakers.auditionOther")}
                aria-label={ctxPlayingId === sameName.id ? t("speakers.stop") : t("speakers.auditionOther")}
                onclick={() => toggleCtxSample(sameName.id)}
              >
                {#if ctxPlayingId === sameName.id}
                  <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true"><rect x="3.5" y="3.5" width="9" height="9" rx="1.5" fill="currentColor" /></svg>
                {:else}
                  <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true"><path d="M5 2.9v10.2c0 .7.8 1.2 1.4.8l7.4-5.1c.6-.4.6-1.2 0-1.6L6.4 2.1c-.6-.4-1.4.1-1.4.8z" fill="currentColor" /></svg>
                {/if}
              </Button>
            {/if}
            <Button variant="outline" size="sm" href="/speakers/{sameName.id}">{t("speakers.viewOther")}</Button>
          </div>
        {/if}
      </div>
    {/if}

    <!-- 声纹样本:样本是声纹的唯一真源。区块纵排:标题行(计数 · 状态 · 重建)+ 列表
         (圆形播放钮 · 样本名/日期 · 来源会议 · 悬停操作)+ 脚注。 -->
    <section class="panel">
      <div class="panel-head">
        <h2 class="panel-title">
          {t("speakers.audition")}
          {#if person.sample_paths.length > 0}<span class="count">{person.sample_paths.length}</span>{/if}
        </h2>
        <div class="panel-actions">
          {#if sampleJobRunning || rebuilding}
            <span class="status running"><span class="dot" aria-hidden="true"></span>{rebuilding ? t("speakers.rebuilding") : t("speakers.sampleJobRunning")}</span>
          {:else if sampleMsg || rebuildMsg}
            <span class="status">{sampleMsg || rebuildMsg}</span>
          {/if}
          {#if person.sample_paths.length > 0}
            <Button variant="ghost" size="sm" class="text-[var(--ink-faint)]" disabled={rebuilding || sampleJobRunning || recording.isLive} title={t("speakers.rebuildTitle")} onclick={doRebuildFromSamples}>
              {t("speakers.rebuildFromSamples")}
            </Button>
          {/if}
        </div>
      </div>
      {#if person.sample_paths.length > 0}
        <ul class="sample-list">
          {#each person.sample_paths as sp, i (sp)}
            {@const origin = person.sample_notes[i] ?? null}
            {@const date = formatDate(person.sample_dates[i] ?? "")}
            <li class="sample-row" class:active={moveSampleIdx === i || confirmSampleIdx === i} class:playing={playingIdx === i}>
              <Button
                variant="outline"
                size="icon"
                class={playBtnClass(playingIdx === i)}
                title={playingIdx === i ? t("speakers.stop") : t("speakers.playSample")}
                aria-label={playingIdx === i ? t("speakers.stop") : t("speakers.playSample")}
                onclick={() => toggleSample(i)}
              >
                {#if playingIdx === i}
                  <span class="bars" aria-hidden="true"><span></span><span></span><span></span></span>
                {:else}
                  <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
                    <path d="M5 2.9v10.2c0 .7.8 1.2 1.4.8l7.4-5.1c.6-.4.6-1.2 0-1.6L6.4 2.1c-.6-.4-1.4.1-1.4.8z" fill="currentColor" />
                  </svg>
                {/if}
              </Button>
              <div class="sample-main">
                <div class="sample-title">
                  <span class="sample-name">{t("speakers.sampleN", { n: i + 1 })}</span>
                  {#if date !== "—"}<span class="sample-date">{date.slice(0, 10)}</span>{/if}
                </div>
                <div class="sample-sub">
                  {#if origin}
                    <a href="/notes/{origin.note_id}" title={origin.cluster_id ? `${origin.note_id} · ${origin.cluster_id}` : origin.note_id}>
                      {t("speakers.sampleFrom", { title: origin.title || origin.note_id })}
                    </a>
                    {#if origin.inferred}<span class="tag">{t("speakers.sampleFromInferred")}</span>{/if}
                  {:else}
                    <span class="muted">{t("speakers.sampleNoOrigin")}</span>
                  {/if}
                </div>
              </div>
              <div class="sample-ops">
                {#if confirmSampleIdx === i}
                  <span class="confirm-text">{t("speakers.deleteSampleConfirm")}</span>
                  <Button variant="destructive" size="sm" onclick={() => doDeleteSample(i)}>{t("speakers.confirmDelete")}</Button>
                  <Button variant="outline" size="sm" onclick={() => (confirmSampleIdx = null)}>{t("speakers.cancel")}</Button>
                {:else}
                  <!-- 归到别人:与「合并到…」同一套选人菜单;选定后二段确认。开合仍归 moveSampleIdx 管,
                       点外面/Esc 经 onOpenChange 走 closeAllOps(旧透明遮罩的语义)。 -->
                  <Popover.Root
                    open={moveSampleIdx === i}
                    onOpenChange={(open) => {
                      closeAllOps();
                      if (open) moveSampleIdx = i;
                    }}
                  >
                    <Popover.Trigger>
                      {#snippet child({ props })}
                        <Button
                          {...props}
                          variant="outline"
                          size="sm"
                          disabled={recording.isLive || moving || others.length === 0}
                          title={recording.isLive ? t("speakers.noMergeWhileRecording") : t("speakers.moveSample")}
                        >{t("speakers.moveSample")}</Button>
                      {/snippet}
                    </Popover.Trigger>
                    <Popover.Content align="end" class={menuContentClass}>
                      {#if !moveTarget}
                        <div class="menu-title">{t("speakers.moveSampleTitle", { n: i + 1 })}</div>
                        <!-- svelte-ignore a11y_autofocus -->
                        <Input class="mb-1" autofocus placeholder={t("speakers.searchPlaceholder")} bind:value={moveQuery} />
                        <PersonPickList people={others} query={moveQuery} onpick={(p) => (moveTarget = p.id)} />
                      {:else}
                        {@const target = people.find((o) => o.id === moveTarget)}
                        <div class="menu-title confirm-title">
                          {t("speakers.moveSampleConfirm", { n: i + 1, name: target ? displayName(target) : "?" })}
                        </div>
                        <div class="confirm-row">
                          <Button variant="default" size="sm" disabled={moving} onclick={doMoveSample}>
                            {moving ? t("speakers.rebuilding") : t("speakers.moveSampleGo")}
                          </Button>
                          <Button variant="outline" size="sm" onclick={closeAllOps}>{t("speakers.cancel")}</Button>
                        </div>
                      {/if}
                    </Popover.Content>
                  </Popover.Root>
                  <Button
                    variant="outline"
                    size="icon-sm"
                    class="size-[1.7rem] text-muted-foreground hover:border-destructive hover:text-destructive"
                    title={t("speakers.deleteSample")}
                    aria-label={t("speakers.deleteSample")}
                    onclick={() => {
                      closeAllOps();
                      confirmSampleIdx = i;
                    }}
                  >
                    <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" aria-hidden="true">
                      <path d="M3 4.5h10M6.5 4.5V3h3v1.5M5 4.5l.6 8h4.8l.6-8" />
                    </svg>
                  </Button>
                {/if}
              </div>
            </li>
          {/each}
        </ul>
        <p class="panel-foot">{t("speakers.auditionHint")}</p>
      {:else}
        <p class="panel-empty">{t("speakers.noSamplesYet")}</p>
      {/if}
    </section>

    <!-- 出现过的会议:试听之外确认"认对了人"的第二手段;点击直达笔记 -->
    <section class="panel">
      <div class="panel-head">
        <h2 class="panel-title">
          {t("speakers.appearTitle")}
          {#if appearNotes.length > 0}<span class="count">{appearNotes.length}</span>{/if}
        </h2>
      </div>
      {#if appearNotes.length > 0}
        <ul class="appear-list">
          {#each appearNotes as n (n.id)}
            <li>
              <a class="appear-row" href="/notes/{n.id}">
                <span class="appear-title">{n.title}</span>
                <span class="appear-meta">{formatDate(n.started_at)}</span>
                <svg class="chev" width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 3.5L10.5 8 6 12.5" /></svg>
              </a>
            </li>
          {/each}
        </ul>
      {:else}
        <p class="panel-empty">{t("speakers.noAppear")}</p>
      {/if}
    </section>

    <!-- 管理动作:合并/删除,两段确认;录制中后端会拒,前端同步禁用 -->
    <section class="panel">
      <div class="panel-head">
        <h2 class="panel-title">{t("speakers.manage")}</h2>
      </div>
      <div class="ops">
        <!-- 合并到…:开合仍归 mergeOpen/pendingMergeWinner 管;点外面/Esc 经 onOpenChange 走
             closeAllOps(旧透明遮罩的语义),再点按钮同样收起。 -->
        <Popover.Root
          open={mergeOpen || Boolean(pendingMergeWinner)}
          onOpenChange={(open) => {
            closeAllOps();
            mergeOpen = open;
          }}
        >
          <Popover.Trigger>
            {#snippet child({ props })}
              <Button
                {...props}
                variant="outline"
                disabled={others.length === 0 || recording.isLive}
                title={recording.isLive
                  ? t("speakers.noMergeWhileRecording")
                  : others.length === 0
                    ? t("speakers.noOthers")
                    : t("speakers.mergeTitle")}
              >
                {t("speakers.mergeTo")}
              </Button>
            {/snippet}
          </Popover.Trigger>
          <Popover.Content align="start" class={menuContentClass}>
            {#if !pendingMergeWinner}
              <div class="menu-title">{t("speakers.mergeInto", { name: displayName(person) })}</div>
              <!-- svelte-ignore a11y_autofocus -->
              <Input class="mb-1" autofocus placeholder={t("speakers.searchPlaceholder")} bind:value={mergeQuery} />
              <PersonPickList people={others} query={mergeQuery} onpick={(p) => (pendingMergeWinner = p.id)} />
            {:else}
              {@const target = people.find((o) => o.id === pendingMergeWinner)}
              <div class="menu-title confirm-title">
                {t("speakers.confirmMergeTitle", { name: target ? displayName(target) : "?" })}
                {#if mergePreview}{t("speakers.confirmMergePreview", {
                    name: mergePreview.name || t("speakers.unnamed"),
                    dur: formatDuration(Math.floor(mergePreview.total / 1000)),
                  })}{/if}
                {t("speakers.confirmMergeTail")}
              </div>
              {#if mergePreview?.thickerLoser}
                <div class="menu-note">{t("speakers.reverseNote")}</div>
              {/if}
              <div class="confirm-row">
                <Button variant="destructive" size="sm" disabled={merging} onclick={doMerge}>
                  {merging ? t("speakers.merging") : t("speakers.confirmMerge")}
                </Button>
                <Button variant="outline" size="sm" onclick={closeAllOps}>{t("speakers.cancel")}</Button>
              </div>
            {/if}
          </Popover.Content>
        </Popover.Root>

        {#if confirmDelete}
          <div class="confirm-inline">
            <!-- 删除后果一句话说清:笔记文字不动,只是不再认得这个人 -->
            <span class="delete-hint">{t("speakers.deleteHint")}</span>
            <Button variant="destructive" size="sm" onclick={doDelete}>{t("speakers.confirmDelete")}</Button>
            <Button variant="outline" size="sm" onclick={() => (confirmDelete = false)}>{t("speakers.cancel")}</Button>
          </div>
        {:else}
          <Button
            variant="outline"
            class="hover:border-destructive hover:bg-transparent hover:text-destructive dark:hover:bg-transparent"
            disabled={recording.isLive}
            title={recording.isLive ? t("speakers.noDeleteWhileRecording") : t("speakers.deleteTitle")}
            onclick={() => {
              closeAllOps();
              confirmDelete = true;
            }}
          >
            {t("speakers.deleteFromList")}
          </Button>
        {/if}
      </div>
    </section>
  {/if}
</main>

<svelte:window
  onkeydown={(e) => {
    if (e.key !== "Escape") return;
    // Esc 关闭操作浮层(合并菜单/删除确认/样本删除确认);改名输入框自带 Esc 语义
    if (mergeOpen || pendingMergeWinner || confirmDelete || confirmSampleIdx !== null) closeAllOps();
  }}
/>

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
    /* 改名重名确认条以本行为锚点向下弹出 */
    position: relative;
  }
  .dup-menu {
    top: calc(100% + 4px);
    min-width: 20rem;
  }
  .menu-note {
    color: var(--ink-faint);
    font-size: 0.76rem;
    line-height: 1.45;
    padding: 0 0.5rem 0.35rem;
  }
  .delete-hint {
    color: var(--warning-ink);
    font-size: 0.78rem;
    max-width: 24rem;
    line-height: 1.45;
  }
  /* 上下文整理提示:warning 横幅家族(待办语义),行内直达动作 */
  .ctx-card {
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    border-radius: var(--radius-lg);
    padding: 0.55rem 0.8rem;
    margin-bottom: 0.9rem;
  }
  .ctx-row {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.15rem 0;
    flex-wrap: wrap;
  }
  .ctx-text {
    color: var(--warning-ink);
    font-size: 0.85rem;
    line-height: 1.5;
  }
  .ctx-text a {
    color: var(--accent);
  }
  h1 {
    margin: 0;
    display: inline;
  }
  .unnamed {
    font-style: italic;
    color: var(--ink-faint);
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
  .naming-hint {
    color: var(--ink-secondary);
    font-size: 0.82rem;
    margin: 0.4rem 0 0;
  }
  /* ── 区块(panel):surface 底 + 大圆角,标题行与内容纵排 ── */
  .panel {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.85rem 1.1rem 0.9rem;
    margin-bottom: 0.8rem;
  }
  .panel-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
    min-height: 1.9rem;
    margin-bottom: 0.35rem;
  }
  .panel-title {
    margin: 0;
    display: inline-flex;
    align-items: center;
    gap: 0.45rem;
    font-size: 0.86rem;
    font-weight: 600;
    color: var(--ink-secondary);
    letter-spacing: 0.01em;
  }
  .count {
    font-size: 0.72rem;
    font-weight: 500;
    color: var(--ink-faint);
    background: var(--surface-press);
    border-radius: var(--radius-full);
    padding: 0.05em 0.55em;
    font-variant-numeric: tabular-nums;
  }
  .panel-actions {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    min-width: 0;
  }
  .panel-foot,
  .panel-empty {
    margin: 0.55rem 0 0;
    color: var(--ink-faint);
    font-size: 0.8rem;
    line-height: 1.5;
  }
  .panel-empty {
    margin-top: 0.2rem;
    color: var(--ink-secondary);
  }
  /* 状态胶囊:后台在跑时 accent 呼吸点;完成后中性文案,几秒后随下次操作被替换 */
  .status {
    display: inline-flex;
    align-items: center;
    gap: 0.4em;
    font-size: 0.78rem;
    color: var(--ink-secondary);
    background: var(--surface-press);
    border-radius: var(--radius-full);
    padding: 0.18em 0.7em;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 22rem;
  }
  .status.running {
    color: var(--accent);
    background: var(--accent-tint);
  }
  .status .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: currentColor;
    animation: breathe 1.2s ease-in-out infinite;
  }
  @keyframes breathe {
    0%, 100% { opacity: 0.35; transform: scale(0.8); }
    50% { opacity: 1; transform: scale(1); }
  }
  /* 头部统计胶囊 */
  .stat {
    display: inline-flex;
    align-items: baseline;
    gap: 0.2em;
    color: var(--ink-secondary);
  }
  .stat b {
    font-weight: 600;
    color: var(--ink);
    font-variant-numeric: tabular-nums;
  }
  .stat-sep {
    width: 1px;
    height: 0.9em;
    background: var(--hairline-strong);
    margin: 0 0.2rem;
  }
  /* ── 样本列表:圆形播放钮 · 两行信息 · 悬停操作 ── */
  .sample-list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .sample-row {
    position: relative;
    display: grid;
    grid-template-columns: auto 1fr auto;
    align-items: center;
    gap: 0.8rem;
    padding: 0.5rem 0.45rem;
    margin: 0 -0.45rem;
    border-radius: var(--radius-md);
    border-bottom: 1px solid var(--hairline);
    transition: background 0.12s ease;
  }
  .sample-row:last-child {
    border-bottom: none;
  }
  .sample-row:hover,
  .sample-row.active {
    background: var(--surface-soft);
  }
  .sample-row.playing {
    background: var(--accent-tint);
  }
  .sample-main {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 0.12rem;
  }
  .sample-title {
    display: flex;
    align-items: baseline;
    gap: 0.55rem;
    font-size: 0.9rem;
  }
  .sample-name {
    font-weight: 500;
    color: var(--ink);
  }
  .sample-date {
    color: var(--ink-faint);
    font-size: 0.76rem;
    font-variant-numeric: tabular-nums;
  }
  .sample-sub {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    min-width: 0;
    font-size: 0.79rem;
    color: var(--ink-secondary);
  }
  .sample-sub a {
    color: var(--ink-secondary);
    text-decoration: none;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .sample-sub a:hover {
    color: var(--accent);
    text-decoration: underline;
  }
  .tag {
    flex: none;
    font-size: 0.7rem;
    color: var(--ink-faint);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-full);
    padding: 0 0.45em;
  }
  .muted {
    color: var(--ink-faint);
  }
  .sample-ops {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    opacity: 0;
    transition: opacity 0.12s ease;
  }
  .sample-row:hover .sample-ops,
  .sample-row.active .sample-ops,
  .sample-ops:focus-within {
    opacity: 1;
  }
  .confirm-text {
    color: var(--warning-ink);
    font-size: 0.78rem;
    margin-right: 0.2rem;
  }
  /* ── 会议列表:整行可点,标题 · 日期 · 箭头 ── */
  .appear-list {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 18rem;
    overflow-y: auto;
  }
  .appear-row {
    display: grid;
    grid-template-columns: 1fr auto auto;
    align-items: center;
    gap: 0.7rem;
    padding: 0.42rem 0.45rem;
    margin: 0 -0.45rem;
    border-radius: var(--radius-md);
    color: var(--ink);
    text-decoration: none;
    transition: background 0.12s ease;
  }
  .appear-row:hover {
    background: var(--surface-soft);
  }
  .appear-row:hover .appear-title {
    color: var(--accent);
  }
  .appear-title {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 0.9rem;
  }
  .appear-meta {
    color: var(--ink-faint);
    font-size: 0.76rem;
    font-variant-numeric: tabular-nums;
  }
  .chev {
    color: var(--ink-faint);
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
  /* 改名重名确认条(.dup-menu)仍是行内浮层:surface-press 底(暗色下 canvas 会成"洞");
     合并/归到别人两个选人菜单已换 Popover */
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
  .confirm-title {
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
  .undo-strip {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.5rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.85rem;
    color: var(--ink-secondary);
  }
  .hint {
    color: var(--ink-faint);
    font-weight: 400;
  }
</style>
