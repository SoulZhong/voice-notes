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
    testMirror,
    type ModelsStatus,
    type Settings,
    type ModelDownloadEvent,
    type MigrateEvent,
  } from "$lib/models";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { getVersion } from "@tauri-apps/api/app";
  import { checkUpdate, type UpdateInfo } from "$lib/update";

  let settings = $state<Settings | null>(null);

  // —— 关于 / 更新 ——
  let appVersion = $state("");
  let updateInfo = $state<UpdateInfo | null>(null);
  let updateChecking = $state(false);
  let updateError = $state("");
  let mirrorTest = $state<{ ok: boolean; msg: string } | null>(null);
  let mirrorTesting = $state(false);
  let expandedId = $state<string | null>(null);

  /** 镜像开启时返回「前缀+原始url」(等同后端 apply_mirror);关闭/空前缀返回原始 url。 */
  function effectiveUrl(url: string): string {
    const p = (settings?.mirror_prefix ?? "").trim();
    if (!settings?.mirror_enabled || !p) return url;
    return p.endsWith("/") ? `${p}${url}` : `${p}/${url}`;
  }
  async function runMirrorTest() {
    if (!settings) return;
    mirrorTesting = true;
    mirrorTest = null;
    try {
      mirrorTest = { ok: true, msg: await testMirror(settings.mirror_prefix) };
    } catch (e) {
      mirrorTest = { ok: false, msg: String(e) };
    } finally {
      mirrorTesting = false;
    }
  }
  async function doCheckUpdate() {
    updateChecking = true;
    updateError = "";
    try {
      updateInfo = await checkUpdate();
    } catch (e) {
      updateError = `检查失败: ${e}`;
    } finally {
      updateChecking = false;
    }
  }
  let status = $state<ModelsStatus | null>(null);
  /**
   * ASR radio 的本地绑定值(bind:group)。不直接从 settings 派生:切型失败回弹时
   * settings.asr_model 前后同值,派生 checked 可能跳过 DOM 回写,而浏览器原生已把
   * 选中移到新项——本地 state 显式改回旧值必触发 DOM 对齐,天然回弹。
   */
  let asrChoice = $state("sense_voice");
  /** asr_model 后端值 → radio 本地 value 的三态映射(whisper/paraformer/sense_voice)。 */
  function asrModelToChoice(m: string | undefined): string {
    return m === "whisper" ? "whisper" : m === "paraformer" ? "paraformer" : "sense_voice";
  }
  /** danger 横幅：迁移/删除/切型/下载的错误统一在此显示。 */
  let error = $state("");

  // —— 新四区块的本地绑定值 ——
  // 一律用本地 $state + bind(group/checked),不直接从 settings 派生 checked：
  // 失败回弹时后端值可能与点击前同值,派生表达式因缓存相等而跳过 DOM 回写(浏览器已把
  // 勾选/选中改到新态),本地 state 显式改回旧值必触发 DOM 对齐——与 asrChoice 同理。
  /** 外观主题 radio:"light" | "dark" | "system"。 */
  let themeChoice = $state("system");
  /** 设置开关的本地镜像(为什么用本地 state 见上方注释)。 */
  let sysOnly = $state(false);
  let keepVol = $state(false);
  let langFilter = $state(false);
  let keepAudio = $state(false);
  let refineOn = $state(false);
  let telemetryOn = $state(true);
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

  // 当前 ASR 选型对应的工件 id:sense_voice→asr,whisper→whisper,paraformer→paraformer。
  const asrArtifactId = $derived(
    settings?.asr_model === "whisper" ? "whisper" : settings?.asr_model === "paraformer" ? "paraformer" : "asr",
  );
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
      asrChoice = asrModelToChoice(settings.asr_model);
      speakerChoice = settings.speaker_model === "eres2netv2" ? "eres2netv2" : "campplus";
      syncLocalFromSettings(settings);
    } catch (e) {
      error = `读取设置失败: ${e}`;
    }
  }

  /** 把后端真值同步到各本地镜像(初始化 / 保存失败回弹后重新对齐 DOM)。 */
  function syncLocalFromSettings(s: Settings) {
    themeChoice = s.theme;
    sysOnly = s.record_system_only;
    keepVol = s.keep_output_volume;
    langFilter = s.language_filter;
    keepAudio = s.keep_audio;
    refineOn = s.refine_enabled;
    shortcutEnabled = s.shortcut_enabled;
    trayEnabled = s.tray_enabled;
    telemetryOn = s.telemetry_enabled;
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
    getVersion().then((v) => (appVersion = v)).catch(() => {});
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

  // —— 声纹模型选型 ——
  let speakerChoice = $state("campplus");
  const eres2Missing = $derived(
    !!status && !status.artifacts.find((a) => a.id === "speaker-eres2netv2")?.present,
  );
  async function changeSpeakerModel(model: string) {
    if (settings?.speaker_model === model) return;
    error = "";
    try {
      const fresh = await getSettings();
      fresh.speaker_model = model;
      await setSettings(fresh);
      settings = fresh;
      speakerChoice = model;
    } catch (e) {
      error = `${e}`;
      speakerChoice = settings?.speaker_model === "eres2netv2" ? "eres2netv2" : "campplus";
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
      asrChoice = asrModelToChoice(model);
      await refreshStatus(); // required_for_recording 随选型重算
    } catch (e) {
      // 失败(如录制中被后端拒绝):danger 横幅 + 回弹选项显示。
      error = `${e}`;
      settings = await getSettings().catch(() => settings);
      // 回弹本地绑定值:与点击前同值也必然不同于 bind:group 已改的新值,
      // 本地 state 变更强制触发 DOM 回写,浏览器原生移走的 checked 被拉回。
      asrChoice = asrModelToChoice(settings?.asr_model);
    }
  }

  // —— 镜像加速(逻辑照搬 ModelDownloadCard)——
  async function toggleMirror() {
    mirrorTest = null;
    if (!settings) return;
    settings = { ...settings, mirror_enabled: !settings.mirror_enabled };
    await setSettings(settings);
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
  <p class="desc">所有录音与识别都在本机完成,不上传任何音频。</p>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  <!-- —— 通用 —— -->
  <section>
    <h2 class="section-title">通用</h2>
    <div class="rows">
      <div class="row">
        <div class="row-info"><span class="row-label">外观</span></div>
        <div class="seg">
          <label class="seg-item">
            <input type="radio" name="theme" value="light" bind:group={themeChoice} disabled={!settings} onchange={changeTheme} />亮色
          </label>
          <label class="seg-item">
            <input type="radio" name="theme" value="dark" bind:group={themeChoice} disabled={!settings} onchange={changeTheme} />暗色
          </label>
          <label class="seg-item">
            <input type="radio" name="theme" value="system" bind:group={themeChoice} disabled={!settings} onchange={changeTheme} />跟随系统
          </label>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label">全局快捷键</span>
          <span class="row-desc">在任意应用中按组合键开始 / 停止录制</span>
        </div>
        {#if shortcutEnabled}
          <input
            class="shortcut-input"
            readonly
            value={capturingShortcut ? "" : displayShortcut(settings?.shortcut ?? "")}
            placeholder="按下组合键…"
            onfocus={() => (capturingShortcut = true)}
            onblur={() => (capturingShortcut = false)}
            onkeydown={onShortcutKeydown}
          />
        {/if}
        <input
          type="checkbox"
          class="ctl switch"
          aria-label="启用全局快捷键"
          bind:checked={shortcutEnabled}
          disabled={!settings}
          onchange={toggleShortcutEnabled}
        />
      </div>
      <label class="row">
        <div class="row-info"><span class="row-label">开机自动启动</span></div>
        <input type="checkbox" class="ctl switch" bind:checked={autostartEnabled} onchange={toggleAutostart} />
      </label>
      <label class="row">
        <div class="row-info">
          <span class="row-label">菜单栏常驻</span>
          <span class="row-desc">关闭窗口只隐藏到菜单栏,录制不中断</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          bind:checked={trayEnabled}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.tray_enabled = trayEnabled))}
        />
      </label>
      <label class="row">
        <div class="row-info">
          <span class="row-label">匿名使用统计</span>
          <span class="row-desc">仅上报功能使用次数与版本信息，绝不包含任何会议内容</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          bind:checked={telemetryOn}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.telemetry_enabled = telemetryOn))}
        />
      </label>
    </div>
  </section>

  <!-- —— 存储 ——(紧随通用:目录选择要在下载模型前被看到,新装用户价值最高) -->
  <section>
    <h2 class="section-title">存储</h2>
    <div class="rows">
      {@render storeRow("data", "数据存储目录", dataDirLabel)}
      {@render storeRow("models", "模型存储目录", modelsDirLabel)}
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

  <!-- —— 录制 —— -->
  <section>
    <h2 class="section-title">录制</h2>
    <div class="rows">
      <label class="row">
        <div class="row-info">
          <span class="row-label">仅录制系统声音</span>
          <span class="row-desc">不开麦克风,只录电脑播放的声音。适合直播、网课等旁听场景</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          bind:checked={sysOnly}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.record_system_only = sysOnly))}
        />
      </label>
      <label class="row" class:dim={sysOnly}>
        <div class="row-info">
          <span class="row-label">保持外放音量</span>
          <span class="row-desc">外放开会时音量不再被系统压低,回声自动消除{sysOnly ? "(仅录系统声音时无效)" : ""}</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          bind:checked={keepVol}
          disabled={!settings || sysOnly}
          onchange={() => saveSetting((s) => (s.keep_output_volume = keepVol))}
        />
      </label>
      <label class="row">
        <div class="row-info">
          <span class="row-label">乱码过滤</span>
          <span class="row-desc">丢弃静音、噪声被误识别出的文字。多语种会议误伤时可关闭</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          bind:checked={langFilter}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.language_filter = langFilter))}
        />
      </label>
      <label class="row">
        <div class="row-info">
          <span class="row-label">保留录音音频</span>
          <span class="row-desc">录完可回放核对;关闭可节省磁盘</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          bind:checked={keepAudio}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.keep_audio = keepAudio))}
        />
      </label>
      <div class="row">
        <div class="row-info">
          <span class="row-label">识别引擎</span>
          <span class="row-desc">
            {asrChoice === "whisper"
              ? "多语种支持广,说话人区分较粗"
              : asrChoice === "paraformer"
                ? "中文更准、英文较弱"
                : "推荐 · 中英日韩粤语,功能最全"}
          </span>
        </div>
        <div class="seg" class:disabled={recording.isLive}>
          <label class="seg-item">
            <input
              type="radio"
              name="asr"
              value="sense_voice"
              bind:group={asrChoice}
              disabled={recording.isLive || !settings}
              onchange={() => changeAsr("sense_voice")}
            />SenseVoice
          </label>
          <label class="seg-item">
            <input
              type="radio"
              name="asr"
              value="whisper"
              bind:group={asrChoice}
              disabled={recording.isLive || !settings}
              onchange={() => changeAsr("whisper")}
            />Whisper
          </label>
          <label class="seg-item">
            <input
              type="radio"
              name="asr"
              value="paraformer"
              bind:group={asrChoice}
              disabled={recording.isLive || !settings}
              onchange={() => changeAsr("paraformer")}
            />Paraformer
          </label>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label">声纹模型</span>
          <span class="row-desc">
            {speakerChoice === "eres2netv2"
              ? "备选模型;切换后后台用录音样本重建声纹库(约半分钟),期间录制暂不自动认人"
              : "推荐 · 切换后后台用录音样本重建声纹库(约半分钟),期间录制暂不自动认人"}
          </span>
        </div>
        <div class="seg" class:disabled={recording.isLive}>
          <label class="seg-item">
            <input
              type="radio"
              name="speaker-model"
              value="campplus"
              bind:group={speakerChoice}
              disabled={recording.isLive || !settings}
              onchange={() => changeSpeakerModel("campplus")}
            />CAM++
          </label>
          <label
            class="seg-item"
            title={eres2Missing ? "模型未下载:请先在下方「语音模型」中下载 ERes2NetV2" : ""}
          >
            <input
              type="radio"
              name="speaker-model"
              value="eres2netv2"
              bind:group={speakerChoice}
              disabled={recording.isLive || !settings || eres2Missing}
              onchange={() => changeSpeakerModel("eres2netv2")}
            />ERes2NetV2
          </label>
        </div>
      </div>
      <label class="row">
        <div class="row-info">
          <span class="row-label">会后 Aing</span>
          <span class="row-desc">停止录制后自动用大模型 Aing 转写稿(错字修正、段落归并);在线接口或本机 Agent,在左侧 AI 页配置</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          bind:checked={refineOn}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.refine_enabled = refineOn))}
        />
      </label>
    </div>
    {#if asrModelMissing}
      <div class="banner warn">所选识别引擎的模型未下载,请在下方「语音模型」中下载。</div>
    {/if}
    <p class="lock-hint">
      {recording.isLive ? "录制进行中:识别引擎已锁定,其余更改下一场录制生效。" : "更改在下一场录制生效。"}
    </p>
  </section>

  <!-- —— 语音模型 —— -->
  <section>
    <h2 class="section-title">语音模型</h2>
    <div class="rows">
      {#if status}
        {#each status.artifacts as a (a.id)}
          <div class="row">
            <div class="row-info">
              <button
                class="url-toggle"
                aria-expanded={expandedId === a.id}
                aria-label={expandedId === a.id ? "收起下载地址" : "展开下载地址"}
                onclick={() => (expandedId = expandedId === a.id ? null : a.id)}
              >
                <span class="caret" class:open={expandedId === a.id}>▸</span>
                <span class="row-label">{a.label} · 约 {a.approx_mb}MB</span>
              </button>
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
          {#if expandedId === a.id}
            <div class="url-detail">
              <div class="url-line">
                <span class="url-tag">原始地址</span>
                <code class="url-text">{a.url}</code>
                <button class="link" onclick={() => navigator.clipboard.writeText(a.url)}>复制</button>
              </div>
              <div class="url-line">
                <span class="url-tag">镜像地址</span>
                {#if settings?.mirror_enabled && (settings?.mirror_prefix ?? "").trim()}
                  <code class="url-text">{effectiveUrl(a.url)}</code>
                  <button class="link" onclick={() => navigator.clipboard.writeText(effectiveUrl(a.url))}>复制</button>
                {:else}
                  <span class="url-muted">未启用镜像加速</span>
                {/if}
              </div>
            </div>
          {/if}
        {/each}
      {/if}
      <div class="row">
        <div class="row-info">
          <span class="row-label">镜像加速</span>
          <span class="row-desc">
            {#if mirrorTest}
              <span class={mirrorTest.ok ? "mtest-ok" : "mtest-err"}>
                {mirrorTest.ok ? `测试成功(${mirrorTest.msg})` : `测试失败: ${mirrorTest.msg}`}
              </span>
            {:else}
              国内网络下载模型更快
            {/if}
          </span>
        </div>
        {#if settings?.mirror_enabled}
          <button class="btn-secondary" onclick={runMirrorTest} disabled={mirrorTesting}>
            {mirrorTesting ? "测试中…" : "测试"}
          </button>
        {/if}
        <input
          type="checkbox"
          class="ctl switch"
          aria-label="使用镜像加速"
          checked={settings?.mirror_enabled ?? false}
          disabled={!settings}
          onchange={toggleMirror}
        />
      </div>
    </div>
  </section>

  <!-- —— 关于 / 更新 —— -->
  <section>
    <h2 class="section-title">关于</h2>
    <div class="rows">
      <div class="row">
        <div class="row-info">
          <span class="row-label">当前版本 v{appVersion || updateInfo?.current || "…"}</span>
          <span class="row-desc">
            {#if updateChecking}
              正在检查…
            {:else if updateError}
              {updateError}
            {:else if updateInfo?.has_update}
              发现新版 v{updateInfo.latest}，前往下载页更新
            {:else if updateInfo}
              已是最新版本
            {:else}
              从 GitHub Releases 检查是否有新版
            {/if}
          </span>
        </div>
        {#if updateInfo?.has_update}
          <button class="btn-secondary" onclick={() => updateInfo && openUrl(updateInfo.url)}>下载 v{updateInfo.latest}</button>
        {:else}
          <button class="btn-secondary" disabled={updateChecking} onclick={doCheckUpdate}>
            {updateChecking ? "检查中…" : "检查更新"}
          </button>
        {/if}
      </div>
    </div>
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
    max-width: 46rem;
  }
  h1 {
    margin: 0 0 0.3rem;
  }
  .desc {
    color: var(--ink-faint);
    font-size: 0.85rem;
    margin: 0 0 0.5rem;
  }
  section {
    margin-top: 1.3rem;
  }
  .section-title {
    font-size: 0.82rem;
    font-weight: 500;
    color: var(--ink-secondary);
    margin: 0 0 0.45rem;
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
  .row.dim {
    opacity: 0.55;
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
  .row-path {
    font-size: 0.8rem;
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
  /* 右侧控件与行级操作 */
  .ctl {
    flex: none;
    margin: 0;
  }
  .row-action {
    visibility: hidden;
  }
  .row:hover .row-action {
    visibility: visible;
  }
  /* segmented(分段选择):surface-press 槽 + 选中项 canvas 浮起,单行放下多选一 */
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
  .seg.disabled {
    opacity: 0.6;
  }
  .seg.disabled .seg-item {
    cursor: default;
  }
  .mtest-ok { color: var(--success, var(--ink-secondary)); }
  .mtest-err { color: var(--danger-ink); }
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
    background: var(--warning-tint);
    border-top: 1px solid var(--warning-line);
    border-bottom: 1px solid var(--hairline);
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
  .lock-hint {
    font-size: 0.8rem;
    color: var(--ink-faint);
    margin: 0.45rem 0 0;
  }
  /* 快捷键录入框:input 形态(聚焦浮出 canvas + accent 环) */
  .shortcut-input {
    flex: none;
    width: 10rem;
    box-sizing: border-box;
    padding: 0.3em 0.6em;
    border-radius: var(--radius-md);
    border: 1px solid var(--hairline-strong);
    background: var(--surface-press);
    color: var(--ink);
    font-size: 0.88rem;
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
    margin: 0.4rem 0 0;
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
  .url-toggle {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    color: inherit;
    font: inherit;
    text-align: left;
  }
  .caret {
    display: inline-block;
    transition: transform 0.15s ease;
    opacity: 0.6;
    font-size: 0.85em;
  }
  .caret.open {
    transform: rotate(90deg);
  }
  .url-detail {
    padding: 6px 0 10px 20px;
    display: flex;
    flex-direction: column;
    gap: 6px;
    border-bottom: 1px solid var(--hairline);
  }
  .url-line {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .url-tag {
    flex: 0 0 auto;
    font-size: 0.8em;
    opacity: 0.6;
  }
  .url-text {
    flex: 1 1 auto;
    min-width: 0;
    overflow-x: auto;
    white-space: nowrap;
    font-size: 0.8em;
    opacity: 0.85;
  }
  .url-muted {
    font-size: 0.8em;
    opacity: 0.5;
  }
</style>
