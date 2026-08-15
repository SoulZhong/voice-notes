<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import { playback, shouldShowMiniPlayer } from "$lib/playback.svelte";
  import { t } from "$lib/i18n/index.svelte";

  /* 全局迷你播放浮层:切到非笔记页后仍能看到并控制正在播放的笔记。
     形态选择:曾经是底部通栏(position:fixed + left/right:0),真机实测仍会遮住
     页面底部内容;角落圆形浮层把遮挡面积从整条通栏收窄到一个圆(直径见 --orb-size),但圆本身
     仍会压住右下角内容(如设置页最后一行的「检查更新」按钮)——浮层不占布局流,
     不会自动帮可滚动内容让位。真正的避让由 +layout.svelte 负责:浮层显示时给
     .main 加一段底部安全区,量与本文件 .orb 的尺寸/离边距共用同一份 --orb-size/
     --orb-inset token(定义在 app.css),不用测量元素高度回填。
     进度只读——要精确跳转就右键「回到笔记」,那里有波形和逐句时间戳。 */
  const show = $derived(shouldShowMiniPlayer(playback.session?.noteId ?? null, $page.url.pathname));
  const pct = $derived(
    playback.session && playback.session.totalMs > 0
      ? Math.min(100, (playback.currentMs / playback.session.totalMs) * 100)
      : 0,
  );

  /* 进度环几何:这是 SVG 自己的坐标系,不是像素——viewBox 会按元素实际尺寸等比缩放,
     所以改 app.css 的 --orb-size **不需要**动这里(数值恰好同为 72 是巧合,别当成耦合)。
     半径取坐标系半径减半个描边宽度(stroke-width 3),让描边落在圆内不越界。 */
  const RING_VB = 72;
  const RING_R = 33;
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
    <svg class="ring" viewBox="0 0 {RING_VB} {RING_VB}" aria-hidden="true">
      <circle class="track" cx={RING_VB / 2} cy={RING_VB / 2} r={RING_R} />
      <circle
        class="fill"
        cx={RING_VB / 2}
        cy={RING_VB / 2}
        r={RING_R}
        stroke-dasharray={RING_C}
        stroke-dashoffset={dashoffset}
      />
    </svg>
    <!-- 图标遵循 DESIGN.md 第 7 条:禁用 emoji/Unicode 符号字符(▶⏸ 各平台字形/基线不一),
         统一走 SVG 图形(fill: currentColor,随按钮的 --on-accent 走)——与
         AudioPlayer.svelte 的播放键同款,16 单位坐标系按 22px 渲染。 -->
    <button
      class="toggle"
      onclick={toggle}
      aria-label={playback.playing ? t("notes.player.pause") : t("notes.player.play")}
    >
      {#if playback.playing}
        <svg width="22" height="22" viewBox="0 0 16 16" aria-hidden="true">
          <rect x="3" y="2.5" width="3.4" height="11" rx="1" fill="currentColor" />
          <rect x="9.6" y="2.5" width="3.4" height="11" rx="1" fill="currentColor" />
        </svg>
      {:else}
        <svg width="22" height="22" viewBox="0 0 16 16" aria-hidden="true">
          <path
            d="M4.5 2.8v10.4c0 .8.9 1.3 1.6.9l8-5.2c.6-.4.6-1.4 0-1.8l-8-5.2c-.7-.4-1.6.1-1.6.9z"
            fill="currentColor"
          />
        </svg>
      {/if}
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
    /* 圆心按钮另立一套:上面两个是给 surface 灰底调的,按钮底色是 accent 实底,明暗
       两主题的 accent luminance 相反(亮色 #0f7fd1 深蓝 / 暗色 #57c1ff 浅蓝),沿用
       surface 那套会让暗色下 0.08 的高光在浅蓝上几乎看不见。故按 accent 单独取值。
       亮色这一档卡在图标对比度上,不能凭手感调:白字形(--on-accent)压在高光最亮处,
       高光越强底色越白、字越糊。实算 0.4 白叠 #0f7fd1 得 rgb(111,178,227),对白字
       仅 2.30:1,低于 3:1;取 0.16 得 3.31:1,留出余量。暗色是近黑字形叠浅蓝,
       高光反而把对比拉到 ~11.6:1,故两档不对称是刻意的,别"顺手统一"。 */
    --orb-toggle-sheen: light-dark(rgba(255, 255, 255, 0.16), rgba(255, 255, 255, 0.22));
    --orb-toggle-shade: light-dark(rgba(0, 0, 0, 0.18), rgba(0, 0, 0, 0.12));
    position: fixed;
    /* 尺寸/离边距取全局 token(见 app.css),不在这里写死——+layout.svelte 的主内容区
       底部安全区要按同一份数字算(浮层直径 + 上下留白),两处各写一份数字会互相漂移。 */
    right: var(--orb-inset);
    bottom: var(--orb-inset);
    width: var(--orb-size);
    height: var(--orb-size);
    border-radius: var(--radius-full);
    background: radial-gradient(circle at 32% 26%, var(--orb-sheen), transparent 60%), var(--surface);
    border: 1px solid var(--hairline-strong);
    /* 四层阴影叠出立体感 + 醒目度:悬浮投影 + 顶缘高光描边 + 底缘暗压边 + 常驻的
       accent 色柔光——不止播放时才发光,静止态在任意页面背景上也能一眼找到,
       不用等呼吸动画启动才显眼(冒烟反馈:配色不够醒目)。 */
    box-shadow:
      var(--shadow-popover),
      inset 0 1px 0 var(--orb-sheen),
      inset 0 -3px 5px -3px var(--orb-shade),
      0 2px 14px -2px color-mix(in srgb, var(--accent) 45%, transparent);
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
        0 2px 14px -2px color-mix(in srgb, var(--accent) 45%, transparent),
        0 0 0 0 color-mix(in srgb, var(--accent) 45%, transparent);
    }
    50% {
      box-shadow:
        var(--shadow-popover),
        inset 0 1px 0 var(--orb-sheen),
        inset 0 -3px 5px -3px var(--orb-shade),
        0 2px 14px -2px color-mix(in srgb, var(--accent) 45%, transparent),
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
  /* 圆心按钮走 accent 实心底(此前是 surface 灰底,和外壳同色调,不够醒目)——
     真正可点的控件用全局主强调色撑起来,和 accent 描边的进度环互相呼应,
     一眼能认出这是个"播放器"而不是一块装饰性圆牌。 */
  .toggle {
    position: absolute;
    /* 8px 留给外圈进度环,其余全部是可点的实心按钮——按钮本身要占大头,
       环只是它外面的一圈细边,不能反过来大半个浮层都是空的外壳。 */
    inset: 8px;
    border-radius: var(--radius-full);
    background: radial-gradient(circle at 34% 28%, var(--orb-toggle-sheen), transparent 65%), var(--accent);
    box-shadow:
      inset 0 1px 2px var(--orb-toggle-shade),
      inset 0 1px 0 var(--orb-toggle-sheen);
    border: none;
    color: var(--on-accent);
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: transform 0.1s ease, background-color 0.15s ease;
  }
  .toggle:hover {
    background: radial-gradient(circle at 34% 28%, var(--orb-toggle-sheen), transparent 65%), var(--accent-pressed);
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
    right: var(--orb-inset);
    /* 浮层顶边 + 8px 间距,随 --orb-size 变化自动跟着走,不用跟 .orb 的尺寸手动对账。 */
    bottom: calc(var(--orb-inset) + var(--orb-size) + 8px);
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
