<script lang="ts">
  import { onMount } from "svelte";
  import { open } from "@tauri-apps/plugin-dialog";
  import { recording } from "$lib/recording.svelte";
  import {
    modelsStatus,
    getSettings,
    setSettings,
    downloadModels,
    deleteModel,
    migrateDataDir,
    migrateModelsDir,
    onMigrate,
    onModelDownload,
    type ModelsStatus,
    type Settings,
    type ModelDownloadEvent,
    type MigrateEvent,
  } from "$lib/models";

  let settings = $state<Settings | null>(null);
  let status = $state<ModelsStatus | null>(null);
  /** danger 横幅：迁移/删除/切型/下载的错误统一在此显示。 */
  let error = $state("");

  /** 选好新目录待确认的迁移项(一次只允许一项)。 */
  let pendingMigrate = $state<{ kind: "data" | "models"; path: string } | null>(null);
  /** copying 阶段:两行按钮禁用并显示「迁移中…」。 */
  let migrating = $state(false);

  /** 模型删除的行内确认态。 */
  let confirmDeleteId = $state<string | null>(null);

  /** 各工件下载进度:received/total 字节 + phase(done/error/cancelled 后清除)。 */
  let prog = $state<Record<string, { received: number; total: number; phase: string }>>({});

  // 任一工件处于未终结的下载态(downloading/verifying/extracting)→ 视为下载进行中。
  const downloadingActive = $derived(
    Object.values(prog).some(
      (p) => p.phase === "downloading" || p.phase === "verifying" || p.phase === "extracting",
    ),
  );

  // 当前 ASR 选型对应的工件 id:sense_voice→asr,whisper→whisper。
  const asrArtifactId = $derived(settings?.asr_model === "whisper" ? "whisper" : "asr");
  const asrModelMissing = $derived(
    !!status && !status.artifacts.find((a) => a.id === asrArtifactId)?.present,
  );

  // 迁移/更改目录被阻断的原因(禁用 title 用);录制中/下载中/迁移中皆阻断。
  const migrateBlockReason = $derived(
    recording.isLive
      ? "录制中不能迁移数据"
      : downloadingActive
        ? "模型下载进行中不能迁移"
        : migrating
          ? "正在迁移…"
          : "",
  );

  const dataDirLabel = $derived(settings?.data_dir || "默认(应用数据目录)");
  const modelsDirLabel = $derived(settings?.models_dir || "默认(应用数据目录)");

  async function refreshSettings() {
    try {
      settings = await getSettings();
    } catch (e) {
      error = `读取设置失败: ${e}`;
    }
  }
  async function refreshStatus() {
    try {
      status = await modelsStatus();
    } catch (e) {
      error = `读取模型状态失败: ${e}`;
    }
  }

  function handleMigrate(e: MigrateEvent) {
    if (e.phase === "copying") {
      migrating = true;
    } else if (e.phase === "done") {
      migrating = false;
      pendingMigrate = null;
      refreshSettings(); // 刷新显示新路径
    } else if (e.phase === "error") {
      migrating = false;
      pendingMigrate = null;
      error = e.message;
    }
  }

  function handleDownload(e: ModelDownloadEvent) {
    if (e.artifact === "all") {
      if (e.phase === "done") refreshStatus(); // 批次收尾
      return;
    }
    if (e.phase === "error") {
      error = `下载失败: ${e.message}`;
      dropProg(e.artifact);
      return;
    }
    if (e.phase === "cancelled") {
      dropProg(e.artifact);
      return;
    }
    if (e.phase === "done") {
      dropProg(e.artifact);
      refreshStatus(); // 单工件完成:刷新 present 态(行变「已下载」)
      return;
    }
    prog = { ...prog, [e.artifact]: { received: e.received_bytes, total: e.total_bytes, phase: e.phase } };
  }

  function dropProg(id: string) {
    const { [id]: _drop, ...rest } = prog;
    prog = rest;
  }

  onMount(() => {
    refreshSettings();
    refreshStatus();
    // 事件监听随页面生命周期注册/解绑(下载/迁移跨页面继续,回到本页续接进度流)。
    const unD = onModelDownload(handleDownload);
    const unM = onMigrate(handleMigrate);
    return () => {
      unD.then((f) => f());
      unM.then((f) => f());
    };
  });

  // —— 存储迁移 ——
  async function chooseDir(kind: "data" | "models") {
    error = "";
    const picked = await open({ directory: true });
    if (typeof picked === "string") {
      pendingMigrate = { kind, path: picked };
    }
  }

  async function startMigrate() {
    if (!pendingMigrate) return;
    const { kind, path } = pendingMigrate;
    error = "";
    migrating = true; // 乐观置位;copying 事件确认,done/error 复位
    try {
      if (kind === "data") await migrateDataDir(path);
      else await migrateModelsDir(path);
    } catch (e) {
      // 同步 Err(非法目录等):复位并冒泡到 danger 横幅。事件驱动的 done/error 走 handleMigrate。
      migrating = false;
      pendingMigrate = null;
      error = `${e}`;
    }
  }

  // —— 模型下载/删除 ——
  async function download(id: string) {
    error = "";
    prog = { ...prog, [id]: { received: 0, total: 0, phase: "downloading" } };
    try {
      await downloadModels([id]);
    } catch (e) {
      // "下载已在进行中" 不算错:保留进度态继续收事件。
      if (!String(e).includes("已在进行中")) {
        dropProg(id);
        error = `下载失败: ${e}`;
      }
    }
  }

  async function doDelete(id: string) {
    confirmDeleteId = null;
    error = "";
    try {
      await deleteModel(id);
      await refreshStatus();
      // 删的是当前选型模型 → asrModelMissing 会自动亮警示横幅。
    } catch (e) {
      error = `删除失败: ${e}`;
    }
  }

  // —— ASR 选型 ——
  async function changeAsr(model: string) {
    if (settings?.asr_model === model) return;
    error = "";
    try {
      const fresh = await getSettings(); // 取新鲜值再改,避免覆盖其它并发写入
      fresh.asr_model = model;
      await setSettings(fresh);
      settings = fresh;
      await refreshStatus(); // required_for_recording 随选型重算
    } catch (e) {
      // 失败(如录制中被后端拒绝):danger 横幅 + 回弹选项显示。
      error = `${e}`;
      settings = await getSettings().catch(() => settings);
    }
  }

  // —— 镜像加速(逻辑照搬 ModelDownloadCard)——
  async function toggleMirror() {
    if (!settings) return;
    settings = { ...settings, mirror_enabled: !settings.mirror_enabled };
    await setSettings(settings);
  }
  async function savePrefix() {
    if (settings) await setSettings(settings);
  }

  const pct = (p: { received: number; total: number }) =>
    p.total > 0 ? Math.min(100, Math.floor((p.received / p.total) * 100)) : 0;
  const mb = (n: number) => (n / 1024 / 1024).toFixed(0);
  const phaseText: Record<string, string> = {
    downloading: "下载中",
    verifying: "校验中",
    extracting: "解压中",
    done: "完成",
  };
</script>

<main class="container">
  <h1>设置</h1>
  <p class="desc">
    管理数据与模型的存储位置、下载或删除语音模型、选择语音识别引擎。全部处理在本机完成,不上传任何音频。
  </p>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  <!-- —— 存储位置 —— -->
  <section>
    <h2 class="section-title">存储位置</h2>
    <div class="rows">
      {@render storeRow("data", "数据存储目录", dataDirLabel)}
      {@render storeRow("models", "模型存储目录", modelsDirLabel)}
    </div>
  </section>

  <!-- —— 语音模型 —— -->
  <section>
    <h2 class="section-title">语音模型</h2>
    <div class="rows">
      {#if status}
        {#each status.artifacts as a (a.id)}
          <div class="row">
            <div class="row-info">
              <span class="row-label">{a.label} · 约 {a.approx_mb}MB</span>
              {#if a.present}
                <span class="present">已下载</span>
              {/if}
            </div>

            {#if prog[a.id]}
              <div class="dl">
                <span class="phase">
                  {phaseText[prog[a.id].phase] ?? prog[a.id].phase}
                  {#if prog[a.id].phase === "downloading" && prog[a.id].total > 0}
                    {mb(prog[a.id].received)}/{mb(prog[a.id].total)}MB
                  {/if}
                </span>
                <div class="bar"><div class="fill" style="width:{pct(prog[a.id])}%"></div></div>
              </div>
            {:else if a.present}
              {#if confirmDeleteId === a.id}
                <div class="confirm-inline">
                  <button class="link danger" onclick={() => doDelete(a.id)}>确认删除</button>
                  <button class="link" onclick={() => (confirmDeleteId = null)}>取消</button>
                </div>
              {:else}
                <button
                  class="link danger row-action"
                  disabled={recording.isLive}
                  title={recording.isLive ? "录制中不能删除模型" : "删除本模型(可随时重新下载)"}
                  onclick={() => {
                    confirmDeleteId = a.id;
                  }}>删除</button
                >
              {/if}
            {:else}
              <button class="btn-secondary" onclick={() => download(a.id)}>下载</button>
            {/if}
          </div>
        {/each}
      {/if}
    </div>

    <label class="mirror">
      <input
        type="checkbox"
        checked={settings?.mirror_enabled ?? false}
        disabled={!settings}
        onchange={toggleMirror}
      />
      使用镜像加速(国内网络推荐)
    </label>
    {#if settings?.mirror_enabled}
      <input
        class="prefix"
        bind:value={settings.mirror_prefix}
        onblur={savePrefix}
        placeholder="镜像前缀,如 https://ghproxy.net/"
      />
    {/if}
  </section>

  <!-- —— 语音识别 —— -->
  <section>
    <h2 class="section-title">语音识别</h2>
    <div class="radios">
      <label class="radio" class:disabled={recording.isLive}>
        <input
          type="radio"
          name="asr"
          checked={settings?.asr_model !== "whisper"}
          disabled={recording.isLive || !settings}
          onchange={() => changeAsr("sense_voice")}
        />
        <span class="radio-body">
          <span class="radio-title">SenseVoice</span>
          <span class="radio-desc"
            >推荐。中英日韩粤,带语言幻觉过滤与段内说话人分离的完整功能。</span
          >
        </span>
      </label>
      <label class="radio" class:disabled={recording.isLive}>
        <input
          type="radio"
          name="asr"
          checked={settings?.asr_model === "whisper"}
          disabled={recording.isLive || !settings}
          onchange={() => changeAsr("whisper")}
        />
        <span class="radio-body">
          <span class="radio-title">Whisper</span>
          <span class="radio-desc"
            >多语种。段内说话人分离退化为段级标签,语言过滤仅按文本兜底。切换后下一场录制生效。</span
          >
        </span>
      </label>
    </div>
    {#if asrModelMissing}
      <div class="banner warn">所选识别模型未下载,请在上方模型区块下载。</div>
    {/if}
    {#if recording.isLive}
      <p class="lock-hint">录制进行中,识别引擎切换已锁定,停止录制后可更改。</p>
    {/if}
  </section>
</main>

{#snippet storeRow(kind: "data" | "models", label: string, path: string)}
  <div class="row">
    <div class="row-info">
      <span class="row-label">{label}</span>
      <span class="row-path">{path}</span>
    </div>
    {#if !(pendingMigrate && pendingMigrate.kind === kind)}
      <button
        class="btn-secondary row-action"
        disabled={!!migrateBlockReason || !!pendingMigrate}
        title={migrateBlockReason || "选择新目录并迁移现有内容"}
        onclick={() => chooseDir(kind)}>更改…</button
      >
    {/if}
  </div>
  {#if pendingMigrate && pendingMigrate.kind === kind}
    <div class="confirm-bar">
      {#if migrating}
        <span class="migrating">迁移中…</span>
      {:else}
        <span class="confirm-text"
          >将把现有数据完整迁移到 <span class="confirm-path">{pendingMigrate.path}</span
          >,期间不能录制。</span
        >
        <div class="confirm-actions">
          <button class="btn-primary" onclick={startMigrate}>开始迁移</button>
          <button class="btn-secondary" onclick={() => (pendingMigrate = null)}>取消</button>
        </div>
      {/if}
    </div>
  {/if}
{/snippet}

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 52rem;
  }
  h1 {
    margin: 0 0 0.75rem;
  }
  .desc {
    color: var(--ink-secondary);
    font-size: 0.85rem;
    line-height: 1.5;
    margin: 0 0 1.25rem;
    max-width: 46rem;
  }
  section {
    margin-top: 1.6rem;
  }
  .section-title {
    font-size: 0.82rem;
    font-weight: 600;
    color: var(--ink-secondary);
    margin: 0 0 0.45rem;
  }
  /* list 卡片:surface 底承载各行,行间 hairline 分隔 */
  .rows {
    background: var(--surface);
    border-radius: var(--radius-lg);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.9rem;
    padding: 0.7rem 1rem;
    border-bottom: 1px solid var(--hairline);
  }
  .rows .row:first-child {
    border-top-left-radius: var(--radius-lg);
    border-top-right-radius: var(--radius-lg);
  }
  .rows .row:last-child {
    border-bottom: none;
    border-bottom-left-radius: var(--radius-lg);
    border-bottom-right-radius: var(--radius-lg);
  }
  .row-info {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
  }
  .row-label {
    font-size: 0.92rem;
    color: var(--ink);
  }
  .row-path {
    font-size: 0.82rem;
    color: var(--ink-secondary);
    word-break: break-all;
  }
  .present {
    font-size: 0.8rem;
    color: var(--ink-faint);
  }
  .row-info:has(.present) {
    flex-direction: row;
    align-items: baseline;
    gap: 0.6rem;
  }
  /* 行级操作:悬停显影(有确认/进度态时常驻显示) */
  .row-action {
    visibility: hidden;
  }
  .row:hover .row-action {
    visibility: visible;
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
  /* button-primary:开始迁移是确认条唯一主动作 */
  .btn-primary {
    flex: none;
    border-radius: var(--radius-md);
    border: 1px solid transparent;
    padding: 0.35em 0.9em;
    font-size: 0.85rem;
    font-weight: 500;
    cursor: pointer;
    background: var(--accent);
    color: var(--on-accent);
    box-shadow: var(--shadow-btn);
  }
  .btn-primary:hover {
    background: var(--accent-pressed);
  }
  /* button-link:行级删除/取消 */
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
  .link.danger {
    color: var(--danger);
  }
  .link:disabled {
    color: var(--ink-faint);
    cursor: default;
    text-decoration: none;
  }
  .confirm-inline {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    flex: none;
  }
  /* 迁移确认条:接在存储行下方,warning 色系提示不可撤销的迁移 */
  .confirm-bar {
    display: flex;
    align-items: center;
    gap: 0.8rem;
    flex-wrap: wrap;
    padding: 0.6rem 1rem;
    margin: 0.4rem 0 0.6rem;
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    border-radius: var(--radius-lg);
  }
  .confirm-text {
    flex: 1;
    min-width: 12rem;
    font-size: 0.85rem;
    color: var(--warning-ink);
    line-height: 1.45;
  }
  .confirm-path {
    font-weight: 600;
    word-break: break-all;
  }
  .confirm-actions {
    display: flex;
    gap: 0.4rem;
    flex: none;
  }
  .migrating {
    font-size: 0.85rem;
    color: var(--warning-ink);
    font-weight: 500;
  }
  /* 下载进度:轨 hairline、填充 accent、rounded-full(复用 download-card) */
  .dl {
    flex: none;
    min-width: 9rem;
  }
  .phase {
    color: var(--ink-secondary);
    font-size: 0.8rem;
  }
  .bar {
    height: 6px;
    background: var(--hairline);
    border-radius: var(--radius-full);
    margin-top: 0.25rem;
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--accent);
    transition: width 0.3s;
  }
  /* 镜像开关 + 前缀(照搬 ModelDownloadCard) */
  .mirror {
    display: flex;
    align-items: center;
    gap: 0.4em;
    font-size: 0.85rem;
    color: var(--ink);
    margin: 0.8rem 0 0;
  }
  .prefix {
    width: 100%;
    box-sizing: border-box;
    margin-top: 0.5rem;
    padding: 0.35em 0.6em;
    border-radius: var(--radius-md);
    border: 1px solid var(--hairline-strong);
    background: var(--canvas);
    color: var(--ink);
    font-size: 0.85rem;
  }
  .prefix:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  /* ASR 单选:整块可点,选中/悬停换底 */
  .radios {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .radio {
    display: flex;
    align-items: flex-start;
    gap: 0.6rem;
    padding: 0.7rem 0.9rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--surface);
    cursor: pointer;
  }
  .radio:hover {
    background: var(--surface-soft);
  }
  .radio input {
    margin-top: 0.15rem;
    flex: none;
  }
  .radio.disabled {
    cursor: default;
    opacity: 0.7;
  }
  .radio-body {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
  }
  .radio-title {
    font-size: 0.92rem;
    font-weight: 600;
    color: var(--ink);
  }
  .radio-desc {
    font-size: 0.82rem;
    color: var(--ink-secondary);
    line-height: 1.45;
  }
  .lock-hint {
    font-size: 0.8rem;
    color: var(--ink-faint);
    margin: 0.5rem 0 0;
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
</style>
