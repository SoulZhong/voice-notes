import { describe, expect, it } from "vitest";

type Fs = { readFileSync(path: string, encoding: "utf8"): string };
type Runtime = typeof globalThis & {
  process: { cwd(): string; getBuiltinModule(name: "fs"): Fs };
};

const runtime = globalThis as Runtime;
const fs = runtime.process.getBuiltinModule("fs");
const read = (path: string) => fs.readFileSync(`${runtime.process.cwd()}\\${path}`, "utf8");

describe("Windows release documentation", () => {
  it("documents the official v0.5.0 Windows artifacts in both READMEs", () => {
    for (const path of ["README.md", "README.en.md"]) {
      const readme = read(path);
      expect(readme, path).toContain("voice-notes_0.5.0_x64-setup.exe");
      expect(readme, path).toContain("voice-notes_0.5.0_x64_en-US.msi");
      expect(readme, path).toContain("SHA256SUMS-windows.txt");
    }
  });

  it("removes obsolete source-build-only Windows guidance", () => {
    const chinese = read("README.md");
    expect(chinese).not.toContain(
      "Releases 目前只提供 macOS arm64 安装包；Windows 请按下方步骤从源码构建",
    );
    expect(chinese).not.toContain("目前未提供官方 Windows 安装包，需从源码构建");

    const english = read("README.en.md");
    expect(english).not.toContain(
      "Releases currently provide macOS arm64 packages only; build from source on Windows",
    );
    expect(english).not.toContain("No official Windows installer is published yet");
  });
});
