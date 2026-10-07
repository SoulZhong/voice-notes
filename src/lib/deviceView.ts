// 设备听写界面的纯逻辑(可单测):链路状态 → 文案键,停下原因 → 提示与可用动作。
import type { LinkEnded, LinkView } from "./device";

export type TextKey = { key: string; params?: Record<string, unknown> };

/** 链路状态的文案键。state 为空 = 运行时没在跑。 */
export function linkTextKey(view: LinkView, platform: string): TextKey {
  const s = view.state;
  if (!s) return { key: "device.link.stopped" };
  switch (s.kind) {
    case "unavailable":
      return { key: "device.link.unavailable" };
    case "searching":
      return { key: "device.link.searching" };
    case "connecting":
      return { key: "device.link.connecting", params: { name: s.name } };
    case "pairing":
      return {
        key: platform === "windows" ? "device.link.pairingWin" : "device.link.pairingMac",
        params: { name: s.name },
      };
    case "pairing_failed":
      return { key: "device.link.pairingFailed", params: { reason: s.reason } };
    case "bond_lost":
      return { key: "device.link.bondLost", params: { name: s.name } };
    case "connected":
      return { key: "device.link.connected", params: { name: s.name } };
    case "retrying":
      return { key: "device.link.retrying" };
  }
}

/** 停下来要用户做什么:提示文案 + 该露出哪个按钮。 */
export function endedHelp(
  ended: LinkEnded | null,
  platform: string,
  name: string,
): { text: TextKey; action: "reconnect" | "repair" } | null {
  switch (ended) {
    case null:
      return null;
    case "pairing_failed":
      return { text: { key: "device.ended.pairingFailed" }, action: "reconnect" };
    case "bond_lost":
      // Windows 能在应用里删旧配对;macOS 得去系统设置删,删完回来点重新连接。
      return platform === "windows"
        ? { text: { key: "device.ended.bondLostWin" }, action: "repair" }
        : { text: { key: "device.ended.bondLostMac", params: { name } }, action: "reconnect" };
    case "standalone_running":
      return { text: { key: "device.ended.standalone" }, action: "reconnect" };
  }
}

/** 下载进度百分比(0-100,总量未知时 0)。 */
export function percent(received: number, total: number): number {
  if (!total || total <= 0) return 0;
  return Math.min(100, Math.floor((received / total) * 100));
}
