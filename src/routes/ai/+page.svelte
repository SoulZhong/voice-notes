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
    type AgentStatus,
  } from "$lib/mcp";
  import { openUrl } from "@tauri-apps/plugin-opener";

  let settings = $state<Settings | null>(null);
  /** danger 横幅：本页保存类操作的错误统一在此显示(精简自设置页的全局 error 横幅)。 */
  let error = $state("");

  /** 智能精修:开关 + 接口三字段的本地镜像(失败回弹靠本地 state 强制 DOM 对齐)。 */
  let refineOn = $state(false);
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

  onMount(() => {
    (async () => {
      try {
        const s = await getSettings();
        settings = s;
        refineOn = s.refine_enabled;
        refineBaseUrl = s.refine_base_url;
        refineModel = s.refine_model;
        refineKey = s.refine_api_key;
        mcpAllowControl = s.mcp_allow_control;
      } catch { /* 首载失败:控件保持默认,操作时会再报错 */ }
    })();
    refreshMcp();
    refreshSkill();
    mcpManualSnippet().then((v) => (mcpSnippet = v)).catch(() => {});
    mcpHealedCount().then((n) => (mcpHealed = n)).catch(() => {});
  });

  // —— 通用「取新鲜值→改→存」保存(精简自设置页 saveSetting:只回弹本页用到的字段) ——
  async function saveSetting(mut: (s: Settings) => void) {
    error = "";
    try {
      const fresh = await getSettings();
      mut(fresh);
      await setSettings(fresh);
      settings = fresh;
      refineOn = fresh.refine_enabled;
      refineBaseUrl = fresh.refine_base_url;
      refineModel = fresh.refine_model;
      refineKey = fresh.refine_api_key;
      mcpAllowControl = fresh.mcp_allow_control;
    } catch (e) {
      error = `保存失败: ${e}`;
      settings = await getSettings().catch(() => settings);
      if (settings) {
        refineOn = settings.refine_enabled;
        refineBaseUrl = settings.refine_base_url;
        refineModel = settings.refine_model;
        refineKey = settings.refine_api_key;
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
      s.refine_enabled = refineOn;
      s.refine_base_url = refineBaseUrl.trim();
      s.refine_model = refineModel.trim();
      s.refine_api_key = refineKey.trim();
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
      }
      await refreshSkill();
    } catch (e) {
      mcpError = String(e);
    } finally {
      skillBusy = false;
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
      <label class="row">
        <div class="row-info">
          <span class="row-label">会后 AI 精修</span>
          <span class="row-desc">录完自动纠错字、清理口头语、合并段落。会议文字会发送给所选服务商</span>
        </div>
        <input type="checkbox" class="ctl" bind:checked={refineOn} disabled={!settings} onchange={saveRefine} />
      </label>
      {#if refineOn}
        <div class="config">
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
        </div>
      {/if}
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
          <span class="row-label">Claude Code 技能</span>
          <span class="row-desc">
            {#if skillState === "current"}已安装:Claude 掌握会议纪要/周报/检索工作流
            {:else if skillState === "stale"}已安装(旧版,应用启动时自动更新)
            {:else if skillState === "unmanaged"}检测到自定义同名技能,不自动管理
            {:else}让 Claude Code 掌握会议纪要/周报/检索工作流(写入 ~/.claude/skills)
            {/if}
          </span>
        </div>
        {#if skillState !== null && skillState !== "unmanaged"}
          <button class="btn-secondary" disabled={skillBusy} onclick={toggleSkill}>
            {skillState === "not_installed" ? "安装" : "移除"}
          </button>
        {/if}
      </div>
      <label class="row">
        <div class="row-info">
          <span class="row-label">允许 AI 控制录制</span>
          <span class="row-desc">开启后,已接入的 AI 助手可远程开始/停止/暂停录制。默认关闭</span>
        </div>
        <input type="checkbox" class="ctl" bind:checked={mcpAllowControl} disabled={!settings} onchange={saveMcpAllowControl} />
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
</div>

<style>
  .page { padding: 0 1.5rem 2rem; }
  .topbar { position: sticky; top: 0; background: var(--canvas); padding: 1.1rem 0 0.6rem; }
  h1 { font-size: 1.15rem; font-weight: 600; margin: 0; }

  section {
    margin-top: 1.3rem;
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
  /* 智能精修配置块:卡片内嵌面板(开关行下方展开) */
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
  .snippet {
    margin: 0 0 0.5rem;
    padding: 0.6rem 0.8rem;
    background: var(--surface-soft);
    border-radius: var(--radius-sm);
    font-size: 0.8rem;
    overflow-x: auto;
    user-select: text;
  }
</style>
