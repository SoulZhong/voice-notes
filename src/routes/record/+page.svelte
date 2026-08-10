<script lang="ts">
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
  import { matchesSpeakerFilter, searchHits } from "$lib/liveView";

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

  // 停止二段确认:悬停态胶囊仅在"这一场"有效——录制结束(isLive 翻 false，无论是
  // 经这次确认停止、还是场次因错误/别处自行终止)都必须复位，否则下一场开录后
  // 控制条会带着上一场遗留的确认胶囊出现。
  let confirmStop = $state(false);
  $effect(() => {
    if (!recording.isLive) confirmStop = false;
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
  $effect(() => {
    if (!recording.isLive) {
      editingSeq = null;
      speakerMenuSeq = null;
      renamingSeq = null;
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
  const statusLabel = $derived(
    isError(recording.status)
      ? t("record.status.error")
      : recording.status === "recording"
        ? t("record.status.recording")
        : recording.status === "paused"
          ? t("record.status.paused")
          : recording.status === "stopped"
            ? t("record.status.stopped")
            : t("record.status.ready"),
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

  // ── 实时音轨(录音机式):录制中每 120ms 采样一次电平,新条从右缘进入、旧条左移,
  //    滚动保留最近 240 条(约 29s);暂停冻结不清空,停止清空。interval 回调里读
  //    micPct/sysPct 是瞬时值,不进 effect 依赖。mic/system 各一条独立数组。 ──
  const LIVE_BARS = 240;
  let liveBarsMic = $state<number[]>([]);
  let liveBarsSys = $state<number[]>([]);
  $effect(() => {
    if (!recording.isLive) {
      liveBarsMic = [];
      liveBarsSys = [];
      return;
    }
    if (recording.paused) return; // 冻结:不采样,已有波形保留
    const t = setInterval(() => {
      liveBarsMic = [...liveBarsMic.slice(-(LIVE_BARS - 1)), micPct];
      liveBarsSys = [...liveBarsSys.slice(-(LIVE_BARS - 1)), sysPct];
    }, 120);
    return () => clearInterval(t);
  });
  /** 渲染用:前导补零到 LIVE_BARS,让波形从开录起就铺满整行(与详情页全宽波形一致),
      而非少量样本挤在右缘、左侧留大片空——补的零段是低平基线,新声仍从右侧进入。 */
  const liveBarsMicView = $derived(
    liveBarsMic.length >= LIVE_BARS
      ? liveBarsMic
      : [...new Array(LIVE_BARS - liveBarsMic.length).fill(0), ...liveBarsMic],
  );
  const liveBarsSysView = $derived(
    liveBarsSys.length >= LIVE_BARS
      ? liveBarsSys
      : [...new Array(LIVE_BARS - liveBarsSys.length).fill(0), ...liveBarsSys],
  );

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
      if (sc.scrollHeight - sc.scrollTop - sc.clientHeight <= BOTTOM_SLOP) follow = true;
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
  const hits = $derived(searchHits(recording.finals, searchQuery));
  /** each 内 O(n) 判定命中(而非 hits.includes(i) 的 O(n²))：finals 行数虽然通常
      只有数百，但录制可长达数小时，直接派生 Set 零成本。 */
  const hitSet = $derived(new Set(hits));
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
    activeHit = (activeHit + delta + hits.length) % hits.length;
    document
      .getElementById(`seg-${recording.finals[hits[activeHit]].seq}`)
      ?.scrollIntoView({ block: "center", behavior: "smooth" });
  }
  function toggleSpeaker(id: string) {
    const next = new Set(selectedSpeakers);
    next.has(id) ? next.delete(id) : next.add(id);
    selectedSpeakers = next;
  }
</script>

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
      <div class="controls" class:paused={recording.paused}>
        <!-- 左:控制钮组 -->
        <div class="ctl-group">
          {#if recording.stopping}
            <button class="ctl danger" disabled>
              <span class="sym square"></span>{t("record.btn.stopping")}
            </button>
          {:else if !recording.isLive}
            <button class="ctl primary" disabled={recording.pending} onclick={startRecording}>
              <span class="sym dot on-blue"></span>{t("record.btn.start")}
            </button>
          {:else}
            {#if recording.paused}
              <button class="ctl" disabled={recording.pending} onclick={() => recording.unpause()}>
                <span class="sym play"></span>{t("record.btn.resume")}
              </button>
            {:else}
              <button class="ctl" disabled={recording.pending} onclick={() => recording.pause()}>
                <span class="sym pause"></span>{t("record.btn.pause")}
              </button>
            {/if}
            {#if confirmStop}
              <span class="stop-confirm">
                {t("record.btn.stopConfirmMsg")}
                <button
                  class="ctl danger"
                  onclick={() => {
                    confirmStop = false;
                    recording.stop().catch((err) => console.error("停止录制失败", err));
                  }}
                >{t("record.btn.stopConfirmYes")}</button>
                <button class="ctl" onclick={() => (confirmStop = false)}>{t("record.btn.stopConfirmNo")}</button>
              </span>
            {:else}
              <button class="ctl danger" disabled={recording.pending} onclick={() => (confirmStop = true)}>
                <span class="sym square"></span>{t("record.btn.stop")}
              </button>
            {/if}
          {/if}
        </div>

        <!-- 中:实时音轨(录制中才有),限宽居中,滚动电平波形/电平表,新声从右缘进入 -->
        {#if recording.isLive}
          <div class="wave-stack" aria-hidden="true">
            <div class="wave-live" class:frozen={recording.paused} title={t("record.micLevel")}>
              <span class="wave-tag mic">{t("record.badge.me")}</span>
              {#each liveBarsMicView as h, i (i)}<span class="bar" style="height: {Math.max(6, h)}%"></span>{/each}
            </div>
            <div class="wave-live" class:frozen={recording.paused} title={t("record.systemLevel")}>
              <span class="wave-tag system">{t("record.badge.them")}</span>
              {#each liveBarsSysView as h, i (i)}<span class="bar" style="height: {Math.max(6, h)}%"></span>{/each}
            </div>
          </div>
        {/if}

        <!-- 右:计时 + 状态,同一簇(不再单挂一行);状态点是唯一动态信号。
             仅录制中出现——空闲态只需左侧「开始录制」CTA,不让一个「就绪」标签
             孤浮右缘重演失衡;空闲若开录失败,错误仍由下方红色详情行兜底。 -->
        {#if recording.isLive}
          <div class="live-meta">
            <span class="timer" class:pausedTimer={recording.paused}>{formatTs(recording.elapsedMs)}</span>
            <span class="status-inline" class:pausedTag={recording.paused}>
              <span class="status-dot" class:live={!recording.paused}></span>{statusLabel}
            </span>
          </div>
        {/if}
      </div>

      <!-- 回看工具条:页内搜索(高亮+跳转) + 说话人过滤 chips。只在有转写内容可回看时
           出现(录制中或已有定稿行)，空转写页不占位。 -->
      {#if recording.isLive || recording.finals.length > 0}
        <div class="review-bar">
          <input
            class="search"
            placeholder={t("record.search.placeholder")}
            bind:value={searchQuery}
            onkeydown={(e) => {
              if (e.key === "Enter") gotoHit(e.shiftKey ? -1 : 1);
              if (e.key === "Escape") clearReview();
            }}
          />
          {#if searchQuery.trim()}
            <span class="hit-count">
              {hits.length ? `${activeHit + 1}/${hits.length}` : t("record.search.none")}
            </span>
            <button class="ghosty" onclick={() => gotoHit(-1)} title={t("record.search.prev")}>↑</button>
            <button class="ghosty" onclick={() => gotoHit(1)} title={t("record.search.next")}>↓</button>
          {/if}
          {#each speakerIds as sid (sid)}
            <button
              class="chip"
              class:on={selectedSpeakers.has(sid)}
              onclick={() => toggleSpeaker(sid)}
            >{speakerLabel(sid, "mic", recording.speakers)}</button>
          {/each}
          {#if reviewActive}
            <button class="ghosty" onclick={clearReview}>{t("record.search.clear")}</button>
          {/if}
        </div>
      {/if}

      <!-- 出错时才展开完整错误文案(可能较长);正常态收进右侧「录制中/就绪」标签,不占行。
           System 分类错误(isSystemDenied/isSystemUnavailable)不重复展示这行原始
           串——下方引导卡是这类错误的唯一信息面,避免同一件事说两遍。 -->
      {#if isError(recording.status) && !isSystemDenied && !isSystemUnavailable}
        <p class="status error"><span class="status-dot"></span>{displayStatus}</p>
      {/if}

      <!-- 硬承诺双轨拒录引导卡:System 起不来时后端整场拆除(不静默降级),这里按分类
           分支引导——权限缺失给可操作的「打开系统设置」;设备/组件不可用（含 Windows）
           只给说明,没有可操作的跳转。 -->
      {#if isSystemDenied}
        <div class="banner">
          <strong>{t("record.systemDenied.title")}</strong>
          {t("record.systemDenied.desc")}
          <button class="link" onclick={openSystemAudioPrivacySettings}>
            {t("record.banner.openSettings")}
          </button>
        </div>
      {:else if isSystemUnavailable}
        <div class="banner">
          {t("record.systemUnavailable.desc")}
          <span class="hint">{displayStatus}</span>
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

    <div class="transcript" class:live={recording.isLive} bind:this={transcriptEl}>
      {#each recording.finals as line, i (line.seq)}
        <p
          id="seg-{line.seq}"
          class="final"
          class:current={recording.isLive && !hasPartial && line.seq === lastFinalSeq}
          class:hidden={!matchesSpeakerFilter(line, selectedSpeakers)}
          class:hit={hitSet.has(i)}
          class:hit-active={hits[activeHit] === i}
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
    gap: 0.75rem;
    margin: 0 0 1rem;
    padding: 0.5rem 0.75rem;
    border: 1px solid transparent;
    border-radius: var(--radius-lg);
    /* 几何量常驻，暂停只切颜色——零跳动（照 #84 胶囊纪律） */
  }
  /* 暂停:整条控制条升格为 warning 基调，呼应"没在录"这一异常态——不再只靠
     右侧小灰点交代，误以为还在录、白等一场的事故率最高的一刻。 */
  .controls.paused {
    background: var(--warning-tint);
    border-color: var(--warning-line);
  }
  .ctl-group {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    flex: none;
  }
  /* 右簇:计时 + 状态标签同一组 */
  .live-meta {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    flex: none;
  }
  /* 状态短标签(并进右簇):caption 级次要信息,状态点是唯一动态信号;出错转 danger */
  .status-inline {
    display: inline-flex;
    align-items: center;
    gap: 0.4em;
    color: var(--ink-faint);
    font-size: 0.85rem;
    white-space: nowrap;
  }
  /* 暂停时状态标签升格：不再是小灰字，warning 墨色 + 加粗，与整条变调呼应 */
  .status-inline.pausedTag {
    color: var(--warning-ink);
    font-weight: 600;
  }
  /* 录制控制条：裸 .ctl 是 button-secondary（暂停/恢复）；.primary 是开始录制的
     唯一主动作；.danger（停止）形态同 secondary，只是字色换 record，呼应
     “录制红点是唯一常驻彩色信号”。 */
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
  .ctl.danger { color: var(--record); font-weight: 500; }
  /* 录制符号用 CSS 图形而非 Unicode 字符(●■▶ 各平台字形/基线不一,显糙) */
  .sym {
    width: 9px;
    height: 9px;
    flex-shrink: 0;
  }
  .sym.dot { border-radius: var(--radius-full); background: var(--record); }
  .sym.dot.on-blue { background: var(--on-primary); }
  .sym.square { border-radius: 2px; background: var(--record); }
  .sym.pause {
    width: 8px;
    height: 10px;
    border-left: 3px solid currentColor;
    border-right: 3px solid currentColor;
  }
  .sym.play {
    width: 0;
    height: 0;
    border-left: 9px solid currentColor;
    border-top: 5px solid transparent;
    border-bottom: 5px solid transparent;
  }
  /* 停止二段确认胶囊：#84 同款 warning-tint 行内胶囊，120ms 淡入，不引起行高跳动
     （padding/字号/行高与常态 .ctl-group 一致，只是行内多出一段文案+两枚按钮）。 */
  .stop-confirm {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    color: var(--warning-ink);
    border-radius: var(--radius-full);
    padding: 0.15rem 0.6rem;
    animation: fadein 120ms ease-out;
  }
  @keyframes fadein {
    from { opacity: 0; }
    to { opacity: 1; }
  }
  @media (prefers-reduced-motion: reduce) {
    .stop-confirm { animation: none; }
  }
  /* 计时用等宽数字：秒数跳动时数字宽度不抖动，视觉更稳定 */
  .timer {
    font-variant-numeric: tabular-nums;
    font-weight: 500;
    font-size: 1rem;
    color: var(--ink-secondary);
  }
  .timer.pausedTimer { color: var(--ink-faint); }
  /* 双通道纵排:mic/system 各占一行,行高减半(32px→16px×2),总高与原单通道一致。
     flex:1 从单行 wave-live 移到外层 wave-stack,继续吃满控制条与右侧计时之间的整行。
     空闲时容器空置但保留 flex:1 占位,把计时推到行尾、行高不跳。 */
  .wave-stack {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    justify-content: center;
    gap: 2px;
  }
  /* 实时音轨:滚动电平条,新条从右缘进入(justify-content:flex-end + overflow 裁左侧)。
     record 红呼应"录制中"是唯一常驻彩色信号;暂停冻结退 ink-faint。 */
  .wave-live {
    min-width: 0;
    height: 16px;
    display: flex;
    align-items: center;
    gap: 1px;
    overflow: hidden;
  }
  /* 行首来源徽章:配色令牌复用 .badge mic/system,12px 次级墨小标,不参与 flex:1 均分。 */
  .wave-tag {
    flex: none;
    font-size: 0.68rem;
    font-weight: 500;
    line-height: 1;
    padding: 0.1em 0.35em;
    margin-right: 0.35em;
    border-radius: var(--radius-sm);
  }
  .wave-tag.mic { background: var(--tint-sky); color: var(--tint-sky-ink); }
  .wave-tag.system { background: var(--tint-mint); color: var(--tint-mint-ink); }
  .wave-live .bar {
    flex: 1;
    min-width: 1px;
    min-height: 2px;
    border-radius: var(--radius-full);
    background: var(--record);
  }
  .wave-live.frozen .bar {
    background: var(--ink-faint);
  }

  /* 回看工具条:页内搜索 + 说话人过滤 chips，紧贴 controls 下方。 */
  .review-bar {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    flex-wrap: wrap;
    margin-top: 0.4rem;
  }
  .review-bar .search {
    min-width: 12rem;
    font: inherit;
    color: var(--ink);
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-md);
    padding: 0.35em 0.6em;
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
  .status-dot.live {
    background: var(--record);
    animation: breathe 1.6s ease-in-out infinite;
  }
  @keyframes breathe {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.35; }
  }
  @media (prefers-reduced-motion: reduce) {
    .status-dot.live { animation: none; }
  }

  .status.error {
    color: var(--danger);
    font-weight: 500;
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
