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
      <div class="chip">
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
        {:else}
          <span class="dot" style="background: {speakerColor(id, 'mic')}"></span>
          {#if editable}
            <button class="name" onclick={() => beginEdit(id)}>{label(id)}</button>
            <button class="me" onclick={() => markAsMe(id)}>这是我</button>
          {:else}
            <span class="name">{label(id)}</span>
          {/if}
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
  .chip {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    background: #f5f5f7;
    border-radius: 999px;
    padding: 0.2em 0.35em 0.2em 0.5em;
    font-size: 0.85em;
  }
  .dot {
    width: 0.7em;
    height: 0.7em;
    border-radius: 50%;
    flex-shrink: 0;
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
  button.name:hover {
    color: #396cd8;
  }
  .me {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    padding: 0.05em 0.3em;
    font-size: 0.9em;
    box-shadow: none;
  }
  .edit {
    font-size: 1em;
    padding: 0.05em 0.3em;
    border-radius: 6px;
    border: 1px solid #396cd8;
    max-width: 8em;
  }

  @media (prefers-color-scheme: dark) {
    .chip {
      background: #2a2a2a;
      color: #f0f0f0;
    }
    .edit {
      background: #2a2a2a;
      color: #f0f0f0;
    }
  }
</style>
