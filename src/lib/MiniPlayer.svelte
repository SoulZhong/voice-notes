<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import { playback, shouldShowMiniPlayer } from "$lib/playback.svelte";
  import { t } from "$lib/i18n/index.svelte";

  /* 全局迷你播放浮层:切到非笔记页后仍能看到并控制正在播放的笔记。
     形态选择:曾经是底部通栏(position:fixed + left/right:0),真机实测仍会遮住
     页面底部内容——罪魁是 padding-bottom 避让对内部自带 height:100%/overflow:hidden
     的页面（如设置页）无效,那类容器按 .main 的内容盒算高,父级加 padding 不会让
     它们跟着缩短。角落圆形浮层不占布局流、不需要任何避让,从根上绕开这个问题。
     进度只读——要精确跳转就右键「回到笔记」,那里有波形和逐句时间戳。 */
  const show = $derived(shouldShowMiniPlayer(playback.session?.noteId ?? null, $page.url.pathname));
  const pct = $derived(
    playback.session && playback.session.totalMs > 0
      ? Math.min(100, (playback.currentMs / playback.session.totalMs) * 100)
      : 0,
  );

  // 进度环几何:半径取容器半径减半个描边宽度,让描边落在浮层圆内不越界。
  const RING_R = 26;
  const RING_C = 2 * Math.PI * RING_R;
  const dashoffset = $derived(RING_C * (1 - pct / 100));

  let menuOpen = $state(false);

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

  function openMenu(e: MouseEvent) {
    e.preventDefault();
    menuOpen = true;
  }

  function closeMenu() {
    menuOpen = false;
  }

  function backToNote() {
    closeMenu();
    if (playback.session) goto(`/notes/${playback.session.noteId}`);
  }

  function stopPlayback() {
    closeMenu();
    close();
  }
</script>

{#if show && playback.session}
  <!-- 右键菜单是纯指针便利入口,键盘用户走圆心按钮(播放/暂停)与右键菜单里
       的两个真按钮即可覆盖全部操作,故此处不需要额外 role/键盘处理 -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="orb" class:playing={playback.playing} title={playback.session.title} oncontextmenu={openMenu}>
    <svg class="ring" viewBox="0 0 56 56" aria-hidden="true">
      <circle class="track" cx="28" cy="28" r={RING_R} />
      <circle
        class="fill"
        cx="28"
        cy="28"
        r={RING_R}
        stroke-dasharray={RING_C}
        stroke-dashoffset={dashoffset}
      />
    </svg>
    <button
      class="toggle"
      onclick={toggle}
      aria-label={playback.playing ? t("notes.player.pause") : t("notes.player.play")}
    >
      {playback.playing ? "⏸" : "▶"}
    </button>
  </div>

  {#if menuOpen}
    <!-- 点击任意处关闭;键盘路径由 svelte:window 的 Esc 承担,遮罩是纯指针便利层 -->
    <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
    <div
      class="menu-overlay"
      onclick={closeMenu}
      oncontextmenu={(e) => {
        e.preventDefault();
        closeMenu();
      }}
    ></div>
    <!-- 菜单锚定在浮层同一个角落(右/下),不跟随光标定位——浮层本身贴着视口
         右下角,菜单从这个角向左上方长出去,天然不会被窗口边缘裁掉。 -->
    <div class="ctx-menu">
      <button class="ctx-item" onclick={backToNote}>{t("notes.player.backToNote")}</button>
      <button class="ctx-item" onclick={stopPlayback}>{t("notes.player.stopPlayback")}</button>
    </div>
  {/if}
{/if}

<svelte:window
  onkeydown={(e) => {
    if (e.key === "Escape" && menuOpen) closeMenu();
  }}
/>

<style>
  .orb {
    /* 玻璃质感:径向高光(左上角)叠一层 surface 底色,给纯色圆盘一点厚度——
       高光强度按明暗主题分别取值,不引入新令牌,只是对 --surface 的局部提亮/压暗。 */
    --orb-sheen: light-dark(rgba(255, 255, 255, 0.65), rgba(255, 255, 255, 0.08));
    --orb-shade: light-dark(rgba(0, 0, 0, 0.06), rgba(0, 0, 0, 0.35));
    position: fixed;
    right: 16px;
    bottom: 16px;
    width: 56px;
    height: 56px;
    border-radius: var(--radius-full);
    background: radial-gradient(circle at 32% 26%, var(--orb-sheen), transparent 60%), var(--surface);
    border: 1px solid var(--hairline-strong);
    /* 三层阴影叠出立体感:悬浮投影 + 顶缘高光描边 + 底缘暗压边,而不是单一平面阴影 */
    box-shadow:
      var(--shadow-popover),
      inset 0 1px 0 var(--orb-sheen),
      inset 0 -3px 5px -3px var(--orb-shade);
    color: var(--ink);
    z-index: 40;
    transition: transform 0.15s ease, box-shadow 0.15s ease;
  }
  .orb:hover {
    transform: translateY(-1px);
  }
  /* 播放中的呼吸效果:外圈柔光随 2.4s 周期扩散又收回,不摇晃位置/不闪烁颜色,
     只用 accent 的透明度做"吸气—呼气",安静地提示"这里正在活动"。 */
  .orb.playing {
    animation: orb-breathe 2.4s ease-in-out infinite;
  }
  @keyframes orb-breathe {
    0%,
    100% {
      box-shadow:
        var(--shadow-popover),
        inset 0 1px 0 var(--orb-sheen),
        inset 0 -3px 5px -3px var(--orb-shade),
        0 0 0 0 color-mix(in srgb, var(--accent) 45%, transparent);
    }
    50% {
      box-shadow:
        var(--shadow-popover),
        inset 0 1px 0 var(--orb-sheen),
        inset 0 -3px 5px -3px var(--orb-shade),
        0 0 0 7px color-mix(in srgb, var(--accent) 0%, transparent);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .orb.playing {
      animation: none;
    }
  }
  .ring {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    /* 描边从几何起点(3 点钟方向)转到 12 点钟方向,再靠 dashoffset 顺时针铺开 */
    transform: rotate(-90deg);
    pointer-events: none;
  }
  .ring circle {
    fill: none;
    stroke-width: 3;
  }
  .ring .track {
    stroke: var(--hairline-strong);
  }
  .ring .fill {
    stroke: var(--accent);
    stroke-linecap: round;
    transition: stroke-dashoffset 0.15s linear;
  }
  .toggle {
    position: absolute;
    inset: 8px;
    border-radius: var(--radius-full);
    background: radial-gradient(circle at 34% 30%, var(--orb-sheen), transparent 65%), var(--surface);
    box-shadow:
      inset 0 1px 2px var(--orb-shade),
      inset 0 1px 0 var(--orb-sheen);
    border: none;
    color: var(--ink);
    cursor: pointer;
    font-size: 18px;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: transform 0.1s ease, background-color 0.15s ease;
  }
  .toggle:hover {
    background: radial-gradient(circle at 34% 30%, var(--orb-sheen), transparent 65%), var(--surface-soft);
  }
  .toggle:active {
    transform: scale(0.92);
  }
  .menu-overlay {
    position: fixed;
    inset: 0;
    z-index: 40;
  }
  .ctx-menu {
    position: fixed;
    right: 16px;
    bottom: 80px;
    z-index: 41;
    min-width: 9rem;
    background: var(--surface-press);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    padding: 4px;
    display: flex;
    flex-direction: column;
  }
  .ctx-item {
    background: none;
    border: none;
    text-align: left;
    color: var(--ink);
    font: inherit;
    font-size: 13px;
    padding: 7px 10px;
    border-radius: var(--radius-md);
    cursor: pointer;
  }
  .ctx-item:hover {
    background: var(--surface-soft);
  }
</style>
