// 备注文档的保存队列:每篇笔记一个,活得比编辑器组件长(模块级)。
//
// 合并在后端锁内做(save_memo_doc 带「这份文本依据的盘面原文」,盘面变了就三方合并),
// 前端这里只负责两件事:
// - 串行:同一篇的保存一份接一份发,离开页面时交的最后一份排在在途那份之后。
// - 基线:每份保存说明自己依据的是哪一版。离页那份是在在途那份之上继续改的(prev);
//   在途那份**成功**了,基线就是它提交的文本;**失败**了它从未落盘,基线退回编辑器上次
//   对齐的盘面(base)——看的是这一份具体的前驱,不是「最后一次成功的是谁」(Codex 审出)。
import { saveMemoDoc, type MemoDoc } from "./memos";

export type SaveOp = { text: string; ok: boolean | null; done: Promise<MemoDoc> };

type Queue = { chain: Promise<unknown>; pending: number };
const queues = new Map<string, Queue>();

function queue(noteId: string): Queue {
  let q = queues.get(noteId);
  if (!q) {
    q = { chain: Promise.resolve(), pending: 0 };
    queues.set(noteId, q);
  }
  return q;
}

/** 这一篇在途(排队 + 执行中)的保存全部结束。 */
export function memoDocIdle(noteId: string): Promise<unknown> {
  return queue(noteId).chain;
}

export function memoDocPending(noteId: string): number {
  return queue(noteId).pending;
}

export function saveSerial(noteId: string, text: string, basis: { base: string; prev: SaveOp | null }): SaveOp {
  const q = queue(noteId);
  q.pending++;
  const op: SaveOp = { text, ok: null, done: Promise.resolve({ text: "", rev: "0" }) };
  const run = async (): Promise<MemoDoc> => {
    const base = basis.prev && basis.prev.ok ? basis.prev.text : basis.base;
    try {
      const doc = await saveMemoDoc(noteId, text, base);
      op.ok = true;
      return doc;
    } catch (e) {
      op.ok = false;
      throw e;
    }
  };
  op.done = q.chain.then(run, run).finally(() => {
    q.pending--;
  });
  q.chain = op.done.catch(() => {});
  return op;
}
