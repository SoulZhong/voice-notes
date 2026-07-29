// 编辑器纯逻辑层:文档构建/保存载荷/提交决策。刻意不 import Milkdown/DOM,
// vitest node 环境可测;schema 与壳组件(有副作用)在同目录其他文件。
import { splitMentions, type RefinedDoc, type ParagraphPayload } from "../notes";

export type { ParagraphPayload };

export type InlineRun = { text: string; entityId: string | null };

/** 精修稿一个顶层块的构建说明。kind="runs":纯文本 + 实体标注(有 live mention 的
    干净段,文本按字面载入,不做 markdown 解析——mention 偏移只对字面文本有效);
    kind="markdown":文本按行内 markdown 解析(编辑过的段/无 mention 段)。 */
export type BlockSpec = {
  origIndex: number;
  speaker: string;
  name: string | null;
  personId: string | null;
  startMs: number;
  kind: "runs" | "markdown";
  runs: InlineRun[];
  markdown: string;
};

export function refinedToBlocks(doc: RefinedDoc): BlockSpec[] {
  const support = new Set(doc.graph_support_mentions ?? []);
  return doc.paragraphs.map((p, i) => {
    const live = (p.mentions ?? []).filter((m) => !m.id || !support.has(m.id));
    const base = {
      origIndex: i,
      speaker: p.speaker,
      name: p.name ?? null,
      personId: p.person_id ?? null,
      startMs: p.start_ms,
    };
    return live.length > 0
      ? { ...base, kind: "runs" as const, runs: splitMentions(p.text, live), markdown: p.text }
      : { ...base, kind: "markdown" as const, runs: [], markdown: p.text };
  });
}

export type EditedBlock = { origIndex: number | null; markdown: string };

/** 整篇保存载荷。dirty 判定基线:载入时同一块的序列化结果(baseline),没有基线
    (理论上不发生)退回存储原文;空白块直接丢弃(后端也会拒空,双保险)。 */
export function refinedSavePayload(
  doc: RefinedDoc,
  blocks: EditedBlock[],
  baseline: Map<number, string>,
): { revision: number; paragraphs: ParagraphPayload[] } {
  const paragraphs: ParagraphPayload[] = [];
  for (const b of blocks) {
    const text = b.markdown.trim();
    if (!text) continue;
    if (b.origIndex === null) {
      paragraphs.push({ orig_index: null, text, dirty: true });
    } else {
      const base = baseline.get(b.origIndex) ?? doc.paragraphs[b.origIndex]?.text ?? "";
      paragraphs.push({ orig_index: b.origIndex, text, dirty: text !== base.trim() });
    }
  }
  return { revision: doc.revision ?? 0, paragraphs };
}

/** Enter 分段/复制粘贴会复制块属性 → 同一 origIndex 出现多次。保首个,其余视为
    用户新插入块(origIndex=null,调用方同时清 speaker 属性)。 */
export function normalizeOrigIndices(indices: (number | null)[]): (number | null)[] {
  const seen = new Set<number>();
  return indices.map((i) => {
    if (i === null || seen.has(i)) return null;
    seen.add(i);
    return i;
  });
}

export type SegmentCommit =
  | { kind: "skip" }
  | { kind: "commit"; newText: string; roundTripOk: boolean };

/** 段落失焦提交决策。往返不稳定(serialize(parse(md)) ≠ md)时按纯文本提交,
    避免编辑器序列化 bug 静默改写用户内容(spec 错误处理节)。 */
export function segmentCommitDecision(args: {
  storedText: string;
  baselineMd: string;
  currentMd: string;
  currentPlain: string;
  reparsedMd: string;
}): SegmentCommit {
  const cur = args.currentMd.trim();
  const plain = args.currentPlain.trim();
  if (!plain) return { kind: "skip" };
  if (cur === args.baselineMd.trim()) return { kind: "skip" };
  if (args.reparsedMd.trim() !== cur) {
    if (plain === args.storedText.trim()) return { kind: "skip" };
    return { kind: "commit", newText: plain, roundTripOk: false };
  }
  return { kind: "commit", newText: cur, roundTripOk: true };
}

export type SegSkeleton = { seq: number; speaker: string | null };

/** 原始稿结构锁定判据:任何改变段数/顺序/seq/speaker 的事务都被拒绝
    (增删段与改说话人只走命令按钮,不走键盘)。 */
export function sameSegmentSkeleton(a: SegSkeleton[], b: SegSkeleton[]): boolean {
  return a.length === b.length && a.every((x, i) => x.seq === b[i].seq && x.speaker === b[i].speaker);
}
