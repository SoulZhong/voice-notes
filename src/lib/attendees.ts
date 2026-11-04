import type { NoteMeta } from "$lib/notes";
import type { PersonSummary } from "$lib/people";

/** 与会人员名单条目(说话人条一键指认用):email 供指认学习回路,空串 = 手动与会人。 */
export type AttendeeEntry = { name: string; email: string };
type AttendeeLike = { name: string; email: string };

/** 日历参会人显示名解析所需的声纹库索引(由人物列表一次建好,笔记页与录制页共用)。 */
export type AttendeeIndex = {
  /** 档案邮箱(小写)→ 人名:确认关联时记过邮箱,wangyuqi@… → 王宇琪。 */
  byEmail: Map<string, string>;
  /** 人名全拼(小写无分隔)→ 人名:拼音邮箱前缀的兜底。 */
  byPinyin: Map<string, string>;
};

export function buildAttendeeIndex(people: PersonSummary[] | null | undefined): AttendeeIndex {
  const byEmail = new Map<string, string>();
  const byPinyin = new Map<string, string>();
  for (const p of people ?? []) {
    if (!p.name) continue;
    for (const e of p.emails ?? []) byEmail.set(e.toLowerCase(), p.name);
    if (p.name_pinyin) byPinyin.set(p.name_pinyin, p.name);
  }
  return { byEmail, byPinyin };
}

/** 拼音兜底:邮箱前缀(去点/去数字)与库中人名全拼比对;带点的企微前缀是
    「名.姓」序(zhenjie.yang),再试倒序拼接(yangzhenjie→杨振杰)。 */
export function pinyinResolve(email: string, idx: AttendeeIndex): string | null {
  const local = email.split("@")[0]?.toLowerCase().replace(/\d+/g, "") ?? "";
  if (!local) return null;
  const parts = local.split(".").filter(Boolean);
  const forms = new Set<string>([parts.join(""), [...parts].reverse().join("")]);
  for (const f of forms) {
    const hit = idx.byPinyin.get(f);
    if (hit) return hit;
  }
  return null;
}

/** 日历参会人显示名(2026-09-18 用户实报:企微订阅日历参会人无姓名,邮箱前缀是拼音):
    日历自带名 > 声纹库邮箱命中的人名 > 拼音命中的人名 > 邮箱前缀兜底。 */
export function attendeeDisplay(a: AttendeeLike, idx: AttendeeIndex): string {
  return (
    a.name ||
    idx.byEmail.get(a.email.toLowerCase()) ||
    pinyinResolve(a.email, idx) ||
    a.email.split("@")[0] ||
    ""
  ).trim();
}

/** 本篇隐藏名单的键集合(键=日历参会人的邮箱或名字,小写)。 */
export function removedAttendeeKeys(meta: Pick<NoteMeta, "attendees_removed"> | null | undefined): Set<string> {
  return new Set(meta?.attendees_removed ?? []);
}

/** 该日历参会人是否被本篇隐藏(没到场的从展示/指认/识别先验同步剔除)。 */
export function isAttendeeRemoved(a: AttendeeLike, removed: Set<string>): boolean {
  return removed.has(a.email.toLowerCase()) || removed.has(a.name.toLowerCase());
}

/** 与会人员名单:日历参会人(排除「我」与已隐藏者,按上面的解析出显示名,附邮箱供
    学习回路)+ 手动录入,按显示名去重、保持先后。 */
export function attendeeEntries(
  meta: Pick<NoteMeta, "calendar" | "attendees" | "attendees_removed"> | null | undefined,
  idx: AttendeeIndex,
): AttendeeEntry[] {
  const removed = removedAttendeeKeys(meta);
  const out: AttendeeEntry[] = [];
  for (const a of meta?.calendar?.attendees ?? []) {
    if (a.is_me || isAttendeeRemoved(a, removed)) continue;
    const n = attendeeDisplay(a, idx);
    if (n && !out.some((x) => x.name === n)) out.push({ name: n, email: a.email });
  }
  for (const n of meta?.attendees ?? []) {
    if (n && !out.some((x) => x.name === n)) out.push({ name: n, email: "" });
  }
  return out;
}
