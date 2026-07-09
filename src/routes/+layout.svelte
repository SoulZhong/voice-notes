<script lang="ts">
  import "../app.css";
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import Sidebar from "$lib/Sidebar.svelte";
  import WelcomeOverlay from "$lib/WelcomeOverlay.svelte";
  import { recording } from "$lib/recording.svelte";
  import { getSettings, setSettings, modelsStatus, type ModelsStatus } from "$lib/models";
  import { applyTheme } from "$lib/theme";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { checkUpdate, updateDismissed, dismissUpdate, type UpdateInfo } from "$lib/update";

  let { children } = $props();

  // 升级提示放全局布局:app 启动落地页可能是笔记详情/录制/空态任一(见根路由重定向),
  // 布局却必挂载且跨路由常驻——查一次、有新版且未忽略就在内容区顶部出可关闭横幅。
  let update = $state<UpdateInfo | null>(null);
  function openUpdate() {
    if (update) openUrl(update.url);
  }
  function dismissUpdateBanner() {
    if (update) dismissUpdate(update.latest);
    update = null;
  }

  // 首启引导:未 onboarded 且模型未就绪才弹欢迎层;模型已就绪(老用户升级出新字段)
  // 静默补 onboarded,不打扰。任何 IPC 失败都按"不弹"处理——引导是增强,不能挡主界面。
  let welcomeStatus = $state<ModelsStatus | null>(null);
  async function checkOnboarding() {
    try {
      const s = await getSettings();
      if (s.onboarded) return;
      const m = await modelsStatus();
      if (m.recording_ready) {
        await setSettings({ ...s, onboarded: true });
      } else {
        welcomeStatus = m;
      }
    } catch {
      /* 静默:见上 */
    }
  }
  function onWelcomeDone(target: "/record" | "/settings") {
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
        <button class="link" onclick={openUpdate}>查看更新</button>
        <button class="link" onclick={dismissUpdateBanner}>知道了</button>
      </div>
    {/if}
    {@render children()}
  </main>
</div>
{#if welcomeStatus}
  <WelcomeOverlay status={welcomeStatus} onDone={onWelcomeDone} />
{/if}

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
