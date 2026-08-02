import { describe, expect, it } from "vitest";
import { dupNameSet, filterPeople, personLabel, recentLabel, sortPeopleAlpha } from "./personPick";
import type { PersonSummary } from "./people";

const person = (id: string, name = "", lastSeen = "2026-07-31T10:00:00+08:00"): PersonSummary => ({
  id,
  name,
  total_ms: 60_000,
  last_seen: lastSeen,
  sources: ["mic"],
  sample_paths: [],
  sample_dates: [],
});

describe("personLabel", () => {
  it("未命名按全局编号兜底", () => {
    expect(personLabel(person("P3", ""))).toBe("说话人 3");
  });

  it("已命名直接给名字", () => {
    expect(personLabel(person("P3", "张三"))).toBe("张三");
  });
});

describe("recentLabel", () => {
  it("无日期给空串", () => {
    expect(recentLabel(person("P1", "", ""))).toBe("");
  });

  it("有日期给「最近 MM-DD」", () => {
    expect(recentLabel(person("P1", "", "2026-07-31T10:00:00+08:00"))).toBe("最近 07-31");
  });
});

describe("dupNameSet", () => {
  it("同名超过一次才收进集合,未命名不计", () => {
    const people = [person("P1", "张三"), person("P2", "张三"), person("P3", "李四"), person("P4", "")];
    const dup = dupNameSet(people);
    expect(dup.has("张三")).toBe(true);
    expect(dup.has("李四")).toBe(false);
    expect(dup.has("")).toBe(false);
  });
});

describe("filterPeople", () => {
  const people = [person("P1", "张三"), person("P2", "李四"), person("P3", "")];

  it("空查询给全量", () => {
    expect(filterPeople(people, "")).toEqual(people);
    expect(filterPeople(people, "   ")).toEqual(people);
  });

  it("按显示名包含匹配命中", () => {
    expect(filterPeople(people, "张")).toEqual([people[0]]);
  });

  it("未命中返回空数组", () => {
    expect(filterPeople(people, "不存在的名字")).toEqual([]);
  });

  it("全拼命中中文名(排序按拼音,检索也得认拼音)", () => {
    expect(filterPeople(people, "lisi")).toEqual([people[1]]);
    expect(filterPeople(people, "zhangsan")).toEqual([people[0]]);
  });

  it("拼音首字母命中中文名", () => {
    expect(filterPeople(people, "ls")).toEqual([people[1]]);
  });

  it("拼音大小写不敏感", () => {
    expect(filterPeople(people, "ZhangSan")).toEqual([people[0]]);
  });

  it("拼音前缀命中(边输边筛)", () => {
    expect(filterPeople(people, "zhang")).toEqual([people[0]]);
  });

  it("纯数字查询不被拼音误伤,仍按编号匹配", () => {
    expect(filterPeople(people, "3")).toEqual([people[2]]);
  });
});

describe("sortPeopleAlpha", () => {
  it("中文名按拼音序", () => {
    const people = [person("P1", "张三"), person("P2", "陈博"), person("P3", "郎佳奇")];
    expect(sortPeopleAlpha(people).map((p) => p.name)).toEqual(["陈博", "郎佳奇", "张三"]);
  });

  it("未命名按编号数值序,「说话人 2」<「说话人 10」", () => {
    const people = [person("P10", ""), person("P2", "")];
    expect(sortPeopleAlpha(people).map(personLabel)).toEqual(["说话人 2", "说话人 10"]);
  });

  it("混合已命名与未命名不抛错,原数组不被修改", () => {
    const people = [person("P10", ""), person("P1", "张三"), person("P2", "")];
    expect(() => sortPeopleAlpha(people)).not.toThrow();
    const sorted = sortPeopleAlpha(people);
    expect(sorted).toHaveLength(3);
    expect(people.map((p) => p.id)).toEqual(["P10", "P1", "P2"]);
  });
});
