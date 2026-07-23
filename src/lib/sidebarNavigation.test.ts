import { describe, expect, it } from "vitest";

const sources = import.meta.glob("./Sidebar.svelte", {
  eager: true,
  query: "?raw",
  import: "default",
}) as Record<string, string>;

describe("sidebar route navigation", () => {
  it("preloads the hooks route before a click from the AI tab", () => {
    const sidebar = sources["./Sidebar.svelte"];

    expect(sidebar).toMatch(
      /<a[\s\S]*?class="vtab"[\s\S]*?href="\/hooks"[\s\S]*?data-sveltekit-preload-code="eager"[\s\S]*?>钩子<\/a\s*>/,
    );
    expect(sidebar).toMatch(
      /<a[\s\S]*?class="vtab vtab-upright"[\s\S]*?href="\/ai"[\s\S]*?>AI<\/a\s*>/,
    );
  });
});
