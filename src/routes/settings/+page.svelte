<script lang="ts">
  import { onMount } from "svelte";
  import { open } from "@tauri-apps/plugin-dialog";
  import { enable, disable, isEnabled } from "@tauri-apps/plugin-autostart";
  import { recording } from "$lib/recording.svelte";
  import { applyTheme } from "$lib/theme";
  import { acceleratorFromEvent, displayShortcut } from "$lib/shortcut";
  import {
    modelsStatus,
    getSettings,
    setSettings,
    downloadModels,
    deleteModel,
    migrateDataDir,
    migrateModelsDir,
    applyShortcut,
    audioDiskUsage,
    purgeAudio,
    onMigrate,
    onModelDownload,
    type ModelsStatus,
    type Settings,
    type ModelDownloadEvent,
    type MigrateEvent,
  } from "$lib/models";

  let settings = $state<Settings | null>(null);
  let status = $state<ModelsStatus | null>(null);
  /**
   * ASR radio 的本地绑定值(bind:group)。不直接从 settings 派生:切型失败回弹时
   * settings.asr_model 前后同值,派生 checked 可能跳过 DOM 回写,而浏览器原生已把
   * 选中移到新项——本地 state 显式改回旧值必触发 DOM 对齐,天然回弹。
   */
  let asrChoice = $state("sense_voice");
  /** danger 横幅：迁移/删除/切型/下载的错误统一在此显示。 */
  let error = $state("");

  // —— 新四区块的本地绑定值 ——
  // 一律用本地 $state + bind(group/checked),不直接从 settings 派生 checked：
  // 失败回弹时后端值可能与点击前同值,派生表达式因缓存相等而跳过 DOM 回写(浏览器已把
  // 勾选/选中改到新态),本地 state 显式改回旧值必触发 DOM 对齐——与 asrChoice 同理。
  /** 外观主题 radio:"light" | "dark" | "system"。 */
  let themeChoice = $state("system");
  /** 录制三开关的本地镜像。 */
  let sysOnly = $state(false);
  let langFilter = $state(false);
  let keepAudio = $state(false);
  /** 系统区:全局快捷键开关 / 菜单栏常驻 / 开机自启(自启为系统真值,非 settings)。 */
  let shortcutEnabled = $state(false);
  let trayEnabled = $state(false);
  let autostartEnabled = $state(false);
  /** 快捷键录入框聚焦态:聚焦时清空显示并提示「按下组合键…」。 */
  let capturingShortcut = $state(false);

  /** 磁盘:录音音频占用字节(null=统计中);清理展开态与选项;上次释放量文案。 */
  let audioBytes = $state<number | null>(null);
  let showPurge = $state(false);
  let purgeChoice = $state<"30" | "90" | "all">("30");
  let freedText = $state("");

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
      asrChoice = settings.asr_model === "whisper" ? "whisper" : "sense_voice";
      syncLocalFromSettings(settings);
    } catch (e) {
      error = `读取设置失败: ${e}`;
    }
  }

  /** 把后端真值同步到各本地镜像(初始化 / 保存失败回弹后重新对齐 DOM)。 */
  function syncLocalFromSettings(s: Settings) {
    themeChoice = s.theme;
    sysOnly = s.record_system_only;
    langFilter = s.language_filter;
    keepAudio = s.keep_audio;
    shortcutEnabled = s.shortcut_enabled;
    trayEnabled = s.tray_enabled;
  }

  async function refreshDiskUsage() {
    try {
      audioBytes = await audioDiskUsage();
    } catch {
      audioBytes = null;
    }
  }

  /** 字节格式化:<1MB 用 KB,<1GB 用 MB,否则 GB。 */
  function fmtBytes(n: number): string {
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
    const inMb = n / 1024 / 1024;
    if (inMb < 1024) return `${inMb.toFixed(1)} MB`;
    return `${(inMb / 1024).toFixed(2)} GB`;
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
    refreshDiskUsage();
    // 开机自启读系统真值(与 settings 无关);失败静默,保持未勾选。
    isEnabled()
      .then((v) => (autostartEnabled = v))
      .catch(() => {});
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

  // —— 外观 / 录制 / 系统:通用「取新鲜值→改→存」保存 ——
  // 成功后 settings 与本地镜像同步;失败时 danger 横幅 + 从后端真值回弹本地镜像。
  async function saveSetting(mut: (s: Settings) => void) {
    error = "";
    try {
      const fresh = await getSettings();
      mut(fresh);
      await setSettings(fresh);
      settings = fresh;
      syncLocalFromSettings(fresh);
    } catch (e) {
      error = `保存失败: ${e}`;
      settings = await getSettings().catch(() => settings);
      if (settings) syncLocalFromSettings(settings);
    }
  }

  // 外观:存 settings 后立即 applyTheme(即时生效)。themeChoice 已由 bind:group 改到新值。
  async function changeTheme() {
    error = "";
    try {
      const fresh = await getSettings();
      fresh.theme = themeChoice;
      await setSettings(fresh);
      settings = fresh;
      applyTheme(themeChoice);
    } catch (e) {
      error = `切换主题失败: ${e}`;
      settings = await getSettings().catch(() => settings);
      if (settings) syncLocalFromSettings(settings); // 回弹 themeChoice
    }
  }

  // —— 全局快捷键 ——
  // 开关:存 settings 后 applyShortcut。失败时后端已把 shortcut_enabled 落回 false,
  // 重新 getSettings 同步,shortcutEnabled 随之回弹为未勾选。
  async function toggleShortcutEnabled() {
    error = "";
    try {
      const fresh = await getSettings();
      fresh.shortcut_enabled = shortcutEnabled;
      await setSettings(fresh);
      settings = fresh;
      await applyShortcut();
    } catch (e) {
      error = `快捷键设置失败: ${e}`;
      settings = await getSettings().catch(() => settings);
      if (settings) syncLocalFromSettings(settings);
    }
  }

  // 录入框:preventDefault 拦截浏览器默认;Esc 失焦取消;组合键经 acceleratorFromEvent
  // 组装,非 null 才落库并 applyShortcut(enabled 不变)。
  async function onShortcutKeydown(e: KeyboardEvent) {
    e.preventDefault();
    const input = e.currentTarget as HTMLInputElement;
    if (e.key === "Escape") {
      input.blur();
      return;
    }
    const acc = acceleratorFromEvent(e);
    if (acc === null) return;
    error = "";
    try {
      const fresh = await getSettings();
      fresh.shortcut = acc;
      await setSettings(fresh);
      settings = fresh;
      await applyShortcut();
      input.blur();
    } catch (err) {
      error = `快捷键设置失败: ${err}`;
      settings = await getSettings().catch(() => settings);
      if (settings) syncLocalFromSettings(settings);
    }
  }

  // —— 开机自启:直连插件真值,onMount 读、toggle 写 ——
  async function toggleAutostart() {
    error = "";
    try {
      if (autostartEnabled) await enable();
      else await disable();
    } catch (e) {
      error = `开机自启设置失败: ${e}`;
      autostartEnabled = await isEnabled().catch(() => autostartEnabled); // 回读真值回弹
    }
  }

  // —— 磁盘:清理录音音频(两段确认后)——
  async function doPurge() {
    error = "";
    const days = purgeChoice === "all" ? null : purgeChoice === "30" ? 30 : 90;
    try {
      const freed = await purgeAudio(days);
      freedText = `已释放 ${fmtBytes(freed)}`;
      showPurge = false;
      await refreshDiskUsage();
    } catch (e) {
      error = `清理失败: ${e}`;
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
      // 幂等对齐本地绑定:bind:group 已把 asrChoice 改到新项,这里再显式对齐一次,
      // 消掉后端 migrate 事件毫秒级竞态下 asrChoice 可能与 settings 短暂不一致的窗口。
      asrChoice = model === "whisper" ? "whisper" : "sense_voice";
      await refreshStatus(); // required_for_recording 随选型重算
    } catch (e) {
      // 失败(如录制中被后端拒绝):danger 横幅 + 回弹选项显示。
      error = `${e}`;
      settings = await getSettings().catch(() => settings);
      // 回弹本地绑定值:与点击前同值也必然不同于 bind:group 已改的新值,
      // 本地 state 变更强制触发 DOM 回写,浏览器原生移走的 checked 被拉回。
      asrChoice = settings?.asr_model === "whisper" ? "whisper" : "sense_voice";
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

  <!-- —— 外观 —— -->
  <section>
    <h2 class="section-title">外观</h2>
    <div class="radios">
      <label class="radio">
        <input
          type="radio"
          name="theme"
          value="light"
          bind:group={themeChoice}
          disabled={!settings}
          onchange={changeTheme}
        />
        <span class="radio-body"><span class="radio-title">亮色</span></span>
      </label>
      <label class="radio">
        <input
          type="radio"
          name="theme"
          value="dark"
          bind:group={themeChoice}
          disabled={!settings}
          onchange={changeTheme}
        />
        <span class="radio-body"><span class="radio-title">暗色</span></span>
      </label>
      <label class="radio">
        <input
          type="radio"
          name="theme"
          value="system"
          bind:group={themeChoice}
          disabled={!settings}
          onchange={changeTheme}
        />
        <span class="radio-body">
          <span class="radio-title">跟随系统</span>
          <span class="radio-desc">随 macOS 外观在亮色与暗色间自动切换。</span>
        </span>
      </label>
    </div>
  </section>

  <!-- —— 录制 —— -->
  <section>
    <h2 class="section-title">录制</h2>
    <div class="toggles">
      <label class="toggle">
        <input
          type="checkbox"
          bind:checked={sysOnly}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.record_system_only = sysOnly))}
        />
        <span class="toggle-body">
          <span class="toggle-title">仅录制系统声音</span>
          <span class="toggle-desc"
            >只录扬声器外放,不录麦克风。纯外放的会议开启后可根治麦克风串入的残渣。</span
          >
        </span>
      </label>
      <label class="toggle">
        <input
          type="checkbox"
          bind:checked={langFilter}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.language_filter = langFilter))}
        />
        <span class="toggle-body">
          <span class="toggle-title">语言幻觉过滤</span>
          <span class="toggle-desc"
            >过滤识别引擎在静音或噪声段产生的幻觉文字。多语种会议中若误伤可关闭。</span
          >
        </span>
      </label>
      <label class="toggle">
        <input
          type="checkbox"
          bind:checked={keepAudio}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.keep_audio = keepAudio))}
        />
        <span class="toggle-body">
          <span class="toggle-title">保留录音音频</span>
          <span class="toggle-desc">保留原始录音以便回放核对。关闭可节省磁盘占用。</span>
        </span>
      </label>
    </div>
    <p class="lock-hint">录制中可随时更改,下一场录制生效。</p>
  </section>

  <!-- —— 磁盘 —— -->
  <section>
    <h2 class="section-title">磁盘</h2>
    <div class="rows">
      <div class="row">
        <div class="row-info">
          <span class="row-label">录音音频占用</span>
          <span class="row-path">
            {audioBytes === null ? "统计中…" : fmtBytes(audioBytes)}{freedText ? ` · ${freedText}` : ""}
          </span>
        </div>
        {#if !showPurge}
          <button
            class="btn-secondary row-action"
            disabled={recording.isLive}
            title={recording.isLive ? "录制中不能清理音频" : "清理历史录音音频"}
            onclick={() => {
              freedText = "";
              showPurge = true;
            }}>清理…</button
          >
        {/if}
      </div>
    </div>
    {#if showPurge}
      <div class="purge-bar">
        <div class="purge-choices">
          <label class="mini-radio">
            <input type="radio" name="purge" value="30" bind:group={purgeChoice} />清理 30 天前
          </label>
          <label class="mini-radio">
            <input type="radio" name="purge" value="90" bind:group={purgeChoice} />清理 90 天前
          </label>
          <label class="mini-radio">
            <input type="radio" name="purge" value="all" bind:group={purgeChoice} />清理全部
          </label>
        </div>
        <span class="confirm-text">只删除音频文件,笔记文字与说话人保留。</span>
        <div class="purge-actions">
          <button class="link danger" onclick={doPurge}>确认清理</button>
          <button class="link" onclick={() => (showPurge = false)}>取消</button>
        </div>
      </div>
    {/if}
  </section>

  <!-- —— 系统 —— -->
  <section>
    <h2 class="section-title">系统</h2>
    <div class="toggles">
      <div class="toggle toggle-col">
        <label class="toggle-head">
          <input
            type="checkbox"
            bind:checked={shortcutEnabled}
            disabled={!settings}
            onchange={toggleShortcutEnabled}
          />
          <span class="toggle-body">
            <span class="toggle-title">全局快捷键</span>
            <span class="toggle-desc">在任意应用中按下组合键即可开始或停止录制。</span>
          </span>
        </label>
        <input
          class="shortcut-input"
          readonly
          value={capturingShortcut ? "" : displayShortcut(settings?.shortcut ?? "")}
          placeholder="按下组合键…"
          onfocus={() => (capturingShortcut = true)}
          onblur={() => (capturingShortcut = false)}
          onkeydown={onShortcutKeydown}
        />
      </div>
      <label class="toggle">
        <input type="checkbox" bind:checked={autostartEnabled} onchange={toggleAutostart} />
        <span class="toggle-body">
          <span class="toggle-title">开机自动启动</span>
          <span class="toggle-desc">登录 macOS 后在后台自动运行。</span>
        </span>
      </label>
      <label class="toggle">
        <input
          type="checkbox"
          bind:checked={trayEnabled}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.tray_enabled = trayEnabled))}
        />
        <span class="toggle-body">
          <span class="toggle-title">菜单栏常驻</span>
          <span class="toggle-desc">开启时关闭窗口只隐藏到菜单栏,录制不中断。</span>
        </span>
      </label>
    </div>
  </section>

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
                  disabled={recording.isLive || downloadingActive}
                  title={recording.isLive
                    ? "录制中不能删除模型"
                    : downloadingActive
                      ? "下载进行中不能删除模型"
                      : "删除本模型(可随时重新下载)"}
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
          value="sense_voice"
          bind:group={asrChoice}
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
          value="whisper"
          bind:group={asrChoice}
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
    font-weight: 500;
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
    border-radius: var(--radius-full);
    border: 1px solid transparent;
    padding: 0.35em 0.9em;
    font-size: 0.85rem;
    font-weight: 500;
    cursor: pointer;
    background: var(--primary);
    color: var(--on-primary);
    box-shadow: var(--shadow-btn);
  }
  .btn-primary:hover {
    background: var(--primary-pressed);
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
    font-weight: 500;
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
    font-weight: 500;
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
  /* 开关组:checkbox + 标题 + 说明小字(录制/系统区共用) */
  .toggles {
    display: flex;
    flex-direction: column;
    gap: 0.85rem;
  }
  .toggle,
  .toggle-head {
    display: flex;
    align-items: flex-start;
    gap: 0.6rem;
    cursor: pointer;
  }
  .toggle-col {
    flex-direction: column;
    gap: 0.5rem;
    cursor: default;
  }
  .toggle input[type="checkbox"],
  .toggle-head input[type="checkbox"] {
    margin-top: 0.2rem;
    flex: none;
  }
  .toggle-body {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
  }
  .toggle-title {
    font-size: 0.92rem;
    color: var(--ink);
  }
  .toggle-desc {
    font-size: 0.82rem;
    color: var(--ink-secondary);
    line-height: 1.45;
  }
  /* 快捷键录入框:input 形态(surface-press 底、聚焦浮出 canvas + accent 环) */
  .shortcut-input {
    margin-left: 1.6rem;
    width: 12rem;
    box-sizing: border-box;
    padding: 0.35em 0.6em;
    border-radius: var(--radius-md);
    border: 1px solid var(--hairline-strong);
    background: var(--surface-press);
    color: var(--ink);
    font-size: 0.9rem;
    font-family: -apple-system, system-ui, sans-serif;
    cursor: pointer;
  }
  .shortcut-input:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
    background: var(--canvas);
  }
  /* 磁盘清理确认条:warning 色系,行内展开三选 + 两段确认 */
  .purge-bar {
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
    padding: 0.7rem 1rem;
    margin: 0.4rem 0 0.6rem;
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    border-radius: var(--radius-lg);
  }
  .purge-choices {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem 1.1rem;
  }
  .mini-radio {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    font-size: 0.85rem;
    color: var(--warning-ink);
    cursor: pointer;
  }
  .purge-bar .confirm-text {
    color: var(--warning-ink);
  }
  .purge-actions {
    display: flex;
    align-items: center;
    gap: 0.5rem;
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
