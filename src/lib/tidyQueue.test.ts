import { describe, expect, it } from "vitest";
import { buildTidyQueue, keyCommand, orderWithSkips, tidyItemKey, type TidyItem } from "./tidyQueue";
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

const receipt = (id: string): MergeReceipt => ({
  journal_id: id,
  time: "t",
  origin: "auto",
  loser: "P1",
  loser_name: "",
  winner: "P2",
  winner_name: "张三",
  similarity: 0.9,
  invalid_reason: null,
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

describe("orderWithSkips", () => {
  it("跳过的挪到队尾并保持跳过先后序", () => {
    const items = buildTidyQueue(
      [person("P4", "", []), person("P5", "", []), person("P6", "", [])],
      [],
      [],
    );
    const ordered = orderWithSkips(items, ["n:P4", "n:P5"]);
    expect(ordered.map(tidyItemKey)).toEqual(["n:P6", "n:P4", "n:P5"]);
  });
});

describe("keyCommand", () => {
  it("Enter=主动作,S=跳过,数字=试听", () => {
    expect(keyCommand("Enter", "suggestion")).toBe("primary");
    expect(keyCommand("s", "receipt")).toBe("skip");
    expect(keyCommand("1", "suggestion")).toEqual({ play: 0 });
    expect(keyCommand("2", "receipt")).toEqual({ play: 1 });
  });
  it("X 忽略/保留,但回执卡无 X(撤销只走点击,防误触)", () => {
    expect(keyCommand("x", "suggestion")).toBe("dismiss");
    expect(keyCommand("X", "nosample")).toBe("dismiss");
    expect(keyCommand("x", "receipt")).toBeNull();
  });
  it("数字键上限:双栏卡 1-2,同名组卡 1-9", () => {
    expect(keyCommand("3", "suggestion")).toBeNull();
    expect(keyCommand("9", "dup")).toEqual({ play: 8 });
    expect(keyCommand("0", "dup")).toBeNull();
  });
  it("无样本卡数字键返回 null(没有可试听的)", () => {
    expect(keyCommand("1", "nosample")).toBeNull();
    expect(keyCommand("2", "nosample")).toBeNull();
  });
});
