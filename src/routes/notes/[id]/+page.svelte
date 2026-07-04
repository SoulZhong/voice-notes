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
    speakerIdCompare,
    editSegment,
    deleteSegment,
    setSegmentSpeaker,
    type Note,
    type SegmentRecord,
  } from "$lib/notes";
  import SpeakerChips from "$lib/SpeakerChips.svelte";

  let note = $state<Note | null>(null);
  let error = $state("");
  let editing = $state(false);
  let editingTitle = $state("");
  let exportMsg = $state("");

  // 段落编辑状态
  let editingSeq = $state<number | null>(null);
  let editingText = $state("");
  let confirmSeq = $state<number | null>(null);
  let speakerMenuSeq = $state<number | null>(null);

  const id = $derived($page.params.id as string);

  /** 展示序:filter+sort 已下沉 NoteStore::load(单一真值源),后端保证无空白段、
      按 (start_ms, seq) 升序,前端直接消费。 */
  const displaySegments = $derived(note ? note.segments : []);
  /** 本笔记正在录制（含暂停）时禁用一切编辑入口（后端另有 guard 兜底）。 */
  const canEdit = $derived(!(recording.isLive && recording.noteId === id));
  const speakerIds = $derived(note ? Object.keys(note.speakers).sort(speakerIdCompare) : []);

  function durationSecs(n: Note): number | null {
    // 活跃时长优先：段落时间轴最大 end_ms（与转写时间戳/录制计时一致，不含暂停）；
    // 无段落回退墙钟时长。
    if (n.segments.length > 0) {
      return Math.floor(Math.max(...n.segments.map((s) => s.end_ms)) / 1000);
    }
    if (n.meta.ended_at && n.meta.started_at) {
      const d = (new Date(n.meta.ended_at).getTime() - new Date(n.meta.started_at).getTime()) / 1000;
      return isNaN(d) ? null : Math.max(0, Math.floor(d));
    }
    return null;
  }

  async function refresh() {
    try {
      note = await getNote(id);
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }

  // id 切换：无条件复位一切编辑态。
  $effect(() => {
    void id;
    editing = false;
    editingSeq = null;
    speakerMenuSeq = null;
    confirmSeq = null;
  });
  // 刷新：任何编辑进行中都跳过（编辑态是 effect 依赖，编辑结束会自动重跑并刷新）。
  $effect(() => {
    void id;
    void recording.notesVersion;
    if (editing || editingSeq !== null || speakerMenuSeq !== null) return;
    exportMsg = "";
    refresh();
  });

  function beginEditSeg(s: SegmentRecord) {
    editingSeq = s.seq;
    editingText = s.text;
    speakerMenuSeq = null;
    confirmSeq = null;
  }

  async function commitEditSeg(s: SegmentRecord) {
    if (editingSeq !== s.seq) return;
    const text = editingText.trim();
    editingSeq = null;
    if (!text || text === s.text) return;
    try {
      await editSegment(id, s.seq, s.text, text);
      await refresh();
    } catch (e) {
      error = `编辑失败: ${e}`;
      await refresh(); // 乐观冲突：重载最新内容
    }
  }

  async function doDeleteSeg(s: SegmentRecord) {
    confirmSeq = null;
    try {
      await deleteSegment(id, s.seq, s.text);
      await refresh();
    } catch (e) {
      error = `删除失败: ${e}`;
      await refresh();
    }
  }

  async function doSetSpeaker(s: SegmentRecord, speakerId: string) {
    speakerMenuSeq = null;
    try {
      await setSegmentSpeaker(id, s.seq, s.text, speakerId);
      await refresh();
    } catch (e) {
      error = `修改说话人失败: ${e}`;
      await refresh();
    }
  }

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
      <h1 class="title">
        <button class="title-btn" title="点击改名" onclick={beginRename}>{note.meta.title}</button>
      </h1>
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
      <button disabled={recording.isLive} onclick={doResume}>继续录制</button>
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
      {#each displaySegments as seg (seg.seq)}
        <div class="seg">
          {#if canEdit && speakerMenuSeq === seg.seq}
            <span class="badge-menu">
              {#each speakerIds as sid (sid)}
                <button class="menu-item" onclick={() => doSetSpeaker(seg, sid)}>
                  {speakerLabel(sid, seg.source, note.speakers)}
                </button>
              {/each}
              <button class="menu-item new" onclick={() => doSetSpeaker(seg, "new")}>＋ 新说话人</button>
              <button class="menu-item" onclick={() => (speakerMenuSeq = null)}>取消</button>
            </span>
          {:else}
            <button
              class="badge as-btn"
              style="background: {speakerColor(seg.speaker, seg.source)}"
              disabled={!canEdit}
              title={canEdit ? "点击改说话人" : ""}
              onclick={() => (speakerMenuSeq = seg.seq)}
            >
              {speakerLabel(seg.speaker, seg.source, note.speakers)}
            </button>
          {/if}
          <span class="ts">{formatTs(seg.start_ms)}</span>
          {#if editingSeq === seg.seq}
            <!-- svelte-ignore a11y_autofocus -->
            <textarea
              class="seg-edit"
              autofocus
              bind:value={editingText}
              onkeydown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  commitEditSeg(seg);
                }
                if (e.key === "Escape") editingSeq = null;
              }}
              onblur={() => commitEditSeg(seg)}
            ></textarea>
          {:else if canEdit}
            <!-- 文字本身即编辑入口：span+role 保持行内排版（button 是原子行内盒,长文无法跨行断行） -->
            <span
              class="seg-text editable"
              role="button"
              tabindex="0"
              title="点击编辑"
              onclick={() => beginEditSeg(seg)}
              onkeydown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  beginEditSeg(seg);
                }
              }}>{seg.text}</span>
            <span class="seg-actions">
              {#if confirmSeq === seg.seq}
                <button class="link danger" onclick={() => doDeleteSeg(seg)}>确认删除</button>
                <button class="link" onclick={() => (confirmSeq = null)}>取消</button>
              {:else}
                <button class="link" onclick={() => (confirmSeq = seg.seq)}>删除</button>
              {/if}
            </span>
          {:else}
            <span class="seg-text">{seg.text}</span>
          {/if}
        </div>
      {/each}
      {#if displaySegments.length === 0}
        <p class="hint">（这场会议没有转写内容）</p>
      {/if}
    </div>
  {/if}
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
  }
  .title {
    cursor: text;
    margin: 0 0 0.25rem;
  }
  .title-btn {
    background: none;
    border: none;
    box-shadow: none;
    padding: 0;
    margin: 0;
    font: inherit;
    color: inherit;
    cursor: text;
    text-align: left;
  }
  .title-btn:focus-visible {
    outline: 2px solid #396cd8;
    border-radius: 4px;
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
  .seg {
    margin: 0 0 0.35rem;
    line-height: 1.6;
  }
  .badge.as-btn {
    border: none;
    cursor: pointer;
    font-family: inherit;
  }
  .badge.as-btn:disabled {
    cursor: default;
  }
  .seg-text.editable {
    cursor: text;
    border-radius: 4px;
  }
  .seg-text.editable:hover {
    background: rgba(57, 108, 216, 0.08);
  }
  .seg-text.editable:focus-visible {
    outline: 2px solid #396cd8;
  }
  .seg-actions {
    visibility: hidden;
    margin-left: 0.4em;
  }
  .seg:hover .seg-actions {
    visibility: visible;
  }
  .link {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    padding: 0.1em 0.25em;
    font-size: 0.8em;
    box-shadow: none;
  }
  .link.danger {
    color: #c0392b;
    font-weight: 600;
  }
  .seg-edit {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    line-height: 1.5;
    border-radius: 6px;
    border: 1px solid #396cd8;
    padding: 0.3em 0.5em;
    margin-top: 0.2em;
    resize: vertical;
    min-height: 2.4em;
  }
  .badge-menu {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 0.25em;
    background: #fff;
    border: 1px solid #ccc;
    border-radius: 8px;
    padding: 0.2em 0.4em;
    margin-right: 0.4em;
  }
  .menu-item {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    font-size: 0.8em;
    padding: 0.15em 0.4em;
  }
  .menu-item.new {
    font-weight: 600;
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
    .seg-edit {
      background: #2a2a2a;
      color: #f0f0f0;
    }
    .badge-menu {
      background: #2a2a2a;
      border-color: #555;
    }
  }
</style>
