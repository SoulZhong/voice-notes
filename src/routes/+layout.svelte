<script lang="ts">
  import "../app.css";
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import Sidebar from "$lib/Sidebar.svelte";
  import WelcomeOverlay from "$lib/WelcomeOverlay.svelte";
  import { recording } from "$lib/recording.svelte";
  import { getSettings, setSettings, modelsStatus, type ModelsStatus } from "$lib/models";
  import { applyTheme } from "$lib/theme";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { checkUpdate, applyUpdate, updateProgressLabel, updateDismissed, dismissUpdate, type UpdateInfo } from "$lib/update";
  import { AI_TOOLS_GUIDE_ID } from "$lib/onboarding";
  import ContextGuide from "$lib/ContextGuide.svelte";

  let { children } = $props();

  // 升级提示放全局布局:app 启动落地页可能是笔记详情/录制/空态任一(见根路由重定向),
  // 布局却必挂载且跨路由常驻——查一次、有新版且未忽略就在内容区顶部出可关闭横幅。
  let update = $state<UpdateInfo | null>(null);
  // 一键更新态:进行中禁点、按钮显进度;失败退回「打开发布页」手动路径(文案联动)。
  let updating = $state(false);
  let updatingLabel = $state("更新中…");
  let updateFailed = $state(false);
  async function openUpdate() {
    if (!update) return;
    if (updateFailed) {
      // 一键失败后本按钮降级为手动路径,绝不堵死更新。
      openUrl(update.url);
      return;
    }
    updating = true;
    updatingLabel = "更新中…";
    try {
      const r = await applyUpdate((d, t) => (updatingLabel = updateProgressLabel(d, t)));
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

  onMount(() => {
    recording.init();
    // 启动即按已保存设置切主题;取不到设置(如首启动/IPC 失败)时静默放弃——
    // 根元素 color-scheme 保持默认,等价于跟随系统,不需要额外兜底分支
    getSettings()
      .then((s) => applyTheme(s.theme))
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
  <main class="main">
    {#if update}
      <div class="update-banner">
        <span class="upd-dot"></span>发现新版 v{update.latest}(当前 v{update.current})
        <button class="link" onclick={openUpdate} disabled={updating}>
          {updating ? updatingLabel : updateFailed ? "打开发布页" : "一键更新"}
        </button>
        <button class="link" onclick={dismissUpdateBanner}>知道了</button>
      </div>
    {/if}
    {@render children()}
  </main>
</div>
{#if welcomeStatus}
  <WelcomeOverlay status={welcomeStatus} onDone={onWelcomeDone} />
{/if}
<ContextGuide />

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
  /* 全局升级横幅:内容区顶部,与页面内边距对齐(各页 container 多为 1.5rem 内边距) */
  .update-banner {
    display: flex;
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
</style>
