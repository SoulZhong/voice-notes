<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { listNotes } from "$lib/notes";

  let empty = $state(false);

  onMount(async () => {
    try {
      const notes = await listNotes();
      if (notes.length > 0) {
        goto(`/notes/${notes[0].id}`, { replaceState: true });
      } else {
        empty = true;
      }
    } catch {
      empty = true;
    }
  });
</script>

{#if empty}
  <div class="empty">
    <p>还没有会议笔记。</p>
    <p class="hint">点击左上角「● 开始录制」来第一场。</p>
  </div>
{/if}

<style>
  .empty {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    color: #888;
  }
  .hint {
    color: #aaa;
    font-size: 0.9em;
  }
</style>
