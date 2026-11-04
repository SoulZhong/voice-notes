// 会议备注(memo):整场一篇 Markdown 文档(后端 memo.md,行尾 `<!-- mm:ss -->` 是时间点)。
// 编辑器整篇读写(memoDoc / saveMemoDoc,版本号乐观校验);其余地方读行视图(Memo[])。
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** 行视图:文档里带时间点的一行。`id` 是行号 `L<n>`,文档一改就变,只在一次读取内有意义。 */
export type Memo = {
  id: string;
  at_ms: number;
  /** 去掉 Markdown 行首记号后的文字;空串 = 时间标记(📍)。 */
  text: string;
  /** 这一行的 Markdown 原文(不含时间注释)。 */
  md: string;
  line: number;
};

/** 整篇备注 + 版本号(内容哈希;空文档 "0")。 */
export type MemoDoc = { text: string; rev: string };
export type SaveMemoDocResult = { kind: "saved"; doc: MemoDoc } | { kind: "conflict"; doc: MemoDoc };

export type MemosChangedEvent = { note_id: string; memos: Memo[]; doc: MemoDoc };
export type MemoShortcutStatus = { ok: boolean; reason: string; shortcut: string };
export type MemoHit = { note_id: string; memo_id: string; at_ms: number; text: string };
export type MemoPanelOpen = { note_id: string; memo: Memo };

/** 浮窗一次补字的上限(与后端 MAX_TEXT_CHARS 一致)。 */
export const MEMO_MAX_CHARS = 2000;
/** 整篇备注上限(与后端 MAX_DOC_CHARS 一致)。 */
export const MEMO_DOC_MAX_CHARS = 200_000;

export const noteMemos = (noteId: string) => invoke<Memo[]>("note_memos", { noteId });
export const memoDoc = (noteId: string) => invoke<MemoDoc>("memo_doc", { noteId });
export const saveMemoDoc = (noteId: string, text: string, baseRev: string) =>
  invoke<SaveMemoDocResult>("save_memo_doc", { noteId, text, baseRev });
export const memoAnchorNow = (noteId: string) => invoke<number>("memo_anchor_now", { noteId });
/** 浮窗补字:改「时间点 + 原文字」对得上的那一段;找不到就追加。 */
export const editMemoLine = (noteId: string, memo: Pick<Memo, "at_ms" | "text">, text: string) =>
  invoke<void>("edit_memo_line", { noteId, atMs: memo.at_ms, oldText: memo.text, text });
export const searchMemos = (query: string) => invoke<MemoHit[]>("search_memos", { query });
export const memoPanelState = () => invoke<MemoPanelOpen | null>("memo_panel_state");
export const memoPanelHide = () => invoke<void>("memo_panel_hide");

export const onMemosChanged = (cb: (e: MemosChangedEvent) => void) =>
  listen<MemosChangedEvent>("memos_changed", (e) => cb(e.payload));
export const onMemoShortcutStatus = (cb: (e: MemoShortcutStatus) => void) =>
  listen<MemoShortcutStatus>("memo_shortcut_status", (e) => cb(e.payload));
export const onMemoPanelOpen = (cb: (e: MemoPanelOpen) => void) =>
  listen<MemoPanelOpen>("memo_panel_open", (e) => cb(e.payload));

// ───────────────────────── 纯函数(配单测) ─────────────────────────

export function sortMemos(memos: readonly Memo[]): Memo[] {
  return [...memos].sort((a, b) => a.at_ms - b.at_ms || a.line - b.line);
}

/** 半开区间:落在任一删减区间 [start, end) 内。 */
export function inCut(atMs: number, cuts: readonly { start_ms: number; end_ms: number }[]): boolean {
  return cuts.some((c) => c.start_ms <= atMs && atMs < c.end_ms);
}

/** 时间点钳位到 [0, max]。 */
export function clampAt(atMs: number, maxMs: number): number {
  return Math.max(0, Math.min(Math.round(atMs), Math.max(0, Math.round(maxMs))));
}

/** mm:ss(满一小时 h:mm:ss)。与后端 fmt_clock 一致。 */
export function fmtClock(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  const p = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${p(m)}:${p(ss)}` : `${p(m)}:${p(ss)}`;
}

/** 解析 mm:ss / h:mm:ss / 纯秒数;无法解析返回 null。 */
export function parseClock(s: string): number | null {
  const t = s.trim();
  if (!t) return null;
  if (/^\d+$/.test(t)) return Number(t) * 1000;
  const parts = t.split(":");
  if (parts.length < 2 || parts.length > 3 || parts.some((p) => !/^\d+$/.test(p))) return null;
  const n = parts.map(Number);
  const [h, m, sec] = n.length === 3 ? n : [0, n[0], n[1]];
  if (sec >= 60 || (n.length === 3 && m >= 60)) return null;
  return ((h * 60 + m) * 60 + sec) * 1000;
}

/**
 * 笔记页前台按 `M` 加备注的守卫:无修饰键、非输入法组合中、焦点不在可编辑区域、
 * 本笔记不在录制中——全都满足才响应。
 */
export function memoKeyAllowed(
  e: Pick<KeyboardEvent, "key" | "altKey" | "ctrlKey" | "metaKey" | "shiftKey" | "isComposing" | "repeat">,
  target: { closest?: (sel: string) => unknown; tagName?: string } | null,
  noteIsRecording: boolean,
): boolean {
  if (e.key !== "m" && e.key !== "M") return false;
  if (e.altKey || e.ctrlKey || e.metaKey || e.shiftKey || e.isComposing || e.repeat) return false;
  if (noteIsRecording) return false;
  if (target?.closest?.("input, textarea, select, [contenteditable], [contenteditable='true'], .ProseMirror")) return false;
  return true;
}

/** 备注栏展开/收起:用户手动选过就听用户的;否则主区宽度 ≥ 900px 默认展开(窄窗口默认收起)。 */
export const MEMO_COLUMN_BREAKPOINT = 900;
export function memoColumnOpen(containerWidth: number, stored: boolean | null): boolean {
  if (stored !== null) return stored;
  return containerWidth >= MEMO_COLUMN_BREAKPOINT;
}

/** 读写备注栏偏好:键按页面区分;读写失败(隐私模式等)一律当没存过。 */
export function loadMemoColumnPref(page: "record" | "note"): boolean | null {
  try {
    const v = localStorage.getItem(`memoColumn.${page}`);
    return v === "1" ? true : v === "0" ? false : null;
  } catch {
    return null;
  }
}
export function saveMemoColumnPref(page: "record" | "note", open: boolean) {
  try {
    localStorage.setItem(`memoColumn.${page}`, open ? "1" : "0");
  } catch {
    /* 存不了就只在本次生效 */
  }
}
