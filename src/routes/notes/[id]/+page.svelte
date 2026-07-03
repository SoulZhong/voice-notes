<script lang="ts">
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { revealItemInDir } from "@tauri-apps/plugin-opener";
  import { recording } from "$lib/recording.svelte";
  import {
    getNote,
    renameNote,
    exportNote,
    formatTs,
    formatDate,
    formatDuration,
    speakerLabel,
    speakerColor,
    type Note,
  } from "$lib/notes";
  import SpeakerChips from "$lib/SpeakerChips.svelte";

  let note = $state<Note | null>(null);
  let error = $state("");
  let editing = $state(false);
  let editingTitle = $state("");
  let exportMsg = $state("");

  const id = $derived($page.params.id as string);

  function durationSecs(n: Note): number | null {
    if (n.meta.ended_at && n.meta.started_at) {
      const d = (new Date(n.meta.ended_at).getTime() - new Date(n.meta.started_at).getTime()) / 1000;
      return isNaN(d) ? null : Math.max(0, Math.floor(d));
    }
    const last = n.segments.at(-1);
    return last ? Math.floor(last.end_ms / 1000) : null;
  }

  async function refresh() {
    try {
      note = await getNote(id);
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }

  // id 变化（侧栏切换笔记时组件被复用）或笔记改名/删除时重新加载。
  $effect(() => {
    void id;
    void recording.notesVersion;
    editing = false;
    exportMsg = "";
    refresh();
  });

  function beginRename() {
    if (!note) return;
    editing = true;
    editingTitle = note.meta.title;
  }

  async function commitRename() {
    if (!editing || !note) return;
    editing = false;
    try {
      await renameNote(id, editingTitle);
      recording.bumpNotes();
    } catch (e) {
      error = `改名失败: ${e}`;
    }
  }

  async function doExport(format: "md" | "txt") {
    exportMsg = "";
    try {
      const path = await exportNote(id, format);
      exportMsg = `已导出：${path}`;
      await revealItemInDir(path);
    } catch (e) {
      error = `导出失败: ${e}`;
    }
  }

  async function doResume() {
    const ok = await recording.resume(id);
    if (ok) goto("/record");
    else
      error = recording.status.startsWith("error:")
        ? recording.status
        : "无法继续录制:请确认没有正在进行的录制";
  }
</script>

<main class="container">
  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if note}
    {#if editing}
      <!-- svelte-ignore a11y_autofocus -->
      <input
        class="rename"
        autofocus
        bind:value={editingTitle}
        onkeydown={(e) => {
          if (e.key === "Enter") commitRename();
          if (e.key === "Escape") editing = false;
        }}
        onblur={commitRename}
      />
    {:else}
      <h1 class="title" title="点击改名" onclick={beginRename}>{note.meta.title}</h1>
    {/if}

    <p class="meta">
      {formatDate(note.meta.started_at)} · {formatDuration(durationSecs(note))}
      {#if note.meta.state === "recording"}
        <span class="state interrupted">已中断</span>
      {/if}
    </p>

    {#if note.meta.state === "recording"}
      <div class="banner">这场会议曾意外中断，以下是中断前保存的全部内容。可点击上方「继续录制」接着记。</div>
    {/if}
    {#if note.skipped_lines > 0}
      <div class="banner">有 {note.skipped_lines} 行记录损坏被跳过。</div>
    {/if}

    <div class="row">
      <button onclick={() => doExport("md")}>导出 Markdown</button>
      <button onclick={() => doExport("txt")}>导出纯文本</button>
      <button disabled={recording.isRecording} onclick={doResume}>继续录制</button>
      {#if exportMsg}<span class="hint">{exportMsg}</span>{/if}
    </div>

    <SpeakerChips
      speakers={note.speakers}
      noteId={id}
      editable={true}
      onRenamed={() => {
        refresh();
        recording.bumpNotes();
      }}
    />

    <div class="transcript">
      {#each note.segments as seg (seg.seq)}
        <p class="final">
          <span class="badge" style="background: {speakerColor(seg.speaker, seg.source)}">
            {speakerLabel(seg.speaker, seg.source, note.speakers)}
          </span>
          <span class="ts">{formatTs(seg.start_ms)}</span>
          {seg.text}
        </p>
      {/each}
      {#if note.segments.length === 0}
        <p class="hint">（这场会议没有转写内容）</p>
      {/if}
    </div>
  {/if}
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 42rem;
  }
  .title {
    cursor: text;
    margin: 0 0 0.25rem;
  }
  .rename {
    font-size: 1.6em;
    font-weight: 700;
    width: 100%;
    box-sizing: border-box;
    padding: 0.1em 0.3em;
    border-radius: 8px;
    border: 1px solid #396cd8;
  }
  .meta {
    color: #888;
    margin: 0 0 1rem;
  }
  .row {
    display: flex;
    gap: 0.75rem;
    align-items: center;
    margin: 0 0 1rem;
  }
  button {
    border-radius: 8px;
    border: 1px solid transparent;
    padding: 0.5em 1.2em;
    font-size: 0.95em;
    font-weight: 500;
    cursor: pointer;
    background-color: #ffffff;
    box-shadow: 0 2px 2px rgba(0, 0, 0, 0.2);
  }
  button:hover {
    border-color: #396cd8;
  }
  .transcript {
    background: #f5f5f7;
    border-radius: 8px;
    padding: 1rem;
    font-size: 1.05rem;
    line-height: 1.6;
  }
  .transcript p {
    margin: 0 0 0.35rem;
  }
  .badge {
    display: inline-block;
    min-width: 2.2em;
    text-align: center;
    font-size: 0.75em;
    font-weight: 600;
    border-radius: 6px;
    padding: 0.05em 0.4em;
    margin-right: 0.4em;
    color: #fff;
  }
  .ts {
    color: #999;
    font-size: 0.8em;
    margin-right: 0.4em;
    font-variant-numeric: tabular-nums;
  }
  .state.interrupted {
    background: #d88a39;
    color: #fff;
    font-size: 0.7em;
    font-weight: 600;
    border-radius: 6px;
    padding: 0.1em 0.45em;
    margin-left: 0.4em;
  }
  .banner {
    background: #fff4e5;
    border: 1px solid #f0c98a;
    color: #8a5a00;
    border-radius: 8px;
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .hint {
    color: #aaa;
  }
  @media (prefers-color-scheme: dark) {
    .transcript {
      background: #2a2a2a;
    }
    .rename {
      background: #2a2a2a;
      color: #f0f0f0;
    }
    button {
      color: #ffffff;
      background-color: #0f0f0f98;
    }
    .banner {
      background: #3a2e18;
      border-color: #6b5426;
      color: #e8c88a;
    }
    .hint {
      color: #555;
    }
  }
</style>
