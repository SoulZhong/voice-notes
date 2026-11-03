<script lang="ts">
  // Windows 上的设备配对码输入框。挂在 layout:配对请求随时可能来(启动自动连、
  // 设置页点连接),用户在哪一页都得看得到。macOS 由系统自己弹框,不会触发这里。
  //
  // 后端 request_pin 阻塞等回答(最多 2 分钟),超时或连接被取消时发 device_pin_closed,
  // 这边据此收起,不留一个已经没人在等的框。
  import { onMount } from "svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { deviceSubmitPin, normalizePin, onPinClosed, onPinRequest } from "$lib/device";
  import * as AlertDialog from "$lib/ui/components/alert-dialog";
  import { Button } from "$lib/ui/components/button";
  import { Input } from "$lib/ui/components/input";

  let input = $state<HTMLInputElement | null>(null);
  let deviceName = $state<string | null>(null);
  let pin = $state("");
  let error = $state("");

  const ready = $derived(pin.length === 6);
  const open = $derived(deviceName !== null);

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

<!-- UI 改版:AlertDialog 外壳(模态、点遮罩不关)。打开/关闭由 deviceName 派生;组件自己发起
     的关闭只有 Esc,等同「取消」——经 bind 的 setter 落到 answer(null),与原生 oncancel 一致。
     打开即聚焦配对码输入框;Enter 走 form submit(凑满 6 位才提交)。 -->
<AlertDialog.Root
  bind:open={
    () => open,
    (v) => {
      if (!v && open) answer(null);
    }
  }
>
  <AlertDialog.Content
    class="gap-0 rounded-xl border border-border bg-card p-5 text-foreground shadow-(--shadow-popover) ring-0 data-[size=default]:max-w-[min(24rem,calc(100vw-2rem))] data-[size=default]:sm:max-w-[min(24rem,calc(100vw-2rem))]"
    onOpenAutoFocus={(e) => {
      e.preventDefault();
      input?.focus();
    }}
  >
    <AlertDialog.Title class="m-0 text-[1.05rem] font-[550]">{t("device.pin.title")}</AlertDialog.Title>
    <AlertDialog.Description class="mt-1.5 text-[0.84rem] leading-relaxed text-muted-foreground">
      {t("device.pin.body", { name: deviceName ?? "" })}
    </AlertDialog.Description>
    <form
      onsubmit={(e) => {
        e.preventDefault();
        if (ready) answer(pin);
      }}
    >
      <Input
        bind:ref={input}
        class="mt-4 h-auto bg-background px-3 py-2.5 text-center text-2xl tracking-[0.4em] tabular-nums md:text-2xl dark:bg-background"
        inputmode="numeric"
        autocomplete="one-time-code"
        maxlength={6}
        placeholder={t("device.pin.placeholder")}
        value={pin}
        oninput={(e) => (pin = normalizePin((e.target as HTMLInputElement).value))}
      />
      {#if error}<p class="error">{error}</p>{/if}
      <div class="actions">
        <Button variant="outline" class="rounded-full px-3.5" onclick={() => answer(null)}>{t("device.pin.cancel")}</Button>
        <Button
          type="submit"
          class="rounded-full px-3.5 shadow-(--shadow-btn) hover:bg-(color:--primary-pressed)"
          disabled={!ready}>{t("device.pin.ok")}</Button
        >
      </div>
    </form>
  </AlertDialog.Content>
</AlertDialog.Root>

<style>
  .error { margin: 6px 0 0; color: var(--danger); font-size: 0.84rem; line-height: 1.6; }
  .actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 18px; }
  @media (pointer: coarse) { .actions :global(button) { min-height: 44px; } }
</style>
