<script lang="ts">
  import { onMount } from "svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { recording } from "$lib/recording.svelte";
  import { speakerLabel, speakerColor } from "$lib/notes";
  import SpeakerChips from "$lib/SpeakerChips.svelte";
  import { modelsStatus, type ModelsStatus } from "$lib/models";
  import ModelDownloadCard from "$lib/ModelDownloadCard.svelte";
  import { formatTs } from "$lib/notes";

  let models = $state<ModelsStatus | null>(null);
  async function refreshModels() {
    try {
      models = await modelsStatus();
    } catch {
      /* 查询失败按就绪处理，不挡老用户 */
    }
  }
  onMount(refreshModels);

  function isError(s: string) {
    return s.startsWith("error:");
  }
  async function openScreenRecordingSettings() {
    await openUrl(
      "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
    );
  }

  async function startRecording() {
    await recording.start(); // 已在录制页，无需跳转
  }
  const levelPct = $derived.by(() => {
    if (!recording.isLive || recording.level <= 0) return 0;
    const db = 20 * Math.log10(recording.level);
    return Math.max(0, Math.min(100, ((db + 50) / 50) * 100)); // -50dBFS..0dBFS → 0..100%
  });
</script>

<div class="container">
  <h1>实时转写</h1>

  <!-- 单实例:compact 由 recording_ready 驱动。若拆成两个 if 分支,识别模型下完
       切小提示条时组件会销毁重建,进行中的下载进度/订阅状态全部清零。 -->
  {#if models && !(models.recording_ready && models.diarization_ready)}
    <ModelDownloadCard status={models} compact={models.recording_ready} onComplete={refreshModels} />
  {/if}

  {#if !models || models.recording_ready}
    <div class="controls">
      {#if !recording.isLive}
        <button class="ctl primary" disabled={recording.pending} onclick={startRecording}>● 开始录制</button>
      {:else}
        {#if recording.paused}
          <button class="ctl" disabled={recording.pending} onclick={() => recording.unpause()}>▶ 恢复</button>
        {:else}
          <button class="ctl" disabled={recording.pending} onclick={() => recording.pause()}>⏸ 暂停</button>
        {/if}
        <button class="ctl danger" disabled={recording.pending} onclick={() => recording.stop()}>■ 停止</button>
      {/if}
      <span class="timer" class:pausedTimer={recording.paused}>{formatTs(recording.elapsedMs)}</span>
      <div class="meter" title="麦克风电平"><div class="meter-fill" style="width:{levelPct}%"></div></div>
      {#if recording.paused}<span class="paused-tag">已暂停</span>{/if}
    </div>

    <p class="status" class:error={isError(recording.status)}>状态：{recording.status}</p>

    {#if recording.isLive && recording.systemAudio !== "on" && recording.systemAudio !== ""}
      <div class="banner">
        系统声音不可用（未授权屏幕录制）。仅麦克风在录。
        <button class="link" onclick={openScreenRecordingSettings}>打开系统设置</button>
        <span class="hint">授权后重新开录生效。</span>
      </div>
    {/if}

    {#if recording.isLive && recording.diarization === "unavailable"}
      <div class="banner">说话人区分不可用（声纹模型缺失）。转写与录音不受影响。</div>
    {/if}

    {#if recording.storageDegraded}
      <div class="banner">落盘异常：内容暂存内存并自动重试，请检查磁盘空间。录制不受影响。</div>
    {/if}

    <SpeakerChips speakers={recording.speakers} noteId={recording.noteId} editable={true} />

    <div class="transcript">
      {#each recording.finals as line}
        <p class="final">
          <span class="badge" style="background: {speakerColor(line.speaker, line.source)}">
            {speakerLabel(line.speaker, line.source, recording.speakers)}
          </span>
          {line.text}
        </p>
      {/each}
      {#if recording.partialMic}
        <p class="partial"><span class="badge mic">我</span>{recording.partialMic}</p>
      {/if}
      {#if recording.partialSystem}
        <p class="partial"><span class="badge system">对方</span>{recording.partialSystem}</p>
      {/if}
      {#if recording.finals.length === 0 && !recording.partialMic && !recording.partialSystem}
        <p class="hint">（开始说话…）</p>
      {/if}
    </div>
  {/if}
</div>

<style>
  .container {
    padding: 1.5rem;
  }

  h1 {
    margin: 0 0 0.25rem;
  }

  .controls {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    margin: 0 0 0.75rem;
  }
  .ctl {
    border-radius: 8px;
    border: 1px solid #ccc;
    padding: 0.45em 1.1em;
    font-weight: 600;
    cursor: pointer;
    background: #fff;
  }
  .ctl.primary { background: #396cd8; color: #fff; border-color: transparent; }
  .ctl.danger { background: #c0392b; color: #fff; border-color: transparent; }
  .timer {
    font-variant-numeric: tabular-nums;
    font-weight: 600;
    color: #444;
  }
  .timer.pausedTimer { color: #d88a39; }
  .meter {
    width: 120px;
    height: 8px;
    background: #e0e0e3;
    border-radius: 4px;
    overflow: hidden;
  }
  .meter-fill {
    height: 100%;
    background: #2e9e5b;
    transition: width 0.1s linear;
  }
  .paused-tag {
    background: #d88a39;
    color: #fff;
    font-size: 0.75em;
    font-weight: 600;
    border-radius: 6px;
    padding: 0.1em 0.5em;
  }

  .status {
    color: #666;
    margin: 0 0 1rem;
  }

  .status.error {
    color: #c0392b;
    font-weight: 600;
  }

  .transcript {
    min-height: 8rem;
    background: #f5f5f7;
    border-radius: 8px;
    padding: 1rem;
    font-size: 1.1rem;
    line-height: 1.6;
  }

  .transcript p {
    margin: 0 0 0.25rem 0;
  }

  .final {
    color: #1a1a1a;
  }

  .partial {
    color: #888;
    font-style: italic;
  }

  .hint {
    color: #aaa;
  }

  .badge {
    display: inline-block;
    min-width: 2.2em;
    text-align: center;
    font-size: 0.75em;
    font-weight: 600;
    border-radius: 6px;
    padding: 0.05em 0.4em;
    margin-right: 0.4em;
    color: #fff;
  }
  .badge.mic { background: #396cd8; }
  .badge.system { background: #2e9e5b; }

  .banner {
    background: #fff4e5;
    border: 1px solid #f0c98a;
    color: #8a5a00;
    border-radius: 8px;
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .banner .link {
    background: none;
    border: none;
    color: #396cd8;
    text-decoration: underline;
    cursor: pointer;
    padding: 0 0.2em;
    box-shadow: none;
    font-size: inherit;
  }
  .banner .hint { color: #a07a3a; }

  @media (prefers-color-scheme: dark) {
    .ctl { background: #0f0f0f98; color: #fff; border-color: #555; }
    .ctl.primary { background: #396cd8; }
    .ctl.danger { background: #c0392b; }
    .timer { color: #ccc; }
    .meter { background: #444; }
    .status {
      color: #aaa;
    }
    .transcript {
      background: #2a2a2a;
    }
    .final {
      color: #f0f0f0;
    }
    .partial {
      color: #888;
    }
    .hint {
      color: #555;
    }
    .banner { background: #3a2e18; border-color: #6b5426; color: #e8c88a; }
    .banner .hint { color: #c9a866; }
  }
</style>
