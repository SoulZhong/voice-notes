import { describe, expect, it, vi } from "vitest";
import { EMPTY_NOTE_GRAPH, loadNoteGraph, type NoteGraphSnapshot } from "./noteGraphLoader";
import type { GraphData } from "./graph";

describe("loadNoteGraph", () => {
  it("并发只发一次 IPC，失败后允许重试并共享成功结果", async () => {
    const graph: GraphData = {
      nodes: [
        {
          id: "n1",
          kind: "note",
          name: "设计评审",
          aliases: [],
          is_person: false,
          note_count: 2,
          mention_total: 3,
        },
      ],
      edges: [],
    };
    const fetchGraph = vi
      .fn()
      .mockRejectedValueOnce(new Error("temporary"))
      .mockResolvedValueOnce(graph);
    const state: NoteGraphSnapshot = { data: EMPTY_NOTE_GRAPH, status: "idle" };

    const failed = loadNoteGraph(state, fetchGraph);
    const simultaneousFailure = loadNoteGraph(state, fetchGraph);
    await Promise.all([failed, simultaneousFailure]);
    expect(state.status).toBe("error");
    expect(fetchGraph).toHaveBeenCalledTimes(1);

    const retry = loadNoteGraph(state, fetchGraph);
    const simultaneousRetry = loadNoteGraph(state, fetchGraph);
    await Promise.all([retry, simultaneousRetry]);
    expect(state.status).toBe("ready");
    expect(state.data).toBe(graph);
    expect(fetchGraph).toHaveBeenCalledTimes(2);

    await loadNoteGraph(state, fetchGraph);
    expect(fetchGraph).toHaveBeenCalledTimes(2);
  });
});
