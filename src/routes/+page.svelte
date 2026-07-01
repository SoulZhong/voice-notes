<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";
  import { onPartial, onStatus, onFinal } from "$lib/events";

  let status = $state("idle");
  let finals = $state<string[]>([]);
  let partial = $state("");

  onMount(() => {
    const u1 = onPartial((e) => { partial = e.text; });
    const u2 = onStatus((e) => {
      status = e.state;
      if (e.state === "recording") {
        finals = [];
        partial = "";
      } else if (e.state === "stopped" || e.state.startsWith("error:")) {
        // 结束/出错时清掉未定稿的临时行，避免半句灰字残留。
        partial = "";
      }
    });
    const u3 = onFinal((e) => {
      // 跳过空识别结果，避免追加空白行。
      if (e.text.trim()) {
        finals = [...finals, e.text];
      }
      partial = "";
    });
    return () => {
      u1.then((f) => f());
      u2.then((f) => f());
      u3.then((f) => f());
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
  <div class="transcript">
    {#each finals as line}
      <p class="final">{line}</p>
    {/each}
    {#if partial}
      <p class="partial">{partial}</p>
    {/if}
    {#if finals.length === 0 && !partial}
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
</style>
