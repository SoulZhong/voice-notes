<script lang="ts">
  // 首启欢迎层(welcome-overlay,见 DESIGN.md):模型未就绪且未完成引导时全屏覆盖。
  // 下载流整体复用 ModelDownloadCard(进度/续传/暂停均沿用),本组件只负责
  // 欢迎文案、完成后的收尾(置 onboarded + 跳转)与「高级设置」逃生口。
  import ModelDownloadCard from "$lib/ModelDownloadCard.svelte";
  import {
    modelsStatus,
    getSettings,
    setSettings,
    downloadModels,
    testCloudAsr,
    type ModelsStatus,
  } from "$lib/models";
  import { mcpAgentsStatus, mcpRegister, type AgentStatus, type RegisterOutcome } from "$lib/mcp";
  import { AI_TOOLS_GUIDE_ID } from "$lib/onboarding";

  let {
    status,
    onDone,
  }: {
    status: ModelsStatus;
    /** 引导结束(下载完成或进高级设置)。target 为结束后应去的页面。 */
    onDone: (target: "/record" | "/settings" | "/ai" | "/ai?guide=ai-tools-v1") => void;
  } = $props();

  // 欢迎层挂载后状态自持:父层只提供初始快照,后续进度由本层 refresh 驱动。
  // svelte-ignore state_referenced_locally
  let current = $state(status);

  // 相位:choose(本地/云端分支) → download(模型下载,本地分支)/cloud-setup(云端凭证,云端分支)
  //   → learn(先理解 AI 能力) → connect(选择接入方式) → 结束。
  // 即使没检测到 Agent 也展示 connect:「没安装」本身需要可行动的下一步,不能静默跳过。
  // svelte-ignore state_referenced_locally
  let phase = $state<"choose" | "download" | "cloud-setup" | "learn" | "connect">(
    status.recording_ready ? "learn" : "choose",
  );
  // —— 云端识别引导(cloud-setup 相位)本地绑定,逻辑照搬设置页「识别方式」区 ——
  let cloudProvider = $state<"volcano" | "aliyun">("volcano");
  let volcAppKey = $state("");
  let volcAccessKey = $state("");
  let dashKey = $state("");
  let testingCloud = $state(false);
  let cloudTestResult = $state<{ ok: boolean; msg: string } | null>(null);
  // 「完成」按钮门闩:必须先测试成功一次,防止凭证打错却直接进入下载/录制。
  let cloudTestPassed = $state(false);
  let agents = $state<AgentStatus[]>([]);
  let picked = $state<Record<string, boolean>>({});
  let outcomes = $state<RegisterOutcome[] | null>(null);
  let registering = $state(false);
  // 防重入门闩:全部注册成功后 600ms 的自动收尾窗口里,用户仍可点「跳过/高级设置」,
  // 不加闩会 finish 两次(点击一次 + 定时器一次;组件卸载不清定时器)。
  let finishing = $state(false);
  // 失败可原地重试(改选后再点「注册所选」),只有全成功才锁定 checkbox/按钮——
  // 否则用户看到失败结果却无法调整勾选重试,只能靠「跳过」放弃整个引导步骤。
  const registerLocked = $derived(registering || finishing || (outcomes !== null && outcomes.every((o) => o.ok)));

  /** 基础初始化和本功能引导分别记账；后续功能使用自己的 guide ID。 */
  async function markOnboarded() {
    try {
      const s = await getSettings();
      const completed = new Set(s.completed_guides);
      completed.add(AI_TOOLS_GUIDE_ID);
      await setSettings({
        ...s,
        onboarded: true,
        mcp_onboarded: true,
        completed_guides: [...completed],
      });
    } catch {
      /* 落盘失败下次启动会再见到欢迎层,幂等,不打断跳转 */
    }
  }

  async function finish(target: "/record" | "/settings" | "/ai" | "/ai?guide=ai-tools-v1") {
    if (finishing) return;
    finishing = true;
    await markOnboarded();
    onDone(target);
  }

  async function continueToProductGuide() {
    if (finishing) return;
    finishing = true;
    try {
      const s = await getSettings();
      await setSettings({ ...s, onboarded: true });
    } catch {
      /* 基础状态下次可幂等补写；不阻断进入真实操作页 */
    }
    onDone("/ai?guide=ai-tools-v1");
  }

  function backToChoose() {
    phase = "choose";
    cloudTestResult = null;
    cloudTestPassed = false;
  }

  /** 落盘厂商+凭证:testCloudAsr 读的是持久化 settings,测试前必须先写入。 */
  async function saveCloudCreds() {
    const s = await getSettings();
    s.cloud_asr_provider = cloudProvider;
    s.volc_app_key = volcAppKey;
    s.volc_access_key = volcAccessKey;
    s.dashscope_api_key = dashKey;
    await setSettings(s);
  }

  async function doTestCloud() {
    testingCloud = true;
    cloudTestResult = null;
    cloudTestPassed = false;
    try {
      await saveCloudCreds();
      cloudTestResult = { ok: true, msg: await testCloudAsr() };
      cloudTestPassed = true;
    } catch (e) {
      cloudTestResult = { ok: false, msg: String(e) };
    } finally {
      testingCloud = false;
    }
  }

  /** 测试通过后「完成」:切到云端模式落盘,只下载云端模式所需的 vad+speaker,再走既有下载态。 */
  async function completeCloudSetup() {
    if (!cloudTestPassed) return;
    try {
      const s = await getSettings();
      s.asr_mode = "cloud";
      s.cloud_asr_provider = cloudProvider;
      s.volc_app_key = volcAppKey;
      s.volc_access_key = volcAccessKey;
      s.dashscope_api_key = dashKey;
      await setSettings(s);
    } catch {
      /* 落盘失败仍进入下载态,用户可在设置页补救;凭证已在测试时保存过一次 */
    }
    phase = "download";
    try {
      await downloadModels(["vad", "speaker"]);
    } catch {
      /* "下载已在进行中" 等非致命错误:沿用 ModelDownloadCard 的进度事件即可 */
    }
    await refresh();
  }

  async function showConnect() {
    try {
      agents = (await mcpAgentsStatus()).filter((a) => a.installed);
    } catch {
      agents = [];
    }
    // 已拍板:默认全选
    picked = Object.fromEntries(agents.map((a) => [a.key, true]));
    phase = "connect";
  }

  async function registerPicked() {
    // 失败可原地重试,只有全成功才锁定:先清掉上一轮 outcomes,让 UI 回到「待注册」态,
    // 否则 disabled 表达式里的 outcomes !== null 会一直挡住这次重试的点击。
    outcomes = null;
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
      // 模型下载是唯一留在全屏层的基础初始化；功能教学转到真实 AI 设置页。
      await continueToProductGuide();
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

    <div class="steps" aria-label="新手引导进度">
      <span
        class:active={phase === "choose" || phase === "download" || phase === "cloud-setup"}
        class:done={phase === "learn" || phase === "connect"}>准备</span
      >
      <i></i>
      <span class:active={phase === "learn"} class:done={phase === "connect"}>认识 AI</span>
      <i></i>
      <span class:active={phase === "connect"}>接入工具</span>
    </div>

    {#if phase === "choose"}
      <div class="choose">
        <h2>选择识别方式</h2>
        <div class="choice-cards">
          <button type="button" class="choice-card" onclick={() => (phase = "download")}>
            <strong>本地识别 · 隐私优先</strong>
            <p>识别在本机完成，数据不出设备；需下载模型约 1GB。</p>
          </button>
          <button type="button" class="choice-card" onclick={() => (phase = "cloud-setup")}>
            <strong>云端识别 · 更准更快</strong>
            <p>火山引擎 / 阿里云实时识别；录音音频将实时上传至所选厂商；需 API Key。</p>
          </button>
        </div>
        <p class="hints">以后可随时在设置中更改。</p>
      </div>
    {:else if phase === "download"}
      <ModelDownloadCard status={current} onComplete={refresh} primaryLabel="开 始 使 用" />
      <p class="hints">首次录制时，系统会请求麦克风权限；录制系统声音需在系统设置中允许录屏。</p>
      <p class="hints">已开启匿名使用统计（仅功能使用次数与版本，绝不含会议内容），可在设置中关闭。</p>
    {:else if phase === "cloud-setup"}
      <div class="cloud-setup">
        <h2>配置云端识别</h2>
        <p class="hints">录音音频将实时上传至所选厂商；凭证只保存在本机。</p>
        <div class="seg">
          <label class="seg-item">
            <input
              type="radio"
              name="welcome-cloudprovider"
              value="volcano"
              bind:group={cloudProvider}
              onchange={() => (cloudTestResult = null)}
            />火山引擎
          </label>
          <label class="seg-item">
            <input
              type="radio"
              name="welcome-cloudprovider"
              value="aliyun"
              bind:group={cloudProvider}
              onchange={() => (cloudTestResult = null)}
            />阿里云
          </label>
        </div>
        {#if cloudProvider === "volcano"}
          <label class="field">
            <span>APP ID</span>
            <input
              class="row-input"
              placeholder="火山引擎语音技术控制台的 App ID"
              bind:value={volcAppKey}
              oninput={() => (cloudTestResult = null)}
            />
          </label>
          <label class="field">
            <span>Access Token</span>
            <input
              class="row-input"
              type="password"
              placeholder="Access Token"
              bind:value={volcAccessKey}
              oninput={() => (cloudTestResult = null)}
            />
          </label>
        {:else}
          <label class="field">
            <span>API Key</span>
            <input
              class="row-input"
              type="password"
              placeholder="DashScope API Key"
              bind:value={dashKey}
              oninput={() => (cloudTestResult = null)}
            />
          </label>
        {/if}
        {#if cloudTestResult}
          <p class="test-result" class:ok={cloudTestResult.ok} class:bad={!cloudTestResult.ok}>
            {cloudTestResult.ok ? `测试成功（${cloudTestResult.msg}）` : `测试失败：${cloudTestResult.msg}`}
          </p>
        {/if}
        <div class="connect-actions">
          <button class="btn-primary" disabled={testingCloud} onclick={doTestCloud}>
            {testingCloud ? "测试中…" : "测试连接"}
          </button>
          <button class="btn-primary" disabled={!cloudTestPassed} onclick={completeCloudSetup}>完成</button>
        </div>
        <div class="foot">
          <button type="button" class="link" onclick={backToChoose}>← 返回</button>
        </div>
      </div>
    {:else if phase === "learn"}
      <div class="learn">
        <h2>一份录音，三种 AI 用法</h2>
        <div class="ability-list">
          <div class="ability">
            <span class="ability-index">01</span>
            <div><strong>自动整理</strong><p>会后 AI 在录制结束后修订转写、提炼标题，原始稿始终保留。</p></div>
          </div>
          <div class="ability">
            <span class="ability-index">02</span>
            <div><strong>带进对话</strong><p>接入 Claude、Cursor 等助手后，直接让它检索会议、追踪决定或生成周报。</p></div>
          </div>
          <div class="ability">
            <span class="ability-index">03</span>
            <div><strong>串起工作流</strong><p>用钩子在 AI 完成后调用 Shell 或 Webhook，把结果送到现有工具。</p></div>
          </div>
        </div>
        <div class="prompt-example">
          <span>接入后可以直接问</span>
          <p>“找出上周所有提到发布风险的会议，并按负责人整理待办。”</p>
        </div>
        <button class="btn-primary next" onclick={showConnect}>选择如何接入</button>
      </div>
    {:else}
      <div class="connect">
        <h2>连接 AI 助手</h2>
        <p class="hints">
          MCP 是 voice-notes 与 AI 助手之间的本地桥梁。注册后，助手可按需检索笔记；命中的内容会进入该助手的模型上下文。
        </p>
        {#if agents.length}
          {#each agents as a (a.key)}
            <label class="agent-row">
              <input type="checkbox" bind:checked={picked[a.key]} disabled={registerLocked} />
              <span>{a.name}</span>
              {#if outcomes}
                {@const o = outcomes.find((x) => x.key === a.key)}
                {#if o}<span class="mark-txt" class:bad={!o.ok}>{o.ok ? "✓ 已注册" : `✕ ${o.error ?? "失败"}`}</span>{/if}
              {/if}
            </label>
          {/each}
          <div class="connect-actions">
            <button class="btn-primary" disabled={registerLocked} onclick={registerPicked}>注册所选</button>
            <button class="link" disabled={registering} onclick={() => finish("/record")}>暂不接入</button>
          </div>
        {:else}
          <div class="no-agent">
            <strong>暂未发现支持的 AI 助手</strong>
            <p>你仍可先开始录音；安装 Claude Code、Cursor、Codex 或 Gemini CLI 后，到 AI 页一键注册。其他工具可复制通用 MCP 配置。</p>
          </div>
          <div class="connect-actions">
            <button class="btn-primary" onclick={() => finish("/ai")}>查看接入方式</button>
            <button class="link" onclick={() => finish("/record")}>先开始录音</button>
          </div>
        {/if}
        <p class="privacy">默认只开放读取能力；允许 AI 控制录制需在 AI 页单独开启。</p>
      </div>
    {/if}

    <div class="foot">
      {#if phase === "choose" || phase === "download"}<button class="link" onclick={advanced}>高级设置 →</button>{/if}
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
  .steps {
    display: grid;
    grid-template-columns: auto 1fr auto 1fr auto;
    align-items: center;
    gap: 0.55rem;
    margin: 0 0 1.25rem;
    color: var(--ink-faint);
    font-size: 0.76rem;
  }
  .steps i { height: 1px; background: var(--hairline-strong); }
  .steps span.active { color: var(--ink); font-weight: 600; }
  .steps span.done { color: var(--success); }
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
  .connect h2, .learn h2, .choose h2, .cloud-setup h2 { margin: 0 0 0.4rem; font-size: 1.1rem; text-align: center; }
  /* choose:两张等宽卡片,复用 prompt-example 的 surface 底色语言 */
  .choice-cards { display: grid; gap: 0.7rem; margin: 1rem 0 0.6rem; }
  .choice-card {
    text-align: left;
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    padding: 0.85rem 1rem;
    cursor: pointer;
    font: inherit;
    color: inherit;
  }
  .choice-card:hover { background: var(--surface-soft); border-color: var(--hairline-strong); }
  .choice-card strong { display: block; font-size: 0.94rem; font-weight: 600; }
  .choice-card p { margin: 0.3rem 0 0; color: var(--ink-secondary); font-size: 0.82rem; line-height: 1.5; }
  /* cloud-setup:凭证输入,样式照搬设置页「识别方式」区(seg/row-input/mtest-*) */
  .cloud-setup .seg { display: flex; gap: 2px; background: var(--surface-press); border-radius: var(--radius-md); padding: 2px; margin: 1rem 0 0; }
  .cloud-setup .seg-item {
    position: relative; flex: 1; text-align: center;
    padding: 0.32em 0.7em; font-size: 0.85rem; font-weight: 500;
    color: var(--ink-secondary); border-radius: calc(var(--radius-md) - 2px); cursor: pointer;
  }
  .cloud-setup .seg-item:hover { color: var(--ink); }
  .cloud-setup .seg-item:has(input:checked) { background: var(--canvas); color: var(--ink); box-shadow: var(--shadow-btn); }
  .cloud-setup .seg-item input { position: absolute; opacity: 0; pointer-events: none; }
  .field { display: block; margin: 0.7rem 0 0; }
  .field span { display: block; font-size: 0.8rem; color: var(--ink-secondary); margin-bottom: 0.3rem; }
  .row-input {
    width: 100%; box-sizing: border-box;
    padding: 0.42em 0.65em; border: none; border-radius: var(--radius-md);
    background: var(--surface-press); color: var(--ink); font-size: 0.9rem;
  }
  .row-input:focus { outline: none; background: var(--canvas); box-shadow: 0 0 0 1px var(--accent); }
  .row-input::placeholder { color: var(--ink-faint); }
  .test-result { font-size: 0.82rem; margin: 0.7rem 0 0; text-align: center; }
  .test-result.ok { color: var(--success); }
  .test-result.bad { color: var(--danger-ink); }
  .ability-list { margin: 1rem 0; border-top: 1px solid var(--hairline); }
  .ability {
    display: grid; grid-template-columns: 2rem 1fr; gap: 0.55rem;
    padding: 0.72rem 0; border-bottom: 1px solid var(--hairline);
  }
  .ability-index { color: var(--ink-faint); font-size: 0.72rem; padding-top: 0.16rem; }
  .ability strong { font-size: 0.92rem; font-weight: 550; }
  .ability p { margin: 0.18rem 0 0; color: var(--ink-secondary); font-size: 0.82rem; line-height: 1.5; }
  .prompt-example { background: var(--surface); border-radius: var(--radius-lg); padding: 0.7rem 0.85rem; }
  .prompt-example span { color: var(--ink-faint); font-size: 0.74rem; }
  .prompt-example p { margin: 0.25rem 0 0; color: var(--ink-secondary); font-size: 0.84rem; line-height: 1.5; }
  .next { display: block; margin: 1rem auto 0; }
  .agent-row {
    display: flex; align-items: center; gap: 0.6rem;
    padding: 0.55rem 0.4rem; border-radius: var(--radius-sm);
  }
  .agent-row:hover { background: var(--surface-soft); }
  .mark-txt { margin-left: auto; font-size: 0.85rem; color: var(--ink-secondary); }
  .mark-txt.bad { color: var(--record); }
  .connect-actions { display: flex; justify-content: center; gap: 0.8rem; margin-top: 1rem; }
  .no-agent { margin-top: 1rem; padding: 0.8rem 0; border-block: 1px solid var(--hairline); }
  .no-agent strong { font-size: 0.9rem; }
  .no-agent p, .privacy { color: var(--ink-secondary); font-size: 0.8rem; line-height: 1.5; margin: 0.35rem 0 0; }
  .privacy { text-align: center; color: var(--ink-faint); margin-top: 0.9rem; }
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
