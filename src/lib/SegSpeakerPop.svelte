<script lang="ts">
  // 段落说话人选择弹窗(2026-08-22 批量改说话人,甲乙双模式共用):
  // 搜索 + 本篇说话人列表(色点/当前禁点)+ 新说话人;甲模式带作用范围;
  // 乙模式(勾选)从这里入口。视觉与胸牌菜单同一套语言。
  import { speakerColor, speakerInk, speakerLabel } from "$lib/notes";
  import { t } from "$lib/i18n/index.svelte";
  import { Button } from "$lib/ui/components/button";
  import { Input } from "$lib/ui/components/input";
  import { Label } from "$lib/ui/components/label";
  import * as RadioGroup from "$lib/ui/components/radio-group";

  // 列表行:全宽、左对齐、常规字重(覆盖 Button 默认的居中/加粗/定高)。
  const rowCls = "h-auto w-full justify-start gap-2 px-2 py-[5px] text-left text-[13px] font-normal";

  let {
    speakers,
    currentSid = null,
    counts = {},
    scopes = null,
    showMultiEntry = false,
    rect,
    onPick,
    onEnterMulti,
    onClose,
  }: {
    speakers: Record<string, { name: string; sources: string[] }>;
    currentSid?: string | null;
    counts?: Record<string, number>;
    /** 甲:作用范围单选(null = 不显示,乙的「改为…」用)。 */
    scopes?: { key: string; label: string; n: number }[] | null;
    showMultiEntry?: boolean;
    rect: DOMRect;
    /** sid 可为 "new"。 */
    onPick: (sid: string, scopeKey: string) => void;
    onEnterMulti?: () => void;
    onClose: () => void;
  } = $props();

  let query = $state("");
  // 默认选中第一个 n>1 的范围(典型场景:一串连续误标);没有就仅这一段。
  let scope = $state("");
  $effect(() => {
    if (!scope) scope = scopes?.find((s) => s.n > 1)?.key ?? scopes?.[0]?.key ?? "one";
  });

  const ids = $derived(
    Object.keys(speakers)
      .filter((sid) => !query.trim() || speakerLabel(sid, "mic", speakers).includes(query.trim()))
      .sort((a, b) => (counts[b] ?? 0) - (counts[a] ?? 0)),
  );

  let el = $state<HTMLDivElement | null>(null);
  $effect(() => {
    const onDown = (e: PointerEvent) => {
      if (el && !el.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("pointerdown", onDown, true);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onDown, true);
      document.removeEventListener("keydown", onKey);
    };
  });
</script>

<!-- 定位仍由宿主给的 rect 决定(fixed 贴在触发点下方),外点/Esc 关闭也仍由本组件自管:
     宿主两处调用都按「rect + onClose」约定接线,改成 Popover 需要宿主一起换触发器。
     这里只把外观与控件换到组件库:面板用 popover 令牌,单选用 RadioGroup,行用 Button。 -->
<div
  bind:this={el}
  class="pop bg-popover text-popover-foreground shadow-md ring-1 ring-foreground/10"
  style="position: fixed; left: {Math.min(rect.left, window.innerWidth - 300)}px; top: {Math.min(rect.bottom + 4, window.innerHeight - 320)}px;"
>
  {#if scopes}
    <RadioGroup.Root bind:value={scope} class="scopes gap-1">
      {#each scopes as sc (sc.key)}
        <Label class="scope text-[12.5px] font-normal leading-normal" data-off={sc.n === 0 || undefined}>
          <RadioGroup.Item value={sc.key} disabled={sc.n === 0} />
          {sc.label}
        </Label>
      {/each}
    </RadioGroup.Root>
  {/if}
  <Input class="h-7 text-[13px] md:text-[13px]" placeholder={t("notes.segpick.searchPh")} bind:value={query} />
  <div class="list">
    {#each ids as sid (sid)}
      <Button
        variant="ghost"
        class={rowCls}
        disabled={sid === currentSid}
        onclick={() => onPick(sid, scope || "one")}
      >
        <span class="dot" style="background: {speakerColor(sid, 'mic', speakers)}; border-color: {speakerInk(sid, 'mic', speakers)}"></span>
        {speakerLabel(sid, "mic", speakers)}
        {#if counts[sid]}<span class="sub">{counts[sid]}</span>{/if}
        {#if sid === currentSid}<span class="sub">{t("notes.menu.current")}</span>{/if}
      </Button>
    {/each}
  </div>
  <div class="divider"></div>
  <Button variant="ghost" class={rowCls} onclick={() => onPick("new", scope || "one")}>{t("notes.menu.newSpeaker")}</Button>
  {#if showMultiEntry && onEnterMulti}
    <Button variant="ghost" class={rowCls} onclick={onEnterMulti}>{t("notes.segpick.multiEntry")}</Button>
  {/if}
  <Button variant="ghost" class="{rowCls} text-muted-foreground" onclick={onClose}>{t("notes.cancel")}</Button>
</div>

<style>
  .pop {
    z-index: 60;
    width: 280px;
    max-height: 340px;
    overflow: auto;
    border-radius: var(--radius-lg);
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  /* 作用范围单选组与列表之间的发丝线 */
  .pop :global(.scopes) {
    padding: 2px 4px 6px;
    border-bottom: 1px solid var(--hairline);
  }
  .pop :global(.scope[data-off]) {
    opacity: 0.4;
  }
  .list {
    display: flex;
    flex-direction: column;
    overflow: auto;
  }
  .divider {
    height: 1px;
    background: var(--hairline);
    margin: 0 2px;
  }
  .dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    border: 1px solid transparent;
    flex: none;
  }
  .sub {
    margin-left: auto;
    color: var(--ink-faint);
    font-size: 11.5px;
  }
</style>
