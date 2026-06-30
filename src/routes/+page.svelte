<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";
  import { onPartial, onStatus } from "$lib/events";

  let status = $state("idle");
  let transcript = $state("");

  onMount(() => {
    const u1 = onPartial((e) => { transcript = e.text; });
    const u2 = onStatus((e) => { status = e.state; });
    return () => {
      u1.then((f) => f());
      u2.then((f) => f());
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
</script>

<main class="container">
  <h1>实时转写（骨架）</h1>
  <div class="row">
    <button onclick={start} disabled={status === "recording"}>开始录音</button>
    <button onclick={stop} disabled={status !== "recording"}>停止</button>
    <span class="status" class:error={isError(status)}>状态：{status}</span>
  </div>
  <pre class="transcript">{transcript || "（开始说话…）"}</pre>
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
    white-space: pre-wrap;
    min-height: 8rem;
    background: #f5f5f7;
    border-radius: 8px;
    padding: 1rem;
    font-size: 1.1rem;
    line-height: 1.6;
    font-family: inherit;
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
      color: #f0f0f0;
    }
  }
</style>
