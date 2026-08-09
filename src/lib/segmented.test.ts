import { describe, expect, it } from "vitest";
import { nextEnabledIndex } from "./segmented";

describe("nextEnabledIndex(segmented 方向键导航)", () => {
  it("正向走到下一个可选段", () => {
    expect(nextEnabledIndex([{}, {}], 0, 1)).toBe(1);
  });
  it("到末尾环绕回开头", () => {
    expect(nextEnabledIndex([{}, {}], 1, 1)).toBe(0);
  });
  it("跳过 disabled 段", () => {
    expect(nextEnabledIndex([{}, { disabled: true }, {}], 0, 1)).toBe(2);
  });
  it("跳过 momentary 动作段(如「生成成品轨」)——环绕一圈只剩自己", () => {
    expect(nextEnabledIndex([{}, { momentary: true }], 0, 1)).toBe(0);
  });
  it("反向同样跳过不可选段", () => {
    expect(nextEnabledIndex([{}, { disabled: true }, {}], 2, -1)).toBe(0);
  });
  it("空列表原样返回 current", () => {
    expect(nextEnabledIndex([], -1, 1)).toBe(-1);
  });
});
