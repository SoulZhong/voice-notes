<script lang="ts">
  import { speakerColor, speakerInk, speakerLabel, renameSpeaker, speakerIdCompare } from "$lib/notes";
  import type { PersonSummary } from "$lib/people";
  import { recentLabel } from "$lib/personPick";
  import PersonPickList from "$lib/PersonPickList.svelte";
  import { localeVariants, t } from "$lib/i18n/index.svelte";
  import { describeActionError } from "$lib/speakerAction";

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
    previewClips,
    onPreviewClip,
    previewingSeq,
    onDetachClip,
    onDelete,
    onUnlink,
    onMarkMulti,
    blockedReason = null,
  }: {
    speakers: Record<
      string,
      {
        name: string;
        sources: string[];
        person_id?: string | null;
        multi_speaker?: boolean;
        hint_person?: string | null;
      }
    >;
    noteId: string;
    editable: boolean;
    /** 各说话人的段数(可选)。传入则按段数降序排,并折叠只出现 1 段的碎片说话人;
        不传(如录制页实时条)保持原 id 序、不折叠。 */
    counts?: Record<string, number>;
    onRenamed?: () => void;
    /** 改名落点(可选)。缺省走笔记内 renameSpeaker;修订稿视图传 renameRefinedSpeaker
        (改名同步声纹库)。 */
    onRename?: (id: string, name: string, sample?: { auditedSeq?: number; selectedSeqs: number[] }) => Promise<void>;
    /** 会议搭子人物列表(可选)。传入(连同 onPick)则编辑面板附带人物区,
        点选即把该说话人关联到库中人物。 */
    people?: PersonSummary[];
    onPick?: (id: string, personId: string, sample?: { auditedSeq?: number; selectedSeqs: number[] }) => Promise<void>;
    /** 试听(可选)。传入则编辑面板附「试听他的声音」行——不听声音没法确认
        「说话人 N」是谁。点击播该说话人的代表片段,重复点击换一段;
        面板保持展开,听完可直接改名/选人。 */
    onPreview?: (id: string) => void;
    /** 正在试听的说话人 id(供行内提示「播放中,点击换一段」)。 */
    previewingId?: string | null;
    /** 显式片段试听(2026-08-30 用户反馈"再点一次换下一段"不可发现):给出该说话人
        最长的几段(带时间点/时长),面板逐段列出、各自可播;传入时取代 onPreview 单钮。 */
    previewClips?: (id: string) => { seq: number; start_ms: number; end_ms: number }[];
    onPreviewClip?: (id: string, seq: number) => void;
    /** 「这段不是此人」:把该段单独拆成新说话人(不依赖聚类,段内混杂/别人插话时用)。 */
    onDetachClip?: (id: string, seq: number) => Promise<void>;
    /** 正在播放的片段 seq(高亮那一行)。 */
    previewingSeq?: number | null;
    /** 删除(可选,仅原始逐字稿视图传入)。表项移除,名下段落回到未标注;
        只动本笔记,不碰人物库。面板内二段确认。 */
    onDelete?: (id: string) => Promise<void>;
    /** 取消关联(可选)。只断开与库人物的绑定,表项与段落归属都留着,
        显示回落到「新说话人 N」。仅在该说话人确实关联了人物时才显示这一行。 */
    onUnlink?: (id: string) => Promise<void>;
    /** 标记为多人混杂(可选)。回调只负责打开处置面板;真正的打标由面板内确认后执行。
        已标记的说话人不再显示此入口(chip 上会带「多人」徽标)。 */
    onMarkMulti?: (id: string) => void;
    /** 拆分/删除暂不可用的原因(如 Aing 进行中)。有值时两行不藏,置灰并写明原因——
        功能"时有时无"比"现在不能用,因为……"更让人以为坏了(2026-08-31 用户实报)。 */
    blockedReason?: string | null;
  } = $props();

  let editingId = $state<string | null>(null);
  /** 试听面板里勾选「作为样本」的段 seq(2026-08-30 用户:只把听过、最有代表性的段入库)。
      随面板打开复位;为空则后端按旧规则(最后试听段优先 + 最长补足)。 */
  let sampleSel = $state<number[]>([]);
  /** 面板内最后播放过的段(作为 auditedSeq 传给后端)。 */
  let lastPlayed = $state<Record<string, number>>({});
  const sampleOf = (id: string) => ({ auditedSeq: lastPlayed[id], selectedSeqs: sampleSel });
  let editingName = $state("");
  /** 用户是否已敲过字:预填的现名不参与人物过滤(否则一打开列表就只剩自己)。 */
  let editingDirty = $state(false);
  let panelEl = $state<HTMLElement | null>(null);
  /** 改名撞库中现有人名时的待确认态:面板转为确认条,防悄悄造出重名。
      linkedOther=该说话人已关联别人 → 撞名大概率是库里有重复条目,给详情页合并入口。 */
  let dupPending = $state<{ id: string; name: string; person: PersonSummary; linkedOther: boolean } | null>(null);
  /** 上一次操作的失败文案(后端已本地化)。任何一次新操作先清空。 */
  let actionErr = $state<string | null>(null);

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
    actionErr = null;
    // 试听记录/勾选随笔记复位(Codex P2):各篇 seq 都从 0 起,带过去会把上一篇听过的段当本篇的。
    lastPlayed = {};
    sampleSel = [];
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

  /** 片段时间点 mm:ss / h:mm:ss。 */
  function clockOf(ms: number): string {
    const s = Math.floor(ms / 1000);
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = s % 60;
    const mm = String(m).padStart(2, "0");
    const ss = String(sec).padStart(2, "0");
    return h > 0 ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
  }

  // 非 null 分支与徽章共用同一兜底逻辑;source 参数在此分支无关,固定传 "mic"。
  const label = (id: string) => speakerLabel(id, "mic", speakers);

  /** 删除二段确认态:首点变确认行,再点才真删。切说话人/收面板即复位。 */
  let deletePending = $state(false);

  function beginEdit(id: string) {
    editingId = id;
    sampleSel = [];
    editingName = speakers[id]?.name ?? "";
    editingDirty = false;
    dupPending = null;
    deletePending = false;
  }

  function cancelEdit() {
    editingId = null;
    dupPending = null;
    deletePending = false;
  }

  /** 所有落到后端的说话人操作都经这里:失败必须看得见。
   *
   *  2026-08-17 事故:删除说话人被后端守卫拒绝(「该笔记正在 Aing 中」),而调用处
   *  裸 await 无 catch,错误被吞——用户看到的是「面板关了、什么都没发生」,且错误
   *  只走 IPC 回执不进日志,排查时毫无线索。成功才通知外部刷新(与原行为一致:
   *  原先失败会抛出,onRenamed 同样不执行)。 */
  async function run(fn: () => Promise<void>) {
    actionErr = null;
    try {
      await fn();
    } catch (e) {
      actionErr = describeActionError(e, t("speakers.actionFailed"));
      return;
    }
    onRenamed?.();
  }

  /** 取消关联:与选人走同一条 run() 通道,失败必须看得见(那条 2026-08-17 事故的教训)。 */
  async function commitUnlink(id: string) {
    if (!onUnlink) return;
    cancelEdit();
    await run(() => onUnlink(id));
  }

  async function commitDelete(id: string) {
    if (!onDelete) return;
    cancelEdit();
    await run(() => onDelete(id));
  }

  /** 实际改名落点:外部没接管(onRename)就走笔记内 renameSpeaker。 */
  const doRename = (id: string, name: string) =>
    onRename
      ? onRename(id, name, sampleOf(id))
      : renameSpeaker(noteId, id, name, lastPlayed[id], sampleSel.length ? sampleSel : undefined);

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
    await run(() => doRename(id, name));
  }

  /** 重名确认:就是库里那位 → 关联(等价于选人)。 */
  async function dupAssign() {
    const d = dupPending;
    if (!d) return;
    cancelEdit();
    await run(async () => {
      await onPick?.(d.id, d.person.id, sampleOf(d.id));
    });
  }

  /** 重名确认:确实是另一个人 → 照常改名,允许重名(列表以「最近 MM-DD」区分)。 */
  async function dupRename() {
    const d = dupPending;
    if (!d) return;
    cancelEdit();
    await run(() => doRename(d.id, d.name));
  }

  async function commitPick(id: string, personId: string) {
    const sample = sampleOf(id);
    cancelEdit();
    await run(async () => {
      await onPick?.(id, personId, sample);
    });
  }

  async function markAsMe(id: string) {
    // 「这是我」也走重名拦截:库里已有「我」而这个说话人不是他 → 大概率同一人被拆重。
    // 新记录按当前界面语言写(所见即所存),但比对必须认全部语言的写法——用户可能是
    // 在中文界面标的「我」,切到英文后只认 "Me" 就会把同一个人再标一遍。
    const me = t("notes.speaker.me");
    const meNames = localeVariants("notes.speaker.me");
    const hit = people?.find((p) => meNames.includes(p.name) && p.id !== speakers[id]?.person_id);
    if (hit && onPick) {
      dupPending = { id, name: me, person: hit, linkedOther: !!speakers[id]?.person_id };
      return;
    }
    cancelEdit();
    await run(() => doRename(id, me));
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
        {#if speakers[id]?.multi_speaker}
          <span class="multi-tag">{t("speakers.chipMultiTag")}</span>
        {/if}
        {#if editable}
          <button
            class="name"
            title={t("speakers.chipTitle")}
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
              placeholder={t("speakers.chipRenamePlaceholder")}
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
                    {t("speakers.chipDupMsg", {
                      name: dupPending.person.name,
                      recent: recentLabel(dupPending.person),
                    })}
                  </div>
                  <button class="row strong" onclick={dupAssign}>{t("speakers.chipDupAssign")}</button>
                  <button class="row" onclick={dupRename}>{t("speakers.dupKeepNo")}</button>
                {:else}
                  <div class="dup-msg">
                    {t("speakers.chipDupLinked", { name: dupPending.person.name })}
                  </div>
                  <a class="row" href="/speakers/{dupPending.person.id}" onclick={cancelEdit}
                    >{t("speakers.chipViewThat", { name: dupPending.person.name })}</a
                  >
                  <button class="row" onclick={dupRename}>{t("speakers.chipRenameAnyway")}</button>
                {/if}
                <button class="row quiet" onclick={cancelEdit}>{t("speakers.cancel")}</button>
              </div>
            {:else}
              {#if !editingDirty}
                {#if previewClips && onPreviewClip}
                  {@const clips = previewClips(id)}
                  {#if clips.length === 0}
                    <div class="row row-off">
                      <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                        <path d="M5 3.5v9l7.5-4.5z" />
                      </svg>
                      {t("speakers.chipPreview")}
                      <span class="row-sub">{t("speakers.chipPreviewEmpty")}</span>
                    </div>
                  {:else}
                    <!-- 片段列表:最长的几段各成一行,时间点 + 时长,正在播的高亮。听清哪段
                         就据此改名/选人,那段会成为这个人的声纹样本(与后端 audited_seq 一致)。 -->
                    <div class="clips">
                      <div class="clips-title">{t("speakers.chipClipsTitle", { n: clips.length })}</div>
                      {#each clips as c, i (c.seq)}
                        {@const playing = previewingId === id && previewingSeq === c.seq}
                        <div class="clip-line" class:playing class:picked={sampleSel.includes(c.seq)}>
                        <button class="row clip" class:playing onclick={() => { lastPlayed[id] = c.seq; onPreviewClip(id, c.seq); }}>
                          {#if playing}
                            <span class="bars" aria-hidden="true"><span></span><span></span><span></span></span>
                          {:else}
                            <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                              <path d="M5 3.5v9l7.5-4.5z" />
                            </svg>
                          {/if}
                          <span class="clip-idx">{i + 1}</span>
                          <span class="clip-at">{clockOf(c.start_ms)}</span>
                          <span class="row-sub">{t("speakers.chipClipDur", { s: Math.round((c.end_ms - c.start_ms) / 1000) })}</span>
                          {#if playing}<span class="row-sub accent">{t("speakers.chipClipPlaying")}</span>{/if}
                        </button>
                        <label class="clip-pick" title={t("speakers.chipClipPickTitle")}>
                          <input
                            type="checkbox"
                            checked={sampleSel.includes(c.seq)}
                            onchange={(e) => {
                              const on = (e.currentTarget as HTMLInputElement).checked;
                              sampleSel = on ? [...sampleSel.filter((q) => q !== c.seq), c.seq] : sampleSel.filter((q) => q !== c.seq);
                            }}
                          />
                          <span>{t("speakers.chipClipPick")}</span>
                        </label>
                        {#if onDetachClip}
                          <button
                            class="clip-detach"
                            title={t("speakers.chipClipDetachTitle")}
                            aria-label={t("speakers.chipClipDetach")}
                            onclick={() => run(() => onDetachClip(id, c.seq))}
                          >
                            <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10 3.5h3v3M13 3.5L8.5 8M6 3.5H3v9h9V10" /></svg>
                          </button>
                        {/if}
                        </div>
                      {/each}
                      {#if sampleSel.length > 0}
                        {@const secs = Math.round(clips.filter((c) => sampleSel.includes(c.seq)).reduce((a, c) => a + (c.end_ms - c.start_ms), 0) / 1000)}
                        <div class="clips-hint" class:warn={secs < 10}>
                          {secs < 10 ? t("speakers.chipClipsSelShort", { n: sampleSel.length, s: secs }) : t("speakers.chipClipsSel", { n: sampleSel.length, s: secs })}
                        </div>
                      {:else}
                        <div class="clips-hint">{t("speakers.chipClipsHint")}</div>
                      {/if}
                    </div>
                  {/if}
                {:else if onPreview}
                  {#if counts && !counts[id]}
                    <!-- 名下已无段落(如拆分后清空的原始说话人):试听无物可放,静默
                         没反应会被当成坏了(2026-08-22 用户实测)——置灰并说明白。 -->
                    <div class="row row-off">
                      <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                        <path d="M5 3.5v9l7.5-4.5z" />
                      </svg>
                      {t("speakers.chipPreview")}
                      <span class="row-sub">{t("speakers.chipPreviewEmpty")}</span>
                    </div>
                  {:else}
                    <button class="row" onclick={() => onPreview(id)}>
                      <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                        <path d="M5 3.5v9l7.5-4.5z" />
                      </svg>
                      {t("speakers.chipPreview")}
                      {#if previewingId === id}<span class="row-sub">{t("speakers.chipPreviewPlaying")}</span>{/if}
                    </button>
                  {/if}
                {/if}
              {/if}
              <!-- 动作行常驻(输入名字时也在):拆分排第一——分人错了是最常见、
                   也最要紧的修正,不能埋在试听与人物列表之间。 -->
              {#if onMarkMulti && !speakers[id]?.multi_speaker}
                {#if blockedReason}
                  <div class="row row-off">
                    <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" aria-hidden="true">
                      <circle cx="5.5" cy="5.5" r="2.6" /><circle cx="10.5" cy="5.5" r="2.6" />
                      <path d="M2.2 13.4c.6-2.1 1.9-3.2 3.3-3.2m5 0c1.4 0 2.7 1.1 3.3 3.2" />
                    </svg>
                    {t("speakers.chipMarkMulti")}
                    <span class="row-sub">{blockedReason}</span>
                  </div>
                {:else}
                  <button class="row" title={t("speakers.chipMarkMultiTitle")} onclick={() => { cancelEdit(); onMarkMulti(id); }}>
                    <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" aria-hidden="true">
                      <circle cx="5.5" cy="5.5" r="2.6" /><circle cx="10.5" cy="5.5" r="2.6" />
                      <path d="M2.2 13.4c.6-2.1 1.9-3.2 3.3-3.2m5 0c1.4 0 2.7 1.1 3.3 3.2" />
                    </svg>
                    {t("speakers.chipMarkMulti")}
                  </button>
                {/if}
              {/if}
              <button class="row" onclick={() => markAsMe(id)}>
                <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" aria-hidden="true">
                  <circle cx="8" cy="5.2" r="2.6" />
                  <path d="M2.8 13.4c.9-2.4 2.9-3.6 5.2-3.6s4.3 1.2 5.2 3.6" />
                </svg>
                {t("speakers.chipMe")}
              </button>
              {#if onUnlink && speakers[id]?.person_id}
                <button class="row" onclick={() => commitUnlink(id)}>
                  <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" aria-hidden="true">
                    <path d="M6.5 9.5 4.8 11.2a2.4 2.4 0 0 1-3.4-3.4l1.7-1.7M9.5 6.5l1.7-1.7a2.4 2.4 0 0 1 3.4 3.4l-1.7 1.7M6 10l4-4" />
                  </svg>
                  {t("speakers.chipUnlink")}
                </button>
              {/if}
              {#if onDelete && blockedReason}
                <div class="row row-off">
                  <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" aria-hidden="true">
                    <path d="M3 4.5h10M6.5 4.5V3h3v1.5M5 4.5l.7 8.5h4.6l.7-8.5" />
                  </svg>
                  {t("speakers.chipDelete")}
                  <span class="row-sub">{blockedReason}</span>
                </div>
              {:else if onDelete}
                {#if deletePending}
                  <button class="row strong" onclick={() => commitDelete(id)}>{t("speakers.chipDeleteConfirm")}</button>
                  <button class="row quiet" onclick={() => (deletePending = false)}>{t("speakers.cancel")}</button>
                {:else}
                  <button class="row" onclick={() => (deletePending = true)}>
                    <svg class="row-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" aria-hidden="true">
                      <path d="M3 4.5h10M6.5 4.5V3h3v1.5M5 4.5l.7 8.5h4.6l.7-8.5" />
                    </svg>
                    {t("speakers.chipDelete")}
                  </button>
                {/if}
              {/if}
              {#if people && onPick}
                <div class="caption">{t("speakers.title")}</div>
                <PersonPickList
                  people={people ?? []}
                  query={editingDirty ? editingName : ""}
                  onpick={(p) => commitPick(id, p.id)}
                  selectedId={speakers[id]?.person_id ?? null}
                  hintId={speakers[id]?.hint_person ?? null}
                  emptyText={people.length === 0
                    ? t("speakers.noPeopleYet")
                    : t("speakers.noMatch")}
                />
              {/if}
            {/if}
          </div>
        {/if}
      </div>
    {/each}
    {#if collapsible}
      <!-- 碎片折叠钮:声纹没归成簇的一次性说话人收进来,别摊满一整条 -->
      <button class="chip more" onclick={() => (showFragments = !showFragments)}>
        {showFragments
          ? t("speakers.collapse")
          : t("speakers.fragmentsN", { n: fragmentIds.length })}
      </button>
    {/if}
  </div>
{/if}

<!-- 操作失败提示。刻意放在 chips 的 {#if} 之外:删掉最后一个说话人失败时 ids 可能
     已空,提示不能跟着一起消失。role=alert 让读屏当场播报——这条信息此前完全不存在
     (错误被吞),用户与排查者都看不到后端给的原因。 -->
{#if actionErr}
  <div class="action-err" role="alert">{actionErr}</div>
{/if}

<style>
  .multi-tag {
    font-size: 10px;
    padding: 0 5px;
    border-radius: 6px;
    border: 1px solid currentColor;
    opacity: 0.75;
    margin-right: 4px;
  }

  .action-err {
    color: var(--danger);
    font-size: 0.9rem;
    margin: -0.25rem 0 0.75rem;
  }
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
  /* 片段列表(显式试听) */
  .clips {
    padding: 0.15rem 0 0.25rem;
    border-top: 1px solid var(--hairline);
    border-bottom: 1px solid var(--hairline);
    margin: 0.2rem 0;
  }
  .clips-title,
  .clips-hint {
    font-size: 0.72rem;
    color: var(--ink-faint);
    padding: 0.2rem 0.55rem;
  }
  .clips-hint {
    line-height: 1.45;
  }
  .clip-line {
    display: flex;
    align-items: center;
    gap: 0.25rem;
    border-radius: var(--radius-md);
  }
  .clip-line.picked {
    background: var(--surface-soft);
  }
  .clip-line .row.clip {
    flex: 1 1 auto;
    width: auto;
  }
  .clip-pick {
    display: inline-flex;
    align-items: center;
    gap: 0.25em;
    font-size: 0.72rem;
    color: var(--ink-faint);
    cursor: pointer;
    white-space: nowrap;
    padding: 0 0.3rem;
  }
  .clip-pick input {
    accent-color: var(--accent);
    margin: 0;
  }
  .clip-detach {
    width: 1.5rem;
    height: 1.5rem;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    border: none;
    background: none;
    color: var(--ink-faint);
    border-radius: var(--radius-sm);
    cursor: pointer;
    margin-right: 0.25rem;
  }
  .clip-detach:hover {
    color: var(--ink);
    background: var(--surface-press);
  }
  .clips-hint.warn {
    color: var(--warning-ink);
  }
  .row.clip {
    padding: 0.3rem 0.55rem;
    font-size: 0.84rem;
    font-variant-numeric: tabular-nums;
  }
  .row.clip.playing {
    background: var(--accent-tint);
    color: var(--accent);
  }
  .clip-idx {
    width: 1.1em;
    color: var(--ink-faint);
    font-size: 0.74rem;
  }
  .clip-at {
    font-weight: 500;
  }
  .row-sub.accent {
    color: var(--accent);
  }
  .bars {
    display: inline-flex;
    align-items: flex-end;
    gap: 2px;
    width: 14px;
    height: 12px;
  }
  .bars span {
    width: 2.5px;
    border-radius: 1px;
    background: currentColor;
    animation: chipEq 0.9s ease-in-out infinite;
  }
  .bars span:nth-child(1) { height: 60%; }
  .bars span:nth-child(2) { height: 100%; animation-delay: 0.25s; }
  .bars span:nth-child(3) { height: 75%; animation-delay: 0.5s; }
  @keyframes chipEq {
    0%, 100% { transform: scaleY(0.5); }
    50% { transform: scaleY(1); }
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
  .row-off {
    opacity: 0.45;
    cursor: default;
    display: flex;
    align-items: center;
    gap: 8px;
  }
</style>
