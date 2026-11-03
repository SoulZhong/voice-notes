<script lang="ts">
  // 设置页「皮肤」选择:三张预览卡,每张用 data-vn-theme-preview 套上对应皮肤的 token,
  // 画一个迷你界面(侧栏 + 正文行 + 链接色 + 主按钮),选中前就能看出差别。
  // 明暗跟随当前生效的 color-scheme(卡片里的 light-dark() 与整页同一支)。
  import { t } from "$lib/i18n/index.svelte";
  import { THEME_FAMILIES, type ThemeFamily } from "$lib/theme";

  let {
    value,
    disabled = false,
    onSelect,
  }: { value: ThemeFamily; disabled?: boolean; onSelect: (f: ThemeFamily) => void } = $props();

  const label: Record<ThemeFamily, () => string> = {
    classic: () => t("settings.themeFamily.classic"),
    paper: () => t("settings.themeFamily.paper"),
    nord: () => t("settings.themeFamily.nord"),
  };
</script>

<div class="picker" role="radiogroup" aria-label={t("settings.themeFamily.label")}>
  {#each THEME_FAMILIES as f (f)}
    <button
      class="card"
      class:selected={value === f}
      role="radio"
      aria-checked={value === f}
      {disabled}
      onclick={() => onSelect(f)}
    >
      <span class="mock" data-vn-theme-preview={f}>
        <span class="side"><i></i><i></i><i></i></span>
        <span class="body">
          <span class="line w80"></span>
          <span class="line w60"></span>
          <span class="link"></span>
          <span class="pill"></span>
        </span>
      </span>
      <span class="name">{label[f]()}</span>
    </button>
  {/each}
</div>

<style>
  .picker {
    display: flex;
    gap: 0.6rem;
    flex-wrap: wrap;
  }
  .card {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    align-items: center;
    padding: 0.35rem;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    background: none;
    color: var(--ink);
    cursor: pointer;
    font: inherit;
  }
  .card.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .card:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .mock {
    display: flex;
    width: 104px;
    height: 64px;
    border-radius: var(--radius-md);
    overflow: hidden;
    background: var(--canvas);
    border: 1px solid var(--hairline);
  }
  .side {
    width: 28px;
    background: var(--surface);
    border-right: 1px solid var(--hairline);
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 6px 5px;
  }
  .side i {
    height: 4px;
    border-radius: 2px;
    background: var(--hairline-strong);
  }
  .body {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 5px;
    padding: 7px 8px;
  }
  .line {
    height: 4px;
    border-radius: 2px;
    background: var(--ink);
    opacity: 0.75;
  }
  .w80 { width: 80%; }
  .w60 { width: 60%; background: var(--ink-secondary); }
  .link {
    width: 40%;
    height: 4px;
    border-radius: 2px;
    background: var(--accent);
  }
  .pill {
    margin-top: auto;
    width: 34px;
    height: 10px;
    border-radius: var(--radius-full);
    background: var(--primary);
  }
  .name {
    font-size: 0.8rem;
    color: var(--ink-secondary);
  }
  .card.selected .name {
    color: var(--ink);
  }
</style>
