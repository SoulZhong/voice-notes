<script lang="ts">
  // 首启欢迎层(welcome-overlay,见 DESIGN.md):模型未就绪且未完成引导时全屏覆盖。
  // 下载流整体复用 ModelDownloadCard(进度/续传/暂停均沿用),本组件只负责
  // 欢迎文案、完成后的收尾(置 onboarded + 跳转)与「高级设置」逃生口。
  import ModelDownloadCard from "$lib/ModelDownloadCard.svelte";
  import {
    modelsStatus,
    getSettings,
    setSettings,
    type ModelsStatus,
  } from "$lib/models";
  import { mcpAgentsStatus, mcpRegister, type AgentStatus, type RegisterOutcome } from "$lib/mcp";

  let {
    status,
    onDone,
  }: {
    status: ModelsStatus;
    /** 引导结束(下载完成或进高级设置)。target 为结束后应去的页面。 */
    onDone: (target: "/record" | "/settings") => void;
  } = $props();

  // 欢迎层挂载后状态自持:父层只提供初始快照,后续进度由本层 refresh 驱动。
  // svelte-ignore state_referenced_locally
  let current = $state(status);

  // 相位:download(模型下载,现状) → connect(连接 AI 助手,可跳过) → 结束。
  // 未检测到任何 Agent 时 connect 整步自动跳过(spec §四)。
  let phase = $state<"download" | "connect">("download");
  let agents = $state<AgentStatus[]>([]);
  let picked = $state<Record<string, boolean>>({});
  let outcomes = $state<RegisterOutcome[] | null>(null);
  let registering = $state(false);
  // 防重入门闩:全部注册成功后 600ms 的自动收尾窗口里,用户仍可点「跳过/高级设置」,
  // 不加闩会 finish 两次(点击一次 + 定时器一次;组件卸载不清定时器)。
  let finishing = $state(false);

  /** 置 onboarded(含 mcp_onboarded:欢迎流即 MCP 引导,不再二次提示)。 */
  async function markOnboarded() {
    try {
      const s = await getSettings();
      await setSettings({ ...s, onboarded: true, mcp_onboarded: true });
    } catch {
      /* 落盘失败下次启动会再见到欢迎层,幂等,不打断跳转 */
    }
  }

  async function finish(target: "/record" | "/settings") {
    if (finishing) return;
    finishing = true;
    await markOnboarded();
    onDone(target);
  }

  async function maybeConnect() {
    try {
      agents = (await mcpAgentsStatus()).filter((a) => a.installed);
    } catch {
      agents = [];
    }
    if (agents.length === 0) {
      await finish("/record");
      return;
    }
    // 已拍板:默认全选
    picked = Object.fromEntries(agents.map((a) => [a.key, true]));
    phase = "connect";
  }

  async function registerPicked() {
    registering = true;
    const keys = agents.filter((a) => picked[a.key]).map((a) => a.key);
    try {
      outcomes = keys.length ? await mcpRegister(keys) : [];
    } catch {
      outcomes = keys.map((key) => ({ key, ok: false, error: "调用失败" }));
    }
    registering = false;
    if ((outcomes ?? []).every((o) => o.ok)) {
      setTimeout(() => finish("/record"), 600); // 让用户看见打勾再走
    }
  }

  async function refresh() {
    try {
      current = await modelsStatus();
    } catch {
      return;
    }
    if (current.recording_ready) {
      await maybeConnect(); // 原直接 finish("/record"),现插入 connect 步
    }
  }

  async function advanced() {
    await finish("/settings");
  }
</script>

<div class="overlay">
  <div class="panel">
    <div class="hero">
      <div class="mark"><span class="dot"></span></div>
      <h1>voice-notes</h1>
      <p class="tagline">会议实时转写与说话人分离，全程本地运行</p>
    </div>

    {#if phase === "download"}
      <ModelDownloadCard status={current} onComplete={refresh} primaryLabel="开 始 使 用" />
      <p class="hints">首次录制时，系统会请求麦克风权限；录制系统声音需在系统设置中允许录屏。</p>
    {:else}
      <div class="connect">
        <h2>连接 AI 助手</h2>
        <p class="hints">
          让 Claude / Cursor 等直接检索你的会议笔记。注册后,AI 助手查到的笔记内容会进入其模型上下文;随时可在
          设置 → AI 助手接入 移除。
        </p>
        {#each agents as a (a.key)}
          <label class="agent-row">
            <input type="checkbox" bind:checked={picked[a.key]} disabled={registering || outcomes !== null} />
            <span>{a.name}</span>
            {#if outcomes}
              {@const o = outcomes.find((x) => x.key === a.key)}
              {#if o}<span class="mark-txt" class:bad={!o.ok}>{o.ok ? "✓ 已注册" : `✕ ${o.error ?? "失败"}`}</span>{/if}
            {/if}
          </label>
        {/each}
        <div class="connect-actions">
          <button class="btn-primary" disabled={registering || finishing || outcomes !== null} onclick={registerPicked}>注册所选</button>
          <button class="link" disabled={registering} onclick={() => finish("/record")}>跳过</button>
        </div>
      </div>
    {/if}

    <div class="foot">
      <button class="link" onclick={advanced}>高级设置 →</button>
    </div>
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 100;
    background: var(--canvas);
    display: flex;
    align-items: center;
    justify-content: center;
    overflow-y: auto;
  }
  .panel {
    width: min(30rem, calc(100vw - 3rem));
    padding: 1rem;
  }
  .hero {
    text-align: center;
    margin-bottom: 1.2rem;
  }
  /* 品牌记号:录制按钮同构的「白药丸 + record 红点」,与侧栏录制键呼应 */
  .mark {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 3.4rem;
    height: 3.4rem;
    border-radius: var(--radius-full);
    background: var(--primary);
    box-shadow: var(--shadow-btn);
    margin-bottom: 0.8rem;
  }
  .dot {
    width: 14px;
    height: 14px;
    border-radius: var(--radius-full);
    background: var(--record);
  }
  h1 {
    margin: 0 0 0.3rem;
    font-size: 1.5rem;
    font-weight: 600;
    letter-spacing: 0.01em;
  }
  .tagline {
    margin: 0;
    color: var(--ink-secondary);
    font-size: 0.95rem;
  }
  .hints {
    color: var(--ink-faint);
    font-size: 0.85rem;
    text-align: center;
    margin: 0.8rem 0 0;
    line-height: 1.6;
  }
  .foot {
    text-align: center;
    margin-top: 1.2rem;
  }
  .link {
    background: none;
    border: none;
    padding: 0.3em 0.6em;
    font-size: 0.9rem;
    color: var(--ink-secondary);
    cursor: pointer;
    border-radius: var(--radius-sm);
  }
  .link:hover {
    color: var(--ink);
    background: var(--surface-soft);
  }
  .connect { text-align: left; }
  .connect h2 { margin: 0 0 0.4rem; font-size: 1.1rem; text-align: center; }
  .agent-row {
    display: flex; align-items: center; gap: 0.6rem;
    padding: 0.55rem 0.4rem; border-radius: var(--radius-sm);
  }
  .agent-row:hover { background: var(--surface-soft); }
  .mark-txt { margin-left: auto; font-size: 0.85rem; color: var(--ink-secondary); }
  .mark-txt.bad { color: var(--record); }
  .connect-actions { display: flex; justify-content: center; gap: 0.8rem; margin-top: 1rem; }
  /* button-primary:本组件此前无主按钮类,样式对齐 ModelDownloadCard/settings 页的 primary 药丸 */
  .btn-primary {
    border-radius: var(--radius-full);
    border: 1px solid transparent;
    padding: 0.5em 1.4em;
    font-size: 0.9rem;
    font-weight: 500;
    cursor: pointer;
    background: var(--primary);
    color: var(--on-primary);
    box-shadow: var(--shadow-btn);
  }
  .btn-primary:hover {
    background: var(--primary-pressed);
  }
  .btn-primary:disabled {
    opacity: 0.6;
    cursor: default;
  }
</style>
