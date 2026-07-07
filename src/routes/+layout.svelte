<script lang="ts">
  import "../app.css";
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import Sidebar from "$lib/Sidebar.svelte";
  import WelcomeOverlay from "$lib/WelcomeOverlay.svelte";
  import { recording } from "$lib/recording.svelte";
  import { getSettings, setSettings, modelsStatus, type ModelsStatus } from "$lib/models";
  import { applyTheme } from "$lib/theme";

  let { children } = $props();

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
  });
</script>

<div class="shell">
  <Sidebar />
  <main class="main">
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
</style>
