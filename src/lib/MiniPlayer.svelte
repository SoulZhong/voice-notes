<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import { playback, shouldShowMiniPlayer } from "$lib/playback.svelte";
  import { formatTs } from "$lib/notes";
  import { t } from "$lib/i18n/index.svelte";

  /* 全局迷你播放条:切到非笔记页后仍能看到并控制正在播放的笔记。
     进度只读——要精确跳转就点标题回笔记页,那里有波形和逐句时间戳。 */
  const show = $derived(shouldShowMiniPlayer(playback.session?.noteId ?? null, $page.url.pathname));
  const pct = $derived(
    playback.session && playback.session.totalMs > 0
      ? Math.min(100, (playback.currentMs / playback.session.totalMs) * 100)
      : 0,
  );

  function toggle() {
    if (playback.playing) {
      playback.playing = false;
      void invoke("player_pause").catch(() => {});
    } else {
      playback.playing = true;
      void invoke("player_play").catch(() => {});
    }
  }

  function close() {
    void invoke("player_stop", {}).catch(() => {});
    playback.clear();
  }
</script>

{#if show && playback.session}
  <div class="mini">
    <button class="icon" onclick={toggle} aria-label={playback.playing ? t("notes.player.pause") : t("notes.player.play")}>
      {playback.playing ? "⏸" : "▶"}
    </button>
    <button class="title" onclick={() => goto(`/notes/${playback.session?.noteId}`)}>
      {playback.session.title}
    </button>
    <div class="bar"><div class="fill" style:width="{pct}%"></div></div>
    <span class="ts">{formatTs(playback.currentMs)} / {formatTs(playback.session.totalMs)}</span>
    <button class="icon" onclick={close} aria-label={t("common.close")}>✕</button>
  </div>
{/if}

<style>
  .mini {
    position: fixed;
    left: 0;
    right: 0;
    bottom: 0;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 14px;
    background: var(--surface);
    border-top: 1px solid var(--hairline-strong);
    box-shadow: var(--shadow-popover);
    color: var(--ink);
    z-index: 40;
  }
  .icon {
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    font-size: 14px;
    padding: 4px 6px;
  }
  .title {
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    font-size: 13px;
    max-width: 32ch;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    text-align: left;
  }
  .title:hover { text-decoration: underline; }
  .bar { flex: 1; height: 3px; background: var(--hairline-strong); border-radius: var(--radius-sm); }
  .fill { height: 100%; background: var(--accent); border-radius: var(--radius-sm); }
  .ts { font-size: 12px; opacity: 0.7; font-variant-numeric: tabular-nums; }
</style>
