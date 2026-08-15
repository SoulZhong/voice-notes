<script lang="ts">
  import "../app.css";
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import Sidebar from "$lib/Sidebar.svelte";
  import WelcomeOverlay from "$lib/WelcomeOverlay.svelte";
  import { recording } from "$lib/recording.svelte";
  import { tidy } from "$lib/tidy.svelte";
  import { getSettings, setSettings, modelsStatus, type ModelsStatus } from "$lib/models";
  import { applyTheme } from "$lib/theme";
  import { i18n, t } from "$lib/i18n/index.svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import {
    checkUpdate,
    applyUpdate,
    updateDismissed,
    dismissUpdate,
    type UpdateInfo,
    type UpdateProgress,
  } from "$lib/update";
  import { AI_TOOLS_GUIDE_ID } from "$lib/onboarding";
  import ContextGuide from "$lib/ContextGuide.svelte";
  import MiniPlayer from "$lib/MiniPlayer.svelte";
  import { playback, shouldShowMiniPlayer, startPlaybackSubscriptions } from "$lib/playback.svelte";

  let { children } = $props();

  // 迷你播放浮层的显示判定复用同一个纯函数(与 MiniPlayer.svelte 内部判定同源),
  // 避免这里再造一份、和浮层实际是否渲染不同步——那样安全区可能在浮层没显示时白留一块空白,
  // 或者浮层显示了但没留出安全区。
  const showMiniPlayer = $derived(shouldShowMiniPlayer(playback.session?.noteId ?? null, $page.url.pathname));

  // 托盘「停止播放」项的出现条件:有播放会话就有得停。托盘常驻下关掉主窗口后音频
  // 继续播(与音乐 App 一致),那一刻托盘是唯一能停它的地方。判定与浮层同源——都看
  // 会话,不看内核是否装载(进笔记页就自动装载,拿装载判会让只看过没播过的笔记也
  // 冒出「停止播放」)。注意条件比浮层少一条 pathname:在笔记本页上浮层要让位给完整
  // 播放器,托盘却仍该能停(窗口都关了,谈不上"页面里有播放器")。
  // 派在 layout 而非浮层组件:浮层在自己笔记页上不渲染,挂在它身上会漏掉那一整段。
  const playbackActive = $derived(playback.session !== null);
  $effect(() => {
    void invoke("set_playback_active", { active: playbackActive }).catch(() => {});
  });

  // 升级提示放全局布局:app 启动落地页可能是笔记详情/录制/空态任一(见根路由重定向),
  // 布局却必挂载且跨路由常驻——查一次、有新版且未忽略就在内容区顶部出可关闭横幅。
  let update = $state<UpdateInfo | null>(null);
  // 一键更新态:进行中按钮换成锻造进度条;失败退回「打开发布页」手动路径(文案联动)。
  let updating = $state(false);
  // null = 尚无进度事件(刚起步/总长未知),进度条走往复扫描态。
  let updateProg = $state<UpdateProgress | null>(null);
  let updateFailed = $state(false);
  // 安装成功即 relaunch:录音/收尾进行中点更新会把识别尾包和终稿截在半路,
  // 静默丢数据——录音期间禁点,结束后再更新。
  const updateBlocked = $derived(recording.isLive || recording.stopping);
  async function openUpdate() {
    if (!update) return;
    if (updateFailed) {
      // 一键失败后本按钮降级为手动路径,绝不堵死更新。
      openUrl(update.url);
      return;
    }
    if (updateBlocked) return;
    updating = true;
    updateProg = null;
    try {
      const r = await applyUpdate((p) => (updateProg = p));
      if (r === "none") updateFailed = true; // latest.json 未就绪等,退手动
    } catch {
      updateFailed = true;
    } finally {
      updating = false;
    }
  }
  function dismissUpdateBanner() {
    if (update) dismissUpdate(update.latest);
    update = null;
  }

  // 基础初始化由 onboarded 记账；每个新功能由 completed_guides 中的独立 ID 记账。
  // 因此旧用户可以看到新功能引导，同时不会重复经历模型下载。

  let welcomeStatus = $state<ModelsStatus | null>(null);
  async function checkOnboarding() {
    try {
      let s = await getSettings();
      const m = await modelsStatus();
      if (!s.onboarded && m.recording_ready) {
        s = { ...s, onboarded: true };
        await setSettings(s);
      }
      const needsBaseSetup = !s.onboarded;
      const needsAiGuide = !s.completed_guides.includes(AI_TOOLS_GUIDE_ID);
      if (needsBaseSetup) {
        welcomeStatus = m;
      } else if (needsAiGuide && $page.url.searchParams.get("guide") !== AI_TOOLS_GUIDE_ID) {
        // 功能教学必须发生在真实操作页；只把用户导航到对应功能并启动上下文引导。
        goto(`/ai?guide=${AI_TOOLS_GUIDE_ID}`);
      }
    } catch {
      /* 静默:见上 */
    }
  }
  function onWelcomeDone(target: "/record" | "/settings" | "/ai" | "/ai?guide=ai-tools-v1") {
    welcomeStatus = null;
    goto(target);
  }

  // 整理收件箱全局驱动:挂载即跑一次(应用启动),之后随 peopleVersion(录制停止
  // /人物增删改)重算——不锁在人物页签,自动归并不等用户逛到那儿才发生。
  $effect(() => {
    void recording.peopleVersion;
    void tidy.refresh();
  });

  // 位置事件的全局订阅:放这里而不是 AudioPlayer 里——组件一卸载订阅就没了,
  // 迷你条的进度会僵住。
  $effect(() => startPlaybackSubscriptions());

  onMount(() => {
    recording.init();
    // identify(P2a)完成即刷新收件箱:身份建议卡在 Aing 结束后自动出现,
    // 不等下一次 peopleVersion 变化。layout 常驻,不必解绑。
    void listen("identify_done", () => void tidy.refresh());
    // 启动即按已保存设置切主题与 UI 语言;取不到设置(如首启动/IPC 失败)时静默放弃——
    // 主题保持默认(等价跟随系统),语言保持 i18n 默认(zh,与历史行为一致)
    getSettings()
      .then((s) => {
        applyTheme(s.theme);
        i18n.setChoice(s.ui_lang);
      })
      .catch(() => {});
    checkOnboarding();
    checkUpdate()
      .then((u) => {
        if (u.has_update && !updateDismissed(u.latest)) update = u;
      })
      .catch(() => {}); // 断网/限流:静默不打扰
  });
</script>

<div class="shell">
  <Sidebar />
  <main class="main" class:with-orb-safearea={showMiniPlayer}>
    {#if update}
      <div class="update-banner">
        <span class="upd-dot"></span>{t("shell.update.found", { latest: update.latest, current: update.current })}
        {#if updating}
          <!-- 锻造激光进度:确定进度时熔金填充+白热激光头,总长未知/安装期退化为光束往复扫描 -->
          <span
            class="forge"
            role="progressbar"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={updateProg?.pct ?? undefined}
            aria-label={updateProg?.label ?? t("shell.update.updating")}
          >
            <span class="forge-track" class:scan={updateProg?.pct == null}>
              <span class="forge-fill" style:width={updateProg?.pct != null ? `${updateProg.pct}%` : undefined}>
                <span class="forge-head"></span>
              </span>
            </span>
            <span class="forge-label">{updateProg?.label ?? t("shell.update.updating")}</span>
          </span>
        {:else}
          <button
            class="link"
            onclick={openUpdate}
            disabled={!updateFailed && updateBlocked}
            title={!updateFailed && updateBlocked ? t("shell.update.blockedTitle") : undefined}
          >
            {updateFailed ? t("shell.update.openRelease") : t("shell.update.oneClick")}
          </button>
        {/if}
        <!-- 更新中禁「知道了」:横幅一收,下载安装仍在后台跑,几分钟后应用
             无预警重启,用户会以为闪退。 -->
        <button class="link" onclick={dismissUpdateBanner} disabled={updating}>{t("shell.update.dismiss")}</button>
      </div>
    {/if}
    {@render children()}
  </main>
</div>
{#if welcomeStatus}
  <WelcomeOverlay status={welcomeStatus} onDone={onWelcomeDone} />
{/if}
<ContextGuide />
<MiniPlayer />

<style>
  :global(body) {
    margin: 0;
    background: var(--canvas);
    color: var(--ink);
  }
  .shell {
    display: flex;
    height: 100vh;
    font-family: -apple-system, system-ui, sans-serif;
  }
  .main {
    flex: 1;
    overflow-y: auto;
    min-width: 0;
  }
  /* 迷你播放浮层显示时才留底部安全区,不显示时不留——否则会凭空多出一块空白。
     高度 = 浮层直径 + 上下各一份留白,让可滚动页面的最后一行能完全滚到浮层上方
     而不是被压在下面。--orb-size/--orb-inset 与 MiniPlayer.svelte 的 .orb 共用同一份
     token(定义在 app.css),避免尺寸各自硬编码后互相漂移。
     注:不用 bind:clientHeight 量浮层实高再回填——浮层是固定常量尺寸的圆,量了也白量,
     还会在挂载瞬间有一帧安全区缺失/多余的闪烁。
     这段留白对 graph 页那种「height:100% + 自己 overflow」的内部滚动页**同样有效**:
     百分比高度按 .main 的内容盒解析,padding 一加,整棵子树跟着抬到浮层上方。
     2026-08-15 无头 Chromium 复刻真实类链量过:视口 800 高时页面内容底边 696px、
     浮层顶边 712px,不相交(#108/#109 记的"内部自管理滚动页面帮不上忙"是误判)。
     真正吃不到这段留白的只有 position:fixed / 按 100vh 定高的底部元素,全库目前没有。
     契约护栏见 src/lib/orbSafeArea.test.ts。 */
  .main.with-orb-safearea {
    padding-bottom: calc(var(--orb-size) + var(--orb-inset) * 2);
  }
  /* 全局升级横幅:内容区顶部,与页面内边距对齐(各页 container 多为 1.5rem 内边距) */
  .update-banner {
    display: flex;
    /* 窄窗允许折行:定宽进度条+不折行文案会把「知道了」挤出可视区(codex P2) */
    flex-wrap: wrap;
    align-items: center;
    gap: 0.4em;
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    color: var(--warning-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.9rem;
    margin: 1.25rem 1.5rem 0;
    font-size: 0.95rem;
  }
  .upd-dot {
    display: inline-block;
    width: 7px;
    height: 7px;
    border-radius: var(--radius-full);
    background: var(--accent);
    margin-right: 0.2em;
    flex: none;
  }
  .update-banner .link {
    background: none;
    border: none;
    color: var(--accent);
    text-decoration: underline;
    cursor: pointer;
    padding: 0 0.2em;
    font-size: inherit;
  }
  /* —— 锻造激光进度条:更新=把新版本锻出来 —— */
  .forge {
    display: inline-flex;
    align-items: center;
    gap: 0.55em;
    margin: 0 0.2em;
    /* 随横幅弹性伸缩,窄窗时轨道先让位(min-width 兜底可辨认) */
    flex: 1 1 auto;
    min-width: 0;
  }
  .forge-track {
    position: relative;
    flex: 1 1 90px;
    max-width: 150px;
    min-width: 60px;
    height: 7px;
    border-radius: var(--radius-full);
    background: color-mix(in srgb, var(--warning-ink) 18%, transparent);
    box-shadow: inset 0 1px 3px rgb(0 0 0 / 35%);
    overflow: hidden;
  }
  .forge-fill {
    position: absolute;
    top: 0;
    bottom: 0;
    left: 0;
    width: 0;
    border-radius: inherit;
    /* 熔金渐变:尾部暗红余温 → 前端白热 */
    background: linear-gradient(90deg, #b3400c, #ff6a00 45%, #ffb84d 80%, #ffe9c2);
    box-shadow: 0 0 10px rgb(255 122 0 / 75%);
    transition: width 0.25s ease;
  }
  .forge-fill::after {
    /* 熔流高光沿条身反复掠过 */
    content: "";
    position: absolute;
    inset: 0;
    border-radius: inherit;
    background: linear-gradient(100deg, transparent 25%, rgb(255 255 255 / 55%) 50%, transparent 75%);
    background-size: 220% 100%;
    animation: forge-shimmer 1.3s linear infinite;
  }
  .forge-head {
    /* 激光锻造头:白热光点,火花式高频闪烁 */
    position: absolute;
    right: -2px;
    top: 50%;
    width: 13px;
    height: 13px;
    transform: translateY(-50%);
    border-radius: var(--radius-full);
    background: radial-gradient(circle, #fff 0%, #ffd27a 45%, rgb(255 170 60 / 0%) 72%);
    filter: drop-shadow(0 0 5px #ffae00) drop-shadow(0 0 12px rgb(255 110 0 / 80%));
    animation: forge-flicker 0.14s steps(2, end) infinite;
  }
  /* 进度未知(无 Content-Length)与安装期:一束激光往复扫描 */
  .forge-track.scan .forge-fill {
    width: 34%;
    transition: none;
    animation: forge-scan 1.1s ease-in-out infinite alternate;
  }
  .forge-label {
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }
  @keyframes forge-shimmer {
    from {
      background-position: 220% 0;
    }
    to {
      background-position: -120% 0;
    }
  }
  @keyframes forge-flicker {
    0% {
      opacity: 1;
    }
    50% {
      opacity: 0.55;
    }
    100% {
      opacity: 0.9;
    }
  }
  @keyframes forge-scan {
    from {
      left: -4%;
    }
    to {
      left: 70%;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .forge-fill::after,
    .forge-head {
      animation: none;
    }
    .forge-track.scan .forge-fill {
      animation-duration: 2.4s;
    }
  }
</style>
