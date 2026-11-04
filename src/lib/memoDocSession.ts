// 备注文档的保存会话:每篇笔记一个,活得比编辑器组件长(模块级)。
//
// 为什么不放在组件里(Codex 审出两条 P1):
// - 离开页面/切笔记时组件已销毁,但最后一份改动还得存;若绕开串行队列直接发,会与在途的
//   自动保存拿同一个旧版本号竞争,后到的那份被判冲突后没人处理,字就丢了。
// - 保存一律串行(chain),冲突时在这里按行合并后重试,不依赖编辑器是否还活着。
//
// 自己保存引起的 memos_changed 事件可能先于保存回执到达:recent 记下最近存过的文本,
// 编辑器据此把它当「保存确认」而不是「别处改了」去合并(否则会把旧文本再追加一遍)。
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
};

const sessions = new Map<string, MemoDocSession>();

export function memoDocSession(noteId: string): MemoDocSession {
  let s = sessions.get(noteId);
  if (!s) {
    s = { noteId, base: { text: "", rev: "0" }, chain: Promise.resolve(), recent: new Set() };
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
 * 串行保存 `text`(排在在途保存之后)。冲突 → 以最新盘面为基线按行合并后重试(最多 4 次)。
 * 返回最终落盘的文档;`merged` 表示落盘内容与传入的 `text` 不同(合并过或被后端钳过时间)。
 */
export function saveSerial(s: MemoDocSession, text: string): Promise<SaveOutcome> {
  const run = async (): Promise<SaveOutcome> => {
    let cur = text;
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
  const p = s.chain.then(run, run);
  s.chain = p.catch(() => {});
  return p;
}
