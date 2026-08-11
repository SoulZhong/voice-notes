<script lang="ts">
  // AI 页:Aing 大模型配置 + AI 助手接入(Task 2 自设置页迁入)。
  import { onMount } from "svelte";
  import { getSettings, setSettings, testRefineLlm, testRefineAgent, type Settings } from "$lib/models";
  import {
    mcpAgentsStatus,
    mcpRegister,
    mcpUnregister,
    mcpManualSnippet,
    mcpHealedCount,
    mcpSkillStatus,
    mcpSkillInstall,
    mcpSkillUninstall,
    mcpCapabilities,
    mcpSkillRead,
    mcpSkillSave,
    refineAgentsProbe,
    type AgentStatus,
    type Capabilities,
  } from "$lib/mcp";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { t } from "$lib/i18n/index.svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import { aiLogsQuery } from "$lib/ailog";
  import RelationBackfillDialog from "$lib/RelationBackfillDialog.svelte";
  import { AI_TOOLS_GUIDE_ID } from "$lib/onboarding";

  let settings = $state<Settings | null>(null);
  /** danger 横幅：本页保存类操作的错误统一在此显示(精简自设置页的全局 error 横幅)。 */
  let error = $state("");

  /** Aing:接口三字段的本地镜像(失败回弹靠本地 state 强制 DOM 对齐)。开关已移至设置页「录制」区。 */
  let refineBaseUrl = $state("");
  let refineModel = $state("");
  let refineKey = $state("");
  /** modelLabel/modelDesc/modelPlaceholder:该服务商对「模型」字段的定制文案(存 i18n 键,渲染处 t() 取值)。
      豆包(火山方舟)的调用凭据是控制台创建的「推理接入点」ID(ep- 开头),不是
      裸模型名——预填模型名对要求接入点的账号是坏值,故 model 留空、整行换文案。 */
  const REFINE_PRESETS = [
    { label: "DeepSeek", base: "https://api.deepseek.com/v1", model: "deepseek-chat" },
    { label: "Qwen", labelKey: "ai.aing.preset.qwen", base: "https://dashscope.aliyuncs.com/compatible-mode/v1", model: "qwen-plus" },
    {
      label: "Doubao",
      labelKey: "ai.aing.preset.doubao",
      base: "https://ark.cn-beijing.volces.com/api/v3",
      model: "",
      modelLabel: "ai.aing.doubao.modelLabel",
      modelDesc: "ai.aing.doubao.modelDesc",
      modelPlaceholder: "ep-20250712…",
    },
    { label: "Kimi", base: "https://api.moonshot.cn/v1", model: "moonshot-v1-auto" },
    { label: "OpenAI", base: "https://api.openai.com/v1", model: "gpt-4o-mini" },
  ] as {
    label: string;
    /** 品牌名的字典键;仅中英名不同的服务商需要(如通义千问/Qwen),缺省直接显示 label。 */
    labelKey?: string;
    base: string;
    model: string;
    modelLabel?: string;
    modelDesc?: string;
    modelPlaceholder?: string;
  }[];
  /** 当前接口地址命中的预设(用户手改过地址就不再套预设文案)。 */
  const activePreset = $derived(REFINE_PRESETS.find((p) => p.base === refineBaseUrl.trim()));

  /** 各服务商草稿(model/key):切换预设先暂存当前填写,切回时原样回填——配置不再被
      预设默认值覆盖丢失(2026-08-11 冒烟反馈)。localStorage 持久,与 settings.json
      明文存 key 同一威胁模型;真正生效的配置仍以 settings 为唯一真源,草稿只是
      各家的编辑缓冲。自定义地址(不命中任何预设)不入草稿:无稳定身份,避免误回填。 */
  const REFINE_DRAFTS_KEY = "vn.refineDrafts";
  let refineDrafts = $state<Record<string, { base?: string; model: string; key: string }>>({});
  /** 服务商 tab 的当前页:命中预设显示该家,否则落「自定义」页。 */
  const activeProviderTab = $derived(activePreset?.label ?? "custom");
  function stashDraft() {
    const preset = REFINE_PRESETS.find((p) => p.base === refineBaseUrl.trim());
    // 自定义地址也入草稿(连 base 一起存,tab 切回时整体回填);全空则无可存。
    const id = preset ? preset.label : refineBaseUrl.trim() ? "custom" : null;
    if (!id) return;
    refineDrafts = {
      ...refineDrafts,
      [id]: { base: refineBaseUrl.trim(), model: refineModel.trim(), key: refineKey.trim() },
    };
    try {
      localStorage.setItem(REFINE_DRAFTS_KEY, JSON.stringify(refineDrafts));
    } catch {
      /* localStorage 不可用:仅本会话内记忆 */
    }
  }

  // —— Aing 执行体:在线接口(openai) / 本机 Agent CLI(agent,经 MCP 读写回) ——
  let refineProvider = $state("openai");
  let refineAgent = $state("claude");
  let refineAgentBin = $state("");
  let refineAgentModel = $state("");
  // 测试三态(null=没测过);改相关字段即清空,防旧「通过」给改过的配置背书。
  let llmTest = $state<{ ok: boolean; msg: string } | null>(null);
  let llmTesting = $state(false);
  let agentTest = $state<{ ok: boolean; msg: string } | null>(null);
  let agentTesting = $state(false);
  const llmMissing = $derived(!refineBaseUrl.trim() || !refineModel.trim() || !refineKey.trim());
  async function runLlmTest() {
    llmTesting = true;
    llmTest = null;
    try {
      llmTest = { ok: true, msg: await testRefineLlm(refineBaseUrl.trim(), refineModel.trim(), refineKey.trim()) };
    } catch (e) {
      llmTest = { ok: false, msg: String(e) };
    } finally {
      llmTesting = false;
    }
  }
  async function runAgentTest() {
    agentTesting = true;
    agentTest = null;
    try {
      agentTest = { ok: true, msg: await testRefineAgent(refineAgent, refineAgentBin.trim(), refineAgentModel.trim()) };
    } catch (e) {
      agentTest = { ok: false, msg: String(e) };
    } finally {
      agentTesting = false;
    }
  }
  /** 四家 CLI 探测结果(key → 路径或 null);onMount 拉一次,切到 agent 模式时展示。 */
  let agentProbe = $state<Record<string, string | null>>({});
  const agentMissing = $derived(!refineAgentBin.trim() && !agentProbe[refineAgent]);
  // modelHint 存 i18n 键,渲染处 t() 取值。
  const AGENT_OPTIONS = [
    { key: "claude", label: "Claude Code", modelHint: "ai.aing.agentModelHint.claude" },
    { key: "codex", label: "Codex", modelHint: "ai.aing.agentModelHint.codex" },
    { key: "gemini", label: "Gemini", modelHint: "ai.aing.agentModelHint.gemini" },
    { key: "cursor", label: "Cursor", modelHint: "ai.aing.agentModelHint.cursor" },
  ];
  const selectedAgentOption = $derived(AGENT_OPTIONS.find((a) => a.key === refineAgent) ?? AGENT_OPTIONS[0]);
  /** 家目录缩写为 ~,长路径在 row-desc 里不至于喧宾夺主。 */
  const shortPath = (p: string) => p.replace(/^\/Users\/[^/]+/, "~");

  // —— MCP(AI 助手接入):列表现扫现示,注册/移除后重拉;真值源是 Agent 配置文件 ——
  let mcpAgents = $state<AgentStatus[]>([]);
  let mcpAllowControl = $state(false);
  let mcpSnippet = $state("");
  let mcpSnippetOpen = $state(false);
  let mcpHealed = $state(0);
  let mcpBusy = $state<string | null>(null); // 正在操作的 agent key,防连点
  let mcpError = $state("");

  // Claude Code 技能:与 Agent 注册同理,真值源是磁盘文件,现查现示。
  let skillState = $state<string | null>(null);
  let skillBusy = $state(false);
  // 技能查看/编辑卡:展开后持有一份正文的本地拷贝,保存/恢复默认都走「拉新内容→替换」。
  let skillEditOpen = $state(false);
  let skillEditBusy = $state(false);
  let skillContent = $state("");

  // —— Agent 能调用什么(MCP 工具 + CLI 清单):纯静态数据,onMount 拉一次即可 ——
  let capabilities = $state<Capabilities | null>(null);
  let capError = $state("");

  // —— AI 调用日志:本页只留入口行(浏览/导出/打开目录在 /ai/logs 独立页),
  //    条数仅作说明位展示,拉取失败静默(入口不因统计失败而残缺)。 ——
  let aiLogsTotal = $state(0);
  let backfillOpen = $state(false);
  let promptCopied = $state(false);
  let guideStep = $state(0);
  let guidePosition = $state("left: 1rem; top: 1rem;");
  let guidePlacement = $state<"right" | "below">("right");
  const guideActive = $derived($page.url.searchParams.get("guide") === AI_TOOLS_GUIDE_ID);
  // eyebrow/title/body 存 i18n 键,渲染处 t() 取值。
  const GUIDE_STEPS = [
    {
      target: "aing-settings",
      eyebrow: "ai.guide.step1.eyebrow",
      title: "ai.guide.step1.title",
      body: "ai.guide.step1.body",
    },
    {
      target: "assistant-connect",
      eyebrow: "ai.guide.step2.eyebrow",
      title: "ai.guide.step2.title",
      body: "ai.guide.step2.body",
    },
    {
      target: "agent-capabilities",
      eyebrow: "ai.guide.step3.eyebrow",
      title: "ai.guide.step3.title",
      body: "ai.guide.step3.body",
    },
  ] as const;
  const currentGuide = $derived(GUIDE_STEPS[guideStep]);

  function scrollToSection(id: string) {
    document.getElementById(id)?.scrollIntoView({ behavior: "smooth", block: "start" });
  }

  async function copyStarterPrompt() {
    try {
      await navigator.clipboard.writeText(t("ai.quickstart.starterPrompt"));
      promptCopied = true;
      setTimeout(() => (promptCopied = false), 1600);
    } catch {
      error = t("ai.quickstart.copyFailed");
    }
  }

  function positionGuideBubble() {
    if (!guideActive || !currentGuide) return;
    const el = document.getElementById(currentGuide.target);
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const bubbleWidth = Math.min(370, window.innerWidth - 24);
    const roomRight = window.innerWidth - rect.right;
    let left: number;
    let top: number;
    if (roomRight >= bubbleWidth + 24) {
      guidePlacement = "right";
      left = rect.right + 14;
      top = Math.max(12, Math.min(rect.top + 18, window.innerHeight - 260));
    } else {
      guidePlacement = "below";
      left = Math.max(12, Math.min(rect.left + 18, window.innerWidth - bubbleWidth - 12));
      top = Math.max(12, Math.min(rect.top + 62, window.innerHeight - 260));
    }
    guidePosition = `left:${left}px;top:${top}px;width:${bubbleWidth}px;`;
  }

  $effect(() => {
    if (!guideActive || !currentGuide) return;
    const target = currentGuide.target;
    const timer = setTimeout(() => {
      document.getElementById(target)?.scrollIntoView({ behavior: "smooth", block: "center" });
      setTimeout(positionGuideBubble, 260);
    }, 80);
    const reposition = () => positionGuideBubble();
    window.addEventListener("resize", reposition);
    window.addEventListener("scroll", reposition, true);
    return () => {
      clearTimeout(timer);
      window.removeEventListener("resize", reposition);
      window.removeEventListener("scroll", reposition, true);
    };
  });

  async function completeGuide(goHooks = false) {
    try {
      const s = await getSettings();
      const completed = new Set(s.completed_guides);
      completed.add(AI_TOOLS_GUIDE_ID);
      await setSettings({ ...s, completed_guides: [...completed], mcp_onboarded: true });
    } catch {
      /* 保存失败时下次会再次提示，当前仍允许用户继续 */
    }
    goto(goHooks ? "/hooks/new?from=ai-guide" : "/ai", { replaceState: true });
  }

  function nextGuideStep() {
    if (guideStep < GUIDE_STEPS.length - 1) guideStep += 1;
    else void completeGuide(false);
  }

  async function loadAiLogsTotal() {
    try {
      aiLogsTotal = (await aiLogsQuery({ limit: 1 })).total;
    } catch {
      /* 静默 */
    }
  }

  onMount(() => {
    backfillOpen = new URLSearchParams(window.location.search).get("backfill") === "relations";
    try {
      refineDrafts = JSON.parse(localStorage.getItem(REFINE_DRAFTS_KEY) ?? "{}");
    } catch {
      /* 草稿损坏:按空处理,不影响 settings 里的生效配置 */
    }
    loadAiLogsTotal();
    (async () => {
      try {
        const s = await getSettings();
        settings = s;
        refineBaseUrl = s.refine_base_url;
        refineModel = s.refine_model;
        refineKey = s.refine_api_key;
        refineProvider = s.refine_provider;
        refineAgent = s.refine_agent;
        refineAgentBin = s.refine_agent_bin;
        refineAgentModel = s.refine_agent_model;
        mcpAllowControl = s.mcp_allow_control;
      } catch { /* 首载失败:控件保持默认,操作时会再报错 */ }
    })();
    refineAgentsProbe().then((v) => (agentProbe = v)).catch(() => {});
    refreshMcp();
    refreshSkill();
    mcpManualSnippet().then((v) => (mcpSnippet = v)).catch(() => {});
    mcpHealedCount().then((n) => (mcpHealed = n)).catch(() => {});
    mcpCapabilities().then((v) => (capabilities = v)).catch((e) => (capError = String(e)));
  });

  // —— 通用「取新鲜值→改→存」保存(精简自设置页 saveSetting:只回弹本页用到的字段) ——
  async function saveSetting(mut: (s: Settings) => void) {
    error = "";
    try {
      const fresh = await getSettings();
      mut(fresh);
      await setSettings(fresh);
      settings = fresh;
      refineBaseUrl = fresh.refine_base_url;
      refineModel = fresh.refine_model;
      refineKey = fresh.refine_api_key;
      refineProvider = fresh.refine_provider;
      refineAgent = fresh.refine_agent;
      refineAgentBin = fresh.refine_agent_bin;
      refineAgentModel = fresh.refine_agent_model;
      mcpAllowControl = fresh.mcp_allow_control;
    } catch (e) {
      error = t("common.saveFailed", { e });
      settings = await getSettings().catch(() => settings);
      if (settings) {
        refineBaseUrl = settings.refine_base_url;
        refineModel = settings.refine_model;
        refineKey = settings.refine_api_key;
        refineProvider = settings.refine_provider;
        refineAgent = settings.refine_agent;
        refineAgentBin = settings.refine_agent_bin;
        refineAgentModel = settings.refine_agent_model;
        mcpAllowControl = settings.mcp_allow_control;
      }
    }
  }

  /** 切服务商 tab:先暂存当前页填写,再回填目标页草稿(无草稿用该家默认值起底)。
      key 是各家各自的凭证:首次切到新家清空——沿用上一家的 key 只会让「测试连接」
      拿错凭证白失败一次。 */
  function selectProviderTab(id: string) {
    if (id === activeProviderTab) return;
    stashDraft();
    const d = refineDrafts[id];
    if (id === "custom") {
      refineBaseUrl = d?.base ?? "";
      refineModel = d?.model ?? "";
      refineKey = d?.key ?? "";
    } else {
      const p = REFINE_PRESETS.find((x) => x.label === id);
      if (!p) return;
      refineBaseUrl = p.base;
      refineModel = d ? d.model : p.model;
      refineKey = d ? d.key : "";
    }
    saveRefine();
  }
  function saveRefine() {
    llmTest = null;
    stashDraft(); // 字段编辑同步进当前服务商草稿,切走切回不丢
    saveSetting((s) => {
      s.refine_base_url = refineBaseUrl.trim();
      s.refine_model = refineModel.trim();
      s.refine_api_key = refineKey.trim();
    });
  }
  function saveRefineAgent() {
    agentTest = null;
    saveSetting((s) => {
      s.refine_provider = refineProvider;
      s.refine_agent = refineAgent;
      s.refine_agent_bin = refineAgentBin.trim();
      s.refine_agent_model = refineAgentModel.trim();
    });
  }

  // —— MCP(AI 助手接入)——
  async function refreshMcp() {
    try {
      mcpAgents = await mcpAgentsStatus();
    } catch (e) {
      mcpError = String(e);
    }
  }

  async function refreshSkill() {
    try {
      skillState = await mcpSkillStatus();
    } catch (e) {
      mcpError = String(e);
    }
  }

  async function toggleSkill() {
    skillBusy = true;
    try {
      if (skillState === "not_installed") {
        await mcpSkillInstall();
      } else {
        await mcpSkillUninstall();
        // 卸载成功即关闭并清空编辑卡:否则残留的旧正文再点「保存」会把文件写回磁盘,
        // 悄悄复活刚删除的 skill(save 会按需重建目录)。
        skillEditOpen = false;
        skillContent = "";
      }
      await refreshSkill();
    } catch (e) {
      mcpError = String(e);
    } finally {
      skillBusy = false;
    }
  }

  /** 查看/编辑卡展开(未安装时也可展开:拉到的是渲染默认稿,保存即以自管身份首次落盘)。 */
  async function openSkillEdit() {
    mcpError = "";
    skillEditBusy = true;
    try {
      const r = await mcpSkillRead();
      skillContent = r.content;
      skillEditOpen = true;
    } catch (e) {
      mcpError = String(e);
    } finally {
      skillEditBusy = false;
    }
  }

  /** 保存 = 编辑即接管。失败保留 textarea 当前内容(不回拉覆盖),经既有 error 横幅提示。 */
  async function saveSkillEdit() {
    mcpError = "";
    skillEditBusy = true;
    try {
      await mcpSkillSave(skillContent);
      const r = await mcpSkillRead();
      skillContent = r.content;
      await refreshSkill();
    } catch (e) {
      mcpError = String(e);
    } finally {
      skillEditBusy = false;
    }
  }

  /** 恢复默认:危险操作(覆盖用户编辑),confirm 二次确认后重装受管渲染稿并重拉内容。 */
  async function restoreSkillDefault() {
    if (!confirm(t("ai.mcp.skill.restoreConfirm"))) return;
    mcpError = "";
    skillEditBusy = true;
    try {
      await mcpSkillInstall();
      const r = await mcpSkillRead();
      skillContent = r.content;
      await refreshSkill();
    } catch (e) {
      mcpError = String(e);
    } finally {
      skillEditBusy = false;
    }
  }

  async function mcpToggleRegister(a: AgentStatus) {
    mcpBusy = a.key;
    mcpError = "";
    try {
      if (a.registered) {
        await mcpUnregister(a.key);
      } else {
        const [r] = await mcpRegister([a.key]);
        if (r && !r.ok) mcpError = `${a.name}: ${r.error ?? t("ai.mcp.registerFailed")}`;
      }
      // refreshMcp 也在 try 内:按钮解禁必须等列表真正刷新完成,否则刷新期间
      // 有一个窄窗口按钮已可点,连点可能撞上刷新中的旧数据。
      await refreshMcp();
    } catch (e) {
      mcpError = String(e);
    } finally {
      // finally 保证复位:即使 refreshMcp reject,按钮也不会永久禁用。
      mcpBusy = null;
    }
  }

  /** 手动配置片段复制。剪贴板权限被拒/不可用时静默失败会让用户以为复制成功却粘贴出空内容——
   *  失败时改提示手动选择文本;成功不额外提示(与现状一致)。 */
  async function copyMcpSnippet() {
    try {
      await navigator.clipboard.writeText(mcpSnippet);
    } catch {
      mcpError = t("ai.mcp.copyFailed");
    }
  }

  async function openMcpReadme() {
    await openUrl("https://github.com/SoulZhong/voice-notes#%E6%8E%A5%E5%85%A5-ai-%E5%8A%A9%E6%89%8B%EF%BC%88mcp%EF%BC%89");
  }

  async function saveMcpAllowControl() {
    if (!settings) return;
    const next = { ...settings, mcp_allow_control: mcpAllowControl };
    try {
      await setSettings(next);
      settings = next;
    } catch {
      if (settings) mcpAllowControl = settings.mcp_allow_control; // 失败回弹
    }
  }
</script>

<div class="page">
  <header class="topbar"><h1>AI</h1></header>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  <section class="quickstart" aria-labelledby="quickstart-title">
    <div class="quickstart-heading">
      <div>
        <h2 id="quickstart-title">{t("ai.quickstart.title")}</h2>
        <p>{t("ai.quickstart.intro")}</p>
      </div>
      <div class="quickstart-heading-actions">
        <span class="local-badge">{t("ai.quickstart.localBadge")}</span>
        <button class="btn-secondary" onclick={() => { guideStep = 0; goto(`/ai?guide=${AI_TOOLS_GUIDE_ID}`); }}>{t("ai.quickstart.replay")}</button>
      </div>
    </div>
    <div class="quickstart-path">
      <div class="quickstep">
        <span class="quickstep-no">1</span>
        <div>
          <strong>{t("ai.quickstart.step1.title")}</strong>
          <p>{t("ai.quickstart.step1.desc")}</p>
          <button class="text-action" onclick={() => scrollToSection("aing-settings")}>{t("ai.quickstart.step1.action")}</button>
        </div>
      </div>
      <div class="quickstep">
        <span class="quickstep-no">2</span>
        <div>
          <strong>{t("ai.quickstart.step2.title")}</strong>
          <p>{t("ai.quickstart.step2.desc")}</p>
          <button class="text-action" onclick={() => scrollToSection("assistant-connect")}>
            {mcpAgents.some((a) => a.registered) ? t("ai.quickstart.step2.viewConnected") : t("ai.quickstart.step2.choose")}
          </button>
        </div>
      </div>
      <div class="quickstep">
        <span class="quickstep-no">3</span>
        <div>
          <strong>{t("ai.quickstart.step3.title")}</strong>
          <p class="starter-prompt">{t("ai.quickstart.step3.quote", { prompt: t("ai.quickstart.starterPrompt") })}</p>
          <button class="text-action" onclick={copyStarterPrompt}>{promptCopied ? t("ai.quickstart.copied") : t("ai.quickstart.copyExample")}</button>
        </div>
      </div>
    </div>
    <p class="quickstart-foot">
      {t("ai.quickstart.foot.before")} <button class="text-action" onclick={() => goto("/hooks")}>{t("ai.quickstart.foot.hooks")}</button>
      {t("ai.quickstart.foot.after")}
    </p>
  </section>

  <!-- —— Aing:settings-row 语言,与下方「AI 助手接入」卡同构 —— -->
  <section id="aing-settings" class="anchor-section" class:guide-target={guideActive && guideStep === 0}>
    <h2 class="section-title">{t("ai.aing.sectionTitle")}</h2>
    <div class="rows">
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("ai.aing.provider.label")}</span>
          <span class="row-desc">
            {refineProvider === "agent"
              ? t("ai.aing.provider.agentDesc")
              : t("ai.aing.provider.openaiDesc")}
          </span>
        </div>
        <div class="seg">
          <label class="seg-item">
            <input type="radio" name="refine-provider" value="openai" bind:group={refineProvider} onchange={saveRefineAgent} />{t("ai.aing.provider.openai")}
          </label>
          <label class="seg-item">
            <input type="radio" name="refine-provider" value="agent" bind:group={refineProvider} onchange={saveRefineAgent} />{t("ai.aing.provider.agent")}
          </label>
        </div>
      </div>
      {#if refineProvider === "agent"}
        <div class="row">
          <div class="row-info">
            <span class="row-label">Agent</span>
            <span class="row-desc">
              {#if refineAgentBin.trim()}
                {t("ai.aing.agent.usingPath", { path: shortPath(refineAgentBin) })}
              {:else if agentProbe[refineAgent]}
                {t("ai.aing.agent.found", { path: shortPath(agentProbe[refineAgent] ?? "") })}
              {:else if refineAgent in agentProbe}
                <span class="desc-warn">{t("ai.aing.agent.notFound")}</span>
              {:else}
                {t("ai.aing.agent.detecting")}
              {/if}
            </span>
          </div>
          <div class="seg">
            {#each AGENT_OPTIONS as a (a.key)}
              <label class="seg-item">
                <input type="radio" name="refine-agent" value={a.key} bind:group={refineAgent} onchange={saveRefineAgent} />
                {a.label}
              </label>
            {/each}
          </div>
        </div>
        <div class="row">
          <div class="row-info">
            <span class="row-label">{t("ai.aing.model.label")}</span>
            <span class="row-desc">{t("ai.aing.model.defaultDesc", { label: selectedAgentOption.label })}</span>
          </div>
          <input
            class="row-input"
            placeholder={t(selectedAgentOption.modelHint)}
            bind:value={refineAgentModel}
            onblur={saveRefineAgent}
            oninput={() => (agentTest = null)}
          />
        </div>
        <div class="row">
          <div class="row-info">
            <span class="row-label">{t("ai.aing.cliPath.label")}</span>
            <span class="row-desc">{t("ai.aing.cliPath.desc")}</span>
          </div>
          <input
            class="row-input wide"
            placeholder={t("ai.aing.cliPath.placeholder")}
            bind:value={refineAgentBin}
            onblur={saveRefineAgent}
            oninput={() => (agentTest = null)}
          />
        </div>
        <div class="row">
          <div class="row-info">
            <span class="row-label">{t("ai.aing.testRun.label")}</span>
            <span class="row-desc">{t("ai.aing.testRun.desc")}</span>
          </div>
          <button class="btn-secondary" onclick={runAgentTest} disabled={agentTesting || agentMissing}>
            {agentTesting ? t("ai.aing.testing") : t("ai.aing.test")}
          </button>
        </div>
        {#if agentTest}
          <p class="test-result" class:ok={agentTest.ok} class:err={!agentTest.ok}>
            {agentTest.ok ? t("ai.aing.testOk", { msg: agentTest.msg }) : t("ai.aing.testFail", { msg: agentTest.msg })}
          </p>
        {/if}
        <p class="config-hint">{t("ai.aing.agentFailNote")}</p>
      {:else}
        <!-- 服务商 tab(2026-08-11 用户拍板:去掉「一键填充」说明行,直接 tab 切换):
             选中页 = 下方表单正在编辑的那家;绿点 = 该家已存过密钥,切回即回填。 -->
        <div class="provider-tabs" role="tablist">
          {#each REFINE_PRESETS as p (p.label)}
            <button
              role="tab"
              aria-selected={activeProviderTab === p.label}
              class="ptab"
              class:active={activeProviderTab === p.label}
              title={refineDrafts[p.label]?.key ? t("ai.aing.preset.savedTitle") : undefined}
              onclick={() => selectProviderTab(p.label)}
            >
              {p.labelKey ? t(p.labelKey) : p.label}
              {#if refineDrafts[p.label]?.key}<span class="chip-dot"></span>{/if}
            </button>
          {/each}
          <button
            role="tab"
            aria-selected={activeProviderTab === "custom"}
            class="ptab"
            class:active={activeProviderTab === "custom"}
            title={refineDrafts["custom"]?.key ? t("ai.aing.preset.savedTitle") : undefined}
            onclick={() => selectProviderTab("custom")}
          >
            {t("ai.aing.tab.custom")}
            {#if refineDrafts["custom"]?.key}<span class="chip-dot"></span>{/if}
          </button>
        </div>
        <div class="row">
          <div class="row-info">
            <span class="row-label">{t("ai.aing.baseUrl.label")}</span>
            <span class="row-desc">{t("ai.aing.baseUrl.desc")}</span>
          </div>
          <input
            class="row-input wide"
            placeholder="https://api.deepseek.com/v1"
            bind:value={refineBaseUrl}
            onblur={saveRefine}
            oninput={() => (llmTest = null)}
          />
        </div>
        <div class="row">
          <div class="row-info">
            <span class="row-label">{activePreset?.modelLabel ? t(activePreset.modelLabel) : t("ai.aing.model.label")}</span>
            <span class="row-desc">{activePreset?.modelDesc ? t(activePreset.modelDesc) : t("ai.aing.model.desc")}</span>
          </div>
          <input
            class="row-input"
            placeholder={activePreset?.modelPlaceholder ?? "deepseek-chat"}
            bind:value={refineModel}
            onblur={saveRefine}
            oninput={() => (llmTest = null)}
          />
        </div>
        <div class="row">
          <div class="row-info">
            <span class="row-label">API Key</span>
            <span class="row-desc">{t("ai.aing.apiKey.desc")}</span>
          </div>
          <input
            class="row-input wide"
            type="password"
            placeholder="sk-..."
            bind:value={refineKey}
            onblur={saveRefine}
            oninput={() => (llmTest = null)}
          />
        </div>
        <div class="row">
          <div class="row-info">
            <span class="row-label">{t("ai.aing.testConn.label")}</span>
            <span class="row-desc">{t("ai.aing.testConn.desc")}</span>
          </div>
          <button class="btn-secondary" onclick={runLlmTest} disabled={llmTesting || llmMissing}>
            {llmTesting ? t("ai.aing.testing") : t("ai.aing.test")}
          </button>
        </div>
        {#if llmTest}
          <p class="test-result" class:ok={llmTest.ok} class:err={!llmTest.ok}>
            {llmTest.ok ? t("ai.aing.testOk", { msg: llmTest.msg }) : t("ai.aing.testFail", { msg: llmTest.msg })}
          </p>
        {/if}
        {#if !refineBaseUrl || !refineModel || !refineKey}
          <p class="config-hint">{t("ai.aing.configHint")}</p>
        {/if}
      {/if}
    </div>
  </section>

  <!-- —— AI 助手接入(MCP) —— -->
  <section id="assistant-connect" class="anchor-section" class:guide-target={guideActive && guideStep === 1}>
    <h2 class="section-title">{t("ai.mcp.sectionTitle")}</h2>
    <div class="rows">
      {#if mcpError}
        <div class="banner warn">{mcpError}</div>
      {/if}
      {#each mcpAgents as a (a.key)}
        <div class="row">
          <div class="row-info">
            <span class="row-label">{a.name}</span>
            <span class="row-desc">
              {#if !a.installed && !a.registered}{t("ai.mcp.status.notInstalled")}
              {:else if a.stale}{t("ai.mcp.status.stale")}
              {:else if a.registered}{t("ai.mcp.status.registered")}
              {:else}{t("ai.mcp.status.unregistered")}{/if}
            </span>
          </div>
          {#if a.installed || a.registered}
            <button class="btn-secondary" disabled={mcpBusy === a.key} onclick={() => mcpToggleRegister(a)}>
              {a.registered ? t("ai.action.remove") : t("ai.mcp.register")}
            </button>
          {/if}
        </div>
      {/each}
      <div class="row">
        <div class="row-info">
          <span class="row-label-line">
            <span class="row-label">{t("ai.mcp.skill.label")}</span>
            {#if skillState === "current"}<span class="pill">{t("ai.mcp.skill.current")}</span>
            {:else if skillState === "stale"}<span class="pill warn">{t("ai.mcp.skill.stale")}</span>
            {:else if skillState === "unmanaged"}<span class="pill">{t("ai.mcp.skill.unmanaged")}</span>
            {/if}
          </span>
          <span class="row-desc">
            {#if skillState === "current"}{t("ai.mcp.skill.currentDesc")}
            {:else if skillState === "stale"}{t("ai.mcp.skill.staleDesc")}
            {:else if skillState === "unmanaged"}{t("ai.mcp.skill.unmanagedDesc")}
            {:else}{t("ai.mcp.skill.installDesc")}
            {/if}
          </span>
        </div>
        {#if skillState !== null}
          <div class="row-actions">
            <button class="btn-secondary" disabled={skillEditBusy || skillBusy} onclick={() => (skillEditOpen ? (skillEditOpen = false) : openSkillEdit())}>
              {t("ai.mcp.skill.viewEdit")}
            </button>
            {#if skillState !== "unmanaged"}
              <!-- 忙时禁用而非消失(原可见性语义);加 skillEditBusy 与编辑卡操作互斥,防竞态 -->
              <button class="btn-secondary" disabled={skillBusy || skillEditBusy} onclick={toggleSkill}>
                {skillState === "not_installed" ? t("ai.action.install") : t("ai.action.remove")}
              </button>
            {/if}
          </div>
        {/if}
      </div>
      {#if skillEditOpen}
        <div class="config">
          <textarea
            class="skill-textarea mono"
            bind:value={skillContent}
            spellcheck="false"
            disabled={skillEditBusy}
          ></textarea>
          <div class="skill-edit-actions">
            <div class="skill-edit-buttons">
              <!-- 保存/恢复默认加 skillBusy:与行上「安装/移除」互斥,防止卸载进行中把旧内容写回 -->
              <button class="btn-secondary" disabled={skillEditBusy || skillBusy} onclick={saveSkillEdit}>{t("ai.action.save")}</button>
              <button class="btn-secondary" disabled={skillEditBusy || skillBusy} onclick={restoreSkillDefault}>{t("ai.mcp.skill.restore")}</button>
              <button class="btn-secondary" disabled={skillEditBusy} onclick={() => (skillEditOpen = false)}>{t("ai.action.collapse")}</button>
            </div>
            <p class="config-hint">{t("ai.mcp.skill.savedHint")}</p>
          </div>
        </div>
      {/if}
      <label class="row">
        <div class="row-info">
          <span class="row-label">{t("ai.mcp.allowControl.label")}</span>
          <span class="row-desc">{t("ai.mcp.allowControl.desc")}</span>
        </div>
        <input type="checkbox" class="ctl switch" bind:checked={mcpAllowControl} disabled={!settings} onchange={saveMcpAllowControl} />
      </label>
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("ai.mcp.manual.label")}</span>
          <span class="row-desc">{t("ai.mcp.manual.desc")}</span>
        </div>
        <button class="btn-secondary" onclick={() => (mcpSnippetOpen = !mcpSnippetOpen)}>
          {mcpSnippetOpen ? t("ai.action.collapse") : t("ai.action.view")}
        </button>
      </div>
      {#if mcpSnippetOpen}
        <div class="config">
          <pre class="snippet">{mcpSnippet}</pre>
          <button class="btn-secondary" onclick={copyMcpSnippet}>{t("ai.action.copy")}</button>
        </div>
      {/if}
      {#if mcpHealed > 0}
        <p class="config-hint">{t("ai.mcp.healed", { n: mcpHealed })}</p>
      {/if}
      <p class="config-hint">
        {t("ai.mcp.privacyNote")}
        <button class="link" onclick={openMcpReadme}>{t("ai.mcp.readme")}</button>
      </p>
    </div>
  </section>

  <!-- —— Agent 能调用什么(MCP 工具 + CLI 命令清单,与后端 catalog 同源,纯只读展示) —— -->
  <section id="agent-capabilities" class="anchor-section" class:guide-target={guideActive && guideStep === 2}>
    <h2 class="section-title">{t("ai.cap.sectionTitle")}</h2>
    <div class="rows">
      {#if capError}
        <div class="banner warn">{capError}</div>
      {/if}
      {#if capabilities}
        <div class="group-title">{t("ai.cap.tools")}</div>
        <!-- 2026-08-11 用户拍板去掉 gate 徽章(「需应用运行/已允许控制」这类状态标签
             对用户是噪声):正常状态零标注;唯一值得说的是「控制类工具因上方开关关闭
             而不可用」——用整行置灰 + 描述后缀一句话指路,开关一开即恢复常态。 -->
        {#each capabilities.tools as tool (tool.name)}
          {@const controlLocked = tool.gate === "control" && !mcpAllowControl}
          <div class="row" class:dimmed={controlLocked}>
            <div class="row-info">
              <span class="row-label mono">{tool.name}</span>
              <span class="row-desc">
                {tool.desc}
                {#if controlLocked}{t("ai.cap.controlLockedHint")}{/if}
              </span>
            </div>
          </div>
        {/each}
        <div class="group-title">{t("ai.cap.cli")}</div>
        {#each capabilities.cli as c (c.cmd)}
          <div class="row">
            <div class="row-info">
              <span class="row-label mono">{c.cmd}</span>
              <span class="row-desc">{c.desc}</span>
            </div>
          </div>
        {/each}
      {/if}
    </div>
  </section>

  <!-- —— AI 调用日志:入口行,浏览/导出/打开目录在独立页 /ai/logs —— -->
  <section>
    <h2 class="section-title">{t("ai.relations.sectionTitle")}</h2>
    <div class="rows">
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("ai.relations.label")}</span>
          <span class="row-desc">{t("ai.relations.desc")}</span>
        </div>
        <button class="btn-secondary" onclick={() => (backfillOpen = true)}>{t("ai.relations.analyze")}</button>
      </div>
    </div>
  </section>

  <section>
    <h2 class="section-title">{t("ai.logs.sectionTitle")}</h2>
    <div class="rows">
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("ai.logs.entry.label")}</span>
          <span class="row-desc">
            {t("ai.logs.entry.desc")}{aiLogsTotal > 0 ? t("ai.logs.entry.count", { n: aiLogsTotal }) : ""}
          </span>
        </div>
        <button class="btn-secondary" onclick={() => goto("/ai/logs")}>{t("ai.action.view")}</button>
      </div>
    </div>
  </section>
</div>

<RelationBackfillDialog open={backfillOpen} onClose={() => (backfillOpen = false)} />

{#if guideActive && currentGuide}
  <aside
    class="guide-coach"
    class:placed-right={guidePlacement === "right"}
    class:placed-below={guidePlacement === "below"}
    style={guidePosition}
    aria-live="polite"
  >
    <div>
      <span class="guide-eyebrow">{t(currentGuide.eyebrow)}</span>
      <strong>{t(currentGuide.title)}</strong>
      <p>{t(currentGuide.body)}</p>
    </div>
    <div class="guide-actions">
      <button class="text-action" onclick={() => completeGuide(false)}>{t("ai.guide.skip")}</button>
      {#if guideStep === GUIDE_STEPS.length - 1}
        <button class="btn-secondary" onclick={() => completeGuide(true)}>{t("ai.guide.hooksNext")}</button>
        <button class="guide-next" onclick={nextGuideStep}>{t("ai.guide.done")}</button>
      {:else}
        <button class="guide-next" onclick={nextGuideStep}>{t("ai.guide.next")}</button>
      {/if}
    </div>
  </aside>
{/if}

<style>
  .page { padding: 0 1.5rem 2rem; }
  .topbar { position: sticky; top: 0; background: var(--canvas); padding: 1.1rem 0 0.6rem; }
  h1 { font-size: 1.15rem; font-weight: 600; margin: 0; }

  section {
    margin-top: 1.3rem;
  }
  .anchor-section { scroll-margin-top: 1rem; }
  .guide-target {
    position: relative;
    z-index: 21;
    border-radius: var(--radius-xl);
    outline: 2px solid var(--accent);
    outline-offset: 7px;
  }
  .guide-target::after {
    content: "";
    position: absolute;
    inset: -9px;
    z-index: -1;
    border-radius: var(--radius-xl);
    background: var(--accent-tint);
    pointer-events: none;
  }
  .guide-coach {
    position: fixed;
    z-index: 30;
    padding: 0.9rem 1rem 0.8rem;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-xl);
    background: var(--canvas);
    box-shadow: var(--shadow-popover);
  }
  .guide-coach::before,
  .guide-coach::after {
    content: "";
    position: absolute;
    width: 0;
    height: 0;
    border-style: solid;
  }
  .guide-coach.placed-right::before {
    left: -9px; top: 24px;
    border-width: 8px 9px 8px 0;
    border-color: transparent var(--hairline-strong) transparent transparent;
  }
  .guide-coach.placed-right::after {
    left: -7px; top: 25px;
    border-width: 7px 8px 7px 0;
    border-color: transparent var(--canvas) transparent transparent;
  }
  .guide-coach.placed-below::before {
    left: 24px; top: -9px;
    border-width: 0 8px 9px;
    border-color: transparent transparent var(--hairline-strong);
  }
  .guide-coach.placed-below::after {
    left: 25px; top: -7px;
    border-width: 0 7px 8px;
    border-color: transparent transparent var(--canvas);
  }
  .guide-coach strong { display: block; margin-top: 0.12rem; font-size: 0.92rem; }
  .guide-coach p { margin: 0.25rem 0 0; color: var(--ink-secondary); font-size: 0.8rem; line-height: 1.5; }
  .guide-eyebrow { color: var(--accent); font-size: 0.72rem; }
  .guide-actions {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 0.65rem;
    margin-top: 0.75rem;
  }
  .guide-next {
    border: 1px solid transparent;
    border-radius: var(--radius-full);
    background: var(--primary);
    color: var(--on-primary);
    padding: 0.48em 1.15em;
    font-size: 0.82rem;
    cursor: pointer;
    box-shadow: var(--shadow-btn);
  }
  .quickstart {
    background: var(--surface);
    border-radius: var(--radius-lg);
    overflow: hidden;
    margin-top: 0.75rem;
  }
  .quickstart-heading {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 1rem;
    padding: 1rem;
    border-bottom: 1px solid var(--hairline);
  }
  .quickstart-heading h2 { margin: 0; font-size: 1rem; font-weight: 600; }
  .quickstart-heading p {
    margin: 0.25rem 0 0;
    max-width: 37rem;
    color: var(--ink-secondary);
    font-size: 0.82rem;
    line-height: 1.5;
  }
  .local-badge {
    color: var(--success);
    font-size: 0.74rem;
  }
  .quickstart-heading-actions {
    flex: none;
    display: flex;
    align-items: center;
    gap: 0.7rem;
  }
  .quickstart-path {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
  }
  .quickstep {
    display: grid;
    grid-template-columns: 1.4rem 1fr;
    gap: 0.55rem;
    padding: 0.9rem 1rem;
    border-right: 1px solid var(--hairline);
  }
  .quickstep:last-child { border-right: none; }
  .quickstep-no {
    display: grid;
    place-items: center;
    width: 1.25rem;
    height: 1.25rem;
    border-radius: var(--radius-full);
    background: var(--primary);
    color: var(--on-primary);
    font-size: 0.68rem;
  }
  .quickstep strong { font-size: 0.86rem; font-weight: 600; }
  .quickstep p {
    margin: 0.25rem 0 0.4rem;
    color: var(--ink-secondary);
    font-size: 0.78rem;
    line-height: 1.5;
  }
  .quickstep .starter-prompt { color: var(--ink); }
  .text-action {
    border: none;
    background: none;
    color: var(--accent);
    padding: 0;
    font: inherit;
    font-size: 0.78rem;
    cursor: pointer;
  }
  .text-action:hover { color: var(--accent-pressed); text-decoration: underline; }
  .quickstart-foot {
    margin: 0;
    padding: 0.65rem 1rem;
    border-top: 1px solid var(--hairline);
    color: var(--ink-faint);
    font-size: 0.78rem;
  }
  @media (max-width: 760px) {
    .quickstart-heading { display: block; }
    .quickstart-heading-actions { margin-top: 0.65rem; justify-content: space-between; }
    .quickstart-path { grid-template-columns: 1fr; }
    .quickstep { border-right: none; border-bottom: 1px solid var(--hairline); }
    .quickstep:last-child { border-bottom: none; }
    .guide-coach {
      max-width: calc(100vw - 1.5rem);
    }
  }
  /* 区块标题(settings 页 .section-title 同款):卡片上方的次级标题,只用于新增的
     「Agent 能调用什么」区块——既有区块靠位置隐含上下文,不追加改动。 */
  .section-title {
    font-size: 0.82rem;
    font-weight: 500;
    color: var(--ink-secondary);
    margin: 0 0 0.45rem;
  }
  /* 卡片内的分组小标题(MCP 工具 / CLI 命令):不是 .row,不参与 hairline 分隔逻辑。 */
  .group-title {
    padding: 0.6rem 1rem 0.2rem;
    font-size: 0.78rem;
    font-weight: 600;
    color: var(--ink-faint);
  }
  /* 设置行卡片(macOS 系统设置式):surface 底承载各行,行间 hairline 分隔,
     左标题+一行说明、右侧控件;label 行整行可点切换开关 */
  .rows {
    background: var(--surface);
    border-radius: var(--radius-lg);
    overflow: hidden;
  }
  .row {
    display: flex;
    align-items: center;
    /* 默认窗宽(800,内容区 ~500px)下右侧控件簇可能放不下:允许换行,
       控件整体落到下一行(margin-left:auto 保持右对齐),绝不把左侧说明列
       压成竖排单字。 */
    flex-wrap: wrap;
    gap: 0.5rem 0.9rem;
    padding: 0.55rem 1rem;
    border-bottom: 1px solid var(--hairline);
  }
  .rows > :last-child,
  .rows .row:last-child {
    border-bottom: none;
  }
  label.row {
    cursor: pointer;
  }
  .row-info {
    flex: 1;
    /* 保底宽:窄窗时宁可让右侧控件换行,也不许说明列被压扁 */
    min-width: 11rem;
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
  }
  .row-label {
    font-size: 0.92rem;
    color: var(--ink);
  }
  .row-desc {
    font-size: 0.8rem;
    color: var(--ink-secondary);
    line-height: 1.4;
  }
  /* 行标题 + 状态徽章同一行(技能行的四态徽章用) */
  .row-label-line {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  /* 等宽:工具名/CLI 命令/技能正文,与其余说明文字区分 */
  .mono {
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  }
  /* 多按钮并排的行尾控件(技能行:查看/编辑 + 安装/移除) */
  .row-actions {
    display: flex;
    flex: none;
    gap: 0.4rem;
  }
  /* 徽章:soft 底 + 中性文字色,micro 字级(尺寸沿用说话人徽章的形态,颜色改中性/warning 语义) */
  /* 控制类工具在「允许控制」关闭时整行置灰:状态用视觉表达,不用术语徽章。 */
  .row.dimmed {
    opacity: 0.45;
  }
  .pill {
    flex: none;
    font-size: 0.78rem;
    font-weight: 500;
    border-radius: var(--radius-sm);
    padding: 0.1em 0.5em;
    background: var(--surface-soft);
    color: var(--ink-secondary);
    border: 1px solid var(--hairline);
    white-space: nowrap;
  }
  .pill.warn {
    background: var(--warning-tint);
    color: var(--warning-ink);
    border-color: var(--warning-line);
  }
  /* 右侧控件 */
  .ctl {
    flex: none;
    margin: 0;
  }
  /* button-secondary */
  .btn-secondary {
    flex: none;
    border-radius: var(--radius-md);
    border: 1px solid var(--hairline-strong);
    padding: 0.35em 0.9em;
    font-size: 0.85rem;
    font-weight: 500;
    cursor: pointer;
    background: transparent;
    color: var(--ink);
  }
  .btn-secondary:hover {
    background: var(--surface-soft);
  }
  .btn-secondary:disabled {
    opacity: 0.5;
    cursor: default;
    background: transparent;
  }
  /* button-link:详见 README */
  .link {
    background: none;
    border: none;
    font: inherit;
    font-size: 0.85rem;
    color: var(--accent);
    cursor: pointer;
    padding: 0.2em 0.3em;
  }
  .link:hover {
    text-decoration: underline;
  }
  /* banner:错误用 danger 色系,提示用 warning 色系 */
  .banner {
    background: var(--danger-tint);
    border: 1px solid var(--danger-line);
    color: var(--danger-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.9rem;
  }
  .banner.warn {
    background: var(--warning-tint);
    border-color: var(--warning-line);
    color: var(--warning-ink);
    margin: 0.6rem 0 0;
  }
  /* 卡片内嵌面板(skill 查看/编辑卡、手动配置折叠卡用) */
  .config {
    display: flex;
    flex-direction: column;
    gap: 0.7rem;
    padding: 0.8rem 1rem 0.9rem;
  }
  /* 服务商 tab 条:横贯配置区,底部 hairline,选中页 accent 下划线——
     「下方表单属于哪家」一目了然;各页草稿独立,切换不丢。 */
  .provider-tabs {
    display: flex;
    align-items: stretch;
    gap: 1.15rem;
    border-bottom: 1px solid var(--hairline-strong);
    flex-wrap: wrap;
  }
  .ptab {
    display: inline-flex;
    align-items: center;
    gap: 0.4em;
    border: none;
    background: none;
    padding: 0.45em 0.15em;
    margin-bottom: -1px; /* 下划线压住 tab 条 hairline */
    font-size: 0.88rem;
    font-weight: 500;
    color: var(--ink-secondary);
    cursor: pointer;
    border-bottom: 2px solid transparent;
    transition: color 0.15s ease, border-color 0.15s ease;
  }
  .ptab:hover {
    color: var(--ink);
  }
  .ptab.active {
    color: var(--ink);
    border-bottom-color: var(--accent);
  }
  /* 已存密钥标记:小绿点,含义由 title 提示补全 */
  .chip-dot {
    width: 6px;
    height: 6px;
    border-radius: var(--radius-full);
    background: var(--success, var(--accent));
    flex: none;
  }
  .config-hint {
    font-size: 0.8rem;
    color: var(--ink-faint);
    margin: 0;
  }
  .test-result { font-size: 0.85rem; margin: 0.4rem 0 0.2rem; }
  .test-result.ok { color: var(--success, var(--ink-secondary)); }
  .test-result.err { color: var(--danger-ink); }
  /* 分段单选(与设置页 .seg 同一控件语言);margin-left:auto 保证窄窗换行后仍右对齐 */
  .seg {
    display: flex;
    gap: 2px;
    flex: none;
    margin-left: auto;
    background: var(--surface-press);
    border-radius: var(--radius-md);
    padding: 2px;
  }
  .seg-item {
    position: relative;
    padding: 0.26em 0.7em;
    font-size: 0.85rem;
    font-weight: 500;
    color: var(--ink-secondary);
    border-radius: calc(var(--radius-md) - 2px);
    cursor: pointer;
    white-space: nowrap;
  }
  .seg-item:hover {
    color: var(--ink);
  }
  .seg-item:has(input:checked) {
    background: var(--canvas);
    color: var(--ink);
    box-shadow: var(--shadow-btn);
  }
  .seg-item input {
    position: absolute;
    opacity: 0;
    pointer-events: none;
  }
  /* 行内输入(settings-row 右侧控件版 input:surface-press 底、无边,聚焦浮出 canvas + accent 环) */
  .row-input {
    flex: none;
    width: 11rem;
    margin-left: auto;
    box-sizing: border-box;
    padding: 0.32em 0.6em;
    border: none;
    border-radius: var(--radius-md);
    background: var(--surface-press);
    color: var(--ink);
    font-size: 0.85rem;
  }
  .row-input.wide {
    width: 18rem;
  }
  .row-input:focus {
    outline: none;
    background: var(--canvas);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .row-input::placeholder {
    color: var(--ink-faint);
  }
  .desc-warn {
    color: var(--warning-ink);
  }
  .snippet {
    margin: 0 0 0.5rem;
    padding: 0.6rem 0.8rem;
    background: var(--surface-soft);
    border-radius: var(--radius-sm);
    font-size: 0.8rem;
    overflow-x: auto;
    user-select: text;
  }
  /* 技能查看/编辑卡:.snippet 同族(surface-soft 底、radius-sm),可拖高、等宽字体 */
  .skill-textarea {
    box-sizing: border-box;
    width: 100%;
    height: 360px;
    margin: 0 0 0.5rem;
    padding: 0.6rem 0.8rem;
    background: var(--surface-soft);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-sm);
    color: var(--ink);
    font-size: 0.8rem;
    line-height: 1.5;
    resize: vertical;
  }
  .skill-textarea:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .skill-textarea:disabled {
    opacity: 0.7;
  }
  .skill-edit-actions {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .skill-edit-buttons {
    display: flex;
    gap: 0.4rem;
    flex-wrap: wrap;
  }
</style>
