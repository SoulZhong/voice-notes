<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import { playback, shouldShowMiniPlayer } from "$lib/playback.svelte";
  import { formatTs } from "$lib/notes";
  import { t } from "$lib/i18n/index.svelte";

  /* 迷你条实际高度(px):由 .mini 的 bind:clientHeight 实测回填,不藏在显示与否里的
     常量猜测——+layout.svelte 用它给主内容区留出等高的底部留白,来源与迷你条自身
     的渲染高度是同一个数,padding 与遮挡量恒等,不会跟着以后改 padding/字号跑偏。
     不显示时归零(见下方 $effect),父级据此把留白撤掉,不会凭空多出空白。 */
  let { height = $bindable(0) }: { height?: number } = $props();

  /* 全局迷你播放条:切到非笔记页后仍能看到并控制正在播放的笔记。
     进度只读——要精确跳转就点标题回笔记页,那里有波形和逐句时间戳。 */
  const show = $derived(shouldShowMiniPlayer(playback.session?.noteId ?? null, $page.url.pathname));
  const pct = $derived(
    playback.session && playback.session.totalMs > 0
      ? Math.min(100, (playback.currentMs / playback.session.totalMs) * 100)
      : 0,
  );

  // 条不显示时 .mini 不挂载,bind:clientHeight 不会再收到更新,留着上一次的高度会让
  // 父级留白凭空多出一块——这里主动清零。
  $effect(() => {
    if (!show) height = 0;
  });

  function toggle() {
    // 乐观置位前先存一份"操作前的真值"用于失败回滚——不能写死 true/false:
    // pause 分支失败要回到 true,play 分支失败要回到 false。
    //
    // 回滚判据用 CAS(比较后再写):失败回调触发时,只有 playback.playing 仍等于
    // 我们刚才乐观写入的值,才把它改回 before——这说明这段时间没有 player_pos
    // 事件把状态改写过,回滚安全。一旦不相等,说明后端事件已经把状态纠正/推进
    // 过一次,此时绝不能再用调用前的旧快照去覆盖这个更新的状态。
    const before = playback.playing;
    if (before) {
      playback.playing = false;
      void invoke("player_pause").catch(() => {
        if (playback.playing === false) playback.playing = before;
      });
    } else {
      playback.playing = true;
      void invoke("player_play").catch(() => {
        if (playback.playing === true) playback.playing = before;
      });
    }
  }

  function close() {
    void invoke("player_stop", {}).catch(() => {});
    playback.clear();
  }
</script>

{#if show && playback.session}
  <div class="mini" bind:clientHeight={height}>
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
