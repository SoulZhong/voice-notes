import { describe, it, expect, vi } from "vitest";

vi.mock("$lib/models", () => ({ getSettings: vi.fn() }));
import { uiMode, showDeviceTab, showHooksTab, syncUiMode } from "./uiMode.svelte";
import type { Settings } from "$lib/models";

const s = (over: Partial<Settings>) => ({ show_all_settings: false, device_enabled: true, device_name: null, ...over }) as Settings;

describe("界面模式决定左侧菜单", () => {
  it("简洁模式:钩子不显示;设备只在连接过设备时显示", () => {
    syncUiMode(s({}));
    expect(showHooksTab()).toBe(false);
    expect(showDeviceTab()).toBe(false);
    syncUiMode(s({ device_name: "VibeVoice-1A2B" }));
    expect(showDeviceTab()).toBe(true);
  });
  it("完整模式:钩子与设备都显示;设备总开关关掉则不显示", () => {
    syncUiMode(s({ show_all_settings: true }));
    expect(uiMode.full).toBe(true);
    expect(showHooksTab()).toBe(true);
    expect(showDeviceTab()).toBe(true);
    syncUiMode(s({ show_all_settings: true, device_enabled: false, device_name: "VibeVoice-1A2B" }));
    expect(showDeviceTab()).toBe(false);
  });
});
