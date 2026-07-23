import { describe, expect, it } from "vitest";

type Fs = { readFileSync(path: string, encoding: "utf8"): string };
type Runtime = typeof globalThis & {
  process: { cwd(): string; getBuiltinModule(name: "fs"): Fs };
};

const runtime = globalThis as Runtime;
const fs = runtime.process.getBuiltinModule("fs");
const read = (path: string) => fs.readFileSync(`${runtime.process.cwd()}\\${path}`, "utf8");

describe("system icon asset pipeline", () => {
  it("regenerates every platform icon from one tightly fitted master", () => {
    const refreshScript = read("scripts\\refresh_windows_icons.ps1");
    const tauriConfig = read("src-tauri\\tauri.conf.json");

    expect(refreshScript).toContain("$SafeMarginRatio = 0.03");
    expect(refreshScript).toContain("npm.cmd run tauri -- icon");
    expect(refreshScript).toContain("static\\favicon.png");

    const configured = JSON.parse(tauriConfig).bundle.icon as string[];
    for (const icon of configured) {
      expect(refreshScript, icon).toContain(icon.replace("icons/", ""));
    }
  });

  it("keeps the recording animation but derives every tray frame from the latest master", () => {
    const trayScript = read("scripts\\gen_tray_logo_frames.py");

    expect(trayScript).toContain('SRC = os.path.join(ICONS, "icon.png")');
    expect(trayScript).toContain("tray-logo-idle.png");
    expect(trayScript).toContain("tray-logo-rec-{i}.png");
    expect(trayScript).not.toContain("def cutout_girl");
    expect(trayScript).not.toContain("旧版");
  });
});
