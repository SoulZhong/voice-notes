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

export type Note = { meta: NoteMeta; segments: SegmentRecord[]; skipped_lines: number };

export const listNotes = () => invoke<NoteSummary[]>("list_notes");
export const getNote = (id: string) => invoke<Note>("get_note", { id });
export const renameNote = (id: string, title: string) =>
  invoke<void>("rename_note", { id, title });
export const deleteNote = (id: string) => invoke<void>("delete_note", { id });
/** 返回导出文件绝对路径 */
export const exportNote = (id: string, format: "md" | "txt") =>
  invoke<string>("export_note", { id, format });

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
