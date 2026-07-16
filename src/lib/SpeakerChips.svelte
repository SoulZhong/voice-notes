<script lang="ts">
  import { speakerColor, speakerInk, speakerLabel, renameSpeaker, speakerIdCompare, formatDate } from "$lib/notes";
  import type { PersonSummary } from "$lib/people";

  let {
    speakers,
    noteId,
    editable,
    counts,
    onRenamed,
    onRename,
    people,
    onPick,
    onPreview,
    previewingId,
  }: {
    speakers: Record<string, { name: string; sources: string[]; person_id?: string | null }>;
    noteId: string;
    editable: boolean;
    /** 各说话人的段数(可选)。传入则按段数降序排,并折叠只出现 1 段的碎片说话人;
        不传(如录制页实时条)保持原 id 序、不折叠。 */
    counts?: Record<string, number>;
    onRenamed?: () => void;
    /** 改名落点(可选)。缺省走笔记内 renameSpeaker;修订稿视图传 renameRefinedSpeaker
        (改名同步声纹库)。 */
    onRename?: (id: string, name: string) => Promise<void>;
    /** 会议搭子人物列表(可选)。传入(连同 onPick)则编辑面板附带人物区,
        点选即把该说话人关联到库中人物。 */
    people?: PersonSummary[];
    onPick?: (id: string, personId: string) => Promise<void>;
    /** 试听(可选)。传入则编辑面板附「试听他的声音」行——不听声音没法确认
        「说话人 N」是谁。点击播该说话人的代表片段,重复点击换一段;
        面板保持展开,听完可直接改名/选人。 */
    onPreview?: (id: string) => void;
    /** 正在试听的说话人 id(供行内提示「播放中,点击换一段」)。 */
    previewingId?: string | null;
  } = $props();

  let editingId = $state<string | null>(null);
  let editingName = $state("");
  /** 用户是否已敲过字:预填的现名不参与人物过滤(否则一打开列表就只剩自己)。 */
  let editingDirty = $state(false);
  let panelEl = $state<HTMLElement | null>(null);
  /** 改名撞库中现有人名时的待确认态:面板转为确认条,防悄悄造出重名。
      linkedOther=该说话人已关联别人 → 撞名大概率是库里有重复条目,给详情页合并入口。 */
  let dupPending = $state<{ id: string; name: string; person: PersonSummary; linkedOther: boolean } | null>(null);

  /** 人物面板显示名:库中未命名的人按全局编号「说话人 N」兜底(与徽章一致)。 */
  const personLabel = (p: PersonSummary) => p.name || `说话人 ${p.id.replace(/^P/, "")}`;

  /** "最近 MM-DD":未命名/重名条目的区分后缀。 */
  const recentLabel = (p: PersonSummary) => {
    const d = formatDate(p.last_seen);
    return d === "—" ? "" : `最近 ${d.slice(5, 10)}`;
  };

  /** 出现超过一次的名字集合:同名条目必须带区分后缀,否则列表里两行一模一样。 */
  const dupNames = $derived.by(() => {
    const seen = new Set<string>();
    const dup = new Set<string>();
    for (const p of people ?? []) {
      if (!p.name) continue;
      if (seen.has(p.name)) dup.add(p.name);
      seen.add(p.name);
    }
    return dup;
  });

  /** 人物面板候选:没敲过字给全量,敲过按包含匹配过滤。 */
  const pickCandidates = $derived.by(() => {
    if (!people) return [];
    const q = editingDirty ? editingName.trim() : "";
    return q ? people.filter((p) => personLabel(p).includes(q)) : people;
  });

  const ids = $derived.by(() => {
    const all = Object.keys(speakers).sort(speakerIdCompare);
    // 稳定排序:段数降序为主键,id 序(上面已排好)为次键
    return counts ? all.sort((a, b) => (counts[b] ?? 0) - (counts[a] ?? 0)) : all;
  });

  /** 碎片:只出现 1 段且未命名/未关联人物的说话人(命过名或已关联的不折叠)。 */
  const fragmentIds = $derived(
    counts
      ? ids.filter((id) => (counts[id] ?? 0) <= 1 && !speakers[id]?.name && !speakers[id]?.person_id)
      : [],
  );
  let showFragments = $state(false);
  // 换笔记复位折叠态与编辑态,别把上一篇的带过来
  $effect(() => {
    void noteId;
    showFragments = false;
    editingId = null;
  });
  /** 少于 3 个碎片不值得折叠:展开钮本身比一两枚 chip 更占地。 */
  const collapsible = $derived(fragmentIds.length >= 3);
  const visibleIds = $derived(
    collapsible && !showFragments ? ids.filter((id) => !fragmentIds.includes(id)) : ids,
  );

  // 面板贴视口右缘时整体左收(DESIGN popover 规则:按实测尺寸收回,留 8px 边距)。
  $effect(() => {
    const el = panelEl;
    if (!el) return;
    const r = el.getBoundingClientRect();
    const over = r.right - (window.innerWidth - 8);
    if (over > 0) el.style.left = `-${Math.min(over, Math.max(0, r.left - 8))}px`;
  });

  // 非 null 分支与徽章共用同一兜底逻辑;source 参数在此分支无关,固定传 "mic"。
  const label = (id: string) => speakerLabel(id, "mic", speakers);

  function beginEdit(id: string) {
    editingId = id;
    editingName = speakers[id]?.name ?? "";
    editingDirty = false;
    dupPending = null;
  }

  function cancelEdit() {
    editingId = null;
    dupPending = null;
  }

  /** 实际改名落点:外部没接管(onRename)就走笔记内 renameSpeaker。 */
  const doRename = (id: string, name: string) =>
    onRename ? onRename(id, name) : renameSpeaker(noteId, id, name);

  async function commitEdit() {
    if (!editingId || dupPending) return;
    const id = editingId;
    const name = editingName.trim();
    if (!name || name === (speakers[id]?.name ?? "")) {
      editingId = null;
      return;
    }
    // 重名拦截:新名与库中某人现名一致(且不是该说话人已关联的那位)——十有八九
    // 是同一个人,先确认是"关联他"还是"真的要重名"。面板保持展开转为确认条。
    if (people && onPick) {
      const hit = people.find((p) => p.name && p.name === name && p.id !== speakers[id]?.person_id);
      if (hit) {
        dupPending = { id, name, person: hit, linkedOther: !!speakers[id]?.person_id };
        return;
      }
    }
    editingId = null;
    await doRename(id, name);
    onRenamed?.();
  }

  /** 重名确认:就是库里那位 → 关联(等价于选人)。 */
  async function dupAssign() {
    const d = dupPending;
    if (!d) return;
    cancelEdit();
    await onPick?.(d.id, d.person.id);
    onRenamed?.();
  }

  /** 重名确认:确实是另一个人 → 照常改名,允许重名(列表以「最近 MM-DD」区分)。 */
  async function dupRename() {
    const d = dupPending;
    if (!d) return;
    cancelEdit();
    await doRename(d.id, d.name);
    onRenamed?.();
  }

  async function commitPick(id: string, personId: string) {
    cancelEdit();
    await onPick?.(id, personId);
    onRenamed?.();
  }

  async function markAsMe(id: string) {
    // 「这是我」也走重名拦截:库里已有「我」而这个说话人不是他 → 大概率同一人被拆重。
    const hit = people?.find((p) => p.name === "我" && p.id !== speakers[id]?.person_id);
    if (hit && onPick) {
      dupPending = { id, name: "我", person: hit, linkedOther: !!speakers[id]?.person_id };
      return;
    }
    cancelEdit();
    await doRename(id, "我");
    onRenamed?.();
  }
</script>

{#if ids.length > 0}
  <div class="chips">
    {#each visibleIds as id (id)}
      <!-- speaker-chip：同徽章色系(粉彩底+ink字),chip 本身就是色块。可编辑时点击
           在下方展开编辑面板(chip 保持原形,不原地变形成输入框)。 -->
      <div
        class="chip"
        class:editable
        class:open={editingId === id}
        style="background: {speakerColor(id, 'mic', speakers)}; color: {speakerInk(id, 'mic', speakers)}"
      >
        {#if editable}
          <button
            class="name"
            title="改名或选择人物"
            onmousedown={(e) => {
              // 面板开着时按下不抢焦点:输入框 blur(=提交并关闭)先行,click 再开会闪一下
              if (editingId === id) e.preventDefault();
            }}
            onclick={() => (editingId === id ? commitEdit() : beginEdit(id))}
          >
            {label(id)}
          </button>
        {:else}
          <span class="name">{label(id)}</span>
        {/if}

        {#if editable && editingId === id}
          <!-- 编辑面板(menu/popover 语言):改名输入 + 「这是我」快捷行 + 会议搭子选人。
               面板内按下 preventDefault(输入框除外):点选不能先触发输入框 blur
               把敲了一半的名字提交掉。 -->
          <div
            class="panel"
            bind:this={panelEl}
            role="menu"
            tabindex="-1"
            onmousedown={(e) => {
              if (!(e.target instanceof HTMLInputElement)) e.preventDefault();
            }}
          >
            <!-- svelte-ignore a11y_autofocus -->
            <input
              class="panel-input"
              autofocus
              placeholder="输入名字,回车改名"
              bind:value={editingName}
              onfocus={(e) => e.currentTarget.select()}
              oninput={() => {
                editingDirty = true;
                dupPending = null;
              }}
              onkeydown={(e) => {
                if (e.key === "Enter") commitEdit();
                if (e.key === "Escape") cancelEdit();
              }}
              onblur={commitEdit}
            />
            <div class="sep"></div>
            {#if dupPending}
              <!-- 重名确认条:面板暂时收起动作/人物区,只留三选,不让重名悄悄发生 -->
              <div class="dup">
                {#if !dupPending.linkedOther}
                  <div class="dup-msg">
                    会议搭子里已有「{dupPending.person.name}」{recentLabel(dupPending.person) ? `(${recentLabel(dupPending.person)})` : ""},是同一个人吗?
                  </div>
                  <button class="row strong" onclick={dupAssign}>是,关联他</button>
                  <button class="row" onclick={dupRename}>不是,保留同名</button>
                {:else}
                  <div class="dup-msg">
                    另一位搭子也叫「{dupPending.person.name}」,可能是重复条目——可到他的详情页做合并。
                  </div>
                  <a class="row" href="/speakers/{dupPending.person.id}" onclick={cancelEdit}>查看那位「{dupPending.person.name}」</a>
                  <button class="row" onclick={dupRename}>仍要改名</button>
                {/if}
                <button class="row quiet" onclick={cancelEdit}>取消</button>
              </div>
            {:else}
              {#if !editingDirty}
                {#if onPreview}
                  <button class="row" onclick={() => onPreview(id)}>
                    <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                      <path d="M5 3.5v9l7.5-4.5z" />
                    </svg>
                    试听他的声音
                    {#if previewingId === id}<span class="row-sub">播放中,点击换一段</span>{/if}
                  </button>
                {/if}
                <button class="row" onclick={() => markAsMe(id)}>
                  <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" aria-hidden="true">
                    <circle cx="8" cy="5.2" r="2.6" />
                    <path d="M2.8 13.4c.9-2.4 2.9-3.6 5.2-3.6s4.3 1.2 5.2 3.6" />
                  </svg>
                  这是我
                </button>
              {/if}
              {#if people && onPick}
                <div class="caption">会议搭子</div>
                {#if pickCandidates.length > 0}
                  <div class="list">
                    {#each pickCandidates as p (p.id)}
                      <button class="row" onclick={() => commitPick(id, p.id)}>
                        <!-- 色点用 ink 变体:soft 底(15% alpha)做 9px 点几乎不可见 -->
                        <span class="dot" style="background: {speakerInk(p.id, 'mic')}"></span>
                        <span class="row-label">{personLabel(p)}</span>
                        {#if !p.name || dupNames.has(p.name)}
                          <!-- 未命名/重名条目:补最近出现日期,两行不至于一模一样 -->
                          <span class="row-sub">{recentLabel(p)}</span>
                        {/if}
                        {#if p.id === speakers[id]?.person_id}
                          <svg class="tick" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                            <path d="M3 8.5l3.2 3.2L13 5" />
                          </svg>
                        {/if}
                      </button>
                    {/each}
                  </div>
                {:else}
                  <div class="empty">{people.length === 0 ? "还没有认识的人" : "没有匹配的人"}</div>
                {/if}
              {/if}
            {/if}
          </div>
        {/if}
      </div>
    {/each}
    {#if collapsible}
      <!-- 碎片折叠钮:声纹没归成簇的一次性说话人收进来,别摊满一整条 -->
      <button class="chip more" onclick={() => (showFragments = !showFragments)}>
        {showFragments ? "收起" : `+${fragmentIds.length} 位偶现说话人`}
      </button>
    {/if}
  </div>
{/if}

<style>
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    margin: 0 0 0.75rem;
  }
  /* speaker-chip：粉彩底(内联 style 按说话人取色) + ink 字 + rounded-full。
     relative:编辑面板以 chip 为锚点向下弹出。 */
  .chip {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    position: relative;
    /* 底色与文字色均由内联 style 按说话人配对,此处不设默认(设了也恒被覆盖) */
    border-radius: var(--radius-full);
    padding: 0.2em 0.6em;
    font-size: 0.85em;
  }
  /* 可点击时 hover / 面板展开时 加 accent-tint 外环 */
  .chip.editable:hover,
  .chip.open {
    box-shadow: 0 0 0 2px var(--accent-tint);
  }
  /* 碎片折叠钮:button-secondary 语言(透明底+hairline 边),不与粉彩说话人色争 */
  .chip.more {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink-secondary);
    font: inherit;
    font-size: 0.85em;
    cursor: pointer;
  }
  .chip.more:hover {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .name {
    background: none;
    border: none;
    padding: 0;
    font: inherit;
    color: inherit;
    cursor: default;
  }
  button.name {
    cursor: pointer;
  }

  /* ── 编辑面板:menu/popover 形态(surface-press 底、hairline 边、radius-lg、
     shadow-popover),chip 下缘 6px 处展开,120ms 缓动浮现。字色/字号显式复位
     (chip 内联的粉彩 ink 与 0.85em 不能渗进面板)。 ── */
  .panel {
    position: absolute;
    top: calc(100% + 6px);
    left: 0;
    z-index: 30;
    min-width: 13rem;
    max-width: 17rem;
    padding: 0.3rem;
    background: var(--surface-press);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    color: var(--ink);
    font-size: 0.82rem;
    font-weight: 400;
    cursor: default;
    animation: panel-in 120ms ease-out;
  }
  @keyframes panel-in {
    from {
      opacity: 0;
      transform: translateY(-3px);
    }
    to {
      opacity: 1;
      transform: none;
    }
  }
  /* 改名输入:面板首行,无框(面板本身就是聚焦语境),下缘发丝线分隔 */
  .panel-input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.45rem 0.55rem;
    background: transparent;
    border: none;
    outline: none;
    font: inherit;
    color: var(--ink);
  }
  .panel-input::placeholder {
    color: var(--ink-faint);
  }
  /* 全出血分隔线:负外边距抵掉面板内距 */
  .sep {
    height: 1px;
    background: var(--hairline);
    margin: 0 -0.3rem 0.2rem;
  }
  .caption {
    padding: 0.35rem 0.55rem 0.1rem;
    font-size: 0.68rem;
    color: var(--ink-faint);
    letter-spacing: 0.02em;
  }
  .list {
    max-height: 13rem;
    overflow-y: auto;
  }
  /* 菜单行(「这是我」与人物行同形态):全宽、radius-md、hover surface-soft */
  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    padding: 0.38rem 0.55rem;
    background: none;
    border: none;
    border-radius: var(--radius-md);
    color: var(--ink);
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .row:hover {
    background: var(--surface-soft);
  }
  .row-icon {
    color: var(--ink-secondary);
    flex: none;
  }
  /* 重名确认条:警示语 + 三选行。主推动作 accent 字重 500,取消退 faint */
  .dup {
    display: flex;
    flex-direction: column;
  }
  .dup-msg {
    padding: 0.4rem 0.55rem 0.25rem;
    color: var(--warning-ink);
    font-size: 0.78rem;
    line-height: 1.5;
    max-width: 15rem;
  }
  .row.strong {
    color: var(--accent);
    font-weight: 500;
  }
  .row.quiet {
    color: var(--ink-faint);
  }
  a.row {
    text-decoration: none;
    box-sizing: border-box;
  }
  /* 行内次要信息(最近出现日期):faint 小字,不与名字争 */
  .row-sub {
    color: var(--ink-faint);
    font-size: 0.72rem;
    flex: none;
  }
  /* 人物色点:与徽章同一调色板按 P 号取色(跨会议恒定) */
  .dot {
    width: 9px;
    height: 9px;
    border-radius: var(--radius-full);
    flex: none;
  }
  .row-label {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .tick {
    color: var(--accent);
    flex: none;
  }
  .empty {
    padding: 0.38rem 0.55rem 0.45rem;
    color: var(--ink-faint);
  }
</style>
