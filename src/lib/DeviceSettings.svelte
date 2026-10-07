<script lang="ts">
  // 设置页「设备听写」区。默认开但不激活:没点过「连接设备」之前后端什么都不做
  // (不扫蓝牙、不要权限、不下模型),所以这里第一次点连接时才扫描、才下载。
  import { onMount } from "svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { ask } from "@tauri-apps/plugin-dialog";
  import { t } from "$lib/i18n/index.svelte";
  import Segmented from "$lib/Segmented.svelte";
  import type { SegmentedItem } from "$lib/segmented";
  import { downloadModels, modelsStatus, onModelDownload, type Settings } from "$lib/models";
  import {
    deviceConnect,
    deviceForget,
    deviceReconnect,
    deviceRepair,
    deviceScan,
    deviceGrantSpeech,
    deviceOpenAccessibility,
    deviceStatus,
    onDeviceState,
    type DeviceStatus,
    type FoundDevice,
  } from "$lib/device";
  import { endedHelp, linkTextKey, percent } from "$lib/deviceView";

  let {
    settings,
    save,
  }: {
    settings: Settings | null;
    save: (mut: (s: Settings) => void) => Promise<void>;
  } = $props();

  let status = $state<DeviceStatus | null>(null);
  let error = $state("");
  let busy = $state(false);

  // 连接对话框
  const AI_PASSPORT_URL = "https://ai-passport.folotoy.cn/";
  const VIBEVOICE_URL = "https://ai-passport.folotoy.cn/plays/965/?v=2126-2";

  let picking = $state(false);
  let scanning = $state(false);
  let found = $state<FoundDevice[]>([]);
  let scanError = $state("");

  // 模型下载(只下听写缺的那几件)
  let missingMb = $state(0);
  let downloading = $state<Record<string, number>>({});
  const downloadPct = $derived.by(() => {
    const v = Object.values(downloading);
    return v.length ? Math.floor(v.reduce((a, b) => a + b, 0) / v.length) : 0;
  });
  let downloadError = $state("");

  const isMac = $derived(status?.platform === "macos");
  const engineItems = $derived<SegmentedItem[]>([
    { id: "auto", label: t("device.engine.auto") },
    { id: "apple", label: t("device.engine.apple") },
    { id: "sherpa", label: t("device.engine.sherpa") },
  ]);
  const linkText = $derived.by(() => {
    if (!status) return "";
    const k = linkTextKey(status.link, status.platform);
    return t(k.key, k.params);
  });
  const connectedName = $derived(status?.link.state?.kind === "connected" ? status.link.state.name : null);
  const help = $derived(
    status ? endedHelp(status.link.ended, status.platform, status.device_name ?? "") : null,
  );

  async function refresh() {
    try {
      status = await deviceStatus();
      await refreshMissingSize();
    } catch (e) {
      error = t("common.loadFailed", { e });
    }
  }

  async function refreshMissingSize() {
    if (!status || status.missing_models.length === 0) {
      missingMb = 0;
      return;
    }
    const m = await modelsStatus().catch(() => null);
    missingMb = (m?.artifacts ?? [])
      .filter((a) => status!.missing_models.includes(a.id))
      .reduce((sum, a) => sum + a.approx_mb, 0);
  }

  onMount(() => {
    refresh();
    const subs = [
      onDeviceState((link) => {
        if (status) status = { ...status, link };
      }),
      onModelDownload((e) => {
        if (!(e.artifact in downloading) && !status?.missing_models.includes(e.artifact)) return;
        if (e.phase === "done") {
          const { [e.artifact]: _, ...rest } = downloading;
          downloading = rest;
          refresh();
        } else if (e.phase === "error" || e.phase === "cancelled") {
          const { [e.artifact]: _, ...rest } = downloading;
          downloading = rest;
          if (e.phase === "error") downloadError = t("device.models.failed", { e: e.message });
        } else {
          downloading = { ...downloading, [e.artifact]: percent(e.received_bytes, e.total_bytes) };
        }
      }),
    ];
    return () => subs.forEach((p) => p.then((u) => u()));
  });

  // 辅助功能授权:点「去授权」后系统设置在前台,用户拨完开关回来时不会有任何事件,
  // 所以缺权限期间低频轮询授权状态,拿到即收起提示。
  let axWaiting = $state(false);
  async function grantAccessibility() {
    try {
      if (await deviceOpenAccessibility()) await refresh();
      else axWaiting = true;
    } catch (e) {
      error = t("device.actionFailed", { e });
    }
  }
  // 只依赖这个布尔量:status 整体会随链路事件频繁替换,不能让轮询跟着反复重建。
  const needAx = $derived(!!status?.device_name && status.platform === "macos" && !status.accessibility);
  async function grantSpeech() {
    try {
      const s = await deviceGrantSpeech();
      if (status) status = { ...status, speech_permission: s };
      await refresh();
    } catch (e) {
      error = t("device.actionFailed", { e });
    }
  }
  $effect(() => {
    if (!needAx) {
      axWaiting = false;
      return;
    }
    const timer = setInterval(async () => {
      const s = await deviceStatus().catch(() => null);
      if (s?.accessibility && status) status = { ...status, accessibility: true };
    }, 2000);
    return () => clearInterval(timer);
  });

  async function run(f: () => Promise<DeviceStatus>) {
    busy = true;
    error = "";
    try {
      status = await f();
      await refreshMissingSize();
    } catch (e) {
      error = t("device.actionFailed", { e });
    } finally {
      busy = false;
    }
  }

  async function startDownload() {
    if (!status) return;
    downloadError = "";
    downloading = Object.fromEntries(status.missing_models.map((id) => [id, 0]));
    try {
      await downloadModels(status.missing_models);
    } catch (e) {
      downloading = {};
      downloadError = t("device.models.failed", { e });
    }
  }

  async function scan() {
    scanning = true;
    scanError = "";
    found = [];
    try {
      found = await deviceScan();
    } catch (e) {
      scanError = t("device.scan.failed", { e });
    } finally {
      scanning = false;
    }
  }

  function openPicker() {
    picking = true;
    scan();
  }

  async function choose(d: FoundDevice) {
    picking = false;
    await run(() => deviceConnect(d.name));
    // Windows 第一次连接时把听写要的模型一并下好(还缺才下);macOS 缺模型只发生在
    // 用户拒绝了语音识别权限时,体积大,留给用户自己点下载。
    if (status && status.platform === "windows" && status.missing_models.length > 0) startDownload();
  }

  async function forget() {
    const name = status?.device_name ?? "";
    const yes = await ask(t("device.forgetConfirm.message", { name }), {
      title: t("device.forgetConfirm.title"),
      kind: "warning",
      okLabel: t("device.forgetConfirm.ok"),
      cancelLabel: t("device.pin.cancel"),
    });
    if (yes) await run(deviceForget);
  }

  async function toggleEnabled(on: boolean) {
    await save((s) => (s.device_enabled = on));
    await refresh();
  }
</script>

<section>
  <h2 class="section-title">{t("device.section")}</h2>
  <!-- 多数人没听过 AI Passport / VibeVoice:两个名字都给链接,点开在浏览器里看 -->
  <p class="intro">
    {t("device.desc.before")}<button class="link" onclick={() => openUrl(AI_PASSPORT_URL)}>AI Passport</button>{t("device.desc.mid")}<button
      class="link"
      onclick={() => openUrl(VIBEVOICE_URL)}>VibeVoice</button
    >{t("device.desc.after")}
  </p>
  <div class="rows">
    <label class="row">
      <div class="row-info">
        <span class="row-label">{t("device.enabled.label")}</span>
        <span class="row-desc">{t("device.enabled.desc")}</span>
      </div>
      <input
        type="checkbox"
        class="ctl switch"
        checked={settings?.device_enabled ?? true}
        disabled={!settings}
        onchange={(e) => toggleEnabled((e.target as HTMLInputElement).checked)}
      />
    </label>

    {#if settings?.device_enabled !== false && status}
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("device.current")}</span>
          {#if status.device_name}
            <span class="row-desc device-line">
              <span class="name">{status.sim ? t("device.simName") : status.device_name}</span>
              {#if status.sim}<span class="sim-tag">{t("device.simTag")}</span>{/if}
              <span class="dot" class:on={status.link.state?.kind === "connected"}></span>
              {linkText}
              {#if status.link.firmware}<span class="fw">· {t("device.firmware", { fw: status.link.firmware })}</span>{/if}
            </span>
          {:else}
            <span class="row-desc">{t("device.none")}</span>
          {/if}
        </div>
        <div class="actions">
          {#if !status.device_name}
            <button class="btn" disabled={busy} onclick={openPicker}>{t("device.connect")}</button>
          {:else}
            {#if help?.action === "repair"}
              <button class="btn" disabled={busy} onclick={() => run(deviceRepair)}>{t("device.repair")}</button>
            {:else if help || !status.running}
              <button class="btn" disabled={busy} onclick={() => run(deviceReconnect)}>{t("device.reconnect")}</button>
            {/if}
            <button class="btn" disabled={busy} onclick={openPicker}>{t("device.connect")}</button>
            <button class="btn quiet" disabled={busy} onclick={forget}>{t("device.forget")}</button>
          {/if}
        </div>
      </div>

      {#if help}
        <div class="notice">{t(help.text.key, help.text.params)}</div>
      {/if}
      {#if status.link.mismatch === "device_older"}
        <div class="notice">{t("device.mismatch.deviceOlder")}</div>
      {:else if status.link.mismatch === "device_newer"}
        <div class="notice">{t("device.mismatch.deviceNewer")}</div>
      {/if}
      {#if status.device_name && isMac && !status.accessibility}
        <div class="notice notice-action">
          <span>{axWaiting ? t("device.perm.waiting") : t("device.perm.accessibility")}</span>
          <button class="btn" onclick={grantAccessibility}>{t("device.perm.grant")}</button>
        </div>
      {/if}
      {#if status.device_name && isMac && status.speech_permission !== "authorized"}
        <div class="notice notice-action">
          <span>{status.speech_permission === "not determined" ? t("device.perm.speechAsk") : t("device.perm.speech")}</span>
          <button class="btn" onclick={grantSpeech}>{t("device.perm.grant")}</button>
        </div>
      {/if}
      {#if status.device_name && status.missing_models.length > 0}
        <div class="row">
          <div class="row-info">
            <span class="row-desc">
              {#if Object.keys(downloading).length > 0}
                {t("device.models.downloading", { pct: downloadPct })}
              {:else}
                {t("device.models.missing", { mb: missingMb })}
              {/if}
            </span>
            {#if downloadError}<span class="row-desc err">{downloadError}</span>{/if}
          </div>
          {#if Object.keys(downloading).length === 0}
            <button class="btn" onclick={startDownload}>{t("device.models.download")}</button>
          {/if}
        </div>
      {/if}

      {#if isMac}
        <div class="row">
          <div class="row-info">
            <span class="row-label">{t("device.engine.label")}</span>
            <span class="row-desc">{t("device.engine.descMac")}</span>
          </div>
          <Segmented
            items={engineItems}
            value={settings?.dictation_engine ?? "auto"}
            onSelect={async (id) => {
              await save((s) => (s.dictation_engine = id));
              await refresh();
            }}
          />
        </div>
      {:else}
        <div class="row">
          <div class="row-info">
            <span class="row-label">{t("device.engine.label")}</span>
            <span class="row-desc">{t("device.engine.descOther")}</span>
          </div>
        </div>
      {/if}

      <label class="row">
        <div class="row-info">
          <span class="row-label">{t("device.saveText.label")}</span>
          <span class="row-desc">{t("device.saveText.desc")}</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          checked={settings?.dictation_save_text ?? true}
          disabled={!settings}
          onchange={(e) => save((s) => (s.dictation_save_text = (e.target as HTMLInputElement).checked))}
        />
      </label>
      <label class="row">
        <div class="row-info">
          <span class="row-label">{t("device.saveAudio.label")}</span>
          <span class="row-desc">{t("device.saveAudio.desc")}</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          checked={settings?.dictation_save_audio ?? false}
          disabled={!settings || settings.dictation_save_text === false}
          onchange={(e) => save((s) => (s.dictation_save_audio = (e.target as HTMLInputElement).checked))}
        />
      </label>
      <label class="row">
        <div class="row-info">
          <span class="row-label">{t("device.aiAccess.label")}</span>
          <span class="row-desc">{t("device.aiAccess.desc")}</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          checked={settings?.dictation_ai_access ?? false}
          disabled={!settings}
          onchange={(e) => save((s) => (s.dictation_ai_access = (e.target as HTMLInputElement).checked))}
        />
      </label>
    {/if}
  </div>
  {#if error}<p class="err">{error}</p>{/if}
</section>

{#if picking}
  <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
  <div class="scrim" onclick={() => (picking = false)}></div>
  <div class="picker" role="dialog" aria-labelledby="device-scan-title">
    <h3 id="device-scan-title">{t("device.scan.title")}</h3>
    <p class="hint">{t("device.scan.hint")}</p>
    {#if scanning}
      <p class="hint">{t("device.scan.scanning")}</p>
    {:else if scanError}
      <p class="err">{scanError}</p>
    {:else if found.length === 0}
      <p class="hint">{t("device.scan.empty")}</p>
    {/if}
    <ul>
      <!-- 连着的设备不再广播,搜索里永远找不到它:单列一行,免得以为它不见了 -->
      {#if connectedName}
        <li>
          <div>
            <span class="name">{connectedName}</span>
            <span class="meta">{t("device.scan.current")}</span>
          </div>
        </li>
      {/if}
      {#each found.filter((d) => d.name !== connectedName) as d (d.name)}
        <li>
          <div>
            <span class="name">{d.name}</span>
            <span class="meta">
              {#if d.rssi !== null}{t("device.scan.signal", { rssi: d.rssi })}{/if}
              {#if d.paired}· {t("device.scan.paired")}{/if}
            </span>
          </div>
          <button class="btn primary" onclick={() => choose(d)}>{t("device.scan.choose")}</button>
        </li>
      {/each}
    </ul>
    <div class="picker-actions">
      <button class="btn" disabled={scanning} onclick={scan}>{t("device.scan.rescan")}</button>
      <button class="btn" onclick={() => (picking = false)}>{t("device.pin.cancel")}</button>
    </div>
  </div>
{/if}

<svelte:window
  onkeydown={(e) => {
    if (e.key === "Escape" && picking) picking = false;
  }}
/>

<style>
  section { margin-top: 1.3rem; }
  .section-title { font-size: 0.82rem; font-weight: 500; color: var(--ink-secondary); margin: 0 0 0.45rem; }
  .link {
    background: none;
    border: none;
    padding: 0;
    font: inherit;
    color: var(--accent);
    cursor: pointer;
    text-decoration: underline;
    text-underline-offset: 2px;
  }
  .sim-tag {
    font-size: 0.72rem;
    padding: 0 0.35rem;
    border-radius: 4px;
    background: var(--warning-tint);
    color: var(--warning-ink);
  }
  .intro { margin: 0 0 0.5rem; font-size: 0.8rem; color: var(--ink-faint); line-height: 1.5; }
  .rows { background: var(--surface); border-radius: var(--radius-lg); overflow: hidden; }
  .row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.55rem 0.9rem;
    padding: 0.55rem 1rem;
    border-bottom: 1px solid var(--hairline);
  }
  .rows > :last-child { border-bottom: none; }
  label.row { cursor: pointer; }
  .row-info { flex: 1; min-width: min(16rem, 100%); display: flex; flex-direction: column; gap: 0.1rem; }
  .row-label { font-size: 0.92rem; color: var(--ink); }
  .row-desc { font-size: 0.8rem; color: var(--ink-secondary); line-height: 1.4; }
  .ctl { flex: none; margin: 0; }
  .device-line { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
  .device-line .name { color: var(--ink); font-variant-numeric: tabular-nums; }
  .fw { color: var(--ink-faint); }
  .dot { width: 7px; height: 7px; border-radius: 50%; background: var(--ink-faint); flex: none; }
  .dot.on { background: var(--success); }
  .actions { display: flex; gap: 6px; flex-wrap: wrap; }
  .notice {
    padding: 0.55rem 1rem;
    border-bottom: 1px solid var(--hairline);
    background: var(--warning-tint);
    color: var(--warning-ink);
    font-size: 0.8rem;
    line-height: 1.5;
  }
  .notice-action {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  .err { color: var(--danger); font-size: 0.8rem; }
  .btn {
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
  .btn:hover { background: var(--surface-soft); }
  .btn:disabled { opacity: 0.5; cursor: default; background: transparent; }
  .btn.quiet { border-color: transparent; color: var(--ink-secondary); }
  .btn.primary { border-color: transparent; background: var(--primary); color: var(--on-primary); box-shadow: var(--shadow-btn); }
  .btn.primary:hover { background: var(--primary-pressed); }
  .scrim { position: fixed; inset: 0; background: light-dark(rgba(20, 21, 22, 0.28), rgba(0, 0, 0, 0.64)); z-index: 50; }
  .picker {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    width: min(26rem, calc(100vw - 2rem));
    max-height: min(32rem, calc(100vh - 2rem));
    overflow: auto;
    padding: 20px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-xl);
    background: var(--surface);
    color: var(--ink);
    box-shadow: var(--shadow-popover);
    z-index: 51;
  }
  .picker h3 { margin: 0; font-size: 1.05rem; font-weight: 550; }
  .hint { margin: 6px 0 0; color: var(--ink-secondary); font-size: 0.84rem; line-height: 1.6; }
  .picker ul { list-style: none; margin: 14px 0 0; padding: 0; display: grid; gap: 8px; }
  .picker li {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 8px 10px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-md);
  }
  .picker li .name { display: block; font-size: 0.92rem; }
  .picker li .meta { font-size: 0.76rem; color: var(--ink-faint); }
  .picker-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
  @media (pointer: coarse) { .btn { min-height: 44px; } }
</style>
