<script lang="ts">
  import { speakerColor, speakerLabel, renameSpeaker, speakerIdCompare } from "$lib/notes";

  let {
    speakers,
    noteId,
    editable,
    onRenamed,
  }: {
    speakers: Record<string, { name: string; sources: string[] }>;
    noteId: string;
    editable: boolean;
    onRenamed?: () => void;
  } = $props();

  let editingId = $state<string | null>(null);
  let editingName = $state("");

  const ids = $derived(Object.keys(speakers).sort(speakerIdCompare));

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
    {#each ids as id (id)}
      <!-- speaker-chip：同徽章色系(粉彩底+ink字)，chip 本身就是色块，不再需要单独的色点 -->
      <div class="chip" class:editable style="background: {speakerColor(id, 'mic')}">
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
    background: var(--tint-gray);
    color: var(--ink);
    border-radius: var(--radius-full);
    padding: 0.2em 0.6em;
    font-size: 0.85em;
  }
  /* 可点击时 hover 加 accent-tint 外环 */
  .chip.editable:hover {
    box-shadow: 0 0 0 2px var(--accent-tint);
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
