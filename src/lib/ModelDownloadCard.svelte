<script lang="ts">
  import { onMount } from "svelte";
  import {
    downloadModels,
    cancelModelsDownload,
    getSettings,
    setSettings,
    onModelDownload,
    type ModelsStatus,
    type Settings,
    type ModelDownloadEvent,
  } from "$lib/models";

  let {
    status,
    compact = false,
    onComplete,
  }: { status: ModelsStatus; compact?: boolean; onComplete: () => void } = $props();

  const missing = $derived(status.artifacts.filter((a) => !a.present));
  const totalMb = $derived(missing.reduce((s, a) => s + a.approx_mb, 0));

  let downloading = $state(false);
  let error = $state("");
  let cancelled = $state(false);
  /** 各工件进度：received/total 字节 + phase。 */
  let prog = $state<Record<string, { received: number; total: number; phase: string }>>({});
  let settings = $state<Settings | null>(null);

  onMount(() => {
    getSettings().then((s) => (settings = s)).catch(() => {});
    // 事件监听随组件生命周期注册/解绑（下载跨页面继续，回到本页重新拿到进度流）。
    const un = onModelDownload(handle);
    return () => {
      un.then((f) => f());
    };
  });

  function handle(e: ModelDownloadEvent) {
    if (e.artifact === "all" && e.phase === "done") {
      downloading = false;
      onComplete();
      return;
    }
    if (e.phase === "error") {
      downloading = false;
      error = e.message;
      return;
    }
    if (e.phase === "cancelled") {
      downloading = false;
      cancelled = true;
      return;
    }
    prog = { ...prog, [e.artifact]: { received: e.received_bytes, total: e.total_bytes, phase: e.phase } };
    if (e.phase === "done") onComplete(); // 单工件完成：刷新 present 态
  }

  async function start() {
    error = "";
    cancelled = false;
    downloading = true;
    try {
      await downloadModels();
    } catch (e) {
      // "下载已在进行中" 不算错：保持 downloading 态继续收进度事件。
      if (!String(e).includes("已在进行中")) {
        downloading = false;
        error = String(e);
      }
    }
  }

  async function toggleMirror() {
    if (!settings) return;
    settings = { ...settings, mirror_enabled: !settings.mirror_enabled };
    await setSettings(settings);
  }
  async function savePrefix() {
    if (settings) await setSettings(settings);
  }

  const pct = (p: { received: number; total: number }) =>
    p.total > 0 ? Math.min(100, Math.floor((p.received / p.total) * 100)) : 0;
  const mb = (n: number) => (n / 1024 / 1024).toFixed(0);
  const phaseText: Record<string, string> = {
    downloading: "下载中",
    verifying: "校验中",
    extracting: "解压中",
    done: "完成",
  };
</script>

<div class="card" class:compact>
  {#if compact}
    <span>说话人区分需补下声纹模型（约 {totalMb}MB）。</span>
  {:else}
    <h2>下载语音模型</h2>
    <p class="desc">首次使用需下载识别模型（共约 {totalMb}MB），全程本地运行、不上传任何音频。</p>
  {/if}

  {#each missing as a (a.id)}
    <div class="row">
      <span class="label">{a.label} · 约 {a.approx_mb}MB</span>
      {#if prog[a.id]}
        <span class="phase">
          {phaseText[prog[a.id].phase] ?? prog[a.id].phase}
          {#if prog[a.id].phase === "downloading" && prog[a.id].total > 0}
            {mb(prog[a.id].received)}/{mb(prog[a.id].total)}MB
          {/if}
        </span>
        <div class="bar"><div class="fill" style="width:{pct(prog[a.id])}%"></div></div>
      {/if}
    </div>
  {/each}

  {#if error}
    <div class="error">下载失败：{error}（已下载部分已保留，重试将续传）</div>
  {/if}
  {#if cancelled}
    <div class="hint">已暂停下载，已下载部分保留，可随时继续。</div>
  {/if}

  <div class="actions">
    {#if downloading}
      <button onclick={() => cancelModelsDownload()}>暂停下载</button>
    {:else}
      <button class="primary" onclick={start}>{error || cancelled ? "继续下载" : "下载模型"}</button>
    {/if}
    {#if settings && !compact}
      <label class="mirror">
        <input type="checkbox" checked={settings.mirror_enabled} onchange={toggleMirror} />
        使用镜像加速（国内网络推荐）
      </label>
      {#if settings.mirror_enabled}
        <input class="prefix" bind:value={settings.mirror_prefix} onblur={savePrefix} placeholder="镜像前缀，如 https://ghproxy.net/" />
      {/if}
    {/if}
  </div>
</div>

<style>
  .card {
    background: #f5f5f7;
    border-radius: 10px;
    padding: 1rem 1.2rem;
    margin: 0.5rem 0 1rem;
  }
  .card.compact {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.6rem;
    background: #fff4e5;
    border: 1px solid #f0c98a;
    color: #8a5a00;
    font-size: 0.95rem;
  }
  h2 { margin: 0 0 0.25rem; font-size: 1.1rem; }
  .desc { color: #666; margin: 0 0 0.75rem; font-size: 0.9rem; }
  .row { margin: 0.4rem 0; }
  .label { font-size: 0.9rem; }
  .phase { color: #666; font-size: 0.8rem; margin-left: 0.5em; }
  .bar { height: 6px; background: #e0e0e3; border-radius: 3px; margin-top: 0.25rem; overflow: hidden; }
  .fill { height: 100%; background: #396cd8; transition: width 0.3s; }
  .actions { display: flex; align-items: center; gap: 0.8rem; margin-top: 0.8rem; flex-wrap: wrap; }
  button { border-radius: 8px; border: 1px solid #ccc; padding: 0.45em 1.1em; cursor: pointer; background: #fff; }
  button.primary { background: #396cd8; color: #fff; border-color: transparent; font-weight: 600; }
  .mirror { font-size: 0.85rem; display: flex; align-items: center; gap: 0.3em; }
  .prefix { flex: 1; min-width: 14rem; padding: 0.3em 0.5em; border-radius: 6px; border: 1px solid #ccc; font-size: 0.85rem; }
  .error { color: #c0392b; font-size: 0.9rem; margin-top: 0.5rem; }
  .hint { color: #8a5a00; font-size: 0.9rem; margin-top: 0.5rem; }
  @media (prefers-color-scheme: dark) {
    .card { background: #2a2a2a; }
    .card.compact { background: #3a2e18; border-color: #6b5426; color: #e8c88a; }
    .desc, .phase { color: #aaa; }
    .bar { background: #444; }
    button { background: #0f0f0f98; color: #fff; border-color: #555; }
    .prefix { background: #2a2a2a; color: #f0f0f0; border-color: #555; }
  }
</style>
