import { describe, expect, it } from "vitest";

type Fs = { readFileSync(path: string, encoding: "utf8"): string };
type Runtime = typeof globalThis & {
  process: { cwd(): string; getBuiltinModule(name: "fs"): Fs };
};

const runtime = globalThis as Runtime;
const fs = runtime.process.getBuiltinModule("fs");
const read = (path: string) => fs.readFileSync(`${runtime.process.cwd()}/${path}`, "utf8");

describe("Windows release documentation", () => {
  // 安装指引刻意不与任何具体版本绑定:钉版本号意味着每次发版都要改 README,
  // 漏改就把用户指向旧包。改为一律指向 /releases/latest + x.y.z 占位写法后,
  // 本用例的职责从"跟当前版本一致"变成"永远不许再钉版本"。
  it("points at the latest release instead of pinning a version", () => {
    const { version } = JSON.parse(read("package.json")) as { version: string };

    for (const path of ["README.md", "README.en.md"]) {
      const readme = read(path);
      expect(readme, path).toContain("voice-notes_x.y.z_x64-setup.exe");
      expect(readme, path).toContain("/releases/latest");
      expect(readme, path).not.toContain(`voice-notes_${version}_x64-setup.exe`);
      expect(readme, path).not.toContain("/releases/tag/v");
      expect(readme, path).not.toContain("_x64_en-US.msi");
      expect(readme, path).not.toContain("SHA256SUMS-windows.txt");
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

describe("Windows CI resource staging", () => {
  it("builds the statically linked library without obsolete DLL staging", () => {
    const workflow = read(".github/workflows/windows-check.yml");
    const staging = workflow.indexOf("Stage Tauri runtime resource placeholders");
    const cargoBuild = workflow.indexOf("- name: cargo build (lib, Windows msvc)");

    expect(staging).toBe(-1);
    expect(cargoBuild).toBeGreaterThanOrEqual(0);
    expect(workflow).toContain("run: cargo build --lib");
    for (const dll of [
      "cargs.dll",
      "onnxruntime.dll",
      "onnxruntime_providers_shared.dll",
      "sherpa-onnx-c-api.dll",
      "sherpa-onnx-cxx-api.dll",
    ]) {
      expect(workflow).not.toContain(dll);
    }
  });
});
