import { describe, expect, it } from "vitest";
import {
  attendeeDisplay,
  attendeeEntries,
  buildAttendeeIndex,
  isAttendeeRemoved,
  pinyinResolve,
  removedAttendeeKeys,
} from "./attendees";
import type { NoteMeta } from "./notes";
import type { PersonSummary } from "./people";

const person = (id: string, name: string, extra: Partial<PersonSummary> = {}): PersonSummary => ({
  id,
  name,
  total_ms: 60_000,
  last_seen: "2026-11-01T10:00:00+08:00",
  sources: ["mic"],
  sample_paths: [],
  sample_dates: [],
  sample_notes: [],
  ...extra,
});

const idx = buildAttendeeIndex([
  person("P1", "王宇琪", { emails: ["WangYuQi@corp.com"], name_pinyin: "wangyuqi" }),
  person("P2", "杨振杰", { name_pinyin: "yangzhenjie" }),
  person("P3", "", { emails: ["noname@corp.com"], name_pinyin: "noname" }),
]);

type Meta = Pick<NoteMeta, "calendar" | "attendees" | "attendees_removed">;
const cal = (attendees: { name: string; email: string; is_me: boolean }[]): Meta["calendar"] => ({
  event_id: "e1",
  title: "t",
  attendees,
  matched_at: "",
  match_kind: "auto",
});

describe("buildAttendeeIndex", () => {
  it("邮箱归一小写,无名人物不进索引", () => {
    expect(idx.byEmail.get("wangyuqi@corp.com")).toBe("王宇琪");
    expect(idx.byEmail.has("noname@corp.com")).toBe(false);
    expect(idx.byPinyin.has("noname")).toBe(false);
  });

  it("人物列表缺省给空索引", () => {
    const e = buildAttendeeIndex(undefined);
    expect(e.byEmail.size).toBe(0);
    expect(e.byPinyin.size).toBe(0);
  });
});

describe("pinyinResolve", () => {
  it("去数字后全拼命中", () => {
    expect(pinyinResolve("wangyuqi01@corp.com", idx)).toBe("王宇琪");
  });

  it("「名.姓」序倒拼命中", () => {
    expect(pinyinResolve("zhenjie.yang@corp.com", idx)).toBe("杨振杰");
  });

  it("前缀为空或不命中给 null", () => {
    expect(pinyinResolve("@corp.com", idx)).toBeNull();
    expect(pinyinResolve("someone@corp.com", idx)).toBeNull();
  });
});

describe("attendeeDisplay", () => {
  it("日历自带名优先", () => {
    expect(attendeeDisplay({ name: " 张三 ", email: "wangyuqi@corp.com" }, idx)).toBe("张三");
  });

  it("其次邮箱命中,不分大小写", () => {
    expect(attendeeDisplay({ name: "", email: "WANGYUQI@corp.com" }, idx)).toBe("王宇琪");
  });

  it("再次拼音,最后邮箱前缀兜底", () => {
    expect(attendeeDisplay({ name: "", email: "zhenjie.yang@corp.com" }, idx)).toBe("杨振杰");
    expect(attendeeDisplay({ name: "", email: "lisi@corp.com" }, idx)).toBe("lisi");
    expect(attendeeDisplay({ name: "", email: "" }, idx)).toBe("");
  });
});

describe("isAttendeeRemoved", () => {
  it("按邮箱或名字(小写)匹配隐藏名单", () => {
    const removed = removedAttendeeKeys({ attendees_removed: ["a@x.com", "bob"] });
    expect(isAttendeeRemoved({ name: "", email: "A@x.com" }, removed)).toBe(true);
    expect(isAttendeeRemoved({ name: "Bob", email: "b@x.com" }, removed)).toBe(true);
    expect(isAttendeeRemoved({ name: "Carol", email: "c@x.com" }, removed)).toBe(false);
  });

  it("meta 缺省给空集合", () => {
    expect(removedAttendeeKeys(null).size).toBe(0);
  });
});

describe("attendeeEntries", () => {
  it("排除「我」与已隐藏者,日历在前、手动在后,按显示名去重", () => {
    const meta: Meta = {
      calendar: cal([
        { name: "", email: "me@corp.com", is_me: true },
        { name: "", email: "wangyuqi@corp.com", is_me: false },
        { name: "", email: "gone@corp.com", is_me: false },
        { name: "王宇琪", email: "dup@corp.com", is_me: false },
        { name: "", email: "zhenjie.yang@corp.com", is_me: false },
      ]),
      attendees: ["赵六", "杨振杰", "", "赵六"],
      attendees_removed: ["gone@corp.com"],
    };
    expect(attendeeEntries(meta, idx)).toEqual([
      { name: "王宇琪", email: "wangyuqi@corp.com" },
      { name: "杨振杰", email: "zhenjie.yang@corp.com" },
      { name: "赵六", email: "" },
    ]);
  });

  it("没有日历与手动名单时为空", () => {
    expect(attendeeEntries(null, idx)).toEqual([]);
    expect(attendeeEntries({ calendar: null }, idx)).toEqual([]);
  });
});
