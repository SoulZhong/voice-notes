<script lang="ts">
  // 段落说话人选择弹窗(2026-08-22 批量改说话人,甲乙双模式共用):
  // 搜索 + 本篇说话人列表(色点/当前禁点)+ 新说话人;甲模式带作用范围;
  // 乙模式(勾选)从这里入口。视觉与胸牌菜单同一套语言。
  import { speakerColor, speakerInk, speakerLabel } from "$lib/notes";
  import { t } from "$lib/i18n/index.svelte";

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

<div
  bind:this={el}
  class="pop"
  style="position: fixed; left: {Math.min(rect.left, window.innerWidth - 300)}px; top: {Math.min(rect.bottom + 4, window.innerHeight - 320)}px;"
>
  {#if scopes}
    <div class="scopes">
      {#each scopes as sc (sc.key)}
        <label class="scope" class:off={sc.n === 0}>
          <input type="radio" name="segpick-scope" value={sc.key} bind:group={scope} disabled={sc.n === 0} />
          {sc.label}
        </label>
      {/each}
    </div>
  {/if}
  <input class="q" placeholder={t("notes.segpick.searchPh")} bind:value={query} />
  <div class="list">
    {#each ids as sid (sid)}
      <button
        class="row"
        disabled={sid === currentSid}
        onclick={() => onPick(sid, scope || "one")}
      >
        <span class="dot" style="background: {speakerColor(sid, 'mic', speakers)}; border-color: {speakerInk(sid, 'mic', speakers)}"></span>
        {speakerLabel(sid, "mic", speakers)}
        {#if counts[sid]}<span class="sub">{counts[sid]}</span>{/if}
        {#if sid === currentSid}<span class="sub">{t("notes.menu.current")}</span>{/if}
      </button>
    {/each}
  </div>
  <button class="row new" onclick={() => onPick("new", scope || "one")}>{t("notes.menu.newSpeaker")}</button>
  {#if showMultiEntry && onEnterMulti}
    <button class="row" onclick={onEnterMulti}>{t("notes.segpick.multiEntry")}</button>
  {/if}
  <button class="row quiet" onclick={onClose}>{t("notes.cancel")}</button>
</div>

<style>
  .pop {
    z-index: 60;
    width: 280px;
    max-height: 340px;
    overflow: auto;
    background: var(--pop-bg, #1c1e22);
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 10px;
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 4px;
    box-shadow: 0 8px 28px rgba(0, 0, 0, 0.45);
  }
  .scopes {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 2px 4px 6px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
    font-size: 12.5px;
  }
  .scope {
    display: flex;
    gap: 6px;
    align-items: center;
    cursor: pointer;
  }
  .scope.off {
    opacity: 0.4;
  }
  .q {
    background: rgba(255, 255, 255, 0.06);
    border: none;
    border-radius: 6px;
    padding: 5px 8px;
    color: inherit;
    font-size: 13px;
  }
  .list {
    display: flex;
    flex-direction: column;
    overflow: auto;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 5px 8px;
    border: none;
    background: none;
    color: inherit;
    text-align: left;
    border-radius: 6px;
    cursor: pointer;
    font-size: 13px;
  }
  .row:hover:not(:disabled) {
    background: rgba(255, 255, 255, 0.07);
  }
  .row:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .row.new {
    border-top: 1px solid rgba(255, 255, 255, 0.08);
  }
  .row.quiet {
    opacity: 0.6;
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
    opacity: 0.5;
    font-size: 11.5px;
  }
</style>
