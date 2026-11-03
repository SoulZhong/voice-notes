import { describe, expect, it } from "vitest";
// 护栏(UI 改版 #730):Tailwind 接入的三条纪律,改掉任何一条都会悄悄改坏旧页面或组件。
import css from "./tailwind.css?raw";

describe("Tailwind 接入纪律", () => {
  it("不引 Preflight(全局 reset 会改旧页面与 ProseMirror 正文)", () => {
    expect(css).not.toMatch(/@import\s+"tailwindcss"\s*;/);
    expect(css).not.toContain("preflight");
  });
  it("层序先声明,组件 reset 排在 utilities 之前", () => {
    expect(css).toMatch(/@layer theme, base, components, utilities;/);
    expect(css.indexOf("@layer theme, base")).toBeLessThan(css.indexOf("@layer base {"));
  });
  it("只扫新组件:source(none) + 显式 @source", () => {
    expect(css).toMatch(/utilities\.css" layer\(utilities\) source\(none\)/);
    expect(css).toContain('@source "./";');
  });
  it("shadcn 的 accent 映射到悬停底色,不碰本应用的 --accent(链接/焦点)", () => {
    expect(css).toContain("--color-accent: var(--surface-press);");
    expect(css).not.toMatch(/^\s*--accent\s*:/m);
  });
});
