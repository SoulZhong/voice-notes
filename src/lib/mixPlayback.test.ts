import { describe, expect, it } from "vitest";

// mixed 与源轨同时装载 = 三重叠加(mixed 本就是 mic+system 混出来的):音量翻倍、
// 听感像回声,恰是成品轨要消除的东西。组件运行时行为在 node 环境的单测里跑不起来,
// 这条约束靠读源码守住(手法同 editorReactivity.test.ts)。
const source = import.meta.glob(["../routes/notes/[id]/+page.svelte"], {
  eager: true,
  query: "?raw",
  import: "default",
}) as Record<string, string>;
const page = source["../routes/notes/[id]/+page.svelte"];

describe("mixed 回放装载纪律", () => {
  it("装载数组是二选一表达式,不存在 mixed 与源轨的拼接", () => {
    expect(page).toMatch(/playbackScheme === "mixed"[\s\S]{0,220}\[mixedInfo\.track\]\s*:\s*tracks/);
    expect(page).not.toMatch(/tracks\.concat|\.\.\.tracks,\s*mixedInfo|mixedInfo\.track,\s*\.\.\.tracks/);
  });

  it("不可信成品轨不得进装载数组(untrusted 参与装载判定)", () => {
    expect(page).toMatch(/playbackScheme === "mixed"[\s\S]{0,220}!mixedInfo\.untrusted/);
  });

  it("mixed 态段落 seek 必须带 seek_offset_ms 修正(live 轨首帧偏移)", () => {
    expect(page).toMatch(/seek_offset_ms\[/);
  });

  it("高亮与试听边界与 seek 同一套修正——只修 seek 会让高亮/截停提前一个偏移量", () => {
    // activeSeqs 的区间比较必须经 seekFix(codex P2)
    expect(page).toMatch(/playerMs >= seekFix\(seg\.start_ms, seg\.source\)/);
    // 试听 endMs 同理
    expect(page).toMatch(/endMs: seekFix\(/);
  });
});
