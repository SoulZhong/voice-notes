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

/** 单一判定源:一个块算不算"空"(不该占 origIndex 名额/不该进保存载荷)。只信
    PM 节点的 textContent,不信任 serializer 的输出——commonmark 对空 paragraph
    会序列化出 "<br />" 字面量,是 truthy 但语义上是空块。MarkdownEditor.svelte
    的 collectBlocks(保存载荷)与 markSaved(origIndex 重排)必须共用这一个判定,
    否则两处对同一份文档会数出不同的"块集合",origIndex 就会错位。 */
export function isBlockTextEmpty(textContent: string): boolean {
  return textContent.trim().length === 0;
}

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

/** 保存成功后的 origIndex 重排映射,推导自**已发送的载荷**而不是当前文档:载荷第
    i 段就是保存后盘上的第 i 段,所以映射是 old origIndex → i。载荷里没出现的
    origIndex 说明该段已从盘上消失(空块被载荷构建器丢弃),调用方应把它置 null。
    invoke 往返期间用户可能又编辑了文档,当前文档顺序此刻已经不代表盘上段序——按
    当前顺序顺次编号会把窗口内新增的块编进服务端不存在的下标(orig_index 越界)。 */
export function savedIndexRemap(sent: ParagraphPayload[]): Map<number, number> {
  const remap = new Map<number, number>();
  sent.forEach((p, i) => {
    if (p.orig_index !== null) remap.set(p.orig_index, i);
  });
  return remap;
}

/** 保存成功后的 dirty 基线,同样取自已发送载荷:盘上第 i 段的正文就是载荷第 i 段的
    text。用载荷而非当前文档重建基线,是 in-flight 编辑不丢字的关键——往返窗口内改过
    的段与载荷文本不同,下一轮 flush 就会把它正确判成 dirty(宁多 dirty 不丢字)。 */
export function savedBaseline(sent: ParagraphPayload[]): Map<number, string> {
  return new Map(sent.map((p, i) => [i, p.text] as const));
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
