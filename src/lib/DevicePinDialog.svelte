<script lang="ts">
  // Windows 上的设备配对码输入框。挂在 layout:配对请求随时可能来(启动自动连、
  // 设置页点连接),用户在哪一页都得看得到。macOS 由系统自己弹框,不会触发这里。
  //
  // 后端 request_pin 阻塞等回答(最多 2 分钟),超时或连接被取消时发 device_pin_closed,
  // 这边据此收起,不留一个已经没人在等的框。
  import { onMount } from "svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { deviceSubmitPin, normalizePin, onPinClosed, onPinRequest } from "$lib/device";

  let dialog = $state<HTMLDialogElement | null>(null);
  let input = $state<HTMLInputElement | null>(null);
  let deviceName = $state<string | null>(null);
  let pin = $state("");
  let error = $state("");

  const ready = $derived(pin.length === 6);

  $effect(() => {
    if (!dialog) return;
    if (deviceName && !dialog.open) {
      dialog.showModal();
      queueMicrotask(() => input?.focus());
    } else if (!deviceName && dialog.open) {
      dialog.close();
    }
  });

  onMount(() => {
    const subs = [
      onPinRequest((name) => {
        pin = "";
        error = "";
        deviceName = name;
      }),
      onPinClosed(() => (deviceName = null)),
    ];
    return () => subs.forEach((p) => p.then((u) => u()));
  });

  async function answer(value: string | null) {
    try {
      await deviceSubmitPin(value);
      deviceName = null;
    } catch (e) {
      error = String(e);
    }
  }
</script>

<dialog
  bind:this={dialog}
  class="pin-dialog"
  aria-labelledby="device-pin-title"
  oncancel={(e) => {
    e.preventDefault();
    answer(null);
  }}
>
  <h2 id="device-pin-title">{t("device.pin.title")}</h2>
  <p>{t("device.pin.body", { name: deviceName ?? "" })}</p>
  <form
    onsubmit={(e) => {
      e.preventDefault();
      if (ready) answer(pin);
    }}
  >
    <input
      bind:this={input}
      class="pin"
      inputmode="numeric"
      autocomplete="one-time-code"
      maxlength="6"
      placeholder={t("device.pin.placeholder")}
      value={pin}
      oninput={(e) => (pin = normalizePin((e.target as HTMLInputElement).value))}
    />
    {#if error}<p class="error">{error}</p>{/if}
    <div class="actions">
      <button type="button" onclick={() => answer(null)}>{t("device.pin.cancel")}</button>
      <button class="primary" type="submit" disabled={!ready}>{t("device.pin.ok")}</button>
    </div>
  </form>
</dialog>

<style>
  .pin-dialog {
    width: min(24rem, calc(100vw - 2rem));
    padding: 20px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-xl);
    background: var(--surface);
    color: var(--ink);
    box-shadow: var(--shadow-popover);
  }
  .pin-dialog::backdrop { background: light-dark(rgba(20, 21, 22, 0.28), rgba(0, 0, 0, 0.64)); }
  h2 { margin: 0; font-size: 1.05rem; font-weight: 550; }
  p { margin: 6px 0 0; color: var(--ink-secondary); font-size: 0.84rem; line-height: 1.6; }
  .pin {
    display: block;
    width: 100%;
    margin-top: 16px;
    padding: 10px 12px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-md);
    background: var(--canvas);
    color: var(--ink);
    font-size: 1.5rem;
    font-variant-numeric: tabular-nums;
    letter-spacing: 0.4em;
    text-align: center;
  }
  .pin:focus { outline: 2px solid var(--accent); outline-offset: 1px; }
  .error { color: var(--danger); }
  .actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 18px; }
  button { padding: 7px 14px; border: 1px solid var(--hairline); border-radius: var(--radius-full); background: var(--surface); color: var(--ink); font-size: 0.86rem; cursor: pointer; }
  button:hover { background: var(--surface-soft); }
  button:disabled { opacity: 0.5; cursor: default; }
  .primary { border-color: var(--primary); background: var(--primary); color: var(--on-primary); box-shadow: var(--shadow-btn); }
  .primary:hover:not(:disabled) { background: var(--primary-pressed); }
  @media (pointer: coarse) { button { min-height: 44px; } }
</style>
