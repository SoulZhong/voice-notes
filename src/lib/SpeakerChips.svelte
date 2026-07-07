<script lang="ts">
  import { speakerColor, speakerInk, speakerLabel, renameSpeaker, speakerIdCompare } from "$lib/notes";

  let {
    speakers,
    noteId,
    editable,
    counts,
    onRenamed,
  }: {
    speakers: Record<string, { name: string; sources: string[]; person_id?: string | null }>;
    noteId: string;
    editable: boolean;
    /** 各说话人的段数(可选)。传入则按段数降序排,并折叠只出现 1 段的碎片说话人;
        不传(如录制页实时条)保持原 id 序、不折叠。 */
    counts?: Record<string, number>;
    onRenamed?: () => void;
  } = $props();

  let editingId = $state<string | null>(null);
  let editingName = $state("");

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
  // 换笔记复位折叠态,别把上一篇的展开带过来
  $effect(() => {
    void noteId;
    showFragments = false;
  });
  /** 少于 3 个碎片不值得折叠:展开钮本身比一两枚 chip 更占地。 */
  const collapsible = $derived(fragmentIds.length >= 3);
  const visibleIds = $derived(
    collapsible && !showFragments ? ids.filter((id) => !fragmentIds.includes(id)) : ids,
  );

  // 非 null 分支与徽章共用同一兜底逻辑;source 参数在此分支无关,固定传 "mic"。
  const label = (id: string) => speakerLabel(id, "mic", speakers);

  function beginEdit(id: string) {
    editingId = id;
    editingName = speakers[id]?.name ?? "";
  }

  function cancelEdit() {
    editingId = null;
  }

  async function commitEdit() {
    if (!editingId) return;
    const id = editingId;
    const name = editingName.trim();
    editingId = null;
    if (!name) return;
    await renameSpeaker(noteId, id, name);
    onRenamed?.();
  }

  async function markAsMe(id: string) {
    await renameSpeaker(noteId, id, "我");
    onRenamed?.();
  }
</script>

{#if ids.length > 0}
  <div class="chips">
    {#each visibleIds as id (id)}
      <!-- speaker-chip：同徽章色系(粉彩底+ink字)，chip 本身就是色块，不再需要单独的色点 -->
      <div class="chip" class:editable style="background: {speakerColor(id, 'mic', speakers)}; color: {speakerInk(id, 'mic', speakers)}">
        {#if editable && editingId === id}
          <!-- svelte-ignore a11y_autofocus -->
          <input
            class="edit"
            autofocus
            bind:value={editingName}
            onkeydown={(e) => {
              if (e.key === "Enter") commitEdit();
              if (e.key === "Escape") cancelEdit();
            }}
            onblur={commitEdit}
          />
        {:else if editable}
          <button class="name" onclick={() => beginEdit(id)}>{label(id)}</button>
          <button class="me" onclick={() => markAsMe(id)}>这是我</button>
        {:else}
          <span class="name">{label(id)}</span>
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
  /* speaker-chip：粉彩底(内联 style 按说话人取色) + ink 字 + rounded-full */
  .chip {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    /* 底色与文字色均由内联 style 按说话人配对,此处不设默认(设了也恒被覆盖) */
    border-radius: var(--radius-full);
    padding: 0.2em 0.6em;
    font-size: 0.85em;
  }
  /* 可点击时 hover 加 accent-tint 外环 */
  .chip.editable:hover {
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
    cursor: text;
  }
  .me {
    background: none;
    border: none;
    color: var(--accent);
    cursor: pointer;
    padding: 0.05em 0.3em;
    font-size: 0.9em;
  }
  .me:hover {
    text-decoration: underline;
  }
  .edit {
    font-size: 1em;
    padding: 0.05em 0.3em;
    border-radius: var(--radius-md);
    border: 1px solid var(--accent);
    background: var(--canvas);
    color: var(--ink);
    max-width: 8em;
  }
</style>
