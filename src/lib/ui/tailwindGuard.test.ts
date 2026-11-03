import { describe, expect, it } from "vitest";
// 护栏(UI 改版 #730):Tailwind 接入的三条纪律,改掉任何一条都会悄悄改坏旧页面或组件。
// CSS 的 ?raw 导入在 vitest 里被 CSS 管线吃掉、拿到空串,所以直接读文件
// (本仓没装 @types/node,照 systemIcons.test.ts 的写法取内置 fs)。
type Fs = { readFileSync(path: string, encoding: "utf8"): string };
type Runtime = typeof globalThis & { process: { cwd(): string; getBuiltinModule(name: "fs"): Fs } };
const runtime = globalThis as Runtime;
const css = runtime.process.getBuiltinModule("fs").readFileSync(`${runtime.process.cwd()}/src/lib/ui/tailwind.css`, "utf8");

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
