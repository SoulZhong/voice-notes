import { describe, expect, it } from "vitest";
import { buildTidyQueue, mergeDuplicatePeople, splitArchive, tidyItemKey, type TidyItem } from "./tidyQueue";
import type { MergeReceipt, PersonMergeSuggestion, PersonSummary } from "./people";

const person = (id: string, name = "", samples: string[] = []): PersonSummary => ({
  id,
  name,
  total_ms: 60_000,
  last_seen: "2026-07-31T10:00:00+08:00",
  sources: ["mic"],
  sample_paths: samples,
  sample_dates: samples.map(() => ""),
});

const sug = (loser: string, winner: string): PersonMergeSuggestion => ({
  loser,
  loser_name: "",
  winner,
  winner_name: "张三",
  similarity: 0.7,
  source: "mic",
  salience: null,
});

const receipt = (id: string, invalidReason: string | null = null): MergeReceipt => ({
  journal_id: id,
  time: "t",
  origin: "auto",
  loser: "P1",
  loser_name: "",
  winner: "P2",
  winner_name: "张三",
  similarity: 0.9,
  loser_sample_paths: [],
  winner_sample_paths: [],
  invalid_reason: invalidReason,
});

describe("buildTidyQueue", () => {
  it("按 回执→建议→同名组→无样本 排序,同名分组、无样本逐条", () => {
    const people = [
      person("P2", "张三", ["/a.wav"]),
      person("P3", "张三", ["/b.wav"]),
      person("P4", "", []),
      person("P5", "李四", ["/c.wav"]),
    ];
    const q = buildTidyQueue(people, [sug("P4", "P5")], [receipt("m-P1")]);
    expect(q.map((i) => i.kind)).toEqual(["receipt", "suggestion", "dup", "nosample"]);
    const dup = q[2] as Extract<TidyItem, { kind: "dup" }>;
    expect(dup.name).toBe("张三");
    expect(dup.people.map((p) => p.id)).toEqual(["P2", "P3"]);
  });

  it("dismissed 集合滤掉同名组与无样本条目", () => {
    const people = [person("P2", "张三"), person("P3", "张三"), person("P4", "", [])];
    const all = buildTidyQueue(people, [], []);
    expect(all).toHaveLength(4); // dup(张三) 1 + nosample(P2/P3/P4 均无样本) 3
    const q = buildTidyQueue(people, [], [], new Set(["d:张三", "n:P4"]));
    expect(q.every((i) => tidyItemKey(i) !== "d:张三" && tidyItemKey(i) !== "n:P4")).toBe(true);
  });

  it("同名组不设样本条件:全员无样本也成组,且与无样本卡共存", () => {
    const people = [person("P2", "张三"), person("P3", "张三")];
    const q = buildTidyQueue(people, [], []);
    expect(q.map((i) => i.kind)).toEqual(["dup", "nosample", "nosample"]);
  });

  it("key 稳定且各类型互不冲突", () => {
    expect(tidyItemKey({ kind: "suggestion", suggestion: sug("P4", "P5") })).toBe("s:P4>P5");
    expect(tidyItemKey({ kind: "receipt", receipt: receipt("m-P1") })).toBe("r:m-P1");
    expect(tidyItemKey({ kind: "nosample", person: person("P4") })).toBe("n:P4");
    expect(tidyItemKey({ kind: "dup", name: "张三", people: [] })).toBe("d:张三");
  });
});

describe("buildTidyQueue 失效目标过滤", () => {
  it("winner 不在库的建议被滤掉(已合并/已删除的人不能再作合并目标)", () => {
    const people = [person("P1")];
    const q = buildTidyQueue(people, [sug("P1", "P9")], [], new Set());
    expect(q.filter((i) => i.kind === "suggestion")).toEqual([]);
  });

  it("loser 不在库的建议同样滤掉", () => {
    const people = [person("P9")];
    const q = buildTidyQueue(people, [sug("P1", "P9")], [], new Set());
    expect(q.filter((i) => i.kind === "suggestion")).toEqual([]);
  });

  it("双方都在库的建议保留", () => {
    const people = [person("P1"), person("P9")];
    const q = buildTidyQueue(people, [sug("P1", "P9")], [], new Set());
    expect(q.filter((i) => i.kind === "suggestion")).toHaveLength(1);
  });
});

describe("splitArchive", () => {
  it("失效回执进 archived,其余进 pending,各自保持队列序", () => {
    const items = buildTidyQueue(
      [person("P1")],
      [],
      [receipt("J1"), receipt("J2", "相关人物随后又被合并"), receipt("J3")],
      new Set(),
    );
    const { pending, archived } = splitArchive(items);
    expect(archived.map((i) => i.kind === "receipt" && i.receipt.journal_id)).toEqual(["J2"]);
    expect(pending.filter((i) => i.kind === "receipt")).toHaveLength(2);
  });

  it("空输入两侧都空", () => {
    expect(splitArchive([])).toEqual({ pending: [], archived: [] });
  });
});

describe("mergeDuplicatePeople", () => {
  it("后续合并失败时仍发布最近一次成功合并的撤销 id", async () => {
    const published: string[] = [];
    const merge = async (loser: string) => {
      if (loser === "P3") throw new Error("第二条失败");
      return `m-${loser}`;
    };

    await expect(
      mergeDuplicatePeople(
        [person("P1", "张三"), person("P2", "张三"), person("P3", "张三")],
        "P1",
        merge,
        (journalId) => published.push(journalId),
      ),
    ).rejects.toThrow("第二条失败");
    expect(published).toEqual(["m-P2"]);
  });
});
