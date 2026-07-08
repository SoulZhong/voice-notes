<script lang="ts">
  import { onMount } from "svelte";
  import {
    downloadModels,
    cancelModelsDownload,
    onModelDownload,
    type ModelsStatus,
    type ModelDownloadEvent,
  } from "$lib/models";

  let {
    status,
    compact = false,
    onComplete,
    primaryLabel = "下载模型",
  }: { status: ModelsStatus; compact?: boolean; onComplete: () => void; primaryLabel?: string } = $props();

  const missing = $derived(
    status.artifacts.filter((a) => !a.present && (a.required_for_recording || a.id === "speaker")),
  );
  const totalMb = $derived(missing.reduce((s, a) => s + a.approx_mb, 0));

  let downloading = $state(false);
  let error = $state("");
  let cancelled = $state(false);
  /** 各工件进度：received/total 字节 + phase。 */
  let prog = $state<Record<string, { received: number; total: number; phase: string }>>({});

  onMount(() => {
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
    <span>区分「谁在说话」还需补一个小模型（约 {totalMb}MB）。</span>
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
      <button class="primary" onclick={start}>{error || cancelled ? "继续下载" : primaryLabel}</button>
    {/if}
    {#if !compact}
      <span class="note">下载镜像可在设置页配置。</span>
    {/if}
  </div>
</div>

<style>
  /* download-card：大卡 surface 底 + rounded-xl */
  .card {
    background: var(--surface);
    border-radius: var(--radius-xl);
    padding: 1rem 1.2rem;
    margin: 0.5rem 0 1rem;
  }
  /* compact：改用 banner 形态（warning 色系），只在识别模型已就绪、仅缺声纹模型时出现 */
  .card.compact {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.6rem;
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    color: var(--warning-ink);
    border-radius: var(--radius-lg);
    font-size: 0.95rem;
  }
  h2 { margin: 0 0 0.25rem; font-size: 1.05rem; font-weight: 500; }
  .desc { color: var(--ink-secondary); margin: 0 0 0.75rem; font-size: 0.9rem; }
  .row { margin: 0.4rem 0; }
  .label { font-size: 0.9rem; }
  .phase { color: var(--ink-secondary); font-size: 0.8rem; margin-left: 0.5em; }
  /* 进度条：轨 hairline、填充 accent、rounded-full */
  .bar { height: 6px; background: var(--hairline); border-radius: var(--radius-full); margin-top: 0.25rem; overflow: hidden; }
  .fill { height: 100%; background: var(--accent); transition: width 0.3s; }
  .actions { display: flex; align-items: center; gap: 0.8rem; margin-top: 0.8rem; flex-wrap: wrap; }
  /* button-secondary（暂停下载/继续下载默认态） */
  button {
    border-radius: var(--radius-md);
    border: 1px solid var(--hairline-strong);
    padding: 0.45em 1.1em;
    font-size: 0.9rem;
    font-weight: 500;
    cursor: pointer;
    background: transparent;
    color: var(--ink);
  }
  button:hover { background: var(--surface-soft); }
  /* button-primary：下载模型是本卡唯一主动作 */
  button.primary { background: var(--primary); color: var(--on-primary); border-color: transparent; border-radius: var(--radius-full); font-weight: 500; }
  button.primary:hover { background: var(--primary-pressed); }
  .note { font-size: 0.85rem; color: var(--ink-faint); }
  .error { color: var(--danger); font-size: 0.9rem; margin-top: 0.5rem; }
  .hint { color: var(--warning-ink); font-size: 0.9rem; margin-top: 0.5rem; }
</style>
