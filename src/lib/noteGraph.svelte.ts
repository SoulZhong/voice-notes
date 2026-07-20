import { noteGraphData, type GraphData } from "./graph";
import {
  EMPTY_NOTE_GRAPH,
  loadNoteGraph,
  type NoteGraphSnapshot,
  type NoteGraphStatus,
} from "./noteGraphLoader";

/**
 * 文章视角的共享懒加载态。Sidebar 和 /graph 主画布同时消费同一个实例，避免一边
 * 重试成功、另一边仍停在旧空态；ready（包括成功的空图）会话内缓存，error 可重试。
 */
export class NoteGraphState implements NoteGraphSnapshot {
  data = $state<GraphData>(EMPTY_NOTE_GRAPH);
  status = $state<NoteGraphStatus>("idle");

  async load(): Promise<void> {
    await loadNoteGraph(this, noteGraphData);
  }
}

export const noteGraphState = new NoteGraphState();
