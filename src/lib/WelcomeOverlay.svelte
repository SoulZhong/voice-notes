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
  import { t } from "$lib/i18n/index.svelte";

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
  let cloudTestGeneration = 0;
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
    invalidateCloudTest();
  }

  function invalidateCloudTest() {
    cloudTestGeneration += 1;
    cloudTestResult = null;
    cloudTestPassed = false;
  }

  async function doTestCloud() {
    const generation = ++cloudTestGeneration;
    const input = {
      provider: cloudProvider,
      volcAppKey,
      volcAccessKey,
      dashKey,
    };
    testingCloud = true;
    cloudTestResult = null;
    cloudTestPassed = false;
    try {
      const msg = await testCloudAsr(
        input.provider,
        input.volcAppKey,
        input.volcAccessKey,
        input.dashKey,
      );
      if (generation !== cloudTestGeneration) return;
      cloudTestResult = { ok: true, msg };
      cloudTestPassed = true;
    } catch (e) {
      if (generation !== cloudTestGeneration) return;
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
    } catch (e) {
      cloudTestPassed = false;
      cloudTestResult = { ok: false, msg: t("shell.welcome.saveSettingsFailed", { e: String(e) }) };
      return;
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
      outcomes = keys.map((key) => ({ key, ok: false, error: t("shell.welcome.callFailed") }));
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
      <p class="tagline">{t("shell.welcome.tagline")}</p>
    </div>

    <div class="steps" aria-label={t("shell.welcome.stepsAria")}>
      <span
        class:active={phase === "choose" || phase === "download" || phase === "cloud-setup"}
        class:done={phase === "learn" || phase === "connect"}>{t("shell.welcome.stepPrepare")}</span
      >
      <i></i>
      <span class:active={phase === "learn"} class:done={phase === "connect"}>{t("shell.welcome.stepLearn")}</span>
      <i></i>
      <span class:active={phase === "connect"}>{t("shell.welcome.stepConnect")}</span>
    </div>

    {#if phase === "choose"}
      <div class="choose">
        <h2>{t("shell.welcome.chooseTitle")}</h2>
        <div class="choice-cards">
          <button type="button" class="choice-card" onclick={() => (phase = "download")}>
            <strong>{t("shell.welcome.localTitle")}</strong>
            <p>{t("shell.welcome.localDesc")}</p>
          </button>
          <button type="button" class="choice-card" onclick={() => (phase = "cloud-setup")}>
            <strong>{t("shell.welcome.cloudTitle")}</strong>
            <p>{t("shell.welcome.cloudDesc")}</p>
          </button>
        </div>
        <p class="hints">{t("shell.welcome.changeLater")}</p>
      </div>
    {:else if phase === "download"}
      <ModelDownloadCard status={current} onComplete={refresh} primaryLabel={t("shell.welcome.startUsing")} />
      <p class="hints">{t("shell.welcome.permissionsHint")}</p>
      <p class="hints">{t("shell.welcome.telemetryHint")}</p>
    {:else if phase === "cloud-setup"}
      <div class="cloud-setup">
        <h2>{t("shell.welcome.cloudSetupTitle")}</h2>
        <p class="hints">{t("shell.welcome.cloudSetupHint")}</p>
        <div class="seg">
          <label class="seg-item">
            <input
              type="radio"
              name="welcome-cloudprovider"
              value="volcano"
              bind:group={cloudProvider}
              onchange={() => {
                // 凭证一经改动,上次测试结果即作废,须重测才能完成
                invalidateCloudTest();
              }}
            />{t("shell.welcome.providerVolcano")}
          </label>
          <label class="seg-item">
            <input
              type="radio"
              name="welcome-cloudprovider"
              value="aliyun"
              bind:group={cloudProvider}
              onchange={() => {
                invalidateCloudTest();
              }}
            />{t("shell.welcome.providerAliyun")}
          </label>
        </div>
        {#if cloudProvider === "volcano"}
          <label class="field">
            <span>APP ID</span>
            <input
              class="row-input"
              placeholder={t("shell.welcome.volcAppIdPlaceholder")}
              bind:value={volcAppKey}
              oninput={() => {
                invalidateCloudTest();
              }}
            />
          </label>
          <label class="field">
            <span>Access Token</span>
            <input
              class="row-input"
              type="password"
              placeholder="Access Token"
              bind:value={volcAccessKey}
              oninput={() => {
                invalidateCloudTest();
              }}
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
              oninput={() => {
                invalidateCloudTest();
              }}
            />
          </label>
        {/if}
        {#if cloudTestResult}
          <p class="test-result" class:ok={cloudTestResult.ok} class:bad={!cloudTestResult.ok}>
            {cloudTestResult.ok ? t("shell.welcome.testOk", { msg: cloudTestResult.msg }) : t("shell.welcome.testFail", { msg: cloudTestResult.msg })}
          </p>
        {/if}
        <div class="connect-actions">
          <button class="btn-primary" disabled={testingCloud} onclick={doTestCloud}>
            {testingCloud ? t("shell.welcome.testing") : t("shell.welcome.testConnection")}
          </button>
          <button class="btn-primary" disabled={!cloudTestPassed} onclick={completeCloudSetup}>{t("shell.welcome.done")}</button>
        </div>
        <div class="foot">
          <button type="button" class="link" onclick={backToChoose}>{t("shell.welcome.back")}</button>
        </div>
      </div>
    {:else if phase === "learn"}
      <div class="learn">
        <h2>{t("shell.welcome.learnTitle")}</h2>
        <div class="ability-list">
          <div class="ability">
            <span class="ability-index">01</span>
            <div><strong>{t("shell.welcome.ability1Title")}</strong><p>{t("shell.welcome.ability1Body")}</p></div>
          </div>
          <div class="ability">
            <span class="ability-index">02</span>
            <div><strong>{t("shell.welcome.ability2Title")}</strong><p>{t("shell.welcome.ability2Body")}</p></div>
          </div>
          <div class="ability">
            <span class="ability-index">03</span>
            <div><strong>{t("shell.welcome.ability3Title")}</strong><p>{t("shell.welcome.ability3Body")}</p></div>
          </div>
        </div>
        <div class="prompt-example">
          <span>{t("shell.welcome.promptLabel")}</span>
          <p>{t("shell.welcome.promptExample")}</p>
        </div>
        <button class="btn-primary next" onclick={showConnect}>{t("shell.welcome.chooseConnect")}</button>
      </div>
    {:else}
      <div class="connect">
        <h2>{t("shell.welcome.connectTitle")}</h2>
        <p class="hints">
          {t("shell.welcome.mcpHint")}
        </p>
        {#if agents.length}
          {#each agents as a (a.key)}
            <label class="agent-row">
              <input type="checkbox" bind:checked={picked[a.key]} disabled={registerLocked} />
              <span>{a.name}</span>
              {#if outcomes}
                {@const o = outcomes.find((x) => x.key === a.key)}
                {#if o}<span class="mark-txt" class:bad={!o.ok}>{o.ok ? t("shell.welcome.registered") : `✕ ${o.error ?? t("shell.welcome.failed")}`}</span>{/if}
              {/if}
            </label>
          {/each}
          <div class="connect-actions">
            <button class="btn-primary" disabled={registerLocked} onclick={registerPicked}>{t("shell.welcome.registerSelected")}</button>
            <button class="link" disabled={registering} onclick={() => finish("/record")}>{t("shell.welcome.skipConnect")}</button>
          </div>
        {:else}
          <div class="no-agent">
            <strong>{t("shell.welcome.noAgentTitle")}</strong>
            <p>{t("shell.welcome.noAgentBody")}</p>
          </div>
          <div class="connect-actions">
            <button class="btn-primary" onclick={() => finish("/ai")}>{t("shell.welcome.viewConnect")}</button>
            <button class="link" onclick={() => finish("/record")}>{t("shell.welcome.startRecordingFirst")}</button>
          </div>
        {/if}
        <p class="privacy">{t("shell.welcome.privacy")}</p>
      </div>
    {/if}

    <div class="foot">
      {#if phase === "choose" || phase === "download"}<button class="link" onclick={advanced}>{t("shell.welcome.advanced")}</button>{/if}
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
