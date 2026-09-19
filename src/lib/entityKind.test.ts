import { describe, expect, it } from "vitest";
import {
  ENTITY_KINDS,
  entityKind,
  entityMentionCssVars,
  entityMentionStyle,
  normalizeEntityKind,
} from "./entityKind";

describe("实体类型配色", () => {
  /** 这条是本模块存在的唯一理由:正文提及与头顶那枚 chip 必须**逐字同色**,
      用户才能认出"这个词"和"那枚 chip"是同一个东西。两边各存一份颜色表迟早漂,
      所以只留一份,并在这里锁死"同一 kind 取到同一组色值"。 */
  it("chip 与正文提及取到同一组色值", () => {
    for (const k of ENTITY_KINDS) {
      const chip = entityKind(k.key);
      const mention = entityMentionStyle(k.key);
      expect(chip?.tint).toBe(mention.tint);
      expect(chip?.ink).toBe(mention.ink);
    }
  });

  it("concept 归一到 term(后端历史产物)", () => {
    expect(normalizeEntityKind("concept")).toBe("term");
    expect(entityKind("concept")?.key).toBe("term");
    expect(entityMentionStyle("concept")).toEqual(entityMentionStyle("term"));
  });

  /** 四类之外(date/task/place,或模型新造的词):chip 行过滤掉不展示,正文**仍要**
      上色——"这个词被认出来了"不该因为类型不在白名单里就从正文里消失。 */
  it("白名单外的类型:chip 不收,正文回落中性色", () => {
    expect(entityKind("date")).toBeUndefined();
    expect(entityMentionStyle("date")).toEqual(entityMentionStyle("term"));
    expect(entityMentionStyle("")).toEqual(entityMentionStyle("term"));
  });

  /** 行内变量是 schema 与样式表之间的唯一契约:样式表只认 --ent-tint/--ent-ink,
      不按类型名写选择器。变量名写错了整套配色会静默回落中性灰,故此处钉死。 */
  it("行内 CSS 变量带上两个约定变量名", () => {
    const css = entityMentionCssVars("person");
    expect(css).toContain("--ent-tint:");
    expect(css).toContain("--ent-ink:");
    expect(css).toContain("var(--tint-sky)");
    expect(css).toContain("var(--tint-sky-ink)");
  });

  it("四类各自的色相互不相同(同色就等于这类信息没了)", () => {
    const inks = ENTITY_KINDS.map((k) => k.ink);
    expect(new Set(inks).size).toBe(ENTITY_KINDS.length);
  });
});
