<script lang="ts">
  // 设置页「设备听写」区。默认开但不激活:没点过「连接设备」之前后端什么都不做
  // (不扫蓝牙、不要权限、不下模型),所以这里第一次点连接时才扫描、才下载。
  import { onMount } from "svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { ask } from "@tauri-apps/plugin-dialog";
  import { t } from "$lib/i18n/index.svelte";
  import Segmented from "$lib/Segmented.svelte";
  import type { SegmentedItem } from "$lib/segmented";
  import { Button } from "$lib/ui/components/button";
  import { Switch } from "$lib/ui/components/switch";
  import * as Dialog from "$lib/ui/components/dialog";
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
            <Button variant="outline" size="sm" disabled={busy} onclick={openPicker}>{t("device.connect")}</Button>
          {:else}
            {#if help?.action === "repair"}
              <Button variant="outline" size="sm" disabled={busy} onclick={() => run(deviceRepair)}>{t("device.repair")}</Button>
            {:else if help || !status.running}
              <Button variant="outline" size="sm" disabled={busy} onclick={() => run(deviceReconnect)}>{t("device.reconnect")}</Button>
            {/if}
            <Button variant="outline" size="sm" disabled={busy} onclick={openPicker}>{t("device.connect")}</Button>
            <Button variant="ghost" size="sm" class="text-muted-foreground" disabled={busy} onclick={forget}>{t("device.forget")}</Button>
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
          <Button variant="outline" size="sm" onclick={grantAccessibility}>{t("device.perm.grant")}</Button>
        </div>
      {/if}
      {#if status.device_name && isMac && status.speech_permission !== "authorized"}
        <div class="notice notice-action">
          <span>{status.speech_permission === "not determined" ? t("device.perm.speechAsk") : t("device.perm.speech")}</span>
          <Button variant="outline" size="sm" onclick={grantSpeech}>{t("device.perm.grant")}</Button>
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
            <Button variant="outline" size="sm" onclick={startDownload}>{t("device.models.download")}</Button>
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
        <Switch
          class="flex-none"
          checked={settings?.dictation_save_text ?? true}
          disabled={!settings}
          onCheckedChange={(on) => save((s) => (s.dictation_save_text = on))}
        />
      </label>
      <label class="row">
        <div class="row-info">
          <span class="row-label">{t("device.saveAudio.label")}</span>
          <span class="row-desc">{t("device.saveAudio.desc")}</span>
        </div>
        <Switch
          class="flex-none"
          checked={settings?.dictation_save_audio ?? false}
          disabled={!settings || settings.dictation_save_text === false}
          onCheckedChange={(on) => save((s) => (s.dictation_save_audio = on))}
        />
      </label>
      <label class="row">
        <div class="row-info">
          <span class="row-label">{t("device.aiAccess.label")}</span>
          <span class="row-desc">{t("device.aiAccess.desc")}</span>
        </div>
        <Switch
          class="flex-none"
          checked={settings?.dictation_ai_access ?? false}
          disabled={!settings}
          onCheckedChange={(on) => save((s) => (s.dictation_ai_access = on))}
        />
      </label>
    {/if}
  </div>
  {#if error}<p class="err">{error}</p>{/if}
</section>

<Dialog.Root bind:open={picking}>
  <Dialog.Content
    showCloseButton={false}
    aria-labelledby="device-scan-title"
    class="max-h-[min(32rem,calc(100vh_-_2rem))] gap-0 overflow-auto p-5 sm:max-w-[26rem]"
  >
    <Dialog.Title id="device-scan-title" class="text-[1.05rem] font-[550]">{t("device.scan.title")}</Dialog.Title>
    <Dialog.Description class="mt-1.5 text-[0.84rem] leading-[1.6]">{t("device.scan.hint")}</Dialog.Description>
    {#if scanning}
      <p class="hint">{t("device.scan.scanning")}</p>
    {:else if scanError}
      <p class="err">{scanError}</p>
    {:else if found.length === 0}
      <p class="hint">{t("device.scan.empty")}</p>
    {/if}
    <ul class="picker-list">
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
          <Button size="sm" onclick={() => choose(d)}>{t("device.scan.choose")}</Button>
        </li>
      {/each}
    </ul>
    <div class="picker-actions">
      <Button variant="outline" size="sm" disabled={scanning} onclick={scan}>{t("device.scan.rescan")}</Button>
      <Button variant="outline" size="sm" onclick={() => (picking = false)}>{t("device.pin.cancel")}</Button>
    </div>
  </Dialog.Content>
</Dialog.Root>

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
  .hint { margin: 6px 0 0; color: var(--ink-secondary); font-size: 0.84rem; line-height: 1.6; }
  .picker-list { list-style: none; margin: 14px 0 0; padding: 0; display: grid; gap: 8px; }
  .picker-list li {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 8px 10px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-md);
  }
  .picker-list li .name { display: block; font-size: 0.92rem; }
  .picker-list li .meta { font-size: 0.76rem; color: var(--ink-faint); }
  .picker-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
</style>
