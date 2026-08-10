import type { NoteState } from "./notes";

export type NoteBadgeKind = "active" | "paused" | "interrupted";

/// 侧栏笔记行徽标种类。暂停不是持久化 NoteState(会话运行时标志,不落盘),
/// 所以 active 笔记要叠加 recording.paused 才能显示「已暂停」——与录制按钮、
/// 实时转写页头的口径一致。"recording" 是上次会话悬挂的中断态,与暂停无关。
export function noteBadgeKind(state: NoteState, livePaused: boolean): NoteBadgeKind | null {
  if (state === "active") return livePaused ? "paused" : "active";
  if (state === "recording") return "interrupted";
  return null;
}
