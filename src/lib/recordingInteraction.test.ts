import { describe, expect, it } from "vitest";
import { zh as recordZh } from "./i18n/dict/record";
import { zh as shellZh } from "./i18n/dict/shell";

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
    // 侧栏与录制页文案均已 i18n 化:源码里钉 t() 键,中文值从各自分片字典断言。
    expect(sidebar).toContain('recording.stopping ? t("shell.record.stopping")');
    expect(shellZh["shell.record.stopping"]).toBe("正在停止…");
    expect(page).toContain("{#if recording.stopping}");
    expect(page).toContain('{t("record.btn.stopping")}');
    expect(recordZh["record.btn.stopping"]).toBe("正在停止…");
  });
});
