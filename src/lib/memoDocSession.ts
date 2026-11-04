// 备注文档的保存会话:每篇笔记一个,活得比编辑器组件长(模块级)。
//
// 为什么不放在组件里(Codex 审出两条 P1):
// - 离开页面/切笔记时组件已销毁,但最后一份改动还得存;若绕开串行队列直接发,会与在途的
//   自动保存拿同一个旧版本号竞争,后到的那份被判冲突后没人处理,字就丢了。
// - 保存一律串行(chain),冲突时在这里按行合并后重试,不依赖编辑器是否还活着。
//
// 每次排队的保存都带着它自己的基线(编辑器提交时看到的盘面):轮到它时盘面已经变了(前一次
// 保存合进了别处的改动),就先把「基线 → 本次文本」的改动合到最新盘面上再存——不能拿旧整篇
// 快照配最新版本号直接覆盖,那会把刚合进来的别处改动冲掉(Codex 第二轮 P1)。
//
// 保存在途时(pending > 0)到达的 memos_changed 事件,编辑器先暂存、等回执后再判定:是自己
// 这次保存(后端可能钳过时间点,文本与提交的不同)就只当确认,否则才当别处改动去合并。
// recent 记下最近存过的文本,兜住回执之后才到的事件。
import { mergeMemoDoc } from "./memoDocMerge";
import { saveMemoDoc, type MemoDoc } from "./memos";

export type MemoDocSession = {
  noteId: string;
  /** 与盘对齐的版本(rev)与内容(text = 盘上原文,按编辑器序列化格式)。 */
  base: MemoDoc;
  /** 串行保存链。 */
  chain: Promise<unknown>;
  /** 最近经本会话写盘的文本(用于识别自己的事件)。 */
  recent: Set<string>;
  /** 在途(排队 + 执行中)的保存数。 */
  pending: number;
};

const sessions = new Map<string, MemoDocSession>();

export function memoDocSession(noteId: string): MemoDocSession {
  let s = sessions.get(noteId);
  if (!s) {
    s = { noteId, base: { text: "", rev: "0" }, chain: Promise.resolve(), recent: new Set(), pending: 0 };
    sessions.set(noteId, s);
  }
  return s;
}

function remember(s: MemoDocSession, text: string) {
  s.recent.add(text);
  setTimeout(() => s.recent.delete(text), 5000);
}

export type SaveOutcome = { doc: MemoDoc; merged: boolean };

/**
 * 串行保存 `text`(排在在途保存之后)。`base` = 提交方编辑这份文本时看到的盘面原文。
 * 轮到执行时盘面已变 → 先把 base→text 的改动合到最新盘面;冲突 → 同样合并后重试(最多 4 次)。
 * 返回最终落盘的文档;`merged` 表示落盘内容与传入的 `text` 不同(合并过或被后端钳过时间)。
 */
export function saveSerial(s: MemoDocSession, text: string, base: string): Promise<SaveOutcome> {
  s.pending++;
  const run = async (): Promise<SaveOutcome> => {
    let cur = base === s.base.text ? text : mergeMemoDoc(base, text, s.base.text);
    for (let i = 0; i < 4; i++) {
      if (cur === s.base.text) return { doc: s.base, merged: cur !== text };
      remember(s, cur);
      const r = await saveMemoDoc(s.noteId, cur, s.base.rev);
      if (r.kind === "saved") {
        remember(s, r.doc.text);
        s.base = r.doc;
        return { doc: r.doc, merged: r.doc.text !== text };
      }
      const prevBase = s.base.text;
      s.base = r.doc;
      cur = mergeMemoDoc(prevBase, cur, r.doc.text);
    }
    throw new Error("memo save kept conflicting; try again");
  };
  const p = s.chain.then(run, run).finally(() => {
    s.pending--;
  });
  s.chain = p.catch(() => {});
  return p;
}
