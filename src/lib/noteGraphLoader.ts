import type { GraphData } from "./graph";

export type NoteGraphStatus = "idle" | "loading" | "ready" | "error";
export const EMPTY_NOTE_GRAPH: GraphData = { nodes: [], edges: [] };

export interface NoteGraphSnapshot {
  data: GraphData;
  status: NoteGraphStatus;
}

/** 文章图谱共享态的纯状态机；与 Svelte 响应式包装分离，方便覆盖并发和失败恢复。 */
export async function loadNoteGraph(
  target: NoteGraphSnapshot,
  fetchGraph: () => Promise<GraphData>,
): Promise<void> {
  if (target.status === "loading" || target.status === "ready") return;
  target.status = "loading";
  try {
    target.data = await fetchGraph();
    target.status = "ready";
  } catch {
    target.data = EMPTY_NOTE_GRAPH;
    target.status = "error";
  }
}
