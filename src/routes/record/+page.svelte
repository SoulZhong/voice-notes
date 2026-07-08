<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { recording } from "$lib/recording.svelte";
  import { speakerLabel, speakerColor, speakerInk } from "$lib/notes";
  import SpeakerChips from "$lib/SpeakerChips.svelte";
  import { modelsStatus, getSettings, type ModelsStatus } from "$lib/models";
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
    await refreshScreenPerm();
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

  onMount(() => {
    refreshModels();
    refreshScreenPerm();
    refreshBtRisk();
    // 用户去系统设置勾选/换音频设备后切回来,焦点事件驱动横幅刷新,无需重启页面。
    const onFocus = () => {
      refreshScreenPerm();
      refreshBtRisk();
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  });

  function isError(s: string) {
    return s.startsWith("error:");
  }
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
  <!-- 头部整体吸顶(标题/下载卡/控制条/状态行):录制中转写自动滚到最新,操作不能跟着滚出视口 -->
  <div class="topbar">
    <h1>实时转写</h1>

    <!-- 单实例:compact 由 recording_ready 驱动。若拆成两个 if 分支,识别模型下完
         切小提示条时组件会销毁重建,进行中的下载进度/订阅状态全部清零。 -->
    {#if models && !(models.recording_ready && models.diarization_ready)}
      <ModelDownloadCard status={models} compact={models.recording_ready} onComplete={refreshModels} />
    {/if}

    {#if !models || models.recording_ready}
      <div class="controls">
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
        <!-- 实时音轨:滚动电平波形,兼任电平表(录音机式,新声从右缘进入) -->
        <div class="wave-live" class:frozen={recording.paused} title="麦克风电平" aria-hidden="true">
          {#each liveBars as h, i (i)}
            <span class="bar" style="height: {Math.max(6, h)}%"></span>
          {/each}
        </div>
        {#if recording.paused}<span class="paused-tag">已暂停</span>{/if}
        <span class="timer" class:pausedTimer={recording.paused}>{formatTs(recording.elapsedMs)}</span>
      </div>

      <p class="status" class:error={isError(recording.status)}>
        <span class="status-dot" class:live={recording.isLive && !recording.paused}></span>{recording.status}
      </p>
    {/if}
  </div>

  {#if !models || models.recording_ready}

    {#if btEchoRisk && !recording.isLive}
      <div class="banner">
        检测到蓝牙外放 + 「保持外放音量」：蓝牙延迟会让回声消除失效，录音会混入对方声音（回放像回音）。建议改用有线外放/耳机，或到设置关闭「保持外放音量」。
      </div>
    {/if}

    {#if !screenPerm && !recording.isLive}
      <div class="banner">
        系统声音未授权：只能录到麦克风，对方/外放的声音不会进笔记。
        <button class="link" onclick={requestScreenPerm}>立即授权</button>
        <span class="hint">系统设置里勾选 voice-notes 后切回本页即可。</span>
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

    <SpeakerChips speakers={recording.speakers} noteId={recording.noteId} editable={true} />

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
  .controls {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    margin: 0 0 0.75rem;
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
    flex: 1;
    min-width: 0;
    height: 32px;
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 2px;
    overflow: hidden;
  }
  .wave-live .bar {
    width: 2px;
    flex: none;
    min-height: 2px;
    border-radius: var(--radius-full);
    background: var(--record);
  }
  .wave-live.frozen .bar {
    background: var(--ink-faint);
  }
  .paused-tag {
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    color: var(--warning-ink);
    font-size: 0.75em;
    font-weight: 500;
    border-radius: var(--radius-md);
    padding: 0.1em 0.5em;
  }

  /* 状态行降为 caption 级:辅助信息不与正文争夺注意力;状态点是唯一动态信号 */
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
</style>
