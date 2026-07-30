// 整理收件箱的队列纯逻辑:构建、跳过排序、键盘命令映射。
// UI 无关、可单测;/speakers/tidy 页面与侧栏徽标共同消费。
import type { MergeReceipt, PersonMergeSuggestion, PersonSummary } from "$lib/people";

export type TidyItem =
  | { kind: "receipt"; receipt: MergeReceipt }
  | { kind: "suggestion"; suggestion: PersonMergeSuggestion }
  | { kind: "dup"; name: string; people: PersonSummary[] }
  | { kind: "nosample"; person: PersonSummary };

/** 稳定 key(跳过序/忽略集/Svelte each key 共用)。建议键与 sugKey 同格式。 */
export const tidyItemKey = (it: TidyItem): string =>
  it.kind === "receipt"
    ? `r:${it.receipt.journal_id}`
    : it.kind === "suggestion"
      ? `s:${it.suggestion.loser}>${it.suggestion.winner}`
      : it.kind === "dup"
        ? `d:${it.name}`
        : `n:${it.person.id}`;

/** 收件箱队列:回执 → 拿不准的建议 → 同名组 → 无样本。people 按 last_seen 降序
    传入(listPeople 保证),同名组主条目默认取组首=最近活跃。dismissed 是会话级
    忽略/保留集(键=tidyItemKey),建议的忽略在上游 tidy.visible 里已滤。 */
export function buildTidyQueue(
  people: PersonSummary[],
  suggestions: PersonMergeSuggestion[],
  receipts: MergeReceipt[],
  dismissed: Set<string> = new Set(),
): TidyItem[] {
  const items: TidyItem[] = receipts.map((r) => ({ kind: "receipt", receipt: r }));
  for (const s of suggestions) items.push({ kind: "suggestion", suggestion: s });
  const byName = new Map<string, PersonSummary[]>();
  for (const p of people) {
    if (!p.name) continue;
    byName.set(p.name, [...(byName.get(p.name) ?? []), p]);
  }
  for (const [name, g] of byName) {
    if (g.length > 1) {
      items.push({ kind: "dup", name, people: g });
    }
  }
  for (const p of people) {
    if (p.sample_paths.length === 0) {
      items.push({ kind: "nosample", person: p });
    }
  }
  return items.filter((i) => !dismissed.has(tidyItemKey(i)));
}

/** 跳过=挪队尾:未跳过的保持原序,跳过的按跳过先后排最后。 */
export function orderWithSkips(items: TidyItem[], skippedKeys: string[]): TidyItem[] {
  const rank = new Map(skippedKeys.map((k, i) => [k, i]));
  const kept = items.filter((i) => !rank.has(tidyItemKey(i)));
  const skipped = items
    .filter((i) => rank.has(tidyItemKey(i)))
    .sort((a, b) => rank.get(tidyItemKey(a))! - rank.get(tidyItemKey(b))!);
  return [...kept, ...skipped];
}

/** 键盘命令:Enter=主动作,X=忽略/保留(回执卡除外——撤销只走点击防误触),
    S=跳过,数字=试听(双栏卡 1/2=左右方,同名组卡 1-9=第 n 条)。null=此卡无该命令。 */
export type TidyCommand = "primary" | "dismiss" | "skip" | { play: number };
export function keyCommand(key: string, kind: TidyItem["kind"]): TidyCommand | null {
  if (key === "Enter") return "primary";
  if (key === "x" || key === "X") return kind === "receipt" ? null : "dismiss";
  if (key === "s" || key === "S") return "skip";
  if (kind === "nosample") return null; // 无样本卡没有可试听的
  const digitMax = kind === "dup" ? 9 : 2;
  const n = Number(key);
  if (Number.isInteger(n) && n >= 1 && n <= digitMax) return { play: n - 1 };
  return null;
}
