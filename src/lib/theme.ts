// 外观的唯一实现:皮肤(theme family)× 明暗(color mode)。UI 改版 #730。
//
// - 明暗:app.css 的 token 全部走 light-dark(),由根元素 color-scheme 决定取哪一支。
//   "system" 清空内联 color-scheme 交回浏览器按 prefers-color-scheme 解析。
// - 皮肤:根元素 data-vn-theme 选一组 token 覆盖(classic = app.css 的 :root 默认值)。
// - data-vn-mode:**解析后的**明暗(light/dark),给 Tailwind 的 dark: 变体用
//   (见 src/lib/ui/tailwind.css)。跟随系统时随系统切换实时更新——只有 system 模式监听,
//   手动选了明暗就不被系统覆盖。
// - 首帧:同一份外观缓存进 localStorage,app.html 里的内联脚本在首次绘制前读它,
//   避免启动先闪一下默认色再切过去。后端 settings 仍是真值,读到后对账覆盖。
//
// 设置页、layout、浮窗都只调这里,不允许出现第二份「切主题」逻辑。

export const THEME_FAMILIES = ["classic", "paper", "nord"] as const;
export type ThemeFamily = (typeof THEME_FAMILIES)[number];
export const COLOR_MODES = ["system", "light", "dark"] as const;
export type ColorMode = (typeof COLOR_MODES)[number];

/** app.html 首帧脚本读的同一个键;两处须一致。 */
export const APPEARANCE_CACHE_KEY = "vn.appearance";

/** 未知值一律回退:皮肤 → classic,明暗 → system(旧版/手改坏的配置不至于白屏)。 */
export function normalizeFamily(v: unknown): ThemeFamily {
  return (THEME_FAMILIES as readonly string[]).includes(v as string) ? (v as ThemeFamily) : "classic";
}
export function normalizeMode(v: unknown): ColorMode {
  return (COLOR_MODES as readonly string[]).includes(v as string) ? (v as ColorMode) : "system";
}

/** 明暗解析:system 看系统偏好。纯函数便于测。 */
export function resolveMode(mode: ColorMode, systemDark: boolean): "light" | "dark" {
  return mode === "system" ? (systemDark ? "dark" : "light") : mode;
}

let current: { family: ThemeFamily; mode: ColorMode } = { family: "classic", mode: "system" };
let mql: MediaQueryList | null = null;

function systemDark(): boolean {
  return typeof matchMedia === "function" && matchMedia("(prefers-color-scheme: dark)").matches;
}

function paint() {
  const root = document.documentElement;
  root.style.colorScheme = current.mode === "system" ? "" : current.mode;
  root.dataset.vnTheme = current.family;
  root.dataset.vnMode = resolveMode(current.mode, systemDark());
}

/** 应用外观。省略 family 时保持当前皮肤(兼容只改明暗的旧调用点)。 */
export function applyTheme(mode: string, family?: string) {
  current = { mode: normalizeMode(mode), family: family === undefined ? current.family : normalizeFamily(family) };
  paint();
  if (!mql && typeof matchMedia === "function") {
    mql = matchMedia("(prefers-color-scheme: dark)");
    mql.addEventListener("change", () => {
      if (current.mode === "system") paint();
    });
  }
  try {
    localStorage.setItem(APPEARANCE_CACHE_KEY, JSON.stringify(current));
  } catch {
    /* 存不了只影响下次首帧,不影响本次 */
  }
}

/** 当前生效的外观(设置页回显用)。 */
export function currentAppearance() {
  return { ...current };
}
