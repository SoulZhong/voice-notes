import { describe, expect, it } from "vitest";

// i18n 覆盖率护栏:字典之外的前端源码不得再出现裸中文(spec 2026-08-02-i18n-design)。
// 注释保持中文是本项目惯例,故先剥注释再检测;剥不干净只会让个别注释被误报,
// 不会让真的漏翻文案蒙混过关——护栏宁可吵一点。
const sources = import.meta.glob("/src/**/*.{svelte,ts}", {
  eager: true,
  query: "?raw",
  import: "default",
}) as Record<string, string>;

/** 字典与本护栏自身之外的源码才受检;测试文件断言中文属正常(它们从字典取值比对)。 */
function isChecked(path: string): boolean {
  if (path.includes("/src/lib/i18n/")) return false;
  if (path.endsWith(".test.ts")) return false;
  return true;
}

/** 把一段匹配内容清空但保留其中的换行——行号必须与原文严格对齐,否则豁免判定会错位。 */
const blank = (m: string) => m.replace(/[^\n]/g, "");

/**
 * 剥掉不该受检的部分:块注释、HTML 注释、整行 `//` 注释、代码尾部 `// ` 注释
 * (要求 // 后跟空白,以免误伤 https:// 这类 URL)、以及 <style> 块(CSS 注释与 content 值)。
 */
export function stripComments(source: string): string {
  return source
    .replace(/<style[\s\S]*?<\/style>/g, blank)
    .replace(/\/\*[\s\S]*?\*\//g, blank)
    .replace(/<!--[\s\S]*?-->/g, blank)
    .replace(/^[ \t]*\/\/.*$/gm, "")
    .replace(/[ \t]+\/\/[ \t].*$/gm, "");
}

const CJK = /[一-鿿]/;

/**
 * 豁免:
 *  - console.* 日志——面向开发者不面向用户,与后端 eprintln 保持同一条界线;
 *  - 显式标注 `i18n-exempt` 的行——跨端判等串、字符区间正则、品牌名等。标注写在
 *    行内注释里,注释会被 stripComments 剥掉,所以判断要在剥之前按原始行做。
 */
function isExempt(rawLine: string): boolean {
  return rawLine.includes("i18n-exempt") || /\bconsole\.\w+\(/.test(rawLine);
}

describe("i18n 覆盖率护栏", () => {
  it("字典外的前端源码不含裸中文(剥注释后)", () => {
    const offenders: string[] = [];
    for (const [path, source] of Object.entries(sources)) {
      if (!isChecked(path)) continue;
      const raw = source.split("\n");
      stripComments(source)
        .split("\n")
        .forEach((line, i) => {
          if (!CJK.test(line)) return;
          if (isExempt(raw[i] ?? "")) return;
          offenders.push(`${path}:${i + 1}  ${line.trim().slice(0, 100)}`);
        });
    }
    expect(offenders, `以下位置仍有未进字典的中文:\n${offenders.join("\n")}`).toEqual([]);
  });

  it("豁免:console 日志与 i18n-exempt 标注不受检,普通文案仍受检", () => {
    expect(isExempt('console.warn("序列化失败");')).toBe(true);
    expect(isExempt('if (String(e).includes("已在录制")) { // i18n-exempt: 后端错误判等')).toBe(true);
    expect(isExempt('<p>没有匹配的人</p>')).toBe(false);
  });

  it("剥注释:块注释/HTML 注释/行注释被剥,URL 与真实文案保留", () => {
    expect(stripComments("/* 中文块注释 */ const a = 1;")).not.toMatch(CJK);
    expect(stripComments("<!-- 中文 HTML 注释 -->")).not.toMatch(CJK);
    expect(stripComments("  // 整行中文注释")).not.toMatch(CJK);
    expect(stripComments('const a = 1; // 尾部中文注释')).not.toMatch(CJK);
    expect(stripComments('<style>.a { content: "中文"; }</style>')).not.toMatch(CJK);
    // URL 里的 // 不是注释,其后的中文(若有)必须仍被看见。
    expect(stripComments('const u = "https://x/中文";')).toMatch(CJK);
    expect(stripComments('<p>{t("a.b")}</p>中文')).toMatch(CJK);
  });
});
