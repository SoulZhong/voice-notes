import { describe, expect, it } from "vitest";
import { clampAt, fmtClock, inCut, memoColumnOpen, memoKeyAllowed, parseClock, sortMemos, type Memo } from "./memos";

const memo = (at_ms: number, line: number): Memo => ({ id: `L${line}`, at_ms, text: "x", md: "x", line });

describe("sortMemos", () => {
  it("按时间,同时间按行号", () => {
    expect(sortMemos([memo(5_000, 3), memo(1_000, 9), memo(5_000, 1)]).map((m) => m.id)).toEqual(["L9", "L1", "L3"]);
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
