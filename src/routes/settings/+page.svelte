<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { open } from "@tauri-apps/plugin-dialog";
  import { enable, disable, isEnabled } from "@tauri-apps/plugin-autostart";
  import { recording } from "$lib/recording.svelte";
  import { applyTheme } from "$lib/theme";
  import { i18n, t } from "$lib/i18n/index.svelte";
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
    cancelModelsDownload,
    openModelsDir,
    testMirror,
    testCloudAsr,
    calendarPermission,
    requestCalendarPermission,
    type ModelsStatus,
    type Settings,
    type ModelDownloadEvent,
    type MigrateEvent,
  } from "$lib/models";
  import { countPeopleWithoutSamples, rebuildVoiceprintLibrary, voiceprintLibraryModel } from "$lib/people";
  import { refineReady } from "$lib/refineReady";
  import EditableField from "$lib/EditableField.svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { getVersion } from "@tauri-apps/api/app";
  import { checkUpdate, applyUpdate, type UpdateInfo } from "$lib/update";
  import Segmented from "$lib/Segmented.svelte";
  import { applyTelemetrySetting, reportError } from "$lib/analytics";
  import type { SegmentedItem } from "$lib/segmented";

  let settings = $state<Settings | null>(null);

  // —— 关于 / 更新 ——
  let appVersion = $state("");
  let updateInfo = $state<UpdateInfo | null>(null);
  let updateChecking = $state(false);
  let updateError = $state("");
  let mirrorTest = $state<{ ok: boolean; msg: string } | null>(null);
  let mirrorTesting = $state(false);
  let expandedId = $state<string | null>(null);

  /** 镜像前缀:曾是设置项上的可编辑字段,三删一藏后 UI 从不允许编辑它——值永远等于后端
   * 内置常量,该字段随之从设置类型上移除。这里保留同值字面量只为拼预览 URL / 传给
   * test_mirror,不代表前端可配置。 */
  const MIRROR_PREFIX = "https://ghfast.top/";
  /** 镜像开启时返回「前缀+原始url」(等同后端 apply_mirror);关闭时返回原始 url。 */
  function effectiveUrl(url: string): string {
    if (!settings?.mirror_enabled) return url;
    return `${MIRROR_PREFIX}${url}`;
  }
  async function runMirrorTest() {
    if (!settings) return;
    mirrorTesting = true;
    mirrorTest = null;
    try {
      mirrorTest = { ok: true, msg: await testMirror(MIRROR_PREFIX) };
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
      updateError = t("settings.update.checkFailed", { e });
    } finally {
      updateChecking = false;
    }
  }

  // —— 一键更新:updater 插件下载安装 + 自动重启;失败亮出「打开发布页」兜底 ——
  let updating = $state(false);
  let updatingLabel = $state(t("settings.update.updating"));
  let updateFallback = $state(false);
  // 与 +layout 横幅同款守卫:安装成功即 relaunch,录音/收尾中更新会截断录音尾段。
  const updateBlocked = $derived(recording.isLive || recording.stopping);
  async function doOneClickUpdate() {
    if (updateBlocked) return;
    updating = true;
    updateError = "";
    updatingLabel = t("settings.update.updating");
    try {
      const r = await applyUpdate((p) => (updatingLabel = p.label));
      if (r === "none") {
        // GitHub API 已见新版但 latest.json 未就绪(发布产物上传有时差)。
        updateError = t("settings.update.notReady");
        updateFallback = true;
      }
    } catch (e) {
      updateError = t("settings.update.oneClickFailed", { e });
      updateFallback = true;
      // 更新装不上是本机日志之外无人知晓、又直接卡住所有后续修复的那类故障:
      // 用户停在旧版本上,之后修的每个 bug 都到不了他手里。
      // detail 是上报载荷、不是界面文案,固定英文:看板上不该按 UI 语言劈成两种写法。
      reportError("update_failed", `one-click update failed: ${e}`);
    } finally {
      updating = false;
    }
  }
  let status = $state<ModelsStatus | null>(null);
  /**
   * ASR radio 的本地绑定值(bind:group)。不直接从 settings 派生:切型失败回弹时
   * settings.asr_model 前后同值,派生 checked 可能跳过 DOM 回写,而浏览器原生已把
   * 选中移到新项——本地 state 显式改回旧值必触发 DOM 对齐,天然回弹。
   */
  let asrChoice = $state("sense_voice");
  /** asr_model 后端值 → radio 本地 value 的五态映射(whisper/paraformer/qwen3/firered/sense_voice)。 */
  function asrModelToChoice(m: string | undefined): string {
    return m === "whisper" ? "whisper"
      : m === "paraformer" ? "paraformer"
      : m === "qwen3" ? "qwen3"
      : m === "firered" ? "firered"
      : "sense_voice";
  }
  /** danger 横幅：迁移/删除/切型/下载的错误统一在此显示。 */
  let error = $state("");

  // —— 识别方式(本地/云端)本地绑定值:同 asrChoice 理由,不派生 checked ——
  /** "local" | "cloud"。 */
  let asrMode = $state("local");
  /** 云端厂商:"volcano" | "aliyun"。 */
  let cloudProvider = $state("volcano");
  let volcAppKey = $state("");
  let volcAccessKey = $state("");
  let dashKey = $state("");
  /** 热词词表(仅 Qwen3 引擎消费,声纹人名由后端自动并入)。 */
  let hotwords = $state("");
  /** 「测试连接」按钮态与结果(逻辑照搬 runMirrorTest)。 */
  let testingCloud = $state(false);
  let cloudTestResult = $state<{ ok: boolean; msg: string } | null>(null);
  let cloudTestGeneration = 0;

  // —— 新四区块的本地绑定值 ——
  // 一律用本地 $state + bind(group/checked),不直接从 settings 派生 checked：
  // 失败回弹时后端值可能与点击前同值,派生表达式因缓存相等而跳过 DOM 回写(浏览器已把
  // 勾选/选中改到新态),本地 state 显式改回旧值必触发 DOM 对齐——与 asrChoice 同理。
  /** 外观主题 radio:"light" | "dark" | "system"。 */
  let themeChoice = $state("system");
  /** UI 语言 radio:"system" | "zh" | "en"。 */
  let langChoice = $state("system");
  /** 设置开关的本地镜像(为什么用本地 state 见上方注释)。 */
  let langFilter = $state(false);
  let audioScheme = $state<"a" | "ab" | "b">("a");
  let audioRetention = $state<"forever" | "90d" | "30d">("forever");
  let calendarMatch = $state(true);
  /** 日历授权态(unavailable = 非 macOS,整块隐藏)。 */
  let calPerm = $state("unavailable");
  let calRequesting = $state(false);
  let refineOn = $state(false);
  let identifyAuto = $state(false);
  /** 系统区:全局快捷键开关 / 菜单栏常驻 / 开机自启(自启为系统真值,非 settings)。 */
  let shortcutEnabled = $state(false);
  let trayEnabled = $state(false);
  let telemetryEnabled = $state(true);
  /** 保存中禁用开关:快速连点会让先到的请求把后到的意图覆盖掉。 */
  let telemetrySaving = $state(false);
  let autostartEnabled = $state(false);
  /** 快捷键录入框聚焦态:聚焦时清空显示并提示「按下组合键…」。 */
  let capturingShortcut = $state(false);

  /** 「高级」折叠区展开态:纯组件 $state,只在本次页面挂载内存活——刷新页面/重新
   * 进入设置路由都会回到收起。不落 settings.json,也不跨会话记忆,如实注释。 */
  let advOpen = $state(false);

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

  // 当前 ASR 选型对应的工件 id:sense_voice→asr,其余选型 id 与 asr_model 同名。
  const asrArtifactId = $derived(
    settings?.asr_model === "whisper" ? "whisper"
      : settings?.asr_model === "paraformer" ? "paraformer"
      : settings?.asr_model === "qwen3" ? "qwen3"
      : settings?.asr_model === "firered" ? "firered"
      : "asr",
  );
  const asrModelMissing = $derived(
    !!status && !status.artifacts.find((a) => a.id === asrArtifactId)?.present,
  );

  // 会后 AI 就绪状态徽标:开关已开但配置未齐备(openai 档缺 base_url/model/api_key,
  // 或 provider 未回填)时提示,口径对齐后端 readiness(refineReady,见 $lib/refineReady)。
  // settings 未回填前 refineOn 恒 false,徽标天然不出现,不需要额外判空。
  const refineConfigReady = $derived(!!settings && refineReady(settings));

  // 迁移/更改目录被阻断的原因(禁用 title 用);录制中/下载中/迁移中皆阻断。
  const migrateBlockReason = $derived(
    recording.isLive
      ? t("settings.migrate.blockedRecording")
      : downloadingActive
        ? t("settings.migrate.blockedDownloading")
        : migrating
          ? t("settings.migrate.blockedMigrating")
          : "",
  );

  const dataDirLabel = $derived(settings?.data_dir || t("settings.store.defaultDir"));
  const modelsDirLabel = $derived(settings?.models_dir || t("settings.store.defaultDir"));

  // 声音处理方案三档(spec 2026-08-10)。settings 未回填前整组禁用,与其它开关同纪律。
  // 默认档(成品轨)放首位;底层值 a/ab/b 不变,仅展示顺序与命名(2026-08-11 用户拍板去代号)。
  const audioSchemeItems = $derived<SegmentedItem[]>([
    { id: "b", label: t("settings.record.audioScheme.b"), disabled: !settings },
    { id: "a", label: t("settings.record.audioScheme.a"), disabled: !settings },
    { id: "ab", label: t("settings.record.audioScheme.ab"), disabled: !settings },
  ]);

  // 录音音频保留期三档,同 audioSchemeItems 纪律。
  const audioRetentionItems = $derived<SegmentedItem[]>([
    { id: "forever", label: t("settings.record.audioRetention.forever"), disabled: !settings },
    { id: "90d", label: t("settings.record.audioRetention.d90"), disabled: !settings },
    { id: "30d", label: t("settings.record.audioRetention.d30"), disabled: !settings },
  ]);

  // 外观主题 / UI 语言:settings 未回填前整组禁用,选中态用本地镜像(themeChoice/langChoice)。
  const themeItems = $derived<SegmentedItem[]>([
    { id: "light", label: t("settings.theme.light"), disabled: !settings },
    { id: "dark", label: t("settings.theme.dark"), disabled: !settings },
    { id: "system", label: t("settings.theme.system"), disabled: !settings },
  ]);
  const langItems = $derived<SegmentedItem[]>([
    { id: "zh", label: t("common.language.zh"), disabled: !settings },
    { id: "en", label: t("common.language.en"), disabled: !settings },
    { id: "system", label: t("common.language.system"), disabled: !settings },
  ]);

  // 识别方式 / 云厂商:录制中被后端拒改(set_settings 拦截),前端同步锁 UI——
  // disabled 叠加 recording.isLive,与原 radio 版 `.seg.disabled` 视觉锁同语义。
  const asrModeItems = $derived<SegmentedItem[]>([
    { id: "local", label: t("settings.asrMode.local"), disabled: recording.isLive || !settings },
    { id: "cloud", label: t("settings.asrMode.cloud"), disabled: recording.isLive || !settings },
    { id: "local_cloud", label: t("settings.asrMode.localCloud"), disabled: recording.isLive || !settings },
  ]);
  const cloudProviderItems = $derived<SegmentedItem[]>([
    { id: "volcano", label: t("settings.cloud.volcano"), disabled: recording.isLive || !settings },
    { id: "aliyun", label: t("settings.cloud.aliyun"), disabled: recording.isLive || !settings },
  ]);

  async function refreshSettings() {
    try {
      settings = await getSettings();
      asrChoice = asrModelToChoice(settings.asr_model);
      speakerChoice = settings.speaker_model === "eres2netv2" ? "eres2netv2" : "campplus";
      syncLocalFromSettings(settings);
    } catch (e) {
      error = t("settings.loadSettingsFailed", { e });
    }
  }

  /** 说明卡「继续」:唯一拉起系统日历授权的入口(开关切换绝不直接弹窗)。
      授权成功后后端顺带回填历史笔记;insufficient/denied 由文案引导去系统设置。 */
  async function requestCalendarAuth() {
    if (calRequesting) return;
    calRequesting = true;
    try {
      await requestCalendarPermission();
    } catch {
      // 结果以重新查询的权限态为准,请求错误无需单独提示。
    }
    try {
      calPerm = await calendarPermission();
    } catch {}
    calRequesting = false;
  }

  /** 把后端真值同步到各本地镜像(初始化 / 保存失败回弹后重新对齐 DOM)。 */
  function syncLocalFromSettings(s: Settings) {
    themeChoice = s.theme;
    langChoice = s.ui_lang;
    langFilter = s.language_filter;
    audioScheme = s.audio_scheme;
    audioRetention = s.audio_retention;
    calendarMatch = s.calendar_match_enabled;
    refineOn = s.refine_enabled;
    identifyAuto = s.identify_auto_apply;
    shortcutEnabled = s.shortcut_enabled;
    trayEnabled = s.tray_enabled;
    telemetryEnabled = s.telemetry_enabled;
    asrMode = s.asr_mode === "cloud" || s.asr_mode === "local_cloud" ? s.asr_mode : "local";
    cloudProvider = s.cloud_asr_provider === "aliyun" ? "aliyun" : "volcano";
    volcAppKey = s.volc_app_key;
    volcAccessKey = s.volc_access_key;
    dashKey = s.dashscope_api_key;
    hotwords = s.asr_hotwords;
    speakerMatchChoice = s.speaker_match === "knn_vote" ? "knn_vote" : "nearest";
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
      error = t("settings.models.statusFailed", { e });
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
      error = t("settings.models.downloadFailed", { e: e.message });
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
    calendarPermission().then((p) => (calPerm = p)).catch(() => {});
    refreshStatus();
    refreshDiskUsage();
    void refreshLibModel();
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
      stopRebuildPoll(); // 组件销毁后别再打 IPC
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
      if (!String(e).includes("已在进行中")) { // i18n-exempt: 与后端错误原文判等
        dropProg(id);
        error = t("settings.models.downloadFailed", { e });
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
      error = t("common.deleteFailed", { e });
    }
  }

  // —— 外观 / 录制 / 系统:通用「取新鲜值→改→存」保存 ——
  // 成功后 settings 与本地镜像同步;失败时 danger 横幅 + 从后端真值回弹本地镜像。
  /** 在系统文件管理器中打开模型存储目录(失败走 danger 横幅,如目录被迁移后不存在)。 */
  async function doOpenModelsDir() {
    error = "";
    try {
      await openModelsDir();
    } catch (e) {
      error = `${e}`;
    }
  }

  async function saveSetting(mut: (s: Settings) => void) {
    error = "";
    try {
      const fresh = await getSettings();
      mut(fresh);
      await setSettings(fresh);
      settings = fresh;
      syncLocalFromSettings(fresh);
    } catch (e) {
      error = t("common.saveFailed", { e });
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
      error = t("settings.theme.switchFailed", { e });
      settings = await getSettings().catch(() => settings);
      if (settings) syncLocalFromSettings(settings); // 回弹 themeChoice
    }
  }

  // UI 语言:存 settings 后 i18n.setChoice 即时生效(界面响应式重渲;托盘由后端
  // set_settings 检测 ui_lang 变化自行重建)。套路同 changeTheme。
  async function changeLang() {
    error = "";
    try {
      const fresh = await getSettings();
      fresh.ui_lang = langChoice;
      await setSettings(fresh);
      settings = fresh;
      i18n.setChoice(langChoice);
    } catch (e) {
      error = t("common.language.switchFailed", { e });
      settings = await getSettings().catch(() => settings);
      if (settings) syncLocalFromSettings(settings); // 回弹 langChoice
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
      error = t("settings.shortcut.saveFailed", { e });
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
      error = t("settings.shortcut.saveFailed", { e: err });
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
      error = t("settings.autostart.saveFailed", { e });
      autostartEnabled = await isEnabled().catch(() => autostartEnabled); // 回读真值回弹
    }
  }

  // —— 磁盘:清理录音音频(两段确认后)——
  async function doPurge() {
    error = "";
    const days = purgeChoice === "all" ? null : purgeChoice === "30" ? 30 : 90;
    try {
      const freed = await purgeAudio(days);
      freedText = t("settings.store.freed", { size: fmtBytes(freed) });
      showPurge = false;
      await refreshDiskUsage();
    } catch (e) {
      error = t("settings.store.purgeFailed", { e });
    }
  }

  // —— 声纹模型选型 ——
  let speakerChoice = $state("campplus");
  const eres2Missing = $derived(
    !!status && !status.artifacts.find((a) => a.id === "speaker-eres2netv2")?.present,
  );
  // 说话人识别方法(库声纹匹配策略):nearest=单最近邻(默认,2026-08-12 离线评测
  // 胜出),knn_vote=top-5 多数票(实验;每人多份采集足够密时理论上抗离群劫持)。
  // 未知历史值按 nearest 展示,与后端 matcher_from_key 回落一致。
  let speakerMatchChoice = $state("nearest");
  const speakerMatchItems = $derived<SegmentedItem[]>([
    { id: "nearest", label: t("settings.speakerMatch.nearest"), disabled: !settings },
    { id: "knn_vote", label: t("settings.speakerMatch.knnVote"), disabled: !settings },
  ]);
  const speakerModelItems = $derived<SegmentedItem[]>([
    { id: "campplus", label: "CAM++", disabled: recording.isLive || !settings },
    {
      id: "eres2netv2",
      label: "ERes2NetV2",
      disabled: recording.isLive || !settings || eres2Missing,
      title: eres2Missing ? t("settings.speaker.eres2Missing") : undefined,
    },
  ]);
  // 切换前先弹一步轻量行内确认(同存储区「清理…」的 purge-bar 形态):无样本人物
  // 换模型后质心被清空、重建前认不出,得让用户知情再动手。
  let pendingSpeakerModel = $state<string | null>(null);
  let speakerNoSampleCount = $state(0);
  /** 声纹库**实际**所处的模型空间。分段控件显示的是设置值,重建失败时两者会长期
      不一致——界面显示新模型、库里还是旧的,声纹识别全程停用而用户看不出来。 */
  let libModel = $state("");
  let rebuilding = $state(false);
  let rebuildNote = $state("");
  const modelLabel = (m: string) => (m === "eres2netv2" ? "ERes2NetV2" : m === "campplus" ? "CAM++" : m);
  /** 库与选型不一致 = 声纹识别当前是停用的。libModel 空 = 还没查到,不做判断。 */
  const speakerModelMismatch = $derived(
    !!libModel && !!settings?.speaker_model && libModel !== settings.speaker_model,
  );
  async function refreshLibModel() {
    libModel = await voiceprintLibraryModel().catch(() => "");
  }
  /** 重建轮询句柄:离开页面要停,否则组件销毁后还在打 IPC。 */
  let rebuildPoll: ReturnType<typeof setInterval> | null = null;
  function stopRebuildPoll() {
    if (rebuildPoll) clearInterval(rebuildPoll);
    rebuildPoll = null;
  }
  async function doRebuildVoiceprints() {
    rebuilding = true;
    rebuildNote = "";
    error = "";
    try {
      await rebuildVoiceprintLibrary();
      rebuildNote = t("settings.speaker.rebuildStarted");
      // 后端只负责起线程,IPC 立刻就返回。不轮询的话,重建成功之后这页仍然
      // 一直显示「不一致 / 识别停用」,还允许你反复点重建(codex review 三轮 P2)。
      // 轮询到标签对上为止,最多约 2 分钟(实测重建约半分钟)。
      stopRebuildPoll();
      let left = 40;
      rebuildPoll = setInterval(async () => {
        await refreshLibModel();
        if (!speakerModelMismatch || --left <= 0) {
          stopRebuildPoll();
          rebuilding = false;
          if (!speakerModelMismatch) rebuildNote = "";
        }
      }, 3000);
      return;
    } catch (e) {
      error = `${e}`;
    }
    rebuilding = false;
  }
  function revertSpeakerChoice() {
    speakerChoice = settings?.speaker_model === "eres2netv2" ? "eres2netv2" : "campplus";
  }
  async function changeSpeakerModel(model: string) {
    if (settings?.speaker_model === model) return;
    rebuildNote = "";
    error = "";
    try {
      speakerNoSampleCount = await countPeopleWithoutSamples();
    } catch {
      speakerNoSampleCount = 0; // 查询失败不挡切换,确认文案少一句而已
    }
    pendingSpeakerModel = model;
  }
  async function confirmSpeakerModelChange() {
    const model = pendingSpeakerModel;
    pendingSpeakerModel = null;
    if (!model) return;
    error = "";
    try {
      const fresh = await getSettings();
      fresh.speaker_model = model;
      await setSettings(fresh);
      settings = fresh;
      speakerChoice = model;
      // 切换会起后台重建,库标签要过一会儿才变;先刷一次让"库当前"如实反映此刻
      // (仍是旧值),重建完成后用户再次进设置页会看到新值。
      void refreshLibModel();
    } catch (e) {
      error = `${e}`;
      revertSpeakerChoice();
    }
  }
  function cancelSpeakerModelChange() {
    pendingSpeakerModel = null;
    revertSpeakerChoice();
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

  // —— 识别方式(本地/云端)——(逻辑照搬 changeAsr:取新鲜值再改,失败回弹)
  async function changeAsrMode(mode: string) {
    if (settings?.asr_mode === mode) return;
    error = "";
    try {
      const fresh = await getSettings();
      fresh.asr_mode = mode;
      await setSettings(fresh);
      settings = fresh;
      asrMode = mode;
      invalidateCloudTest(); // 切换方式后旧测试结果不再有意义
      await refreshStatus(); // recording_ready 随方式重算(云端模式看凭证而非本地工件)
    } catch (e) {
      error = `${e}`;
      settings = await getSettings().catch(() => settings);
      asrMode = settings?.asr_mode === "cloud" || settings?.asr_mode === "local_cloud" ? settings.asr_mode : "local";
    }
  }

  // —— 云端 ASR「测试连接」——(逻辑照搬 runMirrorTest)
  function invalidateCloudTest() {
    cloudTestGeneration += 1;
    cloudTestResult = null;
  }

  async function doTestCloud() {
    if (!settings) return;
    const generation = ++cloudTestGeneration;
    const input = {
      provider: cloudProvider,
      volcAppKey,
      volcAccessKey,
      dashKey,
    };
    testingCloud = true;
    cloudTestResult = null;
    try {
      const msg = await testCloudAsr(
        input.provider,
        input.volcAppKey,
        input.volcAccessKey,
        input.dashKey,
      );
      if (generation !== cloudTestGeneration) return;
      cloudTestResult = { ok: true, msg };
    } catch (e) {
      if (generation !== cloudTestGeneration) return;
      cloudTestResult = { ok: false, msg: String(e) };
    } finally {
      testingCloud = false;
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
  const phaseKeys: Record<string, string> = {
    downloading: "settings.models.phase.downloading",
    verifying: "settings.models.phase.verifying",
    extracting: "settings.models.phase.extracting",
    done: "settings.models.phase.done",
  };
  const phaseText = (phase: string) => (phaseKeys[phase] ? t(phaseKeys[phase]) : phase);
</script>

<main class="container">
  <h1>{t("settings.title")}</h1>
  <p class="desc">
    {asrMode === "cloud"
      ? t("settings.desc.cloud")
      : asrMode === "local_cloud"
        ? t("settings.desc.localCloud")
        : t("settings.desc.local")}
  </p>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  <!-- —— 通用 —— -->
  <section>
    <h2 class="section-title">{t("settings.section.general")}</h2>
    <div class="rows">
      <div class="row">
        <div class="row-info"><span class="row-label">{t("settings.theme.label")}</span></div>
        <Segmented
          items={themeItems}
          value={themeChoice}
          onSelect={(id) => {
            themeChoice = id;
            changeTheme();
          }}
        />
      </div>
      <div class="row">
        <div class="row-info"><span class="row-label">{t("common.language.label")}</span></div>
        <Segmented
          items={langItems}
          value={langChoice}
          onSelect={(id) => {
            langChoice = id;
            changeLang();
          }}
        />
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.shortcut.label")}</span>
          <span class="row-desc">{t("settings.shortcut.desc")}</span>
        </div>
        {#if shortcutEnabled}
          <input
            class="shortcut-input"
            readonly
            value={capturingShortcut ? "" : displayShortcut(settings?.shortcut ?? "")}
            placeholder={t("settings.shortcut.placeholder")}
            onfocus={() => (capturingShortcut = true)}
            onblur={() => (capturingShortcut = false)}
            onkeydown={onShortcutKeydown}
          />
        {/if}
        <input
          type="checkbox"
          class="ctl switch"
          aria-label={t("settings.shortcut.enableAria")}
          bind:checked={shortcutEnabled}
          disabled={!settings}
          onchange={toggleShortcutEnabled}
        />
      </div>
      <label class="row">
        <div class="row-info"><span class="row-label">{t("settings.autostart.label")}</span></div>
        <input type="checkbox" class="ctl switch" bind:checked={autostartEnabled} onchange={toggleAutostart} />
      </label>
      <label class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.tray.label")}</span>
          <span class="row-desc">{t("settings.tray.desc")}</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          bind:checked={trayEnabled}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.tray_enabled = trayEnabled))}
        />
      </label>
      <!-- 隐私:欢迎页文案承诺了这个开关的存在(shell.welcome.telemetryHint),
           删它就得同步改文案——上一版正是因为开关被移除而留下了一句假承诺。 -->
      <label class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.privacy.label")}</span>
          <span class="row-desc">{t("settings.privacy.desc")}</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          bind:checked={telemetryEnabled}
          disabled={!settings || telemetrySaving}
          onchange={async () => {
            // desired 当场定格,后面一律用它:saveSetting 内部会 syncLocalFromSettings,
            // 把 telemetryEnabled 改回磁盘真值,再读共享变量就读到了别人的值
            // (codex review 二轮 P1#2)。同时保存期间禁用控件,不给快速连点留窗口。
            const desired = telemetryEnabled;
            telemetrySaving = true;
            try {
              // 两个方向刻意不对称,都倒向"不发"那一侧:
              // 关掉 → 先当场停再落盘。落盘失败也已经停了,回放尤其不能等下次启动。
              // 打开 → 先落盘再起。抢跑的话 start() 去问后端总开关读到的还是旧值
              //        (false),刚打开的开关当场被关回去。
              if (!desired) applyTelemetrySetting(false);
              await saveSetting((s) => (s.telemetry_enabled = desired));
              // 落盘失败时 saveSetting 已把 telemetryEnabled 同步回磁盘真值,
              // 按真值决定起不起,别按用户那次没存住的意图。
              applyTelemetrySetting(telemetryEnabled);
            } finally {
              telemetrySaving = false;
            }
          }}
        />
      </label>
    </div>
  </section>

  <!-- —— 存储 ——(紧随通用:目录选择要在下载模型前被看到,新装用户价值最高) -->
  <section>
    <h2 class="section-title">{t("settings.section.store")}</h2>
    <div class="rows">
      {@render storeRow("data", t("settings.store.dataDir"), dataDirLabel)}
      {@render storeRow("models", t("settings.store.modelsDir"), modelsDirLabel)}
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.store.audioUsage")}</span>
          <span class="row-path">
            {audioBytes === null ? t("settings.store.calculating") : fmtBytes(audioBytes)}{freedText ? ` · ${freedText}` : ""}
          </span>
        </div>
        {#if !showPurge}
          <button
            class="btn-secondary row-action"
            disabled={recording.isLive}
            title={recording.isLive ? t("settings.store.purgeBlockedRecording") : t("settings.store.purgeTitle")}
            onclick={() => {
              freedText = "";
              showPurge = true;
            }}>{t("settings.store.purge")}</button
          >
        {/if}
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.record.audioRetention.label")}</span>
          <span class="row-desc">{t("settings.record.audioRetention.desc")}</span>
        </div>
        <Segmented
          items={audioRetentionItems}
          value={audioRetention}
          onSelect={(id) => {
            audioRetention = id as "forever" | "90d" | "30d";
            saveSetting((s) => (s.audio_retention = audioRetention));
          }}
        />
      </div>
    </div>
    {#if showPurge}
      <div class="purge-bar">
        <div class="purge-choices">
          <label class="mini-radio">
            <input type="radio" name="purge" value="30" bind:group={purgeChoice} />{t("settings.store.purge30")}
          </label>
          <label class="mini-radio">
            <input type="radio" name="purge" value="90" bind:group={purgeChoice} />{t("settings.store.purge90")}
          </label>
          <label class="mini-radio">
            <input type="radio" name="purge" value="all" bind:group={purgeChoice} />{t("settings.store.purgeAll")}
          </label>
        </div>
        <span class="confirm-text">{t("settings.store.purgeNote")}</span>
        <div class="purge-actions">
          <button class="link danger" onclick={doPurge}>{t("settings.store.purgeConfirm")}</button>
          <button class="link" onclick={() => (showPurge = false)}>{t("settings.cancel")}</button>
        </div>
      </div>
    {/if}
  </section>

  <!-- —— 录制 —— -->
  <section>
    <h2 class="section-title">{t("settings.section.record")}</h2>
    <div class="rows">
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.record.audioScheme.label")}</span>
          <span class="row-desc">{t("settings.record.audioScheme.desc")}</span>
        </div>
        <Segmented
          items={audioSchemeItems}
          value={audioScheme}
          onSelect={(id) => {
            audioScheme = id as "a" | "ab" | "b";
            saveSetting((s) => (s.audio_scheme = audioScheme));
          }}
        />
      </div>
      {#if calPerm !== "unavailable"}
        <label class="row">
          <div class="row-info">
            <span class="row-label">{t("settings.calendar.label")}</span>
            <span class="row-desc">{t("settings.calendar.desc")}</span>
          </div>
          <input
            type="checkbox"
            class="ctl switch"
            bind:checked={calendarMatch}
            disabled={!settings}
            onchange={() => saveSetting((s) => (s.calendar_match_enabled = calendarMatch))}
          />
        </label>
        {#if calendarMatch && calPerm === "not_determined"}
          <div class="row">
            <div class="row-info">
              <span class="row-label">{t("settings.calendar.cardTitle")}</span>
              <span class="row-desc">{t("settings.calendar.cardBody")}</span>
            </div>
            <button class="ctl" disabled={calRequesting} onclick={() => void requestCalendarAuth()}>
              {calRequesting ? t("settings.calendar.requesting") : t("settings.calendar.cardContinue")}
            </button>
          </div>
        {:else if calendarMatch && calPerm === "denied"}
          <div class="row"><div class="row-info"><span class="row-desc">{t("settings.calendar.denied")}</span></div></div>
        {:else if calendarMatch && calPerm === "write_only"}
          <div class="row"><div class="row-info"><span class="row-desc">{t("settings.calendar.insufficient")}</span></div></div>
        {/if}
      {/if}
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.asrMode.label")}</span>
          <span class="row-desc">
            {asrMode === "cloud"
              ? t("settings.asrMode.cloudDesc")
              : asrMode === "local_cloud"
                ? t("settings.asrMode.localCloudDesc")
                : t("settings.asrMode.localDesc")}
          </span>
        </div>
        <Segmented
          items={asrModeItems}
          value={asrMode}
          onSelect={(id) => {
            asrMode = id;
            changeAsrMode(id);
          }}
        />
      </div>
      {#if asrMode !== "local"}
        <div class="row">
          <div class="row-info"><span class="row-label">{t("settings.cloud.provider")}</span></div>
          <Segmented
            items={cloudProviderItems}
            value={cloudProvider}
            onSelect={(id) => {
              cloudProvider = id;
              invalidateCloudTest();
              saveSetting((s) => (s.cloud_asr_provider = cloudProvider));
            }}
          />
        </div>
        {#if cloudProvider === "volcano"}
          <div class="row">
            <div class="row-info">
              <span class="row-label">APP ID</span>
              <span class="row-desc">{t("settings.cloud.volcAppIdDesc")}</span>
            </div>
            <EditableField
              value={volcAppKey}
              placeholder="APP ID"
              disabled={recording.isLive}
              onEditStart={invalidateCloudTest}
              onSave={(v) => { volcAppKey = v; return saveSetting((s) => (s.volc_app_key = v)); }}
            />
          </div>
          <div class="row">
            <div class="row-info">
              <span class="row-label">Access Token</span>
              <span class="row-desc">{t("settings.cloud.secretDesc")}</span>
            </div>
            <EditableField
              masked
              value={volcAccessKey}
              placeholder="Access Token"
              disabled={recording.isLive}
              onEditStart={invalidateCloudTest}
              onSave={(v) => { volcAccessKey = v; return saveSetting((s) => (s.volc_access_key = v)); }}
            />
          </div>
        {:else}
          <div class="row">
            <div class="row-info">
              <span class="row-label">API Key</span>
              <span class="row-desc">{t("settings.cloud.secretDesc")}</span>
            </div>
            <EditableField
              masked
              value={dashKey}
              placeholder="DashScope API Key"
              disabled={recording.isLive}
              onEditStart={invalidateCloudTest}
              onSave={(v) => { dashKey = v; return saveSetting((s) => (s.dashscope_api_key = v)); }}
            />
          </div>
        {/if}
        <div class="row">
          <div class="row-info">
            <span class="row-label">{t("settings.cloud.test")}</span>
            <span class="row-desc">
              {#if cloudTestResult}
                <span class={cloudTestResult.ok ? "mtest-ok" : "mtest-err"}>
                  {cloudTestResult.ok
                    ? t("settings.test.ok", { msg: cloudTestResult.msg })
                    : t("settings.test.failed", { msg: cloudTestResult.msg })}
                </span>
              {:else}
                {t("settings.cloud.testDesc")}
              {/if}
            </span>
          </div>
          <button
            class="btn-secondary"
            onclick={doTestCloud}
            disabled={testingCloud || recording.isLive}
          >
            {testingCloud ? t("settings.testing") : t("settings.cloud.test")}
          </button>
        </div>
      {/if}
      <!-- 本地引擎选型:local 与 local_cloud 都要(后者实时仍走本地引擎)。 -->
      {#if asrMode !== "cloud"}
        <div class="row">
          <div class="row-info">
            <span class="row-label">{t("settings.asr.label")}</span>
            <span class="row-desc">
              {asrChoice === "whisper"
                ? t("settings.asr.whisperDesc")
                : asrChoice === "paraformer"
                  ? t("settings.asr.paraformerDesc")
                  : asrChoice === "qwen3"
                    ? t("settings.asr.qwen3Desc")
                    : asrChoice === "firered"
                      ? t("settings.asr.fireredDesc")
                      : t("settings.asr.senseVoiceDesc")}
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
            <label class="seg-item">
              <input
                type="radio"
                name="asr"
                value="qwen3"
                bind:group={asrChoice}
                disabled={recording.isLive || !settings}
                onchange={() => changeAsr("qwen3")}
              />Qwen3
            </label>
            <label class="seg-item">
              <input
                type="radio"
                name="asr"
                value="firered"
                bind:group={asrChoice}
                disabled={recording.isLive || !settings}
                onchange={() => changeAsr("firered")}
              />FireRed
            </label>
          </div>
        </div>
        {#if asrChoice === "qwen3"}
          <div class="row">
            <div class="row-info">
              <span class="row-label">{t("settings.asr.hotwordsLabel")}</span>
              <span class="row-desc">{t("settings.asr.hotwordsDesc")}</span>
            </div>
            <EditableField
              wide
              value={hotwords}
              placeholder={t("settings.asr.hotwordsPlaceholder")}
              disabled={recording.isLive}
              onSave={(v) => { hotwords = v; return saveSetting((s) => (s.asr_hotwords = v)); }}
            />
          </div>
        {/if}
      {/if}
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.speaker.label")}</span>
          <span class="row-desc">
            {speakerChoice === "eres2netv2"
              ? t("settings.speaker.eres2Desc")
              : t("settings.speaker.campplusDesc")}
          </span>
          <!-- 分段控件显示的是设置值;库到底在哪个空间必须单独说,否则不一致时
               界面等于在撒谎(2026-08-19:一台机器这样过了一个多月都没人看出来)。 -->
          {#if libModel}
            <span class="row-desc">{t("settings.speaker.libModel", { m: modelLabel(libModel) })}</span>
          {/if}
        </div>
        <Segmented
          items={speakerModelItems}
          value={speakerChoice}
          onSelect={(id) => {
            speakerChoice = id;
            changeSpeakerModel(id);
          }}
        />
      </div>
      {#if speakerModelMismatch && !pendingSpeakerModel}
        <!-- 库与选型不一致 = 声纹识别正停着。启动自愈会自动重建,这里给一个立刻重试的
             入口,并且把"现在是坏的"这件事明说出来。 -->
        <div class="purge-bar">
          <span class="confirm-text">
            {t("settings.speaker.libMismatch", {
              have: modelLabel(libModel),
              want: modelLabel(settings?.speaker_model ?? ""),
            })}
          </span>
          <div class="purge-actions">
            <button class="link" onclick={doRebuildVoiceprints} disabled={rebuilding || recording.isLive}>
              {t("settings.speaker.rebuildNow")}
            </button>
          </div>
        </div>
      {/if}
      {#if rebuildNote}
        <div class="row-desc">{rebuildNote}</div>
      {/if}
      {#if pendingSpeakerModel}
        <div class="purge-bar">
          <span class="confirm-text">
            <!-- 切回库本来所在的空间不需要重建,别拿"要重算声纹"吓人——两个方向
                 用同一句话正是原来那条提示读不出信息的原因。 -->
            {pendingSpeakerModel === libModel
              ? t("settings.speaker.confirmBack", { m: modelLabel(libModel) })
              : t("settings.speaker.confirmText", { n: speakerNoSampleCount })}
          </span>
          <div class="purge-actions">
            <button class="link danger" onclick={confirmSpeakerModelChange}>{t("settings.speaker.confirmSwitch")}</button>
            <button class="link" onclick={cancelSpeakerModelChange}>{t("settings.cancel")}</button>
          </div>
        </div>
      {/if}
      <!-- 说话人识别方法:立即生效于下一场录制/重转写/精修(进行中会话用开录时快照)。 -->
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.speakerMatch.label")}</span>
          <span class="row-desc">
            {speakerMatchChoice === "knn_vote"
              ? t("settings.speakerMatch.knnVoteDesc")
              : t("settings.speakerMatch.nearestDesc")}
          </span>
        </div>
        <Segmented
          items={speakerMatchItems}
          value={speakerMatchChoice}
          onSelect={(id) => {
            speakerMatchChoice = id;
            saveSetting((s) => (s.speaker_match = id));
          }}
        />
      </div>
      <!-- 非 <label>:徽标里挂了跳转按钮,若整行仍是 <label> 点按钮会被浏览器
           顺带判成"点了 label"而误触开关(nested interactive 元素的经典坑)。 -->
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.refine.label")}</span>
          <span class="row-desc">{t("settings.refine.desc")}</span>
          {#if refineOn && !refineConfigReady}
            <span class="row-badge">
              {t("settings.refine.notReady")}
              <button type="button" class="link" onclick={() => goto("/ai")}>
                {t("settings.refine.goConfig")}
              </button>
            </span>
          {/if}
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          bind:checked={refineOn}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.refine_enabled = refineOn))}
        />
      </div>
    </div>
    {#if asrMode === "local" && asrModelMissing}
      <div class="banner warn">{t("settings.asr.modelMissing")}</div>
    {/if}
    <p class="lock-hint">
      {recording.isLive ? t("settings.lockHint.recording") : t("settings.lockHint.idle")}
    </p>
  </section>

  <!-- —— 语音模型 —— -->
  <section>
    <h2 class="section-title">{t("settings.section.models")}</h2>
    {#if status}
      <button class="models-path" title={t("settings.models.openDir")} onclick={doOpenModelsDir}>
        <span class="row-desc">{t("settings.models.location")}</span>
        <span class="row-path models-path-value">{status.root}</span>
      </button>
    {/if}
    <div class="rows">
      {#if status}
        {#each status.artifacts as a (a.id)}
          <div class="row">
            <div class="row-info">
              <button
                class="url-toggle"
                aria-expanded={expandedId === a.id}
                aria-label={expandedId === a.id ? t("settings.models.collapseUrl") : t("settings.models.expandUrl")}
                onclick={() => (expandedId = expandedId === a.id ? null : a.id)}
              >
                <span class="caret" class:open={expandedId === a.id}>▸</span>
                <span class="row-label">{a.label} · {t("settings.models.approxMb", { mb: a.approx_mb })}</span>
              </button>
              {#if a.present}
                <span class="present">{t("settings.models.present")}</span>
              {/if}
            </div>

            {#if prog[a.id]}
              <div class="dl">
                <span class="phase">
                  {phaseText(prog[a.id].phase)}
                  {#if prog[a.id].phase === "downloading" && prog[a.id].total > 0}
                    {mb(prog[a.id].received)}/{mb(prog[a.id].total)}MB
                  {/if}
                </span>
                <div class="bar"><div class="fill" style="width:{pct(prog[a.id])}%"></div></div>
                <!-- 镜像僵死等场景的逃生口:后端置取消标志,worker 发 cancelled 事件清本行。
                     已下字节保留在 .part,再点下载走 Range 续传不白费。 -->
                {#if prog[a.id].phase === "downloading"}
                  <button class="link" onclick={() => cancelModelsDownload()}>{t("settings.cancel")}</button>
                {/if}
              </div>
            {:else if a.present}
              {#if confirmDeleteId === a.id}
                <div class="confirm-inline">
                  <button class="link danger" onclick={() => doDelete(a.id)}>{t("settings.models.confirmDelete")}</button>
                  <button class="link" onclick={() => (confirmDeleteId = null)}>{t("settings.cancel")}</button>
                </div>
              {:else}
                <button
                  class="link danger row-action"
                  disabled={recording.isLive || downloadingActive}
                  title={recording.isLive
                    ? t("settings.models.deleteBlockedRecording")
                    : downloadingActive
                      ? t("settings.models.deleteBlockedDownloading")
                      : t("settings.models.deleteTitle")}
                  onclick={() => {
                    confirmDeleteId = a.id;
                  }}>{t("settings.models.delete")}</button
                >
              {/if}
            {:else}
              <button class="btn-secondary" onclick={() => download(a.id)}>{t("settings.models.download")}</button>
            {/if}
          </div>
          {#if expandedId === a.id}
            <div class="url-detail">
              <div class="url-line">
                <span class="url-tag">{t("settings.models.originUrl")}</span>
                <code class="url-text">{a.url}</code>
                <button class="link" onclick={() => navigator.clipboard.writeText(a.url)}>{t("settings.models.copy")}</button>
              </div>
              <div class="url-line">
                <span class="url-tag">{t("settings.models.mirrorUrl")}</span>
                {#if settings?.mirror_enabled}
                  <code class="url-text">{effectiveUrl(a.url)}</code>
                  <button class="link" onclick={() => navigator.clipboard.writeText(effectiveUrl(a.url))}>{t("settings.models.copy")}</button>
                {:else}
                  <span class="url-muted">{t("settings.models.mirrorOff")}</span>
                {/if}
              </div>
            </div>
          {/if}
        {/each}
      {/if}
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.mirror.label")}</span>
          <span class="row-desc">
            {#if mirrorTest}
              <span class={mirrorTest.ok ? "mtest-ok" : "mtest-err"}>
                {mirrorTest.ok
                  ? t("settings.test.ok", { msg: mirrorTest.msg })
                  : t("settings.test.failed", { msg: mirrorTest.msg })}
              </span>
            {:else}
              {t("settings.mirror.desc")}
            {/if}
          </span>
        </div>
        {#if settings?.mirror_enabled}
          <button class="btn-secondary" onclick={runMirrorTest} disabled={mirrorTesting}>
            {mirrorTesting ? t("settings.testing") : t("settings.mirror.test")}
          </button>
        {/if}
        <input
          type="checkbox"
          class="ctl switch"
          aria-label={t("settings.mirror.aria")}
          checked={settings?.mirror_enabled ?? false}
          disabled={!settings}
          onchange={toggleMirror}
        />
      </div>
    </div>
  </section>

  <!-- —— 高级(默认收起,低频/进阶选项)—— -->
  <section>
    <h2 class="section-title">
      <button
        type="button"
        class="adv-toggle"
        aria-expanded={advOpen}
        aria-controls="adv-body"
        onclick={() => (advOpen = !advOpen)}
      >
        <span class="caret adv-caret" class:open={advOpen}>▸</span>
        {t("settings.section.advanced")}
      </button>
    </h2>
    <p class="desc adv-desc">{t("settings.section.advancedDesc")}</p>
    <div id="adv-body" class="adv-body" class:open={advOpen} inert={!advOpen}>
      <div class="adv-body-inner">
        <div class="rows">
          <label class="row">
            <div class="row-info">
              <span class="row-label">{t("settings.record.langFilter.label")}</span>
              <span class="row-desc">{t("settings.record.langFilter.desc")}</span>
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
              <span class="row-label">{t("settings.identifyAuto.label")}</span>
              <span class="row-desc">{t("settings.identifyAuto.desc")}</span>
            </div>
            <input
              type="checkbox"
              class="ctl switch"
              bind:checked={identifyAuto}
              disabled={!settings || !refineOn}
              onchange={() => saveSetting((s) => (s.identify_auto_apply = identifyAuto))}
            />
          </label>
        </div>
      </div>
    </div>
  </section>

  <!-- —— 关于 / 更新 —— -->
  <section>
    <h2 class="section-title">{t("settings.section.about")}</h2>
    <div class="rows">
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("settings.about.version", { v: appVersion || updateInfo?.current || "…" })}</span>
          <span class="row-desc">
            {#if updateChecking}
              {t("settings.update.checking")}
            {:else if updateError}
              {updateError}
            {:else if updateInfo?.has_update}
              {t("settings.update.found", { v: updateInfo.latest })}
            {:else if updateInfo}
              {t("settings.update.latest")}
            {:else}
              {t("settings.update.hint")}
            {/if}
          </span>
        </div>
        {#if updateInfo?.has_update}
          <button
            class="btn-secondary"
            onclick={doOneClickUpdate}
            disabled={updating || updateBlocked}
            title={updateBlocked ? t("settings.update.blockedRecording") : undefined}
          >
            {updating ? updatingLabel : t("settings.update.oneClick", { v: updateInfo.latest })}
          </button>
          {#if updateFallback}
            <!-- 一键失败(签名/网络/latest.json 未就绪)兜底手动路径,绝不堵死更新。 -->
            <button class="link" onclick={() => updateInfo && openUrl(updateInfo.url)}>{t("settings.update.openRelease")}</button>
          {/if}
        {:else}
          <button class="btn-secondary" disabled={updateChecking} onclick={doCheckUpdate}>
            {updateChecking ? t("settings.update.checkingBtn") : t("settings.update.check")}
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
        title={migrateBlockReason || t("settings.store.changeTitle")}
        onclick={() => chooseDir(kind)}>{t("settings.store.change")}</button
      >
    {/if}
  </div>
  {#if pendingMigrate && pendingMigrate.kind === kind}
    <div class="confirm-bar">
      {#if migrating}
        <span class="migrating">{t("settings.migrate.inProgress")}</span>
      {:else}
        <span class="confirm-text"
          >{t("settings.migrate.confirmPrefix")}<span class="confirm-path">{pendingMigrate.path}</span
          >{t("settings.migrate.confirmSuffix")}</span
        >
        <div class="confirm-actions">
          <button class="btn-primary" onclick={startMigrate}>{t("settings.migrate.start")}</button>
          <button class="btn-secondary" onclick={() => (pendingMigrate = null)}>{t("settings.cancel")}</button>
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
  /* 「高级」折叠区:disclosure 按钮复用 section-title 字号/颜色,chevron 与「语音模型」
     区的 url-toggle 同款旋转;展开态用 grid-template-rows 0fr→1fr 做无需测高的
     max-height 式过渡(内容行数变化也不用重算),配合 opacity 一起淡入。 */
  .adv-toggle {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    cursor: pointer;
    font: inherit;
    color: inherit;
    text-align: left;
  }
  .adv-desc {
    margin: 0 0 0.45rem;
  }
  .adv-body {
    display: grid;
    grid-template-rows: 0fr;
    opacity: 0;
    transition:
      grid-template-rows 120ms ease,
      opacity 120ms ease;
  }
  .adv-body.open {
    grid-template-rows: 1fr;
    opacity: 1;
  }
  .adv-body-inner {
    overflow: hidden;
    min-height: 0;
  }
  @media (prefers-reduced-motion: reduce) {
    .adv-body,
    .caret {
      transition: none;
    }
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
  .row-path {
    font-size: 0.8rem;
    color: var(--ink-secondary);
    word-break: break-all;
  }
  /* 会后 AI 就绪徽标:开关开但配置未齐备时旁挂,warning-ink 小字 + 去配置链接 */
  .row-badge {
    display: flex;
    align-items: center;
    gap: 0.25rem;
    font-size: 0.78rem;
    color: var(--warning-ink);
  }
  .row-badge .link {
    font-size: inherit;
    padding: 0 0.2em;
  }
  /* 「语音模型」区的存储位置行:整行可点,在文件管理器中打开目录。 */
  .models-path {
    display: flex;
    align-items: baseline;
    gap: 8px;
    background: none;
    border: none;
    padding: 0;
    margin: -0.2rem 0 0.6rem;
    cursor: pointer;
    font: inherit;
    text-align: left;
  }
  .models-path:hover .models-path-value {
    text-decoration: underline;
    color: var(--ink);
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
  /* 行内输入(与 AI 页 .row-input 同款):surface-press 底、无边,聚焦浮出 canvas + accent 环。
     云端 ASR 凭证输入(APP ID / Access Token / API Key)复用此形态。 */
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
