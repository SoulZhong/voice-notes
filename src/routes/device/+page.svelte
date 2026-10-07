<script lang="ts">
  // 设备一级页(用户 2026-10-07:设备听写藏在设置里太隐蔽):连接、权限、识别引擎、
  // 保存与 AI 读取开关都在这里;听写笔记列在左侧栏。
  import { onMount } from "svelte";
  import { t } from "$lib/i18n/index.svelte";
  import DeviceSettings from "$lib/DeviceSettings.svelte";
  import { getSettings, setSettings, type Settings } from "$lib/models";

  let settings = $state<Settings | null>(null);
  let error = $state("");

  onMount(async () => {
    settings = await getSettings().catch((e) => {
      error = t("common.loadFailed", { e });
      return null;
    });
  });

  async function save(mut: (s: Settings) => void) {
    error = "";
    try {
      const fresh = await getSettings();
      mut(fresh);
      await setSettings(fresh);
      settings = fresh;
    } catch (e) {
      error = t("common.saveFailed", { e });
      settings = await getSettings().catch(() => settings);
    }
  }
</script>

<main class="page">
  <h1>{t("device.page.title")}</h1>
  {#if error}
    <div class="banner">{error}</div>
  {/if}
  <DeviceSettings {settings} {save} />
</main>

<style>
  .page {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 46rem;
  }
  h1 {
    margin: 0 0 0.3rem;
  }
  .banner {
    padding: 0.5rem 0.8rem;
    border-radius: 6px;
    background: var(--danger-tint, transparent);
    color: var(--danger);
    font-size: 0.85rem;
  }
</style>
