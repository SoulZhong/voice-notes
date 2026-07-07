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

  /** 置 onboarded 前重取 settings:引导期间用户不可能改设置,但避免覆写并发状态的姿势要统一。 */
  async function markOnboarded() {
    try {
      const s = await getSettings();
      await setSettings({ ...s, onboarded: true });
    } catch {
      /* 落盘失败下次启动会再见到欢迎层,幂等,不打断跳转 */
    }
  }

  async function refresh() {
    try {
      current = await modelsStatus();
    } catch {
      return;
    }
    if (current.recording_ready) {
      await markOnboarded();
      onDone("/record");
    }
  }

  async function advanced() {
    await markOnboarded();
    onDone("/settings");
  }
</script>

<div class="overlay">
  <div class="panel">
    <div class="hero">
      <div class="mark"><span class="dot"></span></div>
      <h1>voice-notes</h1>
      <p class="tagline">会议实时转写与说话人分离，全程本地运行</p>
    </div>

    <ModelDownloadCard status={current} onComplete={refresh} primaryLabel="开 始 使 用" />

    <p class="hints">首次录制时，系统会请求麦克风权限；录制系统声音需在系统设置中允许录屏。</p>

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
</style>
