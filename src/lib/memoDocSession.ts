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

// ── 离页草稿:离开页面时最后一份先记进本地存储,真落盘后才清;重试耗尽(磁盘满等持续故障)
//    也不丢,下次打开这篇时编辑器把它合回来(Codex 审出)。 ──
export type MemoDraft = { text: string; base: string };
const draftKey = (noteId: string) => `memoDraft.${noteId}`;
export function stashDraft(noteId: string, d: MemoDraft) {
  try {
    localStorage.setItem(draftKey(noteId), JSON.stringify(d));
  } catch {
    /* 存不了就只靠队列重试 */
  }
}
export function takeDraft(noteId: string): MemoDraft | null {
  try {
    const raw = localStorage.getItem(draftKey(noteId));
    if (!raw) return null;
    const d = JSON.parse(raw) as MemoDraft;
    return typeof d?.text === "string" && typeof d?.base === "string" ? d : null;
  } catch {
    return null;
  }
}
export function clearDraft(noteId: string, text?: string) {
  try {
    // 只清自己那份:期间若又记了更新的草稿,留着
    if (text === undefined || takeDraft(noteId)?.text === text) localStorage.removeItem(draftKey(noteId));
  } catch {
    /* ignore */
  }
}

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

/**
 * 排队保存。`retry`:离开页面时交的最后一份——编辑器已销毁,没人会再重试,失败(锁超时、
 * 临时 IO 错误)就在队列里按 1/2/4/8/16 秒退避重试,期间同篇后续保存与重新进页的取盘都排在
 * 它后面(Codex 审出:离页那份失败后被吞掉,字就丢了)。
 */
export function saveSerial(
  noteId: string,
  text: string,
  basis: { base: string; prev: SaveOp | null },
  retry = false,
): SaveOp {
  const q = queue(noteId);
  q.pending++;
  const op: SaveOp = { text, ok: null, done: Promise.resolve({ text: "", rev: "0" }) };
  const run = async (): Promise<MemoDoc> => {
    const base = basis.prev && basis.prev.ok ? basis.prev.text : basis.base;
    for (let attempt = 0; ; attempt++) {
      try {
        const doc = await saveMemoDoc(noteId, text, base);
        op.ok = true;
        return doc;
      } catch (e) {
        if (!retry || attempt >= 5) {
          op.ok = false;
          throw e;
        }
        await new Promise((r) => setTimeout(r, 1000 * 2 ** attempt));
      }
    }
  };
  op.done = q.chain.then(run, run).finally(() => {
    q.pending--;
  });
  q.chain = op.done.catch(() => {});
  return op;
}
