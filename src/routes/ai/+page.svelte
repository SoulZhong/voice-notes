<script lang="ts">
  // AI 页:智能精修大模型配置 + AI 助手接入(Task 2 自设置页迁入)。
  import { onMount } from "svelte";
  import { getSettings, setSettings, type Settings } from "$lib/models";
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

  let settings = $state<Settings | null>(null);
  /** danger 横幅：本页保存类操作的错误统一在此显示(精简自设置页的全局 error 横幅)。 */
  let error = $state("");

  /** 智能精修:接口三字段的本地镜像(失败回弹靠本地 state 强制 DOM 对齐)。开关已移至设置页「录制」区。 */
  let refineBaseUrl = $state("");
  let refineModel = $state("");
  let refineKey = $state("");
  const REFINE_PRESETS = [
    { label: "DeepSeek", base: "https://api.deepseek.com/v1", model: "deepseek-chat" },
    { label: "通义千问", base: "https://dashscope.aliyuncs.com/compatible-mode/v1", model: "qwen-plus" },
    { label: "豆包", base: "https://ark.cn-beijing.volces.com/api/v3", model: "doubao-seed-1-6-250615" },
    { label: "Kimi", base: "https://api.moonshot.cn/v1", model: "moonshot-v1-auto" },
    { label: "OpenAI", base: "https://api.openai.com/v1", model: "gpt-4o-mini" },
  ];

  // —— 精修执行体:在线接口(openai) / 本机 Agent CLI(agent,经 MCP 读写回) ——
  let refineProvider = $state("openai");
  let refineAgent = $state("claude");
  let refineAgentBin = $state("");
  let refineAgentModel = $state("");
  /** 四家 CLI 探测结果(key → 路径或 null);onMount 拉一次,切到 agent 模式时展示。 */
  let agentProbe = $state<Record<string, string | null>>({});
  const AGENT_OPTIONS = [
    { key: "claude", label: "Claude Code", modelHint: "如 haiku、sonnet" },
    { key: "codex", label: "Codex", modelHint: "如 gpt-5-codex" },
    { key: "gemini", label: "Gemini", modelHint: "如 gemini-2.5-flash" },
    { key: "cursor", label: "Cursor", modelHint: "如 sonnet-4.5" },
  ];
  const selectedAgentOption = $derived(AGENT_OPTIONS.find((a) => a.key === refineAgent) ?? AGENT_OPTIONS[0]);

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

  onMount(() => {
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
      error = `保存失败: ${e}`;
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

  function applyPreset(p: { base: string; model: string }) {
    refineBaseUrl = p.base;
    refineModel = p.model;
    saveRefine();
  }
  function saveRefine() {
    saveSetting((s) => {
      s.refine_base_url = refineBaseUrl.trim();
      s.refine_model = refineModel.trim();
      s.refine_api_key = refineKey.trim();
    });
  }
  function saveRefineAgent() {
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
    if (!confirm("将覆盖当前内容，恢复为应用内置版本？")) return;
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
        if (r && !r.ok) mcpError = `${a.name}: ${r.error ?? "注册失败"}`;
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
      mcpError = "复制失败,请展开后手动选择文本复制";
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

  <!-- —— 智能精修 —— -->
  <section>
    <div class="rows">
      <div class="config">
        <div class="preset-row">
          <span class="preset-label">执行方式</span>
          <div class="seg">
            <label class="seg-item">
              <input type="radio" name="refine-provider" value="openai" bind:group={refineProvider} onchange={saveRefineAgent} />在线接口
            </label>
            <label class="seg-item">
              <input type="radio" name="refine-provider" value="agent" bind:group={refineProvider} onchange={saveRefineAgent} />本机 Agent
            </label>
          </div>
        </div>
        {#if refineProvider === "agent"}
          <div class="preset-row">
            <span class="preset-label">Agent</span>
            <div class="seg">
              {#each AGENT_OPTIONS as a (a.key)}
                <label class="seg-item">
                  <input type="radio" name="refine-agent" value={a.key} bind:group={refineAgent} onchange={saveRefineAgent} />
                  {a.label}
                </label>
              {/each}
            </div>
            {#if refineAgentBin.trim()}
              <span class="agent-status">使用指定路径</span>
            {:else if agentProbe[refineAgent]}
              <span class="agent-status">已找到 {agentProbe[refineAgent]}</span>
            {:else if refineAgent in agentProbe}
              <span class="agent-status warn">未找到命令行工具</span>
            {/if}
          </div>
          <div class="refine-fields agent-fields">
            <label class="field">
              <span>模型</span>
              <input placeholder="留空用默认,{selectedAgentOption.modelHint}" bind:value={refineAgentModel} onblur={saveRefineAgent} />
            </label>
            <label class="field">
              <span>CLI 路径</span>
              <input placeholder="留空自动探测" bind:value={refineAgentBin} onblur={saveRefineAgent} />
            </label>
          </div>
          <p class="config-hint">
            {#if refineAgentBin.trim() || agentProbe[refineAgent]}
              无需 API Key,精修用 {selectedAgentOption.label} 本机已登录的账号和额度;未登录时精修会失败并保留原文。
            {:else}
              请先安装并登录 {selectedAgentOption.label},或在上方填写它的可执行文件路径。
            {/if}
          </p>
        {:else}
          <div class="preset-row">
            <span class="preset-label">一键填充</span>
            {#each REFINE_PRESETS as p (p.label)}
              <button class="btn-secondary" onclick={() => applyPreset(p)}>{p.label}</button>
            {/each}
          </div>
          <div class="refine-fields">
            <label class="field">
              <span>接口地址</span>
              <input placeholder="https://api.deepseek.com/v1" bind:value={refineBaseUrl} onblur={saveRefine} />
            </label>
            <label class="field">
              <span>模型</span>
              <input placeholder="deepseek-chat" bind:value={refineModel} onblur={saveRefine} />
            </label>
            <label class="field">
              <span>API Key</span>
              <input type="password" placeholder="sk-..." bind:value={refineKey} onblur={saveRefine} />
            </label>
          </div>
          {#if !refineBaseUrl || !refineModel || !refineKey}
            <p class="config-hint">三项配齐后生效;Key 只保存在本机。</p>
          {/if}
        {/if}
      </div>
    </div>
  </section>

  <!-- —— AI 助手接入(MCP) —— -->
  <section>
    <div class="rows">
      {#if mcpError}
        <div class="banner warn">{mcpError}</div>
      {/if}
      {#each mcpAgents as a (a.key)}
        <div class="row">
          <div class="row-info">
            <span class="row-label">{a.name}</span>
            <span class="row-desc">
              {#if !a.installed && !a.registered}未检测到安装
              {:else if a.stale}已注册(路径已由自愈修复或待修复)
              {:else if a.registered}已注册
              {:else}未注册{/if}
            </span>
          </div>
          {#if a.installed || a.registered}
            <button class="btn-secondary" disabled={mcpBusy === a.key} onclick={() => mcpToggleRegister(a)}>
              {a.registered ? "移除" : "注册"}
            </button>
          {/if}
        </div>
      {/each}
      <div class="row">
        <div class="row-info">
          <span class="row-label-line">
            <span class="row-label">Claude Code 技能</span>
            {#if skillState === "current"}<span class="pill">当前版本</span>
            {:else if skillState === "stale"}<span class="pill warn">待更新</span>
            {:else if skillState === "unmanaged"}<span class="pill">已自定义</span>
            {/if}
          </span>
          <span class="row-desc">
            {#if skillState === "current"}已安装:Claude 掌握会议纪要/周报/检索工作流
            {:else if skillState === "stale"}已安装(旧版,应用启动时自动更新)
            {:else if skillState === "unmanaged"}检测到自定义同名技能,不自动管理
            {:else}让 Claude Code 掌握会议纪要/周报/检索工作流(写入 ~/.claude/skills)
            {/if}
          </span>
        </div>
        {#if skillState !== null}
          <div class="row-actions">
            <button class="btn-secondary" disabled={skillEditBusy || skillBusy} onclick={() => (skillEditOpen ? (skillEditOpen = false) : openSkillEdit())}>
              查看 / 编辑
            </button>
            {#if skillState !== "unmanaged"}
              <!-- 忙时禁用而非消失(原可见性语义);加 skillEditBusy 与编辑卡操作互斥,防竞态 -->
              <button class="btn-secondary" disabled={skillBusy || skillEditBusy} onclick={toggleSkill}>
                {skillState === "not_installed" ? "安装" : "移除"}
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
              <button class="btn-secondary" disabled={skillEditBusy || skillBusy} onclick={saveSkillEdit}>保存</button>
              <button class="btn-secondary" disabled={skillEditBusy || skillBusy} onclick={restoreSkillDefault}>恢复默认</button>
              <button class="btn-secondary" disabled={skillEditBusy} onclick={() => (skillEditOpen = false)}>收起</button>
            </div>
            <p class="config-hint">保存后应用升级不再自动更新此文件</p>
          </div>
        </div>
      {/if}
      <label class="row">
        <div class="row-info">
          <span class="row-label">允许 AI 控制录制</span>
          <span class="row-desc">开启后,已接入的 AI 助手可远程开始/停止/暂停录制。默认关闭</span>
        </div>
        <input type="checkbox" class="ctl switch" bind:checked={mcpAllowControl} disabled={!settings} onchange={saveMcpAllowControl} />
      </label>
      <div class="row">
        <div class="row-info">
          <span class="row-label">手动配置</span>
          <span class="row-desc">未内置的 Agent(Windsurf/Cline 等)把左侧片段加进其 MCP 配置即可</span>
        </div>
        <button class="btn-secondary" onclick={() => (mcpSnippetOpen = !mcpSnippetOpen)}>
          {mcpSnippetOpen ? "收起" : "查看"}
        </button>
      </div>
      {#if mcpSnippetOpen}
        <div class="config">
          <pre class="snippet">{mcpSnippet}</pre>
          <button class="btn-secondary" onclick={copyMcpSnippet}>复制</button>
        </div>
      {/if}
      {#if mcpHealed > 0}
        <p class="config-hint">应用位置变更:已自动更新 {mcpHealed} 个 AI 助手的注册路径。</p>
      {/if}
      <p class="config-hint">
        笔记内容经 AI 助手检索后会进入其模型上下文;本应用自身不联网上传任何内容。
        <button class="link" onclick={openMcpReadme}>详见 README</button>
      </p>
    </div>
  </section>

  <!-- —— Agent 能调用什么(MCP 工具 + CLI 命令清单,与后端 catalog 同源,纯只读展示) —— -->
  <section>
    <h2 class="section-title">Agent 能调用什么</h2>
    <div class="rows">
      {#if capError}
        <div class="banner warn">{capError}</div>
      {/if}
      {#if capabilities}
        <div class="group-title">MCP 工具</div>
        {#each capabilities.tools as t (t.name)}
          <div class="row">
            <div class="row-info">
              <span class="row-label mono">{t.name}</span>
              <span class="row-desc">{t.desc}</span>
            </div>
            {#if t.gate === "app"}<span class="pill">需应用运行</span>
            {:else if t.gate === "control"}<span class="pill warn">需允许控制</span>
            {/if}
          </div>
        {/each}
        <div class="group-title">CLI 命令</div>
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
</div>

<style>
  .page { padding: 0 1.5rem 2rem; }
  .topbar { position: sticky; top: 0; background: var(--canvas); padding: 1.1rem 0 0.6rem; }
  h1 { font-size: 1.15rem; font-weight: 600; margin: 0; }

  section {
    margin-top: 1.3rem;
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
    gap: 0.9rem;
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
    min-width: 0;
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
  /* 大模型接口配置块:卡片内嵌面板(常显;启用开关在 设置 → 录制) */
  .config {
    display: flex;
    flex-direction: column;
    gap: 0.7rem;
    padding: 0.8rem 1rem 0.9rem;
  }
  .preset-row {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    flex-wrap: wrap;
  }
  .preset-label {
    font-size: 0.8rem;
    color: var(--ink-faint);
    margin-right: 0.15rem;
  }
  .refine-fields {
    display: grid;
    grid-template-columns: minmax(13rem, 2fr) minmax(8rem, 1fr) minmax(8rem, 1fr);
    gap: 0.6rem;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    min-width: 0;
  }
  .field > span {
    font-size: 0.78rem;
    color: var(--ink-secondary);
  }
  .field input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.32em 0.6em;
    border-radius: var(--radius-md);
    border: 1px solid var(--hairline-strong);
    background: var(--canvas);
    color: var(--ink);
    font-size: 0.85rem;
  }
  .field input:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .config-hint {
    font-size: 0.8rem;
    color: var(--ink-faint);
    margin: 0;
  }
  /* 分段单选(与设置页 .seg 同一控件语言) */
  .seg {
    display: flex;
    gap: 2px;
    flex: none;
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
  /* Agent 模式两个可选项字段并排 */
  .agent-fields {
    grid-template-columns: minmax(10rem, 1fr) minmax(13rem, 2fr);
  }
  /* 所选 Agent 的探测状态:caption 级,跟在分段选择右侧 */
  .agent-status {
    font-size: 0.78rem;
    color: var(--ink-faint);
  }
  .agent-status.warn {
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
