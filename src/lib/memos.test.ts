import { describe, expect, it } from "vitest";
import {
  anchorIndex,
  clampAt,
  fmtClock,
  inCut,
  memoColumnOpen,
  memoKeyAllowed,
  mergeTimeline,
  parseClock,
  type Memo,
} from "./memos";

const memo = (at_ms: number, text = "x", created_at = "a"): Memo => ({
  id: `m${at_ms}${created_at}`,
  at_ms,
  text,
  origin: "note",
  created_at,
  updated_at: created_at,
});

describe("mergeTimeline", () => {
  const blocks = [{ s: 0 }, { s: 10_000 }, { s: 20_000 }];
  const kinds = (memos: Memo[]) =>
    mergeTimeline(blocks, memos, (b) => b.s).map((x) => (x.kind === "block" ? `b${x.item.s / 1000}` : `m${x.memo.at_ms / 1000}`));

  it("备注落在最后一个 start<=at 的块之后", () => {
    expect(kinds([memo(5_000), memo(10_000), memo(25_000)])).toEqual(["b0", "m5", "b10", "m10", "b20", "m25"]);
  });
  it("早于第一块的排最前(第一块不从 0 开始时)", () => {
    const out = mergeTimeline([{ s: 3_000 }], [memo(1_000)], (b) => b.s).map((x) => x.kind);
    expect(out).toEqual(["memo", "block"]);
  });
  it("同一时刻按 created_at 排序", () => {
    const out = mergeTimeline([], [memo(1, "b", "2"), memo(1, "a", "1")], () => 0);
    expect(out.map((x) => (x.kind === "memo" ? x.memo.text : ""))).toEqual(["a", "b"]);
  });
  it("无时间跨度的块不推进游标", () => {
    const bs = [{ s: 0 as number | null }, { s: null }, { s: 9_000 }];
    const out = mergeTimeline(bs, [memo(5_000)], (b) => b.s).map((x) => x.kind);
    expect(out).toEqual(["block", "block", "memo", "block"]);
  });
});

describe("anchorIndex", () => {
  it("返回最后一个 start<=at 的块下标,早于所有块为 -1", () => {
    expect(anchorIndex([0, 10, 20], 15)).toBe(1);
    expect(anchorIndex([5, 10], 1)).toBe(-1);
    expect(anchorIndex([0, null, 20], 25)).toBe(2);
    expect(anchorIndex([0, 10], 10)).toBe(1);
  });
});

describe("删减与时间", () => {
  it("半开区间", () => {
    const cuts = [{ start_ms: 1_000, end_ms: 2_000 }];
    expect(inCut(1_000, cuts)).toBe(true);
    expect(inCut(1_999, cuts)).toBe(true);
    expect(inCut(2_000, cuts)).toBe(false);
  });
  it("钳位", () => {
    expect(clampAt(-5, 100)).toBe(0);
    expect(clampAt(500, 100)).toBe(100);
  });
  it("时钟格式与解析互逆", () => {
    expect(fmtClock(754_000)).toBe("12:34");
    expect(fmtClock(3_754_000)).toBe("1:02:34");
    expect(parseClock("12:34")).toBe(754_000);
    expect(parseClock("1:02:34")).toBe(3_754_000);
    expect(parseClock("90")).toBe(90_000);
    expect(parseClock("1:75")).toBeNull();
    expect(parseClock("abc")).toBeNull();
  });
});

describe("M 键守卫", () => {
  const key = (over: Partial<KeyboardEvent> = {}) =>
    ({ key: "m", altKey: false, ctrlKey: false, metaKey: false, shiftKey: false, isComposing: false, repeat: false, ...over }) as KeyboardEvent;
  const outside = { closest: () => null };
  const inEditor = { closest: (sel: string) => (sel.includes(".ProseMirror") ? {} : null) };
  it("普通按 M 放行", () => expect(memoKeyAllowed(key(), outside, false)).toBe(true));
  it("修饰键/输入法/重复不响应", () => {
    expect(memoKeyAllowed(key({ metaKey: true }), outside, false)).toBe(false);
    expect(memoKeyAllowed(key({ shiftKey: true }), outside, false)).toBe(false);
    expect(memoKeyAllowed(key({ isComposing: true }), outside, false)).toBe(false);
    expect(memoKeyAllowed(key({ repeat: true }), outside, false)).toBe(false);
  });
  it("焦点在编辑区或本笔记录制中不响应", () => {
    expect(memoKeyAllowed(key(), inEditor, false)).toBe(false);
    expect(memoKeyAllowed(key(), outside, true)).toBe(false);
    expect(memoKeyAllowed(key({ key: "n" }), outside, false)).toBe(false);
  });
});

describe("备注栏默认展开", () => {
  it("按宽度默认,手动选择优先", () => {
    expect(memoColumnOpen(1000, null)).toBe(true);
    expect(memoColumnOpen(800, null)).toBe(false);
    expect(memoColumnOpen(800, true)).toBe(true);
    expect(memoColumnOpen(1200, false)).toBe(false);
  });
});
