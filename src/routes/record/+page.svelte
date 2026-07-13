<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { invoke } from "@tauri-apps/api/core";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { recording } from "$lib/recording.svelte";
  import { speakerLabel, speakerColor, speakerInk } from "$lib/notes";
  import SpeakerChips from "$lib/SpeakerChips.svelte";
  import { modelsStatus, getSettings, setSettings, type ModelsStatus } from "$lib/models";
  import ModelDownloadCard from "$lib/ModelDownloadCard.svelte";
  import { formatTs } from "$lib/notes";

  let models = $state<ModelsStatus | null>(null);

  async function refreshModels() {
    try {
      models = await modelsStatus();
    } catch {
      /* 查询失败按就绪处理，不挡老用户 */
    }
  }

  // 屏幕录制权限预检:未授权时系统声音只会在开录后静默降级,这里在开录前就常驻
  // 提示(2026-07-07 实锤:用户所有笔记都没有 system 轨,自己毫无察觉)。
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
      if (!ok) await openScreenRecordingSettings();
    } catch {
      await openScreenRecordingSettings();
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

  // 蓝牙外放预警:「保持外放音量」+ 蓝牙输出时,蓝牙延迟(300~600ms+)超出软件
  // 回声消除的追踪范围,mic 会混入近乎全量的对方声音(面试录音实锤)。开录前提示,
  // 查询失败按"无风险"静默。
  let btEchoRisk = $state(false);
  async function refreshBtRisk() {
    try {
      const [s, bt] = await Promise.all([
        getSettings(),
        invoke<boolean>("output_is_bluetooth"),
      ]);
      btEchoRisk = s.keep_output_volume && bt;
    } catch {
      btEchoRisk = false;
    }
  }

  // 输入音量过低预警(普通麦克风模式):系统输入音量被会议软件拉低会录得很轻。
  // 开录前 + 录制中都检测,一键调回可用电平;VPIO 模式(自带 AGC)/仅系统声不检测。
  const LOW_INPUT_THRESHOLD = 50;
  const INPUT_TARGET = 75;
  const POLL_MS = 4000;
  let lowInputVol = $state<{ vol: number } | null>(null);
  async function refreshInputVol() {
    try {
      const [s, vol] = await Promise.all([
        getSettings(),
        invoke<number | null>("input_volume"),
      ]);
      lowInputVol =
        s.keep_output_volume && !s.record_system_only && vol != null && vol < LOW_INPUT_THRESHOLD
          ? { vol }
          : null;
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
    return () => {
      window.removeEventListener("focus", onFocus);
      clearInterval(volTimer);
    };
  });

  function isError(s: string) {
    return s.startsWith("error:");
  }
  /** 状态机原值(idle/recording/paused/stopped/error:…)映射成右侧簇里的友好短标签;
      错误详情(可能很长)不塞这里,另在下方红色行完整展开。 */
  const statusLabel = $derived(
    isError(recording.status)
      ? "出错"
      : recording.status === "recording"
        ? "录制中"
        : recording.status === "paused"
          ? "已暂停"
          : recording.status === "stopped"
            ? "已停止"
            : "就绪",
  );
  async function openScreenRecordingSettings() {
    await openUrl(
      "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
    );
  }

  async function startRecording() {
    await recording.start(); // 已在录制页，无需跳转
  }
  const levelPct = $derived.by(() => {
    if (!recording.isLive || recording.level <= 0) return 0;
    const db = 20 * Math.log10(recording.level);
    return Math.max(0, Math.min(100, ((db + 50) / 50) * 100)); // -50dBFS..0dBFS → 0..100%
  });

  // ── 实时音轨(录音机式):录制中每 120ms 采样一次电平,新条从右缘进入、旧条左移,
  //    滚动保留最近 240 条(约 29s);暂停冻结不清空,停止清空。interval 回调里读
  //    levelPct 是瞬时值,不进 effect 依赖。 ──
  const LIVE_BARS = 240;
  let liveBars = $state<number[]>([]);
  $effect(() => {
    if (!recording.isLive) {
      liveBars = [];
      return;
    }
    if (recording.paused) return; // 冻结:不采样,已有波形保留
    const t = setInterval(() => {
      liveBars = [...liveBars.slice(-(LIVE_BARS - 1)), levelPct];
    }, 120);
    return () => clearInterval(t);
  });
  /** 渲染用:前导补零到 LIVE_BARS,让波形从开录起就铺满整行(与详情页全宽波形一致),
      而非少量样本挤在右缘、左侧留大片空——补的零段是低平基线,新声仍从右侧进入。 */
  const liveBarsView = $derived(
    liveBars.length >= LIVE_BARS
      ? liveBars
      : [...new Array(LIVE_BARS - liveBars.length).fill(0), ...liveBars],
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
</script>

<div class="container">
  <!-- 头部整体吸顶(标题/下载卡/控制条/状态行/说话人条):录制中转写自动滚到最新,
       操作与说话人对照都不能跟着滚出视口 -->
  <div class="topbar">
    <h1>实时转写</h1>

    <!-- 单实例:compact 由 recording_ready 驱动。若拆成两个 if 分支,识别模型下完
         切小提示条时组件会销毁重建,进行中的下载进度/订阅状态全部清零。 -->
    {#if models && !(models.recording_ready && models.diarization_ready)}
      <ModelDownloadCard status={models} compact={models.recording_ready} onComplete={refreshModels} />
    {/if}

    {#if !models || models.recording_ready}
      <!-- 两端对齐:控制钮组贴左、计时+状态贴右、实时波形限宽居中(space-between 把
           富余横向空间分到两侧间隙,波形不再 flex:1 拉满整屏成一根横贯全宽的细带)。 -->
      <div class="controls">
        <!-- 左:控制钮组 -->
        <div class="ctl-group">
          {#if !recording.isLive}
            <button class="ctl primary" disabled={recording.pending} onclick={startRecording}>
              <span class="sym dot on-blue"></span>开始录制
            </button>
          {:else}
            {#if recording.paused}
              <button class="ctl" disabled={recording.pending} onclick={() => recording.unpause()}>恢复</button>
            {:else}
              <button class="ctl" disabled={recording.pending} onclick={() => recording.pause()}>暂停</button>
            {/if}
            <button class="ctl danger" disabled={recording.pending} onclick={() => recording.stop()}>
              <span class="sym square"></span>停止
            </button>
          {/if}
        </div>

        <!-- 中:实时音轨(录制中才有),限宽居中,滚动电平波形/电平表,新声从右缘进入 -->
        {#if recording.isLive}
          <div class="wave-live" class:frozen={recording.paused} title="麦克风电平" aria-hidden="true">
            {#each liveBarsView as h, i (i)}
              <span class="bar" style="height: {Math.max(6, h)}%"></span>
            {/each}
          </div>
        {/if}

        <!-- 右:计时 + 状态,同一簇(不再单挂一行);状态点是唯一动态信号。
             仅录制中出现——空闲态只需左侧「开始录制」CTA,不让一个「就绪」标签
             孤浮右缘重演失衡;空闲若开录失败,错误仍由下方红色详情行兜底。 -->
        {#if recording.isLive}
          <div class="live-meta">
            <span class="timer" class:pausedTimer={recording.paused}>{formatTs(recording.elapsedMs)}</span>
            <span class="status-inline">
              <span class="status-dot" class:live={!recording.paused}></span>{statusLabel}
            </span>
          </div>
        {/if}
      </div>

      <!-- 出错时才展开完整错误文案(可能较长);正常态收进右侧「录制中/就绪」标签,不占行 -->
      {#if isError(recording.status)}
        <p class="status error"><span class="status-dot"></span>{recording.status}</p>
      {/if}

      <!-- 说话人条随头部整体吸顶:滚到会中段落时仍要能对着条上的名字辨认发言人/改名,
           条不在视口内这个对照就断了(用户反馈)。空说话人时组件自身不渲染,不占高。 -->
      <SpeakerChips speakers={recording.speakers} noteId={recording.noteId} editable={true} />
    {/if}
  </div>

  {#if !models || models.recording_ready}

    {#if showMcpHint}
      <div class="banner">
        新功能：把会议笔记接入 Claude / Cursor 等 AI 助手（MCP）。
        <button class="link" onclick={() => dismissMcpHint(true)}>去 AI 页</button>
        <button class="link" onclick={() => dismissMcpHint(false)}>知道了</button>
      </div>
    {/if}

    {#if btEchoRisk && !recording.isLive}
      <div class="banner">
        检测到蓝牙外放 + 「保持外放音量」：蓝牙延迟会让回声消除失效，录音会混入对方声音（回放像回音）。建议改用有线外放/耳机，或到设置关闭「保持外放音量」。
      </div>
    {/if}

    {#if lowInputVol}
      <div class="banner">
        麦克风输入音量偏低（{lowInputVol.vol}%），可能录得很轻。
        <button class="link" onclick={fixInputVol}>调到 {INPUT_TARGET}%</button>
      </div>
    {/if}

    {#if !screenPerm && !recording.isLive}
      <div class="banner">
        系统声音未授权：只能录到麦克风，对方/外放的声音不会进笔记。
        <button class="link" onclick={requestScreenPerm}>立即授权</button>
        <span class="hint">系统设置里勾选 voice-notes 后切回本页即可。</span>
        {#if showPermFix}
          <div class="fixline">
            系统设置里已勾选却仍提示未授权？多半是旧版本的授权记录残留，开关是失效的。
            <button class="link" onclick={fixScreenPerm}>修复授权</button>
            <span class="hint">清除残留后重新弹出系统授权；若未弹出，退出并重新打开应用后再点「立即授权」。</span>
          </div>
        {/if}
      </div>
    {/if}

    {#if recording.isLive && recording.systemAudio !== "on" && recording.systemAudio !== ""}
      <div class="banner">
        系统声音不可用（未授权屏幕录制）。仅麦克风在录。
        <button class="link" onclick={openScreenRecordingSettings}>打开系统设置</button>
        <span class="hint">授权后重新开录生效。</span>
      </div>
    {/if}

    {#if recording.isLive && recording.diarization === "unavailable"}
      <div class="banner">说话人区分不可用（相关模型未下载）。转写与录音不受影响。</div>
    {/if}

    {#if recording.storageDegraded}
      <div class="banner">落盘异常：内容暂存内存并自动重试，请检查磁盘空间。录制不受影响。</div>
    {/if}

    <div class="transcript" class:live={recording.isLive} bind:this={transcriptEl}>
      {#each recording.finals as line, i}
        <p class="final" class:current={recording.isLive && !hasPartial && i === recording.finals.length - 1}>
          <span class="badge" style="background: {speakerColor(line.speaker, line.source, recording.speakers)}; color: {speakerInk(line.speaker, line.source, recording.speakers)}">
            {speakerLabel(line.speaker, line.source, recording.speakers)}
          </span>
          {line.text}
        </p>
      {/each}
      {#if recording.partialMic}
        <p class="partial" class:current={recording.isLive}><span class="badge mic">我</span>{recording.partialMic}</p>
      {/if}
      {#if recording.partialSystem}
        <p class="partial" class:current={recording.isLive}><span class="badge system">对方</span>{recording.partialSystem}</p>
      {/if}
      {#if recording.finals.length === 0 && !recording.partialMic && !recording.partialSystem}
        <p class="hint">（开始说话…）</p>
      {/if}
    </div>

    <!-- 跟随被用户上滑打断时的返回入口：sticky 钉在滚动视口底部，恢复跟随即消失 -->
    <div class="jump-anchor" aria-hidden={follow || !recording.isLive}>
      {#if !follow && recording.isLive}
        <button class="jump" onclick={jumpToLatest}>↓ 回到最新</button>
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
  /* 计时用等宽数字：秒数跳动时数字宽度不抖动，视觉更稳定 */
  .timer {
    font-variant-numeric: tabular-nums;
    font-weight: 500;
    font-size: 1rem;
    color: var(--ink-secondary);
  }
  .timer.pausedTimer { color: var(--ink-faint); }
  /* 实时音轨:滚动电平条,新条从右缘进入(justify-content:flex-end + overflow 裁左侧)。
     record 红呼应"录制中"是唯一常驻彩色信号;暂停冻结退 ink-faint。
     空闲时容器空置但保留 flex:1 占位,把计时推到行尾、行高不跳。 */
  .wave-live {
    /* 全宽填充:与详情页播放 transport 的 waveform-track 一致——条 flex:1 均分铺满
       控制与右侧计时之间的整行(配合前导补零,开录起就满行,不缩在右缘)。 */
    flex: 1;
    min-width: 0;
    height: 32px;
    display: flex;
    align-items: center;
    gap: 1px;
    overflow: hidden;
  }
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
</style>
