<script lang="ts">
  import { envelopeStep, shapeLevel, normalizeBars, barStyle, barCountFor } from "$lib/liveWave";
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { invoke } from "@tauri-apps/api/core";
  import { recording, type Line } from "$lib/recording.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import {
    speakerLabel,
    speakerColor,
    speakerInk,
    speakerIdCompare,
    editSegment,
    setSegmentSpeaker,
    renameSpeaker,
  } from "$lib/notes";
  import SpeakerChips from "$lib/SpeakerChips.svelte";
  import {
    modelsStatus,
    getSettings,
    setSettings,
    openScreenCaptureSettings,
    type ModelsStatus,
  } from "$lib/models";
  import { onCloudAsrStatus, type CloudAsrStatusEvent } from "$lib/events";
  import ModelDownloadCard from "$lib/ModelDownloadCard.svelte";
  import { formatTs } from "$lib/notes";
  import { matchesSpeakerFilter, nearestIndexByMs, searchHits } from "$lib/liveView";

  let models = $state<ModelsStatus | null>(null);

  async function refreshModels() {
    try {
      models = await modelsStatus();
    } catch {
      /* 查询失败按就绪处理，不挡老用户 */
    }
  }

  // 屏幕录制权限预检:硬承诺双轨下未授权时开录会被后端整场拆除(不再是静默降级),
  // 这里在开录前就常驻提示,把"根本录不了"提前到点开录之前(2026-07-07 实锤:
  // 用户所有笔记都没有 system 轨,自己毫无察觉——那是静默降级年代的教训)。
  // 查询失败按已授权处理,不误伤非 macOS/老系统。
  let screenPerm = $state(true);
  // 拒录引导卡上的「修复授权」行按平台门控:tccutil 双清只在 macOS 有意义,
  // Windows 的 unavailable 卡不能出现一个点了也没用的修复按钮。WKWebView 的
  // UA 恒含 "Macintosh",取一次即定值。
  const isMacPlatform = navigator.userAgent.includes("Mac");
  async function refreshScreenPerm() {
    try {
      screenPerm = await invoke<boolean>("screen_capture_permission");
    } catch {
      screenPerm = true;
    }
  }
  async function requestScreenPerm() {
    try {
      // 系统授权弹窗一生只弹一次;已弹过(返回 false)就直接带去系统设置。
      const ok = await invoke<boolean>("request_screen_capture_permission");
      if (!ok) await openScreenCaptureSettings();
    } catch {
      await openScreenCaptureSettings();
    }
    triedScreenAuth = true;
    await refreshScreenPerm();
  }

  // 授权残留自愈:换签名后旧 TCC 条目压住新二进制——系统设置里开关已开却仍未授权,
  // 拨动开关/重启都无效。走过一轮「立即授权」仍失败才亮出修复入口(避免吓到首次
  // 授权的用户);修复=清掉本应用的屏幕录制授权记录,再重走一遍系统授权。
  // 增强(macOS 26.6.1):修复同时清掉 ScreenCapture 与 AudioCapture 两份旧签名记录。
  let triedScreenAuth = $state(false);
  const showPermFix = $derived(triedScreenAuth && !screenPerm);
  async function fixScreenPerm() {
    try {
      await invoke<boolean>("reset_screen_capture_permission");
    } catch {
      /* 清除失败:下面重走授权仍可能带用户到系统设置,不中断 */
    }
    await requestScreenPerm();
  }

  // 蓝牙外放预警:常态检测,只看设备现实——蓝牙输出正在使用时,蓝牙延迟(300~600ms+)
  // 超出软件回声消除的追踪范围,mic 会混入近乎全量的对方声音(面试录音实锤)。
  // 与 capture_path(aec/vpio)设置无关(那是采集路径的逃生舱,不是这条风险的成因),
  // 现默认 aec 下不再按设置项门控这条提示。开录前提示,查询失败按"无风险"静默。
  let btEchoRisk = $state(false);
  async function refreshBtRisk() {
    try {
      btEchoRisk = await invoke<boolean>("output_is_bluetooth");
    } catch {
      btEchoRisk = false;
    }
  }

  // 输入音量过低预警:系统输入音量被会议软件拉低会录得很轻,与采集路径设置无关,
  // 纯按当前电平判定。开录前 + 录制中都检测,一键调回可用电平。
  const LOW_INPUT_THRESHOLD = 50;
  const INPUT_TARGET = 75;
  const POLL_MS = 4000;
  let lowInputVol = $state<{ vol: number } | null>(null);
  async function refreshInputVol() {
    try {
      const vol = await invoke<number | null>("input_volume");
      lowInputVol = vol != null && vol < LOW_INPUT_THRESHOLD ? { vol } : null;
    } catch {
      lowInputVol = null;
    }
  }
  async function fixInputVol() {
    try {
      await invoke("set_input_volume", { v: INPUT_TARGET });
    } catch {
      /* 设置失败:回读后横幅仍在,用户可见未生效 */
    }
    await refreshInputVol();
  }

  // 存量用户 MCP 引导:onboarded(老用户)且 mcp_onboarded 为 false 时出一次提示条。
  // 新用户在欢迎页已走过(markOnboarded 同置两标记),不会看到。
  let showMcpHint = $state(false);
  async function dismissMcpHint(goSettings: boolean) {
    showMcpHint = false;
    try {
      const s = await getSettings();
      await setSettings({ ...s, mcp_onboarded: true });
    } catch {
      /* 置失败下次再提示,可接受 */
    }
    if (goSettings) goto("/ai");
  }

  // 云端识别连接状态(仅云端模式录制时产生):细提示条,不阻断录制。reconnecting/backfilling/
  // backfill_failed 持续显示到下一个事件覆盖;recovered 闪一下「已恢复」后 3s 自动清空,
  // 避免恢复提示常驻占位。
  // 存键+参数而非成品文案:模板里再过 t(),语言切换时状态条即时跟随重渲。
  let cloudStatus = $state<{
    kind: CloudAsrStatusEvent["state"];
    key: string;
    params?: Record<string, unknown>;
  } | null>(null);
  let cloudStatusClearTimer: ReturnType<typeof setTimeout> | null = null;
  /** 厂商错误原文可能很长(带 requestId 的整串 JSON),状态条只留一行的量。 */
  const CLOUD_REASON_MAX = 80;
  function cloudReason(message?: string) {
    const m = message?.trim();
    if (!m) return "";
    // Array.from 按码点(而非 UTF-16 单元)切:message.slice 会把代理对(如 emoji/
    // 部分 CJK 扩展区字符)从中间切开，渲染成乱码半字符。
    const chars = Array.from(m);
    return `(${chars.length > CLOUD_REASON_MAX ? chars.slice(0, CLOUD_REASON_MAX - 1).join("") + "…" : m})`;
  }
  function handleCloudAsrStatus(e: CloudAsrStatusEvent) {
    // 「已恢复」不许盖掉「补识失败」:补识失败是这场录音真留了窟窿(占位段已落盘),
    // 而 recovered 只说明连接回来了。盖过去用户会以为窟窿被补上了。警告留到下一次
    // reconnecting(新一轮故障)、录制结束(提示条随 isLive 消失)或手动清除为止。
    if (e.state === "recovered" && cloudStatus?.kind === "backfill_failed") return;
    if (cloudStatusClearTimer) {
      clearTimeout(cloudStatusClearTimer);
      cloudStatusClearTimer = null;
    }
    if (e.state === "reconnecting") {
      cloudStatus = { kind: e.state, key: "record.cloud.reconnecting", params: { reason: cloudReason(e.message) } };
    } else if (e.state === "backfilling") {
      cloudStatus = { kind: e.state, key: "record.cloud.backfilling" };
    } else if (e.state === "backfill_failed") {
      cloudStatus = { kind: e.state, key: "record.cloud.backfillFailed" };
    } else {
      // recovered
      cloudStatus = { kind: e.state, key: "record.cloud.recovered" };
      cloudStatusClearTimer = setTimeout(() => {
        cloudStatus = null;
        cloudStatusClearTimer = null;
      }, 3000);
    }
  }

  // 状态条按场次隔离:cloudStatus 是模块顶层 state,录制页不会因换场重新挂载,
  // F4 的粘滞设计(backfill_failed 持续显示到下一事件覆盖)只在"当场"内成立——
  // isLive false→true(开始下一场录制,哪怕是本地模式)必须先清掉上一场遗留的
  // 横幅,否则用户会以为这场也补识失败了,而这场压根还没产生过一个云端事件。
  let wasLive = false;
  $effect(() => {
    const live = recording.isLive;
    if (live && !wasLive) {
      cloudStatus = null;
      if (cloudStatusClearTimer) {
        clearTimeout(cloudStatusClearTimer);
        cloudStatusClearTimer = null;
      }
    }
    wasLive = live;
  });

  // ── 当场纠正:行内编辑文本 / 点行首徽章改派说话人 / 命名改名 ──────────────────
  // segment_edited 是唯一真值源(见 recording.svelte.ts 订阅),这里不做乐观更新——
  // 提交后只清本地的"正在编辑"标记,展示的文字/说话人等事件把 finals 改回来。
  let editingSeq = $state<number | null>(null);
  let editingText = $state("");
  let speakerMenuSeq = $state<number | null>(null);
  let renamingSeq = $state<number | null>(null);
  let renameText = $state("");
  let editError = $state("");

  // 录制结束(停止/出错)后清空所有悬浮编辑态,不带着上一场的态出现在下一场。
  // 回看态(搜索词/说话人过滤/命中下标)一并清空:同一页面实例跨场复用时,说话人 id
  // 按场编号(S0/S1...)跨场高概率复用,上一场遗留的过滤 chip 会静默把新场的转写行
  // 过滤掉、follow 卡暂停。清空会让下面的 reviewActive 产生 true→false 边沿从而
  // 触发 jumpToLatest——停录时刻滚回底部是可接受行为,这里不额外同步 prevReviewActive
  // 去绕过这条边沿。
  $effect(() => {
    if (!recording.isLive) {
      editingSeq = null;
      speakerMenuSeq = null;
      renamingSeq = null;
      searchQuery = "";
      selectedSpeakers = new Set();
      activeHit = 0;
    }
  });

  /** keyed each 用 line.seq；「当前句」判定不再依赖 #each 的索引 i。 */
  const lastFinalSeq = $derived(
    recording.finals.length ? recording.finals[recording.finals.length - 1].seq : null,
  );
  /** 改派菜单只列本场已有说话人——不提供「新说话人」选项(后端会拒 "new",
      Task 6 审查结论:别让用户在这条路上撞错)。 */
  const speakerIds = $derived(Object.keys(recording.speakers).sort(speakerIdCompare));

  function beginEdit(line: Line) {
    speakerMenuSeq = null;
    editingSeq = line.seq;
    editingText = line.text;
  }

  async function commitEdit(line: Line) {
    const newText = editingText.trim();
    if (!newText || newText === line.text) {
      editingSeq = null;
      return;
    }
    try {
      await editSegment(recording.noteId!, line.seq, line.text, newText);
      editingSeq = null;
    } catch (e) {
      // 后端可能报"录制会话已结束,请重试"或"该笔记正被占用…"这类停录竞态错误——
      // 如实展示原文,不自动重试(由用户自行判断是否重试)。
      editError = t("record.edit.failed", { e });
    }
  }

  function toggleSpeakerMenu(seq: number) {
    editingSeq = null;
    renamingSeq = null;
    speakerMenuSeq = speakerMenuSeq === seq ? null : seq;
  }

  async function pickSpeaker(line: Line, speakerId: string) {
    speakerMenuSeq = null;
    try {
      await setSegmentSpeaker(recording.noteId!, line.seq, line.text, speakerId);
    } catch (e) {
      editError = t("record.edit.failed", { e });
    }
  }

  function beginRename(line: Line) {
    if (!line.speaker) return; // 未标注说话人(source 兜底"我/对方")没有 id 可改名
    renamingSeq = line.seq;
    renameText = recording.speakers[line.speaker]?.name ?? "";
  }

  /** renamingSeq 卫语句：Escape 已在按键处同步清空 renamingSeq，随后触发的 blur
      再调本函数会被这层卫语句挡住，不会把取消误提交（同 SpeakerChips 的既有写法）。 */
  async function commitRename(line: Line) {
    if (renamingSeq !== line.seq || !line.speaker) return;
    renamingSeq = null;
    const name = renameText.trim();
    if (!name) return;
    try {
      await renameSpeaker(recording.noteId!, line.speaker, name);
      speakerMenuSeq = null;
    } catch (e) {
      editError = t("record.edit.failed", { e });
    }
  }

  onMount(() => {
    refreshModels();
    refreshScreenPerm();
    refreshBtRisk();
    refreshInputVol();
    getSettings().then((s) => {
      showMcpHint = s.onboarded && !s.mcp_onboarded;
    }).catch(() => {});
    // 用户去系统设置勾选/换音频设备后切回来,焦点事件驱动横幅刷新,无需重启页面。
    const onFocus = () => {
      refreshScreenPerm();
      refreshBtRisk();
      refreshInputVol();
    };
    window.addEventListener("focus", onFocus);
    // 录制中也检测(会议软件中途拉低输入音量):轮询与录制状态无关,一直跑。
    const volTimer = setInterval(refreshInputVol, POLL_MS);
    const unCloud = onCloudAsrStatus(handleCloudAsrStatus);
    return () => {
      window.removeEventListener("focus", onFocus);
      clearInterval(volTimer);
      if (cloudStatusClearTimer) clearTimeout(cloudStatusClearTimer);
      unCloud.then((f) => f());
    };
  });

  function isError(s: string) {
    return s.startsWith("error:");
  }
  /** 状态机原值(idle/recording/paused/stopped/error:…)映射成右侧簇里的友好短标签;
      错误详情(可能很长)不塞这里,另在下方红色行完整展开。 */
  /** 计时后缀:只在"非正常录制中"时出条,正常录制由呼吸红点交代,不再挂常驻标签。
      不含出错:出错时 isLive/stopping 都是 false,整簇根本不挂载,错误由下方专门的
      红色详情行(.status.error)承担——在这里写一条出错分支是永远走不到的死代码(Codex P2)。 */
  const clockNote = $derived(
    recording.stopping ? t("record.btn.stopping") : recording.paused ? t("record.status.paused") : "",
  );

  // 硬承诺双轨(拒录引导卡):Fix A 拆除路径把分类 token 塞进开录失败的错误串——
  // system_denied=屏幕录制权限缺失(可操作:引导去系统设置);system_unavailable=
  // 设备/组件问题或 Windows(该平台无此权限模型,恒 unavailable,见 lib.rs
  // open_screen_capture_settings 注释)。denied 判定放前面:denied 场景下字符串里
  // 不会同时出现 unavailable,顺序无所谓,但先判更符合"先看能不能引导授权"的直觉。
  const isSystemDenied = $derived(
    isError(recording.status) && recording.status.includes("system_denied"),
  );
  const isSystemUnavailable = $derived(
    isError(recording.status) &&
      !isSystemDenied &&
      recording.status.includes("system_unavailable"),
  );
  /** 展示用状态串:剥掉后端塞进错误串给前端分支用的分类 token(用户不需要看到
      " [system_denied]" 这种内部标记)。仅在下方红色详情行渲染时使用；
      isSystemDenied/isSystemUnavailable 场景下该行整体不渲染,引导卡是唯一
      信息面(见下方 markup)。 */
  const displayStatus = $derived(
    recording.status.replace(/ \[system_(denied|unavailable)\]$/, ""),
  );
  async function openSystemAudioPrivacySettings() {
    try {
      await openScreenCaptureSettings();
    } catch {
      // Windows/非 macOS 命令恒 Err;引导卡的「打开系统设置」按钮只在 denied 分支
      // 渲染(该分支目前只有 macOS 权限缺失会触发),此处兜底不弹二次错误打扰用户。
    }
  }

  async function startRecording() {
    await recording.start(); // 已在录制页，无需跳转
  }
  /** rms → 0..100% 显示映射,mic/system 两路共用(数值映射与原单通道版本一致)。 */
  const pctOf = (rms: number) => {
    if (!recording.isLive || rms <= 0) return 0;
    const db = 20 * Math.log10(rms);
    return Math.max(0, Math.min(100, ((db + 50) / 50) * 100)); // -50dBFS..0dBFS → 0..100%
  };
  const micPct = $derived.by(() => pctOf(recording.levels.mic));
  const sysPct = $derived.by(() => pctOf(recording.levels.system));

  // ── 实时音轨(录音机式):录制中每 120ms 采样一次电平,新条从右缘进入、旧条左移;
  //    暂停冻结不清空,停止清空。interval 回调里读 micPct/sysPct 是瞬时值,不进 effect 依赖。
  //    波形只画 mic(冒烟反馈:双轨两行太吵);系统声降级为「对方」指示灯——
  //    sysHold 是带保持的活跃计数(检出电平充 8 格≈1s,无声逐格衰减),灯不闪烁。
  //
  //    存的是**包络**不是瞬时值(见 liveWave.ts):快起慢落填平音节间低谷,否则 120ms
  //    采样撞上 ~4Hz 音节调制,画出来是断续栅栏。整形(噪声门/gamma/峰值归一)在渲染侧做。 ──
  const LIVE_BARS = 420; // 上限约 50s 历史;实际画多少根按容器宽度定(节距 4px)
  const WAVE_PEAK_PX = 24; // 峰值条高;容器 34px,留头免得顶到边
  let liveBarsMic = $state<number[]>([]);
  let sysHold = $state(0);
  let micHold = $state(0);
  let waveW = $state(0);
  $effect(() => {
    if (!recording.isLive) {
      liveBarsMic = [];
      sysHold = 0;
      micHold = 0;
      return;
    }
    if (recording.paused) {
      // 冻结:不采样,已有波形保留(那是历史,该留着)。但两个活跃指示灯必须灭——
      // 它们表达的是"此刻正在收音",暂停前最后一格保持会让「我」在整个暂停期间红着,
      // 谎报仍在收音(Codex P2)。
      micHold = 0;
      sysHold = 0;
      return;
    }
    // 包络的运行值随本轮采样重新起算(Codex P2):它不能跨暂停沿用。暂停前最后一帧若很响,
    // 恢复后会凭空续出半秒多的红色衰减条——而那段时间根本没有声音进来。
    // 注意与 liveBarsMic 的区别:已画出的历史要保留(冻结显示),重置的只是这个运行值。
    let envPrev = 0;
    const t = setInterval(() => {
      envPrev = envelopeStep(envPrev, micPct);
      liveBarsMic = [...liveBarsMic.slice(-(LIVE_BARS - 1)), envPrev];
      // 两路指示灯用同款"带保持"的活跃计数(检出电平充 8 格≈1s,无声逐格衰减),
      // 灯不跟着每帧电平闪烁。mic 侧的门限与波形同一条(shapeLevel 的噪声门)。
      sysHold = sysPct > 0 ? 8 : Math.max(0, sysHold - 1);
      micHold = shapeLevel(micPct) > 0 ? 8 : Math.max(0, micHold - 1);
    }, 120);
    return () => clearInterval(t);
  });
  /** 渲染用:按容器宽度决定根数(节距 4px),取最近这么多帧;不足则前导补零——
      让波形从开录起就铺满整行(补的零段是安静基线),新声仍从右侧进入。 */
  const liveBars = $derived.by(() => {
    const n = barCountFor(waveW, LIVE_BARS);
    const tail = liveBarsMic.slice(-n);
    const pad = n - tail.length;
    const pcts = pad > 0 ? [...new Array(pad).fill(0), ...tail] : tail;
    // 峰值归一在**可见窗口内**做:最响的一根顶到满格,整段都轻时不放大底噪(有下限)。
    return normalizeBars(pcts.map((p) => shapeLevel(p))).map((v) => barStyle(v, WAVE_PEAK_PX));
  });

  // ── 歌词式跟随：新内容到达自动滚到最新；用户上滑即暂停跟随，滚回底部自动恢复 ──
  // 录制中转写容器带 50vh 底部留白(见 .transcript.live)，「滚到底」因此恰好把
  // 当前句钉在屏幕垂直中央——居中定位与跟随/回到最新共用同一套滚动逻辑。
  let transcriptEl = $state<HTMLElement | null>(null);
  let follow = $state(true);
  /** 有在途预览时预览是"当前句"；否则最新定稿是当前句(放大+高亮,历史行变暗)。 */
  const hasPartial = $derived(!!(recording.partialMic || recording.partialSystem));
  /** 距底部多少像素内视为"在底部"（恢复跟随的判定带）。 */
  const BOTTOM_SLOP = 48;

  /** 最近的可滚动祖先（布局里的 .main）；不硬编码布局选择器。 */
  function scrollParent(): HTMLElement | null {
    for (let p = transcriptEl?.parentElement; p; p = p.parentElement) {
      if (/(auto|scroll)/.test(getComputedStyle(p).overflowY)) return p;
    }
    return null;
  }

  function jumpToLatest() {
    follow = true;
    const sc = scrollParent();
    sc?.scrollTo({ top: sc.scrollHeight, behavior: "smooth" });
  }

  // 新定稿/预览更新 → 跟随滚动。依赖显式读取，转写为空时也无副作用。
  $effect(() => {
    void recording.finals.length;
    void recording.partialMic;
    void recording.partialSystem;
    if (!follow || !recording.isLive) return;
    const sc = scrollParent();
    sc?.scrollTo({ top: sc.scrollHeight, behavior: "smooth" });
  });

  // 用户意图判定：wheel 上滑 = 主动离开（平滑滚动只产生 scroll 事件，不会误判）；
  // scroll 回到底部判定带内 = 恢复跟随。监听挂可滚动祖先，卸载时清理。
  $effect(() => {
    if (!transcriptEl) return;
    const sc = scrollParent();
    if (!sc) return;
    const onWheel = (e: WheelEvent) => {
      // 内容不足一屏时无处可滚,上滑不算"离开最新",不亮返回按钮。
      if (e.deltaY < 0 && recording.isLive && sc.scrollHeight > sc.clientHeight + 4) follow = false;
    };
    const onScroll = () => {
      // 回看态(reviewActive)下 follow 由下方边沿 effect 专管：过滤/搜索命中收紧会隐藏行，
      // scrollHeight 收缩触发浏览器钳制 scrollTop，落入贴底判定带——这是布局副作用，不是
      // 用户主动划回底部的意图，scroll 事件在此不得抢跑把 follow 重新打开。
      if (!reviewActive && sc.scrollHeight - sc.scrollTop - sc.clientHeight <= BOTTOM_SLOP) follow = true;
    };
    sc.addEventListener("wheel", onWheel, { passive: true });
    sc.addEventListener("scroll", onScroll, { passive: true });
    return () => {
      sc.removeEventListener("wheel", onWheel);
      sc.removeEventListener("scroll", onScroll);
    };
  });

  // ── 回看工具条:页内搜索(高亮+跳转,不隐藏行) + 说话人过滤(隐藏行) ──────────
  // 口径:状态条件——reviewActive 由 false 变 true 即暂停跟随；由 true 变 false
  // （无论哪条路径：Esc、「清除」按钮、退格删空搜索框、取消最后一个说话人 chip）
  // 都恢复跟随并跳到最新。用 prevReviewActive 记边沿，只在转换瞬间触发一次，
  // 避免每次输入/每次 chip 切换都强制滚动。
  let searchQuery = $state("");
  let activeHit = $state(0); // hits 内下标
  let selectedSpeakers = $state<Set<string>>(new Set());
  // 选中集随说话人表收敛(Codex P2):live 合并会把 loser 从表里移除并把其段落改写给
  // winner——若选中集还留着 loser,过滤会隐藏全部改写行且对应 chip 已消失,整页空白
  // 只能靠「清除」自救。fail-open 剪掉失效 id(不映射到 winner:合并是算法行为,
  // 悄悄替换用户的过滤对象反而更意外)。
  $effect(() => {
    const ids = new Set(speakerIds);
    if ([...selectedSpeakers].some((sid) => !ids.has(sid))) {
      selectedSpeakers = new Set([...selectedSpeakers].filter((sid) => ids.has(sid)));
    }
  });
  // 命中=可见命中:搜索导航永不落在被过滤行上——先拿文本命中，再叠一层说话人过滤，
  // 否则「下一个」可能跳到 display:none 的行（scrollIntoView 对隐藏元素静默 no-op），
  // 计数也会把不可见行算进去。
  const hits = $derived(
    searchHits(recording.finals, searchQuery).filter((i) =>
      matchesSpeakerFilter(recording.finals[i], selectedSpeakers),
    ),
  );
  /** each 内 O(n) 判定命中(而非 hits.includes(i) 的 O(n²))：finals 行数虽然通常
      只有数百，但录制可长达数小时，直接派生 Set 零成本。 */
  const hitSet = $derived(new Set(hits));
  /** hits 收缩时(说话人过滤收紧、命中行被回声撤回…)activeHit 可能越界——渲染计数/
      高亮、gotoHit 的跳转基点统一读钳制值，避免出现「6/5」这种越界展示。 */
  const activeHitClamped = $derived(Math.min(activeHit, Math.max(0, hits.length - 1)));
  const reviewActive = $derived(searchQuery.trim() !== "" || selectedSpeakers.size > 0);

  let prevReviewActive = false;
  $effect(() => {
    if (reviewActive && !prevReviewActive) follow = false;
    if (!reviewActive && prevReviewActive) jumpToLatest();
    prevReviewActive = reviewActive;
  });
  // 换一次查询词，命中列表整个变了，上一次的"第几个命中"下标不再有意义——
  // 重新从第一个命中数起，而非停留在旧下标显示出"5/3"这种错位计数。
  $effect(() => {
    void searchQuery;
    activeHit = 0;
  });
  function clearReview() {
    searchQuery = "";
    selectedSpeakers = new Set();
    activeHit = 0;
    // 恢复跟随/跳到最新交给上面的 reviewActive 边沿监听——这里不再重复调用
    // jumpToLatest()，否则 Esc/「清除」路径会触发两次滚动。
  }
  function gotoHit(delta: number) {
    if (!hits.length) return;
    activeHit = (activeHitClamped + delta + hits.length) % hits.length;
    document
      .getElementById(`seg-${recording.finals[hits[activeHit]].seq}`)
      ?.scrollIntoView({ block: "center", behavior: "smooth" });
  }
  function toggleSpeaker(id: string) {
    const next = new Set(selectedSpeakers);
    next.has(id) ? next.delete(id) : next.add(id);
    selectedSpeakers = next;
  }

  // ── 右缘迷你时间轴:细轨映射 0..elapsedMs,点击定位最近行 ──────────────────
  /** 每 5 分钟一个刻度的纵向百分比。elapsedMs 是走表值(每秒变化),派生成本
      是一次数组构建，量级在"总时长/5分钟"，秒级重算可接受，无需额外节流。 */
  const ticksView = $derived.by(() => {
    const total = recording.elapsedMs;
    if (total < 60_000) return [] as number[];
    const out: number[] = [];
    for (let ms = 300_000; ms < total; ms += 300_000) out.push((ms / total) * 100);
    return out;
  });

  function handleTimelineClick(e: MouseEvent) {
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const ms = ((e.clientY - rect.top) / rect.height) * recording.elapsedMs;
    // 口径同 Task 9 的 hits：定位候选先按当前说话人过滤筛出可见子集，再取最近行，
    // 否则命中的行可能是 display:none（被过滤隐藏），scrollIntoView 会静默 no-op。
    const visible = recording.finals.filter((l) => matchesSpeakerFilter(l, selectedSpeakers));
    const idx = nearestIndexByMs(visible, ms);
    if (idx < 0) return;
    // 时间轴点击不算"回看激活态"（不进 reviewActive），只是暂停跟随——与手动上滑
    // 同类；follow 恢复靠既有的"回到最新"按钮，不额外造恢复逻辑。
    follow = false;
    document
      .getElementById(`seg-${visible[idx].seq}`)
      ?.scrollIntoView({ block: "center", behavior: "smooth" });
  }
</script>

<!-- 录制 transport 的三枚图标。DESIGN.md 第 7 条允许"16px 线性 SVG(stroke currentColor)":
     比 CSS 图形更可控(圆角端点、光学重心),也不受各平台字形影响。抽成片段三处共用。 -->
{#snippet icoPause()}
  <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
    <path d="M6 3.5v9M10 3.5v9" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" />
  </svg>
{/snippet}
{#snippet icoPlay()}
  <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
    <path
      d="M5.5 3.6v8.8a.6.6 0 0 0 .92.5l7-4.4a.6.6 0 0 0 0-1l-7-4.4a.6.6 0 0 0-.92.5z"
      fill="currentColor"
    />
  </svg>
{/snippet}
{#snippet icoSearch()}
  <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
    <circle cx="7.2" cy="7.2" r="4.3" fill="none" stroke="currentColor" stroke-width="1.6" />
    <path d="M10.4 10.4 13.5 13.5" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" />
  </svg>
{/snippet}
{#snippet icoStop()}
  <!-- 实心圆角方块而非描边:描边方块读起来像"录制"按钮,实心才是停止 -->
  <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
    <rect x="4.25" y="4.25" width="7.5" height="7.5" rx="2" fill="currentColor" />
  </svg>
{/snippet}

<div class="container">
  <!-- 头部整体吸顶(标题/下载卡/控制条/状态行/说话人条):录制中转写自动滚到最新,
       操作与说话人对照都不能跟着滚出视口 -->
  <div class="topbar">
    <h1>{t("record.title")}</h1>

    <!-- 单实例:compact 由 recording_ready 驱动。若拆成两个 if 分支,识别模型下完
         切小提示条时组件会销毁重建,进行中的下载进度/订阅状态全部清零。 -->
    {#if models && !(models.recording_ready && models.diarization_ready)}
      <ModelDownloadCard status={models} compact={models.recording_ready} onComplete={refreshModels} />
    {/if}

    {#if !models || models.recording_ready}
      <!-- 两端对齐:控制钮组贴左、计时+状态贴右、实时波形限宽居中(space-between 把
           富余横向空间分到两侧间隙,波形不再 flex:1 拉满整屏成一根横贯全宽的细带)。 -->
      <!-- 录制面板:transport 行 + 回看行同属一块(发丝线分隔)。此前搜索框是块孤零零浮在
           画布上的描边输入框,与上面的卡片各说各话——并进同一块面板后,页面上就只剩
           「标题 / 面板 / 转写」三段,不再有游离元素。
           空转写页(未录且无定稿)不上底:那时面板里只有一个开始录制的 CTA,加底像个空盒。 -->
      <div class="panel" class:card={recording.isLive || recording.finals.length > 0} class:paused={recording.paused}>
      <!-- 录制 transport 条:左图标钮组 / 计时英雄位 / 全宽波形 / 右源指示灯。
           上 surface 底 + 发丝线读成一块整体(此前裸浮在画布上,几个元素各自为政);
           发丝线分隔把三簇分开,靠间距对齐而非边框盒。 -->
      <div class="controls">
        <!-- 左:控制钮。空闲态是主 CTA 药丸(DESIGN.md 第 3 条),录制中转成圆形图标钮
             ——录制中的两个动作高频且语义强(暂停/停止),图标比文字按钮更省横向、更耐看。 -->
        <div class="ctl-group">
          {#if recording.stopping}
            <button class="iconbtn rec" disabled aria-label={t("record.btn.stopping")} title={t("record.btn.stopping")}>
              {@render icoStop()}
            </button>
          {:else if !recording.isLive}
            <button class="ctl primary" disabled={recording.pending} onclick={startRecording}>
              <span class="sym dot on-blue"></span>{t("record.btn.start")}
            </button>
          {:else}
            {#if recording.paused}
              <button
                class="iconbtn"
                disabled={recording.pending}
                onclick={() => recording.unpause()}
                aria-label={t("record.btn.resume")}
                title={t("record.btn.resume")}
              >{@render icoPlay()}</button>
            {:else}
              <button
                class="iconbtn"
                disabled={recording.pending}
                onclick={() => recording.pause()}
                aria-label={t("record.btn.pause")}
                title={t("record.btn.pause")}
              >{@render icoPause()}</button>
            {/if}
            <!-- 停止即停,不再二段确认(用户裁定):误点的代价有限——本场已转写的内容照常
                 落盘成笔记,想接着录可以对同一篇「续录」;而每次停止都要点两下的摩擦是常态成本。 -->
            <button
              class="iconbtn rec"
              disabled={recording.pending}
              onclick={() => recording.stop().catch((err) => console.error("停止录制失败", err))}
              aria-label={t("record.btn.stop")}
              title={t("record.btn.stop")}
            >{@render icoStop()}</button>
          {/if}
        </div>

        <!-- 停止中也要出这一簇(Codex P2):stop() 一发 isLive 就转 false,若只认 isLive,
             「停止中…」这句会连同整簇消失,几秒的收尾期只剩一个禁用图标,用户不知道在等什么。 -->
        {#if recording.isLive || recording.stopping}
          <span class="sep"></span>
          <!-- 计时英雄位:一场录音里最该一眼看到的就是"已经录了多久"。此前是 1rem 的
               ink-secondary 小字挤在最右缘,与状态标签抢注意力;现在等宽大字 + 呼吸红点,
               状态字(已暂停/停止中/出错)作为后缀跟在同一簇里,不再单开一个标签。 -->
          <span class="clock">
            <span class="recdot" class:breathe={!recording.paused && !recording.stopping}></span>
            <span class="t" class:warn={recording.paused}>{formatTs(recording.elapsedMs)}</span>
            {#if clockNote}<span class="lbl">{clockNote}</span>{/if}
          </span>
        {/if}

        {#if recording.isLive}
          <span class="sep"></span>
          <!-- 波形:只画 mic 一条(冒烟反馈:双轨两行太吵),取值与整形见 lib/liveWave.ts -->
          <div
            class="wave-live"
            class:frozen={recording.paused}
            title={t("record.micLevel")}
            aria-hidden="true"
            bind:clientWidth={waveW}
          >
            {#each liveBars as b, i (i)}<span
                class="bar"
                class:silent={b.silent}
                style="height: {b.height}px; opacity: {b.opacity}"
              ></span>{/each}
          </div>

          <!-- 两个源指示灯成对出现:成对才自解释("这是两路收音"),单挂一个「对方」
               谁也说不清指什么。有电平点亮(我=录制红 / 对方=mint),静默退灰。 -->
          <span class="srcs">
            <span class="s mic" class:on={micHold > 0} title={t("record.micLevel")}>
              <span class="d"></span>{t("record.badge.me")}
            </span>
            <span class="s" class:on={sysHold > 0} title={t("record.systemLevel")}>
              <span class="d"></span>{t("record.badge.them")}
            </span>
          </span>
        {/if}
      </div>

      <!-- 回看工具条:页内搜索(高亮+跳转) + 说话人过滤 chips。只在有转写内容可回看时
           出现(录制中或已有定稿行)，空转写页不占位。 -->
      {#if recording.isLive || recording.finals.length > 0}
        <div class="review-bar">
          <!-- 搜索改药丸 + 放大镜:此前是 hairline-strong 描边的方框输入,比同屏卡片的边还重,
               孤零零一个显得突兀。药丸与图标钮/胶囊同一套圆角语言,聚焦时才亮 accent 边。 -->
          <label class="search">
            <span class="ico">{@render icoSearch()}</span>
            <input
              placeholder={t("record.search.placeholder")}
              bind:value={searchQuery}
              onkeydown={(e) => {
                if (e.key === "Enter") gotoHit(e.shiftKey ? -1 : 1);
                if (e.key === "Escape") clearReview();
              }}
            />
          </label>
          {#if searchQuery.trim()}
            <span class="hit-count">
              {hits.length ? `${activeHitClamped + 1}/${hits.length}` : t("record.search.none")}
            </span>
            <button class="ghosty" onclick={() => gotoHit(-1)} title={t("record.search.prev")}>↑</button>
            <button class="ghosty" onclick={() => gotoHit(1)} title={t("record.search.next")}>↓</button>
          {/if}
          <!-- 过滤 chips 只在 ≥2 个说话人时出现:单说话人过滤无意义,还与下方
               SpeakerChips 管理条视觉重复(冒烟反馈)。 -->
          {#if speakerIds.length >= 2}
            {#each speakerIds as sid (sid)}
              <button
                class="chip"
                class:on={selectedSpeakers.has(sid)}
                onclick={() => toggleSpeaker(sid)}
              >{speakerLabel(sid, "mic", recording.speakers)}</button>
            {/each}
          {/if}
          {#if reviewActive}
            <button class="ghosty" onclick={clearReview}>{t("record.search.clear")}</button>
          {/if}
        </div>
      {/if}
      </div>

      <!-- 出错时才展开完整错误文案(可能较长);正常态收进右侧「录制中/就绪」标签,不占行。
           System 分类错误(isSystemDenied/isSystemUnavailable)不重复展示这行原始
           串——下方引导卡是这类错误的唯一信息面,避免同一件事说两遍。 -->
      {#if isError(recording.status) && !isSystemDenied && !isSystemUnavailable}
        <p class="status error"><span class="status-dot"></span>{displayStatus}</p>
      {/if}

      <!-- 硬承诺双轨拒录引导卡:System 起不来时后端整场拆除(不静默降级),这里按分类
           分支引导——权限缺失给可操作的「打开系统设置」;设备/组件不可用（含 Windows）
           给说明,macOS 下附修复入口。两张卡都带「修复授权」(codex 2026-08-11 P1):
           仅 AudioCapture 残留时 ScreenCapture preflight 仍为 true,上方 screenPerm
           横幅(修复入口原本唯一挂载点)根本不亮,而采集失败按分类会落进这两张卡——
           不在这里给入口,本 PR 要修的场景就永远触达不了 tccutil 双清。unavailable
           卡的 Windows 场景没有 TCC,按平台隐藏修复行。 -->
      {#if isSystemDenied}
        <div class="banner">
          <strong>{t("record.systemDenied.title")}</strong>
          {t("record.systemDenied.desc")}
          <button class="link" onclick={openSystemAudioPrivacySettings}>
            {t("record.banner.openSettings")}
          </button>
          <div class="fixline">
            {t("record.banner.permFix")}
            <button class="link" onclick={fixScreenPerm}>{t("record.banner.permFixBtn")}</button>
            <span class="hint">{t("record.banner.permFixHint")}</span>
          </div>
        </div>
      {:else if isSystemUnavailable}
        <div class="banner">
          {t("record.systemUnavailable.desc")}
          <span class="hint">{displayStatus}</span>
          {#if isMacPlatform}
            <div class="fixline">
              {t("record.banner.permFix")}
              <button class="link" onclick={fixScreenPerm}>{t("record.banner.permFixBtn")}</button>
              <span class="hint">{t("record.banner.permFixHint")}</span>
            </div>
          {/if}
        </div>
      {/if}

      <!-- 云端识别连接状态:仅云端模式录制时有事件,细提示条,不打断转写视线 -->
      {#if recording.isLive && cloudStatus}
        <p class="status" class:error={cloudStatus.kind === "backfill_failed"}>
          <span class="status-dot"></span>{t(cloudStatus.key, cloudStatus.params)}
        </p>
      {/if}

      <!-- 说话人条随头部整体吸顶:滚到会中段落时仍要能对着条上的名字辨认发言人/改名,
           条不在视口内这个对照就断了(用户反馈)。空说话人时组件自身不渲染,不占高。 -->
      <SpeakerChips speakers={recording.speakers} noteId={recording.noteId} editable={true} />
    {/if}
  </div>

  {#if !models || models.recording_ready}

    {#if showMcpHint}
      <div class="banner">
        {t("record.banner.mcpHint")}
        <button class="link" onclick={() => dismissMcpHint(true)}>{t("record.banner.mcpGo")}</button>
        <button class="link" onclick={() => dismissMcpHint(false)}>{t("record.banner.mcpDismiss")}</button>
      </div>
    {/if}

    <!-- 常态检测(设备现实驱动,与 isSystemDenied/isSystemUnavailable 引导卡互斥):
         那两张卡是"根本录不了"的唯一信息面,同屏再叠加音质类提示只会分散注意力。 -->
    {#if btEchoRisk && !recording.isLive && !isSystemDenied && !isSystemUnavailable}
      <div class="banner">
        {t("record.banner.btEcho")}
      </div>
    {/if}

    {#if lowInputVol && !isSystemDenied && !isSystemUnavailable}
      <div class="banner">
        {t("record.banner.lowInput", { vol: lowInputVol.vol })}
        <button class="link" onclick={fixInputVol}>{t("record.banner.setVolume", { target: INPUT_TARGET })}</button>
      </div>
    {/if}

    <!-- isSystemDenied 时上方引导卡已是唯一授权引导面,这条常驻预检横幅让位,
         避免同一件"去系统设置授权"的事同屏说两遍。 -->
    {#if !screenPerm && !recording.isLive && !isSystemDenied}
      <div class="banner">
        {t("record.banner.screenPerm")}
        <button class="link" onclick={requestScreenPerm}>{t("record.banner.authorizeNow")}</button>
        <span class="hint">{t("record.banner.screenPermHint")}</span>
        {#if showPermFix}
          <div class="fixline">
            {t("record.banner.permFix")}
            <button class="link" onclick={fixScreenPerm}>{t("record.banner.permFixBtn")}</button>
            <span class="hint">{t("record.banner.permFixHint")}</span>
          </div>
        {/if}
      </div>
    {/if}

    {#if recording.isLive && recording.diarization === "unavailable"}
      <div class="banner">{t("record.banner.diarUnavailable")}</div>
    {/if}

    {#if recording.storageDegraded}
      <div class="banner">{t("record.banner.storageDegraded")}</div>
    {/if}

    <!-- 当场纠正失败提示:后端停录竞态会报"会话已结束/笔记正被占用"等,原文展示、
         不自动重试;可手动关闭。 -->
    {#if editError}
      <div class="banner banner-danger">
        {editError}
        <button class="link" onclick={() => (editError = "")}>{t("record.edit.dismiss")}</button>
      </div>
    {/if}

    <div class="transcript-wrap">
      <div class="transcript" class:live={recording.isLive} bind:this={transcriptEl}>
      {#each recording.finals as line, i (line.seq)}
        <p
          id="seg-{line.seq}"
          class="final"
          class:current={recording.isLive && !hasPartial && line.seq === lastFinalSeq}
          class:hidden={!matchesSpeakerFilter(line, selectedSpeakers)}
          class:hit={hitSet.has(i)}
          class:hit-active={hits[activeHitClamped] === i}
        >
          <span class="spk-anchor">
            <button
              class="badge as-btn"
              style="background: {speakerColor(line.speaker, line.source, recording.speakers)}; color: {speakerInk(line.speaker, line.source, recording.speakers)}"
              disabled={!recording.isLive || recording.stopping}
              title={t("record.edit.speaker")}
              onclick={() => toggleSpeakerMenu(line.seq)}
            >{speakerLabel(line.speaker, line.source, recording.speakers)}</button>
            {#if speakerMenuSeq === line.seq}
              <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events, a11y_interactive_supports_focus -->
              <span class="spk-menu" role="menu" tabindex="-1" onclick={(e) => e.stopPropagation()}>
                {#each speakerIds as sid (sid)}
                  <button class="spk-item" role="menuitem" onclick={() => pickSpeaker(line, sid)}>
                    {speakerLabel(sid, "mic", recording.speakers)}
                  </button>
                {/each}
                {#if line.speaker}
                  <span class="spk-sep"></span>
                  {#if renamingSeq === line.seq}
                    <!-- svelte-ignore a11y_autofocus -->
                    <input
                      class="spk-rename-input"
                      autofocus
                      bind:value={renameText}
                      onkeydown={(e) => {
                        if (e.key === "Enter") commitRename(line);
                        if (e.key === "Escape") renamingSeq = null;
                      }}
                      onblur={() => commitRename(line)}
                    />
                  {:else}
                    <button class="spk-item" role="menuitem" onclick={() => beginRename(line)}>
                      {t("record.edit.rename")}
                    </button>
                  {/if}
                {/if}
              </span>
            {/if}
          </span>
          {#if editingSeq === line.seq}
            <!-- svelte-ignore a11y_autofocus -->
            <input
              class="edit-inline"
              autofocus
              bind:value={editingText}
              onkeydown={(e) => {
                if (e.key === "Enter") commitEdit(line);
                if (e.key === "Escape") editingSeq = null;
              }}
              onblur={() => (editingSeq = null)}
            />
          {:else}
            {line.text}
            {#if recording.isLive && !recording.stopping}
              <button class="row-act" title={t("record.edit.text")} onclick={() => beginEdit(line)}>
                <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <path d="M11.3 2.4l2.3 2.3L5.3 13l-3 .7.7-3z" />
                </svg>
              </button>
            {/if}
          {/if}
        </p>
      {/each}
      {#if recording.partialMic}
        <p class="partial" class:current={recording.isLive}><span class="badge mic">{t("record.badge.me")}</span>{recording.partialMic}</p>
      {/if}
      {#if recording.partialSystem}
        <p class="partial" class:current={recording.isLive}><span class="badge system">{t("record.badge.them")}</span>{recording.partialSystem}</p>
      {/if}
      {#if recording.finals.length === 0 && !recording.partialMic && !recording.partialSystem}
        <p class="hint">{t("record.emptyHint")}</p>
      {/if}
    </div>
    {#if recording.finals.length > 1 && recording.elapsedMs > 60_000}
      <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
      <div class="timeline" onclick={handleTimelineClick}>
        <!-- key 用下标:topPct 随走表每秒漂移,拿它当 key 会整批重建节点 -->
        {#each ticksView as topPct, i (i)}
          <span class="tick" style="top: {topPct}%"></span>
        {/each}
      </div>
    {/if}
    </div>

    <!-- 跟随被用户上滑打断时的返回入口：sticky 钉在滚动视口底部，恢复跟随即消失 -->
    <div class="jump-anchor" aria-hidden={follow || !recording.isLive}>
      {#if !follow && recording.isLive}
        <button class="jump" onclick={jumpToLatest}>{t("record.jumpLatest")}</button>
      {/if}
    </div>
  {/if}
</div>

<style>
  .container {
    padding: 1.5rem;
  }

  h1 {
    margin: 0 0 0.25rem;
  }

  /* 操作栏吸顶:canvas 不透明底钉在滚动视口顶端,转写文字从底下滚过;
     底缘用渐隐代替分隔线,静止在页首时不显突兀,滚动时文字平滑没入。 */
  .topbar {
    position: sticky;
    top: 0;
    z-index: 10;
    background: var(--canvas);
    padding-top: 0.4rem;
    margin-top: -0.4rem;
  }
  .topbar::after {
    content: "";
    position: absolute;
    top: 100%;
    left: 0;
    right: 0;
    height: 14px;
    background: linear-gradient(var(--canvas), transparent);
    pointer-events: none;
  }
  /* 单行整合(与详情页播放 transport 一致):左控制钮组 / 全宽波形 / 右计时+状态。
     波形 flex:1 吃掉中间空间,把右簇顶到行尾。 */
  .controls {
    display: flex;
    align-items: center;
    /* 允许折行(Codex P2):默认 800px 窗口减去侧栏只剩 ~450px,展开停止确认胶囊后
       (英文文案更长)固定簇的总宽会超出,内容直接溢出视口。波形是 flex:1/min-width:0,
       换行计算时按 0 参与,所以常态下永远是一行;只有真放不下时才折第二行。 */
    flex-wrap: wrap;
    row-gap: 0.4rem;
    gap: 0.75rem;
    padding: 0.5rem 0.75rem;
    /* 几何量常驻，暂停只切颜色——零跳动（照 #84 胶囊纪律） */
  }
  /* 面板:transport 行 + 回看行同属一块。底与描边挂在面板上(不再挂 .controls),
     这样两行共享同一个圆角与边界,读成一个整体而不是"卡片 + 一个孤立输入框"。 */
  .panel {
    border: 1px solid transparent;
    border-radius: var(--radius-lg);
    margin: 0 0 1rem;
  }
  .panel.card {
    background: var(--surface);
    border-color: var(--hairline);
  }
  /* 暂停:整块面板升格为 warning 基调,呼应"没在录"这一异常态——误以为还在录、
     白等一场是事故率最高的一刻。必须写在 .card 之后压过它的 surface 底。 */
  .panel.card.paused {
    background: var(--warning-tint);
    border-color: transparent;
  }

  /* 簇间发丝线分隔:控制钮 | 计时 | 波形。比纯间距更能说明"这是三组不同的东西"。 */
  .sep {
    width: 1px;
    height: 22px;
    background: var(--hairline);
    flex: none;
  }
  /* 圆形图标钮:34px 命中区(≥Apple HIG 的紧凑下限),1px 描边立形状,悬停上一级表面。
     停止钮走 record 红描边+红图标:它是破坏性动作,但仍属"录制语义"而非 danger 确认。 */
  .iconbtn {
    width: 34px;
    height: 34px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex: none;
    border-radius: var(--radius-full);
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    cursor: pointer;
    transition: background 0.12s ease, border-color 0.12s ease, color 0.12s ease;
  }
  .iconbtn:hover:not(:disabled) {
    background: var(--surface-soft);
  }
  .iconbtn:active:not(:disabled) {
    background: var(--surface-press);
  }
  .iconbtn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .iconbtn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .iconbtn.rec {
    color: var(--record);
    border-color: color-mix(in srgb, var(--record) 40%, transparent);
  }
  .iconbtn.rec:hover:not(:disabled) {
    background: color-mix(in srgb, var(--record) 12%, transparent);
  }
  /* 计时英雄位:一场录音最该一眼看到的是"录了多久"。等宽数字 + 紧字距,
     红点呼吸(录制中)/静止(暂停·停止中)。后缀只在非正常态出现。 */
  .clock {
    display: inline-flex;
    align-items: center;
    gap: 0.45rem;
    flex: none;
  }
  .clock .t {
    font-variant-numeric: tabular-nums;
    font-size: 1.35rem;
    font-weight: 500;
    letter-spacing: -0.4px;
    line-height: 1;
    color: var(--ink);
  }
  .clock .t.warn {
    color: var(--warning-ink);
  }
  .clock .lbl {
    font-size: 0.8rem;
    font-weight: 500;
    color: var(--warning-ink);
  }
  .recdot {
    width: 8px;
    height: 8px;
    flex: none;
    border-radius: var(--radius-full);
    background: var(--record);
  }
  .panel.paused .recdot {
    background: var(--warning-ink);
  }
  .recdot.breathe {
    animation: recbreathe 2.4s ease-in-out infinite;
  }
  @keyframes recbreathe {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.35;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .recdot.breathe {
      animation: none;
    }
  }
  /* 两个源指示灯成对出现才自解释("这是两路收音");单挂一个「对方」谁也说不清指什么。
     有电平点亮(我=录制红 / 对方=mint),静默退灰。 */
  .srcs {
    display: inline-flex;
    align-items: center;
    gap: 0.7rem;
    flex: none;
    font-size: 0.8rem;
    font-weight: 500;
    color: var(--ink-faint);
    white-space: nowrap;
  }
  .srcs .s {
    display: inline-flex;
    align-items: center;
    gap: 0.35em;
    transition: color 200ms;
  }
  .srcs .d {
    width: 7px;
    height: 7px;
    border-radius: var(--radius-full);
    background: var(--hairline-strong);
    transition: background 200ms;
  }
  .srcs .s.on {
    color: var(--tint-mint-ink);
  }
  .srcs .s.on .d {
    background: var(--tint-mint-ink);
  }
  .srcs .s.mic.on {
    color: var(--record);
  }
  .srcs .s.mic.on .d {
    background: var(--record);
  }
  /* 钮组本身也要能折能缩(Codex P2):窗口可以被拖到比默认 800px 更窄,展开停止确认后
     「胶囊 + 暂停钮」的固定宽会超过内容区,而 flex: none 的组既不换行也不收缩,
     整组直接溢出视口。 */
  .ctl-group {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.5rem 0.75rem;
    flex: 0 1 auto;
    min-width: 0;
  }
  /* 空闲态只剩一个主动作「开始录制」,走 primary 药丸(DESIGN.md 第 3 条 Raycast 签名);
     录制中的暂停/停止改成圆形图标钮(见 .iconbtn),不再用文字按钮占横向。 */
  /* button-secondary 形态：暗色第一公民下 canvas 底=页面底(#07080a 同色)，
     无边+shadow-btn 会让按钮完全隐形；shadow-btn 是主按钮药丸专用高光，这里
     改用 transparent + hairline-strong 描边，靠轮廓立住形状 */
  .ctl {
    display: inline-flex;
    align-items: center;
    gap: 0.45em;
    border-radius: var(--radius-md);
    border: 1px solid var(--hairline-strong);
    padding: 0.45em 1.1em;
    font-weight: 500;
    font-size: 0.9rem;
    cursor: pointer;
    background: transparent;
    color: var(--ink);
  }
  .ctl:hover { background: var(--surface-soft); }
  .ctl:disabled { opacity: 0.6; cursor: default; }
  /* 主停止按钮走 primary 药丸，不需要 secondary 的 hairline 描边 */
  .ctl.primary { background: var(--primary); color: var(--on-primary); border-radius: var(--radius-full); border-color: transparent; }
  .ctl.primary:hover { background: var(--primary-pressed); }
  /* 录制符号用 CSS 图形而非 Unicode 字符(●■▶ 各平台字形/基线不一,显糙) */
  .sym {
    width: 9px;
    height: 9px;
    flex-shrink: 0;
  }
  .sym.dot { border-radius: var(--radius-full); background: var(--record); }
  .sym.dot.on-blue { background: var(--on-primary); }
  /* 方块 8px:9px 的实心方块在 0.9rem 文字旁偏重,压过并排的文字 */
  /* 竖条收到 2px:3px 在这个字号旁显糙(视觉上像两根短粗棍而不是暂停符) */
  /* 实时音轨:滚动电平条,新条从右缘进入(justify-content:flex-end + overflow 裁左侧)。
     record 红呼应"录制中"是唯一常驻彩色信号;暂停冻结退 ink-faint。
     只画 mic 一条(冒烟反馈:双行太吵),flex:1 吃满控制条与右侧计时之间的整行。 */
  .wave-live {
    flex: 1;
    min-width: 0;
    height: 34px;
    position: relative;
    display: flex;
    align-items: center;
    /* 右对齐:最新一根永远贴住右缘(Codex P2)。条是定宽的,容器算出来的根数与实际
       可用宽度总有几像素出入——左对齐时这点出入会落在右边,把最新样本裁掉或推离活动端;
       右对齐则让出入落在左边,正好被下面的淡出蒙版吃掉。 */
    justify-content: flex-end;
    gap: 2px;
    overflow: hidden;
    /* 左缘淡出:滚动历史"淡入过去",顺带消掉 overflow 的硬切边 */
    -webkit-mask-image: linear-gradient(90deg, transparent 0, #000 56px, #000 100%);
    mask-image: linear-gradient(90deg, transparent 0, #000 56px, #000 100%);
  }
  /* 贯穿基线:静音时看到的就是这条线,把条串成一根轴而不是一排浮空的破折号。
     画在条的下面(z-index),静音条 1px 叠上去只是让"采过样"的区段略实一点。 */
  .wave-live::after {
    content: "";
    position: absolute;
    left: 0;
    right: 0;
    top: 50%;
    height: 1px;
    transform: translateY(-0.5px);
    background: var(--hairline);
    pointer-events: none;
  }
  /* 「对方」指示灯:系统声在收音时点亮(mint,同 .badge.system 配色令牌),静默退灰。
     常驻占位不闪现,回答"对方声音有没有在录"而不额外占一行波形。 */
  /* 条:定宽 2px、节距 4px(gap 2px),根数由容器宽度定(见 barCountFor)——
     此前是 flex:1 把 240 根拉满任意宽度,在宽屏上被抻成一排 4~5px 的破折号。 */
  .wave-live .bar {
    position: relative;
    z-index: 1;
    width: 2px;
    flex: none;
    border-radius: 1px;
    background: var(--record);
  }
  /* 静音段不上红:只留 1px 的安静基线色。红是"录制中"的信号色,不该被静音铺满整行。 */
  .wave-live .bar.silent {
    background: var(--hairline-strong);
  }
  .wave-live.frozen .bar {
    background: var(--ink-faint);
  }
  .wave-live.frozen .bar.silent {
    background: var(--hairline);
  }
  /* 冻结波形整体退后(暂停态由整条 warning 底 + 状态字承担醒目度,波形只作背景交代) */
  .wave-live.frozen {
    opacity: 0.45;
  }

  /* 回看工具条:页内搜索 + 说话人过滤 chips，紧贴 controls 下方。 */
  /* 回看行:面板的第二行,用发丝线与 transport 行分隔(第 5 条:发丝线代替阴影)。
     面板没上底时(空转写页)这条线也不出——那时它根本不渲染。 */
  .review-bar {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    flex-wrap: wrap;
    padding: 0.45rem 0.75rem;
  }
  .panel.card .review-bar {
    border-top: 1px solid var(--hairline);
  }
  /* 搜索药丸:与图标钮同一套圆角语言。此前是 hairline-strong 描边的方框,比同屏卡片的
     边还重,孤零零一个尤其突兀;现在默认无边、只靠 surface-soft 的底立形状,聚焦才亮 accent。 */
  .review-bar .search {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    min-width: 12rem;
    max-width: 16rem; /* 冒烟反馈:全宽搜索框喧宾夺主,收窄成工具位 */
    background: var(--surface-soft);
    border: 1px solid transparent;
    border-radius: var(--radius-full);
    padding: 0.3rem 0.7rem;
    color: var(--ink-faint);
  }
  .review-bar .search .ico {
    display: inline-flex;
    flex: none;
  }
  .review-bar .search input {
    all: unset;
    flex: 1;
    min-width: 0;
    font: inherit;
    font-size: 0.86rem;
    color: var(--ink);
  }
  .review-bar .search input::placeholder {
    color: var(--ink-faint);
  }
  .review-bar .search:focus-within {
    border-color: var(--accent);
    color: var(--ink-secondary);
  }
  .review-bar .hit-count {
    font-size: 0.82rem;
    color: var(--ink-faint);
    white-space: nowrap;
  }
  /* 幽灵按钮:无边透明底，弱化成次级操作(上一个/下一个/清除)，不与说话人 chip 抢视觉。 */
  .review-bar .ghosty {
    border: none;
    background: none;
    color: var(--ink-secondary);
    border-radius: var(--radius-md);
    padding: 0.25em 0.5em;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .review-bar .ghosty:hover {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .review-bar .chip {
    border-radius: var(--radius-full);
    padding: 0.1em 0.6em;
    border: 1px solid var(--hairline);
    background: transparent;
    color: var(--ink-secondary);
    font-size: 0.85rem;
    cursor: pointer;
  }
  .review-bar .chip.on {
    background: var(--accent-tint);
    border-color: var(--accent);
    color: var(--ink);
  }

  /* 搜索命中:高亮不隐藏(与说话人过滤的"隐藏"口径不同)。当前命中额外描边定位。 */
  p.final.hidden {
    display: none;
  }
  p.final.hit {
    background: var(--accent-tint);
    border-radius: var(--radius-md);
  }
  p.final.hit-active {
    outline: 2px solid var(--accent);
  }

  /* 错误详情行(仅出错时):danger 色,完整展开可能较长的错误文案 */
  .status {
    display: flex;
    align-items: center;
    gap: 0.4em;
    color: var(--ink-faint);
    font-size: 0.85rem;
    margin: 0 0 1rem;
  }
  .status-dot {
    width: 7px;
    height: 7px;
    border-radius: var(--radius-full);
    background: var(--ink-faint);
  }

  .status.error {
    color: var(--danger);
    font-weight: 500;
  }

  /* 右缘迷你时间轴的定位基准；本身不裁剪，时间轴细轨叠在 transcript 的 20px 右内边距上。 */
  .transcript-wrap {
    position: relative;
  }

  /* transcript-container：surface 底、rounded-xl、正文用 transcript 字级(1.02rem/1.7) */
  .transcript {
    min-height: 8rem;
    background: var(--surface);
    border-radius: var(--radius-xl);
    padding: 20px;
    font-size: 1.02rem;
    line-height: 1.7;
  }

  .transcript p {
    margin: 0 0 6px 0;
    /* 当前句放大/变色/亮底的切换做成过渡,高亮随语句推进平滑下移(歌词感) */
    transition:
      font-size 0.2s ease,
      color 0.2s ease,
      background 0.2s ease;
  }

  /* 录制中：底部 50vh 留白,使"滚到底"恰好把最后一行(当前句)钉在屏幕垂直中央;
     顶部 40vh 留白保证开场内容还很少时容器已可滚——第一句话就能落在中央,
     不用等内容攒满半屏。停止后留白撤掉,恢复普通文档流。 */
  .transcript.live {
    padding-top: 40vh;
    padding-bottom: 50vh;
  }

  .final {
    color: var(--ink);
  }
  /* 录制中历史行退后(次级墨色),把注意力让给中央的当前句 */
  .transcript.live .final {
    color: var(--ink-secondary);
  }

  .partial {
    color: var(--ink-faint);
    font-style: italic;
  }

  /* 当前句(在途预览,或无预览时的最新定稿):放大 + 主墨色 + accent 亮底高亮,
     轻投影让高亮块从页面上浮起一层(歌词舞台感) */
  .transcript.live p.current {
    font-size: 1.5em;
    line-height: 1.55;
    color: var(--ink);
    background: var(--accent-tint);
    border-radius: var(--radius-md);
    padding: 0.3em 0.55em;
    margin-left: -0.55em;
    margin-right: -0.55em;
    box-shadow: 0 4px 14px light-dark(rgba(0, 0, 0, 0.12), rgba(0, 0, 0, 0.45));
  }

  /* 空态居中:一大块灰底里孤零零一行左对齐文字显得没做完 */
  .transcript .hint {
    color: var(--ink-faint);
    text-align: center;
    padding: 2.6rem 0;
    margin: 0;
  }

  /* 右缘迷你时间轴:细轨映射 0..elapsedMs，点击按 start_ms 定位最近可见行；
     长录制（>1min 且 ≥2 行）才现身，短录没有滚动意义。 */
  .timeline {
    position: absolute;
    right: 0;
    top: 0.5rem;
    bottom: 0.5rem;
    width: 14px;
    cursor: pointer;
    border-left: 2px solid var(--hairline);
  }
  .timeline:hover {
    border-left-color: var(--hairline-strong);
  }
  .timeline .tick {
    position: absolute;
    left: -4px;
    width: 6px;
    height: 2px;
    background: var(--ink-faint);
    border-radius: 1px;
  }

  /* speaker-badge：粉彩底 + 同色相文字(soft 公式)、rounded-sm、micro 字级；
     mic/system 是尚未解析出说话人时的占位色，固定取 tint-sky/tint-mint，与
     speakerColor()/speakerInk() 的兜底分支保持一致视觉。 */
  .badge {
    display: inline-block;
    min-width: 2.2em;
    text-align: center;
    font-size: 0.78rem;
    font-weight: 500;
    border-radius: var(--radius-sm);
    padding: 0.05em 0.4em;
    margin-right: 0.4em;
  }
  .badge.mic { background: var(--tint-sky); color: var(--tint-sky-ink); }
  .badge.system { background: var(--tint-mint); color: var(--tint-mint-ink); }

  /* 当场纠正:行首徽章变按钮(点开改派菜单),static 态与 span 视觉一致——border 清空、
     cursor 指示可点;录制结束/停止中禁用(disabled 由模板判定 isLive && !stopping)。 */
  .badge.as-btn {
    border: none;
    cursor: pointer;
    font: inherit;
    font-weight: 500;
  }
  .badge.as-btn:disabled {
    cursor: default;
  }
  /* 徽章 + 改派菜单的定位锚点:inline-block 让 .spk-menu 精确贴着徽章下缘展开,
     不受同行后续文字影响。 */
  .spk-anchor {
    position: relative;
    display: inline-block;
  }
  /* menu/popover（改派说话人）：DESIGN.md 浮层规范——surface-press 底、hairline 边、
     radius-lg、shadow-popover，与 SpeakerChips .panel / 详情页 .export-menu 同规格。 */
  .spk-menu {
    position: absolute;
    top: calc(100% + 4px);
    left: 0;
    z-index: 20;
    display: flex;
    flex-direction: column;
    min-width: 8rem;
    padding: 0.25rem;
    background: var(--surface-press);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    font-size: 0.85rem;
    font-weight: 400;
    cursor: default;
  }
  .spk-item {
    display: block;
    width: 100%;
    background: none;
    border: none;
    color: var(--ink);
    text-align: left;
    padding: 0.35rem 0.55rem;
    border-radius: var(--radius-md);
    font: inherit;
    cursor: pointer;
  }
  .spk-item:hover {
    background: var(--surface-soft);
  }
  .spk-sep {
    display: block;
    height: 1px;
    background: var(--hairline);
    margin: 0.2rem 0.1rem;
  }
  .spk-rename-input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.35rem 0.55rem;
    background: transparent;
    border: none;
    outline: none;
    font: inherit;
    color: var(--ink);
  }

  /* 段落文本行内编辑:与徽章齐平，覆盖当前段文字的量 */
  .edit-inline {
    width: min(100%, 32rem);
    font: inherit;
    color: var(--ink);
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-sm);
    padding: 0.1em 0.4em;
  }

  /* 行级操作(编辑铅笔):悬停显影惯例(DESIGN.md #6)——默认隐身，行 hover 或
     自身 focus-visible 时浮现，保持转写区安静。 */
  .row-act {
    visibility: hidden;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    vertical-align: middle;
    width: 1.5rem;
    height: 1.5rem;
    margin-left: 0.2em;
    padding: 0;
    border: none;
    background: none;
    color: var(--ink-faint);
    border-radius: var(--radius-sm);
    cursor: pointer;
  }
  .final:hover .row-act,
  .row-act:focus-visible {
    visibility: visible;
  }
  .row-act:hover {
    color: var(--accent);
    background: var(--surface-soft);
  }

  /* 「回到最新」药丸：零高锚点 + sticky bottom，钉在滚动视口底部居中，
     不占版面高度、不遮转写。flex-end 让按钮底边贴锚点线向上生长——不能用
     translateY(-100%)：零高容器的默认 stretch 会把按钮使用高度压成 0，
     百分比位移随之失效，药丸会沉到视口底边被裁半截。 */
  .jump-anchor {
    position: sticky;
    bottom: 1rem;
    height: 0;
    display: flex;
    justify-content: center;
    align-items: flex-end;
  }
  .jump {
    border: none;
    border-radius: var(--radius-full);
    background: var(--primary);
    color: var(--on-primary);
    font-size: 0.85rem;
    font-weight: 500;
    padding: 0.4em 1em;
    cursor: pointer;
    box-shadow: var(--shadow-popover);
  }
  .jump:hover {
    background: var(--primary-pressed);
  }

  .banner {
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    color: var(--warning-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .banner .link {
    background: none;
    border: none;
    color: var(--accent);
    text-decoration: underline;
    cursor: pointer;
    padding: 0 0.2em;
    font-size: inherit;
  }
  .banner .hint { color: var(--warning-ink); }
  .banner .fixline {
    margin-top: 0.4rem;
    padding-top: 0.4rem;
    border-top: 1px solid var(--warning-line);
  }
  /* 错误横幅换 danger 色系(当场纠正失败:后端停录竞态报错原样展示) */
  .banner.banner-danger {
    background: var(--danger-tint);
    border-color: var(--danger-line);
    color: var(--danger-ink);
  }
</style>
