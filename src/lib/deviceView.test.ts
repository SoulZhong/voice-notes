import { describe, expect, it } from "vitest";
import { endedHelp, linkTextKey, percent } from "./deviceView";
import { isDictationId, normalizePin } from "./device";

describe("设备听写界面逻辑", () => {
  it("链路状态映射到文案键,配对按平台区分", () => {
    expect(linkTextKey({ state: null, text: "", ended: null, mismatch: null, firmware: null }, "macos").key).toBe("device.link.stopped");
    const pairing = { state: { kind: "pairing", name: "VibeVoice-1A2B" } as const, text: "", ended: null, mismatch: null, firmware: null };
    expect(linkTextKey(pairing, "windows")).toEqual({
      key: "device.link.pairingWin",
      params: { name: "VibeVoice-1A2B" },
    });
    expect(linkTextKey(pairing, "macos").key).toBe("device.link.pairingMac");
    expect(
      linkTextKey({ state: { kind: "pairing_failed", name: "x", reason: "已取消" }, text: "", ended: null, mismatch: null, firmware: null }, "windows"),
    ).toEqual({ key: "device.link.pairingFailed", params: { reason: "已取消" } });
  });

  it("配对失效:Windows 给「重新配对」,macOS 指去系统设置后「重新连接」", () => {
    expect(endedHelp("bond_lost", "windows", "VV")?.action).toBe("repair");
    const mac = endedHelp("bond_lost", "macos", "VV");
    expect(mac?.action).toBe("reconnect");
    expect(mac?.text).toEqual({ key: "device.ended.bondLostMac", params: { name: "VV" } });
    expect(endedHelp(null, "windows", "VV")).toBeNull();
    expect(endedHelp("standalone_running", "macos", "VV")?.action).toBe("reconnect");
  });

  it("配对码只留 6 位数字", () => {
    expect(normalizePin(" 12a3-45 678")).toBe("123456");
    expect(normalizePin("")).toBe("");
  });

  it("听写笔记 id 与会议笔记 id 区分得开", () => {
    expect(isDictationId("d0123456789abcdef")).toBe(true);
    expect(isDictationId("20261007-101500")).toBe(false);
    expect(isDictationId("d0123")).toBe(false);
  });

  it("下载百分比", () => {
    expect(percent(50, 200)).toBe(25);
    expect(percent(10, 0)).toBe(0);
    expect(percent(300, 200)).toBe(100);
  });
});
