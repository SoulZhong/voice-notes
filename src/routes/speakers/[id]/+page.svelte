<script lang="ts">
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { convertFileSrc } from "@tauri-apps/api/core";
  import {
    listPeople,
    personNotes,
    renamePerson,
    mergePerson,
    deletePerson,
    deletePersonSample,
    type PersonSummary,
    type PersonMergeSuggestion,
  } from "$lib/people";
  import { tidy, sugKey } from "$lib/tidy.svelte";
  import { formatDate, formatDuration, speakerColor, speakerInk, type NoteSummary } from "$lib/notes";
  import { recording } from "$lib/recording.svelte";

  // 主从结构的"从":本页只呈现/操作一个人;人物索引在侧栏声纹库页签。
  const personId = $derived($page.params.id as string);

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

  // 出现过的会议(试听之外确认"认对了人"的第二手段);增值层,取失败静默按空处理。
  let appearNotes = $state<NoteSummary[]>([]);

  async function refresh() {
    const forId = personId;
    const notesPromise = personNotes(forId).catch(() => [] as NoteSummary[]);
    try {
      people = await listPeople();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
    const notes = await notesPromise;
    if (forId === personId) appearNotes = notes;
    loaded = true;
  }

  // 路由参数变化(侧栏点选另一人)时重载;同页操作后手动 refresh。
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
  /** 改名撞库中另一人现名:大概率是同一个人被声纹拆重,先确认合并还是真重名。 */
  let dupRename = $state<{ name: string; other: PersonSummary } | null>(null);

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
    const other = others.find((o) => o.name === text);
    if (other) {
      dupRename = { name: text, other };
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
      error = `改名失败: ${err}`;
      await refresh();
    }
  }

  /** 重名确认:就是同一个人 → 当前人并入已有同名者,跳转对方详情。 */
  async function dupMerge() {
    const p = person;
    const d = dupRename;
    if (!p || !d) return;
    editingId = null;
    dupRename = null;
    try {
      await mergePerson(p.id, d.other.id);
      recording.bumpPeople();
      goto(`/speakers/${d.other.id}`);
    } catch (e) {
      error = `${e}`;
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

  /** 合并结果预览:名字继承规则(winner 名优先,winner 无名继承 loser 名)对用户是
      黑盒,确认前把结果摆出来;loser 数据比 winner 厚时提示通常反向合并更好。 */
  const mergePreview = $derived.by(() => {
    const t = people.find((o) => o.id === pendingMergeWinner);
    if (!t || !person) return null;
    return {
      name: t.name || person.name,
      total: t.total_ms + person.total_ms,
      thickerLoser: person.total_ms > t.total_ms,
    };
  });

  function closeAllOps() {
    mergeOpen = false;
    pendingMergeWinner = null;
    confirmDelete = false;
    confirmSampleIdx = null;
  }

  /** 样本删除的行内二段确认:记下标(与 sample_paths 对齐)。 */
  let confirmSampleIdx = $state<number | null>(null);

  async function doDeleteSample(i: number) {
    const p = person;
    if (!p) return;
    const path = p.sample_paths[i];
    confirmSampleIdx = null;
    stopSample(); // 删除后下标会移位,正在播的一律先停,别让播放态指错样本
    try {
      await deletePersonSample(p.id, path);
      await refresh();
    } catch (e) {
      error = `删除样本失败: ${e}`;
      await refresh();
    }
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

  // ── 上下文整理提示(建议跟人走):当前这个人涉及归属建议/同名重复时,详情页
  //    头部直接给出,不必回概览找「整理」。数据走共享 tidy store(侧栏徽标同源)。 ──
  const related = $derived(tidy.involving(personId));
  /** 同名的另一人(重名十有八九=同一人被拆重,给直达入口)。 */
  const sameName = $derived(person?.name ? (others.find((o) => o.name === person!.name) ?? null) : null);

  /** 建议行里"对方"的一侧(当前人可能是 loser 也可能是 winner)。 */
  const ctxOther = (s: PersonMergeSuggestion) =>
    s.loser === personId
      ? { id: s.winner, name: s.winner_name }
      : { id: s.loser, name: s.loser_name };

  async function applyCtxSuggestion(s: PersonMergeSuggestion) {
    stopCtx();
    stopSample();
    try {
      await mergePerson(s.loser, s.winner);
      recording.bumpPeople();
      await tidy.refresh();
      if (s.loser === personId) {
        goto(`/speakers/${s.winner}`); // 本人被并走:跳到归属后的人
      } else {
        await refresh();
      }
    } catch (e) {
      error = `${e}`;
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
    void a.play().catch(() => stopCtx());
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
              oninput={() => (dupRename = null)}
              onkeydown={(e) => {
                if (e.key === "Enter") commitRename();
                if (e.key === "Escape") {
                  editingId = null;
                  dupRename = null;
                }
              }}
              onblur={commitRename}
            />
            {#if dupRename}
              <!-- 重名确认(menu 语言):撞名十有八九=同一个人被拆重,先问合并 -->
              <div class="menu dup-menu">
                <div class="menu-title">
                  已有一位「{dupRename.other.name}」(最近出现 {formatDate(dupRename.other.last_seen)} ·
                  累计 {formatDuration(Math.floor(dupRename.other.total_ms / 1000))})。是同一个人吗?
                </div>
                <div class="confirm-row">
                  <button class="mini accent" onmousedown={(e) => e.preventDefault()} onclick={dupMerge}>是,合并成一个</button>
                  <button class="mini" onmousedown={(e) => e.preventDefault()} onclick={dupKeep}>不是,保留同名</button>
                  <button
                    class="mini quiet"
                    onmousedown={(e) => e.preventDefault()}
                    onclick={() => {
                      editingId = null;
                      dupRename = null;
                    }}>取消</button
                  >
                </div>
              </div>
            {/if}
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

    {#if related.length > 0 || sameName}
      <!-- 上下文整理提示:这个人自己的归属建议/同名重复,就地处理不必回概览 -->
      <div class="ctx-card">
        {#each related as s (sugKey(s))}
          {@const other = ctxOther(s)}
          <div class="ctx-row">
            <span class="ctx-text">
              这个人可能与
              <a href="/speakers/{other.id}">「{other.name || `说话人 ${other.id.replace(/^P/, "")}`}」</a>
              是同一人(相似度 {Math.round(s.similarity * 100)}%{s.similarity >= 0.74 ? " · 很可能" : ""})
            </span>
            {#if (people.find((o) => o.id === other.id)?.sample_paths.length ?? 0) > 0}
              <!-- 对方原声试听:不听没法拍板该不该合 -->
              <button
                class="listen-mini"
                class:playing={ctxPlayingId === other.id}
                title={ctxPlayingId === other.id ? "停止" : "试听对方原声"}
                aria-label={ctxPlayingId === other.id ? "停止" : "试听对方原声"}
                onclick={() => toggleCtxSample(other.id)}
              >
                {#if ctxPlayingId === other.id}
                  <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true"><rect x="3.5" y="3.5" width="9" height="9" rx="1.5" fill="currentColor" /></svg>
                {:else}
                  <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true"><path d="M5 2.9v10.2c0 .7.8 1.2 1.4.8l7.4-5.1c.6-.4.6-1.2 0-1.6L6.4 2.1c-.6-.4-1.4.1-1.4.8z" fill="currentColor" /></svg>
                {/if}
              </button>
            {/if}
            <button
              class="mini accent"
              disabled={recording.isLive}
              title={recording.isLive ? "录制中不能合并" : "合并成一个人(可先试听核对)"}
              onclick={() => applyCtxSuggestion(s)}>合并</button
            >
            <button class="mini" onclick={() => tidy.ignore(s)}>忽略</button>
          </div>
        {/each}
        {#if sameName}
          <div class="ctx-row">
            <span class="ctx-text">
              另有一位也叫「{person.name}」(最近出现 {formatDate(sameName.last_seen)}),可能是重复条目。
            </span>
            {#if sameName.sample_paths.length > 0}
              <button
                class="listen-mini"
                class:playing={ctxPlayingId === sameName.id}
                title={ctxPlayingId === sameName.id ? "停止" : "试听对方原声"}
                aria-label={ctxPlayingId === sameName.id ? "停止" : "试听对方原声"}
                onclick={() => toggleCtxSample(sameName.id)}
              >
                {#if ctxPlayingId === sameName.id}
                  <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true"><rect x="3.5" y="3.5" width="9" height="9" rx="1.5" fill="currentColor" /></svg>
                {:else}
                  <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true"><path d="M5 2.9v10.2c0 .7.8 1.2 1.4.8l7.4-5.1c.6-.4.6-1.2 0-1.6L6.4 2.1c-.6-.4-1.4.1-1.4.8z" fill="currentColor" /></svg>
                {/if}
              </button>
            {/if}
            <a class="mini ctx-link" href="/speakers/{sameName.id}">查看对方</a>
          </div>
        {/if}
      </div>
    {/if}

    <!-- 试听:确认"这个声纹是谁"的主要手段,给成块的卡而非藏在角标里 -->
    <section class="card">
      <div class="card-title">原声试听</div>
      {#if person.sample_paths.length > 0}
        <div class="listen-row">
          {#each person.sample_paths as sp, i (sp)}
            {#if confirmSampleIdx === i}
              <!-- 行内二段确认(页面级破坏性动作既有模式);样本不参与识别,删了不影响认人 -->
              <span class="confirm-inline">
                <button class="mini danger" onclick={() => doDeleteSample(i)}>删除这份样本</button>
                <button class="mini" onclick={() => (confirmSampleIdx = null)}>取消</button>
              </span>
            {:else}
              <span class="sample-wrap">
                <button class="listen" class:playing={playingIdx === i} onclick={() => toggleSample(i)}>
                  {#if playingIdx === i}
                    <span class="bars" aria-hidden="true"><span></span><span></span><span></span></span>
                    停止
                  {:else}
                    <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
                      <path d="M5 2.9v10.2c0 .7.8 1.2 1.4.8l7.4-5.1c.6-.4.6-1.2 0-1.6L6.4 2.1c-.6-.4-1.4.1-1.4.8z" fill="currentColor" />
                    </svg>
                    {person.sample_paths.length === 1 ? "播放样本" : `样本 ${i + 1}`}
                    {#if formatDate(person.sample_dates[i] ?? "") !== "—"}
                      <!-- 样本录制日期(文件时间):标出"哪场的声音",多样本核对时才分得清 -->
                      <span class="listen-date">{formatDate(person.sample_dates[i]).slice(0, 10)}</span>
                    {/if}
                  {/if}
                </button>
                <!-- 悬停显影的删除叉:录坏/混进别人声音的样本可单独删 -->
                <button
                  class="sample-x"
                  title="删除这份样本"
                  aria-label="删除这份样本"
                  onclick={() => {
                    closeAllOps();
                    confirmSampleIdx = i;
                  }}
                >
                  <svg width="11" height="11" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true">
                    <path d="M4 4l8 8M12 4l-8 8" />
                  </svg>
                </button>
              </span>
            {/if}
          {/each}
        </div>
        <span class="card-hint">
          听一段这个人的原声,确认认对了人。{#if person.sample_paths.length > 1}多份样本来自合并带入的不同条目,可逐份核对。{/if}
        </span>
      {:else}
        <span class="card-hint">暂无录音样本:下次录到这个人并停止录制后会自动补上。</span>
      {/if}
    </section>

    <!-- 出现过的会议:试听之外确认"认对了人"的第二手段;点击直达笔记 -->
    <section class="card col">
      <div class="card-title">出现过的会议</div>
      {#if appearNotes.length > 0}
        <ul class="appear-list">
          {#each appearNotes as n (n.id)}
            <li class="appear-row">
              <a href="/notes/{n.id}">{n.title}</a>
              <span class="appear-meta">{formatDate(n.started_at)}</span>
            </li>
          {/each}
        </ul>
      {:else}
        <span class="card-hint">还没有会议记录到这个人(早期笔记可能没有关联信息)。</span>
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
                并入「{target ? displayName(target) : "?"}」?合并后是一个人:
                {#if mergePreview}名字为「{mergePreview.name || "未命名"}」,累计发声
                  {formatDuration(Math.floor(mergePreview.total / 1000))};{/if}
                历史笔记都显示合并后的名字,不可撤销。
              </div>
              {#if mergePreview?.thickerLoser}
                <div class="menu-note">
                  提示:当前这位的录音数据更多。通常把数据少的一方并入数据多的一方——
                  如需反向,可取消后到对方页面操作。
                </div>
              {/if}
              <div class="confirm-row">
                <button class="mini danger" onclick={doMerge}>确认合并</button>
                <button class="mini" onclick={closeAllOps}>取消</button>
              </div>
            </div>
          {/if}
        </div>

        {#if confirmDelete}
          <div class="confirm-inline">
            <!-- 删除后果一句话说清:笔记文字不动,只是不再认得这个人 -->
            <span class="delete-hint">删除后历史笔记里他恢复显示为编号,录音样本一并删除,之后录到他会当作新面孔。</span>
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
  .mini.accent {
    border-color: var(--accent);
    color: var(--accent);
    font-weight: 500;
  }
  .mini.accent:hover {
    background: var(--accent-tint);
  }
  .mini.quiet {
    color: var(--ink-faint);
  }
  .listen-date {
    color: var(--ink-faint);
    font-size: 0.76rem;
    font-weight: 400;
  }
  /* 样本删除叉:悬停样本才显影(行级操作隐身惯例),hover 转 danger */
  .sample-wrap {
    display: inline-flex;
    align-items: center;
    gap: 2px;
  }
  .sample-x {
    visibility: hidden;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 1.35rem;
    height: 1.35rem;
    padding: 0;
    border: none;
    background: none;
    color: var(--ink-faint);
    border-radius: var(--radius-full);
    cursor: pointer;
  }
  .sample-wrap:hover .sample-x,
  .sample-x:focus-visible {
    visibility: visible;
  }
  .sample-x:hover {
    color: var(--danger);
    background: var(--danger-tint);
  }
  /* 列表型卡片(出现过的会议):纵排,标题行与列表上下排布 */
  .card.col {
    display: block;
  }
  .card.col .card-title {
    margin-bottom: 0.45rem;
  }
  .appear-list {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 14rem;
    overflow-y: auto;
  }
  .appear-row {
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    padding: 0.28rem 0;
  }
  .appear-row a {
    color: var(--ink);
    text-decoration: none;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .appear-row a:hover {
    color: var(--accent);
    text-decoration: underline;
  }
  .appear-meta {
    color: var(--ink-faint);
    font-size: 0.78rem;
    flex: none;
  }
  .delete-hint {
    color: var(--warning-ink);
    font-size: 0.78rem;
    max-width: 24rem;
    line-height: 1.45;
  }
  /* 上下文整理提示:warning 横幅家族(与概览疑似重复卡同语义=待办),行内直达动作 */
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
  .ctx-link {
    text-decoration: none;
    display: inline-flex;
    align-items: center;
  }
  /* 行内试听小钮(与概览整理卡同形态):圆形图标钮,播放中 accent */
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
