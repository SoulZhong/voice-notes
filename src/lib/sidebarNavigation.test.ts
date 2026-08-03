import { describe, expect, it } from "vitest";
import { zh } from "./i18n/dict/shell";

const sources = import.meta.glob("./Sidebar.svelte", {
  eager: true,
  query: "?raw",
  import: "default",
}) as Record<string, string>;

describe("sidebar route navigation", () => {
  it("preloads the hooks route before a click from the AI tab", () => {
    const sidebar = sources["./Sidebar.svelte"];

    // 页签文案已抽 i18n:源码断言 t() 键的结构模式,中文值从 zh 字典断言。
    expect(zh["shell.tab.hooks"]).toBe("钩子");
    expect(sidebar).toMatch(
      /<a[\s\S]*?class="vtab"[\s\S]*?href="\/hooks"[\s\S]*?data-sveltekit-preload-code="eager"[\s\S]*?>\{t\("shell\.tab\.hooks"\)\}<\/a\s*>/,
    );
    expect(sidebar).toMatch(
      /<a[\s\S]*?class="vtab vtab-upright"[\s\S]*?href="\/ai"[\s\S]*?>AI<\/a\s*>/,
    );
  });
});
