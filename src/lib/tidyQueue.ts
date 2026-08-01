// 整理收件箱的队列纯逻辑:构建队列本身。
// UI 无关、可单测;/speakers 概览页常驻分析区与侧栏徽标共同消费。
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
    忽略/保留集(键=tidyItemKey),建议的忽略在上游 tidy.visible 里已滤。同名组不设
    样本条件,成员可同时出现在无样本卡。 */
export function buildTidyQueue(
  people: PersonSummary[],
  suggestions: PersonMergeSuggestion[],
  receipts: MergeReceipt[],
  dismissed: Set<string> = new Set(),
): TidyItem[] {
  const items: TidyItem[] = receipts.map((r) => ({ kind: "receipt", receipt: r }));
  const ids = new Set(people.map((p) => p.id));
  // 失效目标保险:后端建议按当前库现算,但拉取与消费之间有时序窗口——已合并/
  // 已删除的人不能再作合并目标(或来源),否则点「合并」必报「人物不存在」。
  for (const s of suggestions) {
    if (ids.has(s.loser) && ids.has(s.winner)) items.push({ kind: "suggestion", suggestion: s });
  }
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

/** 待办/存档分组:失效回执(不能再撤销)只剩回看价值,折叠进存档区,不算待办
    ——徽标与「N 件待处理」按 pending 计。 */
export function splitArchive(items: TidyItem[]): { pending: TidyItem[]; archived: TidyItem[] } {
  const archived = items.filter((i) => i.kind === "receipt" && i.receipt.invalid_reason !== null);
  const pending = items.filter((i) => !archived.includes(i));
  return { pending, archived };
}

/** 同名组按列表顺序并入 winner。每次成功后立刻发布对应 journal id,而不是等整组
    完成:后续某一条失败时,调用方仍能展示最近一次已落盘合并的撤销入口。 */
export async function mergeDuplicatePeople(
  people: PersonSummary[],
  winner: string,
  merge: (loser: string, winner: string) => Promise<string>,
  onMerged: (journalId: string) => void,
): Promise<void> {
  for (const person of people) {
    if (person.id === winner) continue;
    const journalId = await merge(person.id, winner);
    onMerged(journalId);
  }
}
