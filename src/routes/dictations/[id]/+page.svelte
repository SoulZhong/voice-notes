<script lang="ts">
  // 听写笔记页:往同一个目标会话说过的全部听写,按时间顺序一句一句列出。
  // 没有说话人、修订稿、Aing——听写笔记不是会议笔记(CONTEXT.md「设备听写」)。
  import { onMount } from "svelte";
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { convertFileSrc } from "@tauri-apps/api/core";
  import { ask } from "@tauri-apps/plugin-dialog";
  import { t } from "$lib/i18n/index.svelte";
  import { formatDate } from "$lib/notes";
  import { recording } from "$lib/recording.svelte";
  import {
    deleteDictationNote,
    deleteDictationRecord,
    getDictationNote,
    onDictationNotesChanged,
    type DictationNote,
    type DictationRecord,
  } from "$lib/device";
  import PlayIcon from "@lucide/svelte/icons/play";
  import { Button } from "$lib/ui/components/button";
  import { cn } from "$lib/ui/utils";

  // 行内文字操作(播放/复制/删除这一句):accent 链接式小字;danger 只给删除。
  const linkBtn = (danger = false) =>
    cn("h-auto gap-1 px-0 py-0 text-[0.78rem] font-normal text-ring no-underline hover:underline", danger && "text-destructive");

  const id = $derived($page.params.id as string);
  let note = $state<DictationNote | null>(null);
  let error = $state("");
  let copied = $state<string | null>(null);
  let playing = $state<HTMLAudioElement | null>(null);

  async function load(target: string) {
    try {
      note = await getDictationNote(target);
      error = "";
    } catch (e) {
      note = null;
      error = t("device.note.loadFailed", { e });
    }
  }

  $effect(() => {
    void load(id);
  });

  onMount(() => {
    const un = onDictationNotesChanged((changed) => {
      if (changed === id) load(id);
    });
    return () => {
      un.then((u) => u());
      playing?.pause();
    };
  });

  async function copy(text: string, key: string) {
    try {
      await navigator.clipboard.writeText(text);
      copied = key;
      setTimeout(() => {
        if (copied === key) copied = null;
      }, 1500);
    } catch {
      // 剪贴板不可用时不打扰用户。
    }
  }

  const liveText = $derived(
    (note?.records ?? [])
      .filter((r) => !r.undone)
      .map((r) => r.text)
      .join("\n"),
  );

  function play(r: DictationRecord) {
    if (!note || !r.audio) return;
    playing?.pause();
    const a = new Audio(convertFileSrc(`${note.audio_dir}/${r.audio}`));
    playing = a;
    a.play().catch(() => {});
  }

  async function removeRecord(r: DictationRecord) {
    try {
      await deleteDictationRecord(id, r.rid);
      recording.bumpNotes();
      await load(id);
      if (!note) goto("/");
    } catch (e) {
      error = t("common.deleteFailed", { e });
    }
  }

  async function removeNote() {
    if (!note) return;
    const yes = await ask(t("device.note.deleteConfirm.message", { label: note.meta.label }), {
      title: t("device.note.deleteConfirm.title"),
      kind: "warning",
      okLabel: t("device.note.deleteConfirm.ok"),
      cancelLabel: t("device.pin.cancel"),
    });
    if (!yes) return;
    try {
      await deleteDictationNote(id);
      recording.bumpNotes();
      goto("/");
    } catch (e) {
      error = t("common.deleteFailed", { e });
    }
  }
</script>

<div class="page">
  {#if error}
    <p class="err">{error}</p>
  {/if}
  {#if note}
    <header>
      <div>
        <span class="kind">{t("device.note.kind")}</span>
        <h1>{note.meta.label}</h1>
        <p class="sub">{t("device.note.subtitle", { app: note.meta.app })}</p>
      </div>
      <div class="head-actions">
        <Button variant="outline" size="sm" disabled={!liveText} onclick={() => copy(liveText, "all")}>
          {copied === "all" ? t("device.note.copied") : t("device.note.copyAll")}
        </Button>
        <Button
          variant="outline"
          size="sm"
          class="border-(color:--danger-line) text-destructive hover:text-destructive dark:border-(color:--danger-line)"
          onclick={removeNote}>{t("device.note.deleteNote")}</Button
        >
      </div>
    </header>

    {#if note.records.length === 0}
      <p class="sub">{t("device.note.empty")}</p>
    {/if}
    <ol class="records">
      {#each note.records as r (r.rid)}
        <li class:undone={r.undone}>
          <div class="meta">
            <time>{formatDate(r.at)}</time>
            {#if r.undone}<span class="tag">{t("device.note.undone")}</span>{/if}
            {#if r.audio}
              <!-- 播放符号走线性图标(DESIGN.md 第 7 条禁 Unicode 符号字符) -->
              <Button variant="link" class={linkBtn()} onclick={() => play(r)}>
                <PlayIcon class="size-3" aria-hidden="true" />{t("device.note.audio", { secs: Math.max(1, Math.round(r.duration_ms / 1000)) })}
              </Button>
            {/if}
          </div>
          <p class="text">{r.text}</p>
          <div class="row-actions">
            <Button variant="link" class={linkBtn()} onclick={() => copy(r.text, r.rid)}>
              {copied === r.rid ? t("device.note.copied") : t("device.note.copy")}
            </Button>
            <Button variant="link" class={linkBtn(true)} onclick={() => removeRecord(r)}>{t("device.note.deleteRecord")}</Button>
          </div>
        </li>
      {/each}
    </ol>
  {/if}
</div>

<style>
  .page { padding: 1.5rem; max-width: 46rem; }
  header { display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem; flex-wrap: wrap; }
  .kind {
    display: inline-block;
    padding: 1px 8px;
    border-radius: var(--radius-full);
    background: var(--tint-sky);
    color: var(--tint-sky-ink);
    font-size: 0.72rem;
  }
  h1 { margin: 0.35rem 0 0.2rem; font-size: 1.35rem; font-weight: 600; word-break: break-word; }
  .sub { margin: 0; color: var(--ink-secondary); font-size: 0.84rem; }
  .head-actions { display: flex; gap: 6px; }
  .records { list-style: none; margin: 1.2rem 0 0; padding: 0; display: grid; gap: 2px; }
  .records li { padding: 0.65rem 0.9rem; border-radius: var(--radius-md); background: var(--surface); }
  .records li:hover .row-actions { visibility: visible; }
  .meta { display: flex; align-items: center; gap: 8px; color: var(--ink-faint); font-size: 0.76rem; }
  .tag { padding: 0 6px; border-radius: var(--radius-full); background: var(--tint-gray); color: var(--tint-gray-ink); }
  .text { margin: 0.25rem 0 0; font-size: 0.95rem; line-height: 1.6; color: var(--ink); white-space: pre-wrap; word-break: break-word; }
  .undone .text { text-decoration: line-through; color: var(--ink-faint); }
  .row-actions { visibility: hidden; display: flex; gap: 10px; margin-top: 0.2rem; }
  @media (hover: none) { .row-actions { visibility: visible; } }
  .err { color: var(--danger); font-size: 0.85rem; }
</style>
