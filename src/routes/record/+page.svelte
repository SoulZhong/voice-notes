<script lang="ts">
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { recording } from "$lib/recording.svelte";
  import type { Source } from "$lib/events";

  const label = (s: Source) => (s === "mic" ? "我" : "对方");

  function isError(s: string) {
    return s.startsWith("error:");
  }
  async function openScreenRecordingSettings() {
    await openUrl(
      "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
    );
  }
</script>

<div class="container">
  <h1>实时转写</h1>
  <p class="status" class:error={isError(recording.status)}>状态：{recording.status}</p>

  {#if recording.isRecording && recording.systemAudio !== "on" && recording.systemAudio !== ""}
    <div class="banner">
      系统声音不可用（未授权屏幕录制）。仅麦克风在录。
      <button class="link" onclick={openScreenRecordingSettings}>打开系统设置</button>
      <span class="hint">授权后重新开录生效。</span>
    </div>
  {/if}

  {#if recording.storageDegraded}
    <div class="banner">落盘异常：内容暂存内存并自动重试，请检查磁盘空间。录制不受影响。</div>
  {/if}

  <div class="transcript">
    {#each recording.finals as line}
      <p class="final">
        <span class="badge" class:mic={line.source === "mic"} class:system={line.source === "system"}>
          {label(line.source)}
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
</div>

<style>
  .container {
    padding: 1.5rem;
  }

  h1 {
    margin: 0 0 0.25rem;
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
