// UI 多语言唯一实现(spec 2026-08-02-i18n-design)。与 theme.ts 同姿态:设置页切换、
// layout 启动初始化都只走这里,不允许出现第二份"切语言"逻辑。
// 字典按领域分片(dict/*),各分片键带域前缀互不重叠;t() 内读 $state locale,
// 模板表达式随语言切换自动重渲,无需刷新页面。
import type { Dict, Msg } from "./types";
import * as common from "./dict/common";
import * as shell from "./dict/shell";
import * as settings from "./dict/settings";
import * as notes from "./dict/notes";
import * as record from "./dict/record";
import * as graph from "./dict/graph";
import * as governance from "./dict/governance";
import * as speakers from "./dict/speakers";
import * as ai from "./dict/ai";
import * as hooks from "./dict/hooks";

export type Locale = "zh" | "en";

// 分片清单。测试哨兵(i18n.test.ts)依赖此导出检查分片间无重键。
export const shards = { common, shell, settings, notes, record, graph, governance, speakers, ai, hooks } as const;

const zhAll: Dict = Object.assign({}, ...Object.values(shards).map((s) => s.zh));
const enAll: Dict = Object.assign({}, ...Object.values(shards).map((s) => s.en));
const dicts: Record<Locale, Dict> = { zh: zhAll, en: enAll };

/** "system" 按系统语言解析;zh 开头(zh/zh-CN/zh-Hans…)→中文,其余→英文。 */
export function resolveLocale(choice: string): Locale {
  if (choice === "zh" || choice === "en") return choice;
  const sys = typeof navigator !== "undefined" ? (navigator.language ?? "") : "";
  return sys.toLowerCase().startsWith("zh") ? "zh" : "en";
}

class I18n {
  /** 当前生效语言(解析后)。默认 zh:字典以中文为源语言,init 前的极短窗口与现状一致。 */
  locale: Locale = $state("zh");
  /** 设置原值 "system" | "zh" | "en"(设置页回显用)。 */
  choice: string = $state("system");

  /** 启动(layout onMount)与设置页切换共用的唯一入口。 */
  setChoice(choice: string) {
    this.choice = choice;
    this.locale = resolveLocale(choice);
    if (typeof document !== "undefined") {
      // 影响拼写检查/无障碍朗读/系统菜单等浏览器行为,跟着 UI 语言走。
      document.documentElement.lang = this.locale === "zh" ? "zh-CN" : "en";
    }
  }

  /**
   * 查当前语言字典;缺键回退中文(源语言),再缺则返回 key 本身并 warn——
   * 界面永远有字可显,缺翻译由护栏测试兜底而非运行时报错。
   */
  t(key: string, params?: Record<string, unknown>): string {
    const entry: Msg | undefined = dicts[this.locale][key] ?? dicts.zh[key];
    if (entry === undefined) {
      console.warn(`[i18n] 缺键: ${key}`);
      return key;
    }
    if (typeof entry === "function") return entry(params ?? {});
    if (!params) return entry;
    return entry.replace(/\{(\w+)\}/g, (m, name) => (name in params ? String(params[name]) : m));
  }
}

export const i18n = new I18n();
/** 模板/模块通用简写:t("settings.theme.label")、t("notes.count", { n })。 */
export const t = (key: string, params?: Record<string, unknown>) => i18n.t(key, params);
