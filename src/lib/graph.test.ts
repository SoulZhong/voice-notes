import { describe, it, expect } from "vitest";
import { kindLabel } from "./graph";

describe("kindLabel", () => {
  it("已知 kind 给中文标签", () => {
    expect(kindLabel("person")).toBe("人");
    expect(kindLabel("org")).toBe("组织");
    expect(kindLabel("project")).toBe("项目");
    expect(kindLabel("product")).toBe("产品");
    expect(kindLabel("term")).toBe("术语");
    expect(kindLabel("decision")).toBe("决议");
    expect(kindLabel("task")).toBe("任务");
    expect(kindLabel("place")).toBe("地点");
    expect(kindLabel("date")).toBe("日期");
  });
  it("未知 kind 原样返回(前向兼容,不吞新类型)", () => {
    expect(kindLabel("tool")).toBe("tool");
    expect(kindLabel("")).toBe("");
  });
});
