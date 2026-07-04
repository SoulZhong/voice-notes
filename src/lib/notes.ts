import { invoke } from "@tauri-apps/api/core";
import type { Source } from "./events";

export type NoteState = "active" | "recording" | "complete";

export type NoteSummary = {
  id: string;
  title: string;
  started_at: string;
  duration_secs: number | null;
  state: NoteState;
};

export type NoteMeta = {
  schema_version: number;
  id: string;
  title: string;
  started_at: string;
  ended_at: string | null;
  state: string;
};

export type SegmentRecord = {
  seq: number;
  source: Source;
  text: string;
  start_ms: number;
  end_ms: number;
  speaker: string | null;
};

export type Note = {
  meta: NoteMeta;
  segments: SegmentRecord[];
  skipped_lines: number;
  // centroid/count 是后端质心快照（P4.5 续录铺底），随 get_note 下发；前端不消费，
  // 仅补齐类型以匹配后端 SpeakerMeta 的实际字段。
  speakers: Record<
    string,
    { name: string; sources: string[]; centroid?: number[]; count?: number }
  >;
};

export const listNotes = () => invoke<NoteSummary[]>("list_notes");
export const getNote = (id: string) => invoke<Note>("get_note", { id });
export const renameNote = (id: string, title: string) =>
  invoke<void>("rename_note", { id, title });
export const deleteNote = (id: string) => invoke<void>("delete_note", { id });
export const resumeRecording = (noteId: string) => invoke<void>("resume_recording", { noteId });
export const renameSpeaker = (noteId: string, speakerId: string, name: string) =>
  invoke<void>("rename_speaker", { noteId, speakerId, name });
/** 返回导出文件绝对路径 */
export const exportNote = (id: string, format: "md" | "txt") =>
  invoke<string>("export_note", { id, format });

/** 显示名:名字 > 「说话人 N」;null → 按来源 我/对方 */
export function speakerLabel(
  speaker: string | null,
  source: Source,
  speakers: Record<string, { name: string }>,
): string {
  if (!speaker) return source === "mic" ? "我" : "对方";
  const name = speakers[speaker]?.name;
  return name || `说话人 ${speaker.replace(/^S/, "")}`;
}
/** 稳定调色板:S1..Sn 循环取色;非 S<n> 形态 id 用字符串散列兜底(亮/暗色下均可读) */
const PALETTE = ["#396cd8", "#2e9e5b", "#b5651d", "#8e44ad", "#c0392b", "#16808a", "#946200", "#5d6d7e"];
export function speakerColor(speaker: string | null, source: Source): string {
  if (!speaker) return source === "mic" ? "#396cd8" : "#2e9e5b";
  const n = parseInt(speaker.replace(/^S/, ""), 10);
  if (Number.isFinite(n) && n > 0) return PALETTE[(n - 1) % PALETTE.length];
  let h = 0;
  for (const c of speaker) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return PALETTE[h % PALETTE.length];
}

/** 00:01:23 */
export function formatTs(ms: number): string {
  const s = Math.floor(ms / 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(Math.floor(s / 3600))}:${pad(Math.floor((s % 3600) / 60))}:${pad(s % 60)}`;
}

/** 1 小时 8 分 / 12 分 3 秒 / 45 秒 */
export function formatDuration(secs: number | null): string {
  if (secs == null) return "—";
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (h > 0) return `${h} 小时 ${m} 分`;
  if (m > 0) return `${m} 分 ${s} 秒`;
  return `${s} 秒`;
}

/** RFC3339 → "2026-07-03 15:04"；空串（元数据损坏）→ "—" */
export function formatDate(rfc3339: string): string {
  if (!rfc3339) return "—";
  const d = new Date(rfc3339);
  if (isNaN(d.getTime())) return "—";
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export const editSegment = (noteId: string, seq: number, expectedText: string, newText: string) =>
  invoke<void>("edit_segment", { noteId, seq, expectedText, newText });
export const deleteSegment = (noteId: string, seq: number, expectedText: string) =>
  invoke<void>("delete_segment", { noteId, seq, expectedText });
/** 返回实际生效的 speaker id（speakerId="new" 时为后端分配的新 id） */
export const setSegmentSpeaker = (noteId: string, seq: number, expectedText: string, speakerId: string) =>
  invoke<string>("set_segment_speaker", { noteId, seq, expectedText, speakerId });

/** 说话人 id 排序：S2 < S10（数值序）；非 S<n> 形态沉底按字典序。 */
export function speakerIdCompare(a: string, b: string): number {
  const num = (id: string) => {
    const n = parseInt(id.replace(/^S/, ""), 10);
    return Number.isFinite(n) && n > 0 ? n : Number.MAX_SAFE_INTEGER;
  };
  return num(a) - num(b) || a.localeCompare(b);
}
