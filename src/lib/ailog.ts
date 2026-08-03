// AI 调用日志:后端 ailog.rs 的前端封装。真值源是 data_root/ai_logs/ 目录,
// 前端不缓存,查询/导出都现调。
import { invoke } from "@tauri-apps/api/core";

export type AiLogEntry = {
  schema_version: number;
  id: string;
  ts: string;
  kind: "refine_chunk" | "title" | "agent_refine" | "mcp_apply" | string;
  provider: string;
  note_id?: string | null;
  model?: string | null;
  endpoint?: string | null;
  request: unknown;
  response: unknown;
  status: "ok" | "error" | string;
  error?: string | null;
  duration_ms: number;
  truncated?: boolean;
};

export type AiLogFilter = {
  kind?: string;
  note_id?: string;
  from?: string;
  to?: string;
  offset?: number;
  limit?: number;
};

export type AiLogPage = { total: number; entries: AiLogEntry[] };

export const aiLogsQuery = (filter: AiLogFilter = {}) => invoke<AiLogPage>("ai_logs_query", { filter });
/** 全量导出 JSONL,返回 { path, count }。 */
export const aiLogsExport = () => invoke<{ path: string; count: number }>("ai_logs_export");
/** 在访达中打开日志目录(不存在则先创建),返回目录路径。 */
export const aiLogsOpenDir = () => invoke<string>("ai_logs_open_dir");

/** 日志类别 → i18n 键(ai 分片);渲染处用 t() 取当前语言文案。 */
export const AI_LOG_KIND_LABELS: Record<string, string> = {
  refine_chunk: "ai.logs.kind.refine_chunk",
  title: "ai.logs.kind.title",
  agent_refine: "ai.logs.kind.agent_refine",
  mcp_apply: "ai.logs.kind.mcp_apply",
};
