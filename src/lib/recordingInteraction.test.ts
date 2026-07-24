import { describe, expect, it } from "vitest";

const sources = import.meta.glob(
  ["./recording.svelte.ts", "./Sidebar.svelte", "../routes/record/+page.svelte"],
  { eager: true, query: "?raw", import: "default" },
) as Record<string, string>;

describe("recording stop feedback", () => {
  it("shows a stopping state immediately while durable shutdown finishes", () => {
    const recording = sources["./recording.svelte.ts"];
    const sidebar = sources["./Sidebar.svelte"];
    const page = sources["../routes/record/+page.svelte"];

    expect(recording).toContain('status = "stopping";');
    expect(recording).toContain("get stopping() { return status === \"stopping\"; }");
    expect(sidebar).toContain('recording.stopping ? "正在停止…"');
    expect(page).toContain("{#if recording.stopping}");
    expect(page).toContain("正在停止…");
  });
});
