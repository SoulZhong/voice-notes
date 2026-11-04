// 备注文档的保存会话:每篇笔记一个,活得比编辑器组件长(模块级)。
//
// 为什么不放在组件里(Codex 审出):离开页面/切笔记时组件已销毁,但最后一份改动还得存,
// 而且要排在在途的那份之后、以正确的基线合并——这些状态必须活过组件。
//
// 不变量:
// - 保存一律串行(chain)。每份保存带两个基线:`base` = 提交方编辑时依据的盘面原文;
//   `prev` = 提交时还在途的那份文本(没有则 null)。轮到执行时:前一份成功了,本份是在它之上
//   继续改的,基线取 prev;前一份失败了,prev 从未落盘,基线退回 base——否则 prev 里那段
//   会被当成「别处删掉的」而丢失(Codex 第三轮 P1)。
// - 执行时盘面已变 → 先把「基线 → 本次」的改动按行合到最新盘面上再存;冲突同样合并后重试。
// - seen 记下本会话读到/写出过的全部版本号:这些版本的 memos_changed 事件不是新消息
//   (要么是自己写的,要么是保存时已合并过的),编辑器直接忽略,不会把旧版本回放回去。
import { mergeMemoDoc } from "./memoDocMerge";
import { saveMemoDoc, type MemoDoc } from "./memos";

export type MemoDocSession = {
  noteId: string;
  /** 最近一次读到/写出的盘面。 */
  base: MemoDoc;
  /** 串行保存链。 */
  chain: Promise<unknown>;
  /** 在途(排队 + 执行中)的保存数。 */
  pending: number;
  /** 本会话见过的版本号。 */
  seen: Set<string>;
  /** 上一份执行完的保存:成功则是它提交的文本,失败则 null。 */
  lastOk: string | null;
};

const sessions = new Map<string, MemoDocSession>();

export function memoDocSession(noteId: string): MemoDocSession {
  let s = sessions.get(noteId);
  if (!s) {
    s = { noteId, base: { text: "", rev: "0" }, chain: Promise.resolve(), pending: 0, seen: new Set(), lastOk: null };
    sessions.set(noteId, s);
  }
  return s;
}

/** 记下读到的盘面(取盘/事件)。 */
export function noteSeen(s: MemoDocSession, doc: MemoDoc) {
  s.seen.add(doc.rev);
  s.base = doc;
}

export type SaveOutcome = { doc: MemoDoc; merged: boolean };

export function saveSerial(
  s: MemoDocSession,
  text: string,
  basis: { base: string; prev: string | null },
): Promise<SaveOutcome> {
  s.pending++;
  const run = async (): Promise<SaveOutcome> => {
    const base = basis.prev !== null && s.lastOk === basis.prev ? basis.prev : basis.base;
    let cur = base === s.base.text ? text : mergeMemoDoc(base, text, s.base.text);
    try {
      for (let i = 0; i < 4; i++) {
        if (cur === s.base.text) {
          s.lastOk = text;
          return { doc: s.base, merged: cur !== text };
        }
        const r = await saveMemoDoc(s.noteId, cur, s.base.rev);
        if (r.kind === "saved") {
          noteSeen(s, r.doc);
          s.lastOk = text;
          return { doc: r.doc, merged: r.doc.text !== text };
        }
        const prevDisk = s.base.text;
        noteSeen(s, r.doc);
        cur = mergeMemoDoc(prevDisk, cur, r.doc.text);
      }
      throw new Error("memo save kept conflicting; try again");
    } catch (e) {
      s.lastOk = null;
      throw e;
    }
  };
  const p = s.chain.then(run, run).finally(() => {
    s.pending--;
  });
  s.chain = p.catch(() => {});
  return p;
}
