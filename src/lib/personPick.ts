// 选人列表的共享纯逻辑:录音页说话人面板与详情页「合并到…」菜单同源,
// 展示名/区分后缀/同名集合/包含过滤四件套一处维护。
import type { PersonSummary } from "./people";
import { formatDate } from "./notes";

/** 人物显示名:未命名按全局编号「说话人 N」兜底(与徽章一致)。 */
export const personLabel = (p: PersonSummary) => p.name || `说话人 ${p.id.replace(/^P/, "")}`;

/** "最近 MM-DD":未命名/重名条目的区分后缀;无日期给空串。 */
export const recentLabel = (p: PersonSummary) => {
  const d = formatDate(p.last_seen);
  return d === "—" ? "" : `最近 ${d.slice(5, 10)}`;
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

/** 候选过滤:空查询给全量,非空按显示名包含匹配。 */
export function filterPeople(people: PersonSummary[], query: string): PersonSummary[] {
  const q = query.trim();
  return q ? people.filter((p) => personLabel(p).includes(q)) : people;
}

/** 人物字母序:中文按拼音、数字段按数值("说话人 2"<"说话人 10")。已命名按名字,
    未命名按编号标签——分组展示时两组各自内部有序。 */
export function sortPeopleAlpha(people: PersonSummary[]): PersonSummary[] {
  const collator = new Intl.Collator("zh-Hans-CN-u-co-pinyin", { numeric: true });
  return [...people].sort((a, b) => collator.compare(personLabel(a), personLabel(b)));
}
