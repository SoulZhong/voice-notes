// 选人列表的共享纯逻辑:录音页说话人面板与详情页「合并到…」菜单同源,
// 展示名/区分后缀/同名集合/包含过滤四件套一处维护。
import { pinyin } from "pinyin-pro";
import type { PersonSummary } from "./people";
import { formatDate } from "./notes";
import { i18n, t } from "$lib/i18n/index.svelte";

/** 人物显示名:未命名按全局编号「说话人 N」兜底(与徽章一致)。 */
export const personLabel = (p: PersonSummary) =>
  p.name || t("speakers.personFallback", { n: p.id.replace(/^P/, "") });

/** "最近 MM-DD":未命名/重名条目的区分后缀;无日期给空串。 */
export const recentLabel = (p: PersonSummary) => {
  const d = formatDate(p.last_seen);
  return d === "—" ? "" : t("speakers.recentShort", { d: d.slice(5, 10) });
};

/** 出现超过一次的名字集合:同名条目必须带区分后缀,否则列表里两行一模一样。 */
export function dupNameSet(people: PersonSummary[]): Set<string> {
  const seen = new Set<string>();
  const dup = new Set<string>();
  for (const p of people) {
    if (!p.name) continue;
    if (seen.has(p.name)) dup.add(p.name);
    seen.add(p.name);
  }
  return dup;
}

/** 拼音检索键(全拼+首字母),按显示名缓存:列表边输边筛不重算转换。 */
const pinyinKeyCache = new Map<string, { full: string; initials: string }>();
function pinyinKeys(label: string): { full: string; initials: string } {
  let k = pinyinKeyCache.get(label);
  if (!k) {
    k = {
      full: pinyin(label, { toneType: "none", type: "array" }).join("").toLowerCase(),
      initials: pinyin(label, { pattern: "first", toneType: "none", type: "array" }).join("").toLowerCase(),
    };
    pinyinKeyCache.set(label, k);
  }
  return k;
}

/** 候选过滤:空查询给全量;非空按显示名包含匹配,纯字母查询另按拼音全拼/首字母
    匹配(排序是拼音序,检索若不认拼音会与用户心智脱节)。 */
export function filterPeople(people: PersonSummary[], query: string): PersonSummary[] {
  const q = query.trim();
  if (!q) return people;
  const alpha = /^[a-zA-Z]+$/.test(q) ? q.toLowerCase() : null;
  return people.filter((p) => {
    const label = personLabel(p);
    if (label.includes(q)) return true;
    if (!alpha) return false;
    const k = pinyinKeys(label);
    return k.full.includes(alpha) || k.initials.includes(alpha);
  });
}

/** 人物字母序:中文按拼音、数字段按数值("说话人 2"<"说话人 10")。已命名按名字,
    未命名按编号标签——分组展示时两组各自内部有序。
    排序规则跟随界面语言:中文界面按拼音(否则中文名会退化成码点序),英文界面按 en
    默认序;numeric 两边都要,编号标签的数值序与语言无关。 */
export function sortPeopleAlpha(people: PersonSummary[]): PersonSummary[] {
  const collator = new Intl.Collator(
    i18n.locale === "zh" ? "zh-Hans-CN-u-co-pinyin" : "en",
    { numeric: true },
  );
  return [...people].sort((a, b) => collator.compare(personLabel(a), personLabel(b)));
}
