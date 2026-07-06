<script lang="ts">
  import "../app.css";
  import { onMount } from "svelte";
  import Sidebar from "$lib/Sidebar.svelte";
  import { recording } from "$lib/recording.svelte";
  import { getSettings } from "$lib/models";
  import { applyTheme } from "$lib/theme";

  let { children } = $props();

  onMount(() => {
    recording.init();
    // 启动即按已保存设置切主题;取不到设置(如首启动/IPC 失败)时静默放弃——
    // 根元素 color-scheme 保持默认,等价于跟随系统,不需要额外兜底分支
    getSettings()
      .then((s) => applyTheme(s.theme))
      .catch(() => {});
  });
</script>

<div class="shell">
  <Sidebar />
  <main class="main">
    {@render children()}
  </main>
</div>

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
