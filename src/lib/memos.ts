// 会议备注(memo):命令封装 + 事件 + 纯函数。
// 设计:docs/superpowers/specs/2026-11-03-meeting-memo-design.md
// 一律以后端 `memos_changed` 事件(整表)为准,不做乐观更新(与 segment_edited 同纪律)。
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type MemoOrigin = "composer" | "hotkey" | "tray" | "note";

export type Memo = {
  id: string;
  at_ms: number;
  /** 纯文本,可多行;空串 = 时间标记。 */
  text: string;
  origin: MemoOrigin | string;
  created_at: string;
  updated_at: string;
};

export type MemosChangedEvent = { note_id: string; memos: Memo[] };
export type MemoShortcutStatus = { ok: boolean; reason: string; shortcut: string };
export type MemoHit = { note_id: string; memo_id: string; at_ms: number; text: string };
export type MemoPanelOpen = { note_id: string; memo: Memo };

/** 单条备注文字上限(与后端 MAX_TEXT_CHARS 一致)。 */
export const MEMO_MAX_CHARS = 2000;

export const noteMemos = (noteId: string) => invoke<Memo[]>("note_memos", { noteId });
export const memoAnchorNow = (noteId: string) => invoke<number>("memo_anchor_now", { noteId });
export const addMemo = (
  noteId: string,
  atMs: number,
  text: string,
  origin: MemoOrigin,
  restore?: { id: string; created_at: string },
) =>
  invoke<Memo>("add_memo", {
    noteId,
    atMs: Math.max(0, Math.round(atMs)),
    text,
    origin,
    restoreId: restore?.id ?? null,
    restoreCreatedAt: restore?.created_at ?? null,
  });
export const editMemo = (
  noteId: string,
  memo: Pick<Memo, "id" | "updated_at">,
  patch: { text?: string; at_ms?: number },
) =>
  invoke<Memo>("edit_memo", {
    noteId,
    id: memo.id,
    expectedUpdatedAt: memo.updated_at,
    text: patch.text ?? null,
    atMs: patch.at_ms === undefined ? null : Math.max(0, Math.round(patch.at_ms)),
  });
export const deleteMemo = (noteId: string, id: string) => invoke<Memo>("delete_memo", { noteId, id });
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

/** 时间轴上的一项:段落/块(start_ms)或备注。 */
export type TimelineItem<T> = { kind: "block"; item: T } | { kind: "memo"; memo: Memo };

/**
 * 把备注按时间归并进块列表:备注落在「最后一个 start_ms <= at_ms 的块」之后
 * (即第一个 start_ms > at_ms 的块之前);早于第一块的排最前,晚于所有块的排最后。
 * 与后端导出 `MemoCursor`、笔记页装饰同一条规则。块须已按时间升序;
 * `startOf` 返回 null 的块(无时间跨度的自由块)不推进游标。
 */
export function mergeTimeline<T>(
  blocks: readonly T[],
  memos: readonly Memo[],
  startOf: (b: T) => number | null,
): TimelineItem<T>[] {
  const sorted = sortMemos(memos);
  const out: TimelineItem<T>[] = [];
  let i = 0;
  for (const b of blocks) {
    const s = startOf(b);
    if (s !== null) {
      while (i < sorted.length && sorted[i].at_ms < s) out.push({ kind: "memo", memo: sorted[i++] });
    }
    out.push({ kind: "block", item: b });
  }
  while (i < sorted.length) out.push({ kind: "memo", memo: sorted[i++] });
  return out;
}

/**
 * 备注该挂在哪个块之后:返回块下标(-1 = 排在所有块之前)。笔记页装饰用。
 * 规则同 {@link mergeTimeline}。
 */
export function anchorIndex(starts: readonly (number | null)[], atMs: number): number {
  let idx = -1;
  for (let i = 0; i < starts.length; i++) {
    const s = starts[i];
    if (s === null) continue;
    if (s <= atMs) idx = i;
    else break;
  }
  return idx;
}

export function sortMemos(memos: readonly Memo[]): Memo[] {
  return [...memos].sort((a, b) => a.at_ms - b.at_ms || a.created_at.localeCompare(b.created_at));
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

/** 备注栏展开/收起:用户手动选过就听用户的;否则主区宽度 ≥ 900px 默认展开。 */
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
