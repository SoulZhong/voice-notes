import { describe, expect, it } from "vitest";
import { zh as recordZh } from "./i18n/dict/record";
import { zh as shellZh } from "./i18n/dict/shell";

const sources = import.meta.glob(
  ["./recording.svelte.ts", "./Sidebar.svelte", "../routes/record/+page.svelte"],
  { eager: true, query: "?raw", import: "default" },
) as Record<string, string>;

describe("recording stop feedback", () => {
  it("reads as stopped immediately while durable shutdown finishes in the background", () => {
    const recording = sources["./recording.svelte.ts"];
    const sidebar = sources["./Sidebar.svelte"];
    const page = sources["../routes/record/+page.svelte"];

    expect(recording).toContain('status = "stopping";');
    expect(recording).toContain("get stopping() { return status === \"stopping\"; }");
    // 侧栏与录制页文案均已 i18n 化:源码里钉 t() 键,中文值从各自分片字典断言。
    // 立即停止:收尾期按「已停止」呈现——侧栏显示「开始录制」(禁用 + 收尾提示),
    // 录制页开始钮复位、计时后缀写明在整理最后几句,不再出现「正在停止…」。
    expect(sidebar).toContain('recording.stopping ? t("shell.record.start")');
    expect(sidebar).toContain('t("shell.record.finishingHint")');
    expect(shellZh["shell.record.finishingHint"]).toContain("已停止");
    expect(page).toContain("{#if recording.stopping}");
    expect(page).toContain('recording.stopping ? t("record.status.finishing")');
    expect(page).not.toContain('{t("record.btn.stopping")}');
    expect(recordZh["record.status.finishing"]).toContain("已停止");
  });
});
