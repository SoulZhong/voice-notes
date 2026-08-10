import { describe, expect, it } from "vitest";
import { noteBadgeKind } from "./noteBadge";

// 侧栏笔记徽标:持久化 state 与运行时暂停标志的合成。
// 暂停是会话运行时状态(不落盘),徽标必须叠加 recording.paused 才能与
// 录制按钮/实时转写页的「已暂停」口径一致(冒烟反馈:暂停后列表仍显示「录制中」)。
describe("noteBadgeKind", () => {
  it("active + 未暂停 → active(录制中)", () => {
    expect(noteBadgeKind("active", false)).toBe("active");
  });

  it("active + 已暂停 → paused(已暂停)", () => {
    expect(noteBadgeKind("active", true)).toBe("paused");
  });

  it("recording(悬挂)→ interrupted,暂停标志无关", () => {
    expect(noteBadgeKind("recording", false)).toBe("interrupted");
    expect(noteBadgeKind("recording", true)).toBe("interrupted");
  });

  it("完成态 → null,暂停标志无关", () => {
    expect(noteBadgeKind("complete", false)).toBeNull();
    expect(noteBadgeKind("complete", true)).toBeNull();
  });
});
