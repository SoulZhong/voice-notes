import { describe, expect, it } from "vitest";
import { shouldShowMiniPlayer, shouldStopOnCleanup } from "./playback.svelte";
import { playback } from "./playback.svelte";

describe("shouldShowMiniPlayer", () => {
  it("无会话一律不显示", () => {
    expect(shouldShowMiniPlayer(null, "/settings")).toBe(false);
    expect(shouldShowMiniPlayer(null, "/notes/20260815-072046")).toBe(false);
  });

  it("有会话且在其他页 → 显示", () => {
    expect(shouldShowMiniPlayer("20260815-072046", "/settings")).toBe(true);
    expect(shouldShowMiniPlayer("20260815-072046", "/")).toBe(true);
    expect(shouldShowMiniPlayer("20260815-072046", "/speakers/S1")).toBe(true);
  });

  it("正在播放的那篇笔记页上隐藏(完整播放器已在页面里)", () => {
    expect(shouldShowMiniPlayer("20260815-072046", "/notes/20260815-072046")).toBe(false);
    // 尾斜杠、查询串、hash 都不该让它误判成"别的页"
    expect(shouldShowMiniPlayer("20260815-072046", "/notes/20260815-072046/")).toBe(false);
    expect(shouldShowMiniPlayer("20260815-072046", "/notes/20260815-072046?tab=refined")).toBe(false);
  });

  it("在别的笔记页上要显示(那篇尚未接管播放)", () => {
    expect(shouldShowMiniPlayer("20260815-072046", "/notes/20260814-180747")).toBe(true);
  });
});

describe("shouldStopOnCleanup", () => {
  it("从未成功装载 → 无核可收,不发停止", () => {
    expect(shouldStopOnCleanup(null, null)).toBe(false);
    expect(shouldStopOnCleanup(null, 5)).toBe(false);
  });

  it("本组件装的核正是活动会话 → 不停,会话接管所有权", () => {
    expect(shouldStopOnCleanup(5, 5)).toBe(false);
  });

  it("无会话,或会话已换到别的代次 → 停,语义同现状", () => {
    expect(shouldStopOnCleanup(5, null)).toBe(true);
    expect(shouldStopOnCleanup(5, 9)).toBe(true);
  });
});

describe("会话状态机", () => {
  const s = { gen: 3, noteId: "n1", title: "会议", totalMs: 60000 };

  it("begin 建立会话,clear 清空", () => {
    playback.begin(s);
    expect(playback.session?.noteId).toBe("n1");
    expect(playback.playing).toBe(true);
    playback.clear();
    expect(playback.session).toBe(null);
    expect(playback.playing).toBe(false);
  });

  it("只接受本会话代次的位置事件", () => {
    playback.begin(s);
    playback.applyPos({ pos_ms: 1000, playing: true, gen: 3 });
    expect(playback.currentMs).toBe(1000);
    // 旧内核的排队事件:必须丢弃,否则会写进当前会话
    playback.applyPos({ pos_ms: 55555, playing: false, gen: 2 });
    expect(playback.currentMs).toBe(1000);
    expect(playback.playing).toBe(true);
    playback.clear();
  });

  it("位置不超过总时长", () => {
    playback.begin(s);
    playback.applyPos({ pos_ms: 999999, playing: true, gen: 3 });
    expect(playback.currentMs).toBe(60000);
    playback.clear();
  });

  it("播放自然结束保留会话(可重播)", () => {
    playback.begin(s);
    playback.applyPos({ pos_ms: 60000, playing: false, gen: 3 });
    expect(playback.session).not.toBe(null);
    expect(playback.playing).toBe(false);
    playback.clear();
  });

  it("改名只影响同一篇笔记", () => {
    playback.begin(s);
    playback.rename("other", "别的");
    expect(playback.session?.title).toBe("会议");
    playback.rename("n1", "新名字");
    expect(playback.session?.title).toBe("新名字");
    playback.clear();
  });

  it("同篇重装 → rebind 改指新核,位置事件继续被接受", () => {
    playback.begin(s);
    playback.rebind(8);
    expect(playback.session?.gen).toBe(8);
    playback.applyPos({ pos_ms: 2000, playing: true, gen: 8 });
    expect(playback.currentMs).toBe(2000);
    // 旧代次事件仍要丢弃
    playback.applyPos({ pos_ms: 9000, playing: true, gen: 3 });
    expect(playback.currentMs).toBe(2000);
    playback.clear();
  });
});
