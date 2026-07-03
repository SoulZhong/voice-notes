<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { onPartial, onStatus, onFinal, onStorage, type Source, type SystemAudio, type StatusEvent } from "$lib/events";

  type Line = { source: Source; text: string };

  let status = $state("idle");
  let systemAudio = $state<SystemAudio>("");
  let finals = $state<Line[]>([]);
  let partialMic = $state("");
  let partialSystem = $state("");
  let storageDegraded = $state(false);

  const label = (s: Source) => (s === "mic" ? "我" : "对方");

  onMount(() => {
    const u1 = onPartial((e) => {
      if (e.source === "mic") partialMic = e.text;
      else partialSystem = e.text;
    });
    const u2 = onStatus((e) => {
      status = e.state;
      systemAudio = e.system_audio;
      if (e.state === "recording") {
        finals = [];
        partialMic = "";
        partialSystem = "";
        storageDegraded = false;
      } else if (e.state === "stopped" || e.state.startsWith("error:")) {
        partialMic = "";
        partialSystem = "";
        storageDegraded = false;
        if (e.state === "stopped" && e.note_id) {
          goto(`/notes/${e.note_id}`);
        }
      }
    });
    const u3 = onFinal((e) => {
      if (e.text.trim()) finals = [...finals, { source: e.source, text: e.text }];
      if (e.source === "mic") partialMic = "";
      else partialSystem = "";
    });
    const u4 = onStorage((e) => {
      storageDegraded = e.state === "degraded";
    });
    // 重挂载时事件非粘性：主动查询一次当前录制状态，避免离开 /record 再返回
    // 时 status 停在初始的 "idle" 导致停止按钮永久 disabled。返回 idle 时不覆盖
    // 任何状态，避免与几乎同时到达的真实 "status" 事件竞争。
    invoke<StatusEvent>("recording_status").then((s) => {
      if (s.state === "recording") {
        status = s.state;
        systemAudio = s.system_audio;
      }
    });
    return () => {
      u1.then((f) => f());
      u2.then((f) => f());
      u3.then((f) => f());
      u4.then((f) => f());
    };
  });

  async function start() {
    try {
      await invoke("start_recording");
    } catch (err) {
      status = `error: ${err}`;
    }
  }
  async function stop() {
    await invoke("stop_recording");
  }
  function isError(s: string) {
    return s.startsWith("error:");
  }
  async function openScreenRecordingSettings() {
    await openUrl(
      "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
    );
  }
</script>

<main class="container">
  <p><a href="/">← 笔记列表</a></p>
  <h1>实时转写</h1>
  <div class="row">
    <button onclick={start} disabled={status === "recording"}>开始录音</button>
    <button onclick={stop} disabled={status !== "recording"}>停止</button>
    <span class="status" class:error={isError(status)}>状态：{status}</span>
  </div>

  {#if status === "recording" && systemAudio !== "on" && systemAudio !== ""}
    <div class="banner">
      系统声音不可用（未授权屏幕录制）。仅麦克风在录。
      <button class="link" onclick={openScreenRecordingSettings}>打开系统设置</button>
      <span class="hint">授权后重新开录生效。</span>
    </div>
  {/if}

  {#if storageDegraded}
    <div class="banner">落盘异常：内容暂存内存并自动重试，请检查磁盘空间。录制不受影响。</div>
  {/if}

  <div class="transcript">
    {#each finals as line}
      <p class="final">
        <span class="badge" class:mic={line.source === "mic"} class:system={line.source === "system"}>
          {label(line.source)}
        </span>
        {line.text}
      </p>
    {/each}
    {#if partialMic}
      <p class="partial"><span class="badge mic">我</span>{partialMic}</p>
    {/if}
    {#if partialSystem}
      <p class="partial"><span class="badge system">对方</span>{partialSystem}</p>
    {/if}
    {#if finals.length === 0 && !partialMic && !partialSystem}
      <p class="hint">（开始说话…）</p>
    {/if}
  </div>
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
  }

  h1 {
    margin-bottom: 0.5rem;
  }

  .row {
    display: flex;
    gap: 0.75rem;
    align-items: center;
    margin: 1rem 0;
  }

  button {
    border-radius: 8px;
    border: 1px solid transparent;
    padding: 0.5em 1.2em;
    font-size: 1em;
    font-weight: 500;
    font-family: inherit;
    cursor: pointer;
    background-color: #ffffff;
    box-shadow: 0 2px 2px rgba(0, 0, 0, 0.2);
    transition: border-color 0.25s;
  }

  button:hover:not(:disabled) {
    border-color: #396cd8;
  }

  button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .status {
    color: #666;
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
    font-family: inherit;
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

  @media (prefers-color-scheme: dark) {
    button {
      color: #ffffff;
      background-color: #0f0f0f98;
    }

    button:active:not(:disabled) {
      background-color: #0f0f0f69;
    }

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
    .banner { background: #3a2e18; border-color: #6b5426; color: #e8c88a; }
    .banner .hint { color: #c9a866; }
  }
</style>
