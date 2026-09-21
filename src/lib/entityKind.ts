// 实体类型的配色与标签:**唯一真值源**。
//
// 为什么要单独一份(2026-09-19):正文里的实体提及要和它头顶那枚 chip 一眼看出是同
// 一个东西,靠的就是同色。颜色表一旦散落两处(EntityChips 自己一份、编辑器 schema
// 再抄一份),迟早漂成"行里蓝、正文紫"。这里定死,两边都从这里取。
//
// 与 graph.ts 的 kindInk/kindSoft 是**两套刻意不同的口径**,别顺手合并:那边给的是
// 力导图里「kind 这个分类」的代表色(按 KIND_ORDER 轮转七色),覆盖 person/term/org/
// project/product/decision/task/place/date 全谱;这里只收笔记页展示的四类,配色按
// 「人=天蓝、组织=薄荷、产品项目=薰衣草、术语=中性灰」的语义挑过,不是轮转来的。

import { t } from "$lib/i18n/index.svelte";

export type EntityKindStyle = {
  key: string;
  /** 15% alpha 软底(chip 底色 / 正文提及底色)。 */
  tint: string;
  /** 同色相饱和字色(chip 文字 / 正文提及文字)。 */
  ink: string;
};

/** 笔记页展示的四类。顺序即 chip 编辑浮层里的类型选单顺序。 */
export const ENTITY_KINDS: readonly (EntityKindStyle & { labelKey: string })[] = [
  { key: "person", labelKey: "notes.entities.kind.person", tint: "var(--tint-sky)", ink: "var(--tint-sky-ink)" },
  { key: "org", labelKey: "notes.entities.kind.org", tint: "var(--tint-mint)", ink: "var(--tint-mint-ink)" },
  { key: "project", labelKey: "notes.entities.kind.project", tint: "var(--tint-lavender)", ink: "var(--tint-lavender-ink)" },
  { key: "term", labelKey: "notes.entities.kind.term", tint: "var(--tint-gray)", ink: "var(--tint-gray-ink)" },
] as const;

/** 后端历史上产出过 "concept",展示侧一律当 "term"。 */
export function normalizeEntityKind(kind: string): string {
  return kind === "concept" ? "term" : kind;
}

/** 四类之内 → 该类;之外(date/task/place… 或模型新造的词)→ null。
    chip 行据此过滤;正文提及不过滤,走 entityKindStyle 的中性兜底。 */
export function entityKind(kind: string): (EntityKindStyle & { labelKey: string }) | undefined {
  const k = normalizeEntityKind(kind);
  return ENTITY_KINDS.find((x) => x.key === k);
}

/** 类型标签(i18n)。 */
export function entityKindLabel(kind: string): string {
  const e = entityKind(kind);
  return e ? t(e.labelKey) : kind;
}

/** 正文提及用的配色:四类之外兜底到中性灰(term 的那套)。
    为什么不过滤掉:chip 行只展示四类且收在 12 个以内,正文却该把**所有**认出来的词
    都标出来——「这个词被 AI 认出来了」这条信息不该因为它的类型不在白名单里就消失。 */
export function entityMentionStyle(kind: string): EntityKindStyle {
  const e = entityKind(kind);
  if (e) return { key: e.key, tint: e.tint, ink: e.ink };
  const fallback = ENTITY_KINDS[ENTITY_KINDS.length - 1];
  return { key: fallback.key, tint: fallback.tint, ink: fallback.ink };
}

/** 提及 span 的行内 CSS 变量(样式表按 --ent-tint / --ent-ink 取色,见笔记页 CSS)。
    走行内变量而不是 `[data-entity-kind="x"]` 一堆选择器:颜色表只有上面这一份,
    CSS 侧不再复制一遍类型名,新增类型不用两处同步。 */
export function entityMentionCssVars(kind: string): string {
  const s = entityMentionStyle(kind);
  return `--ent-tint:${s.tint};--ent-ink:${s.ink}`;
}
