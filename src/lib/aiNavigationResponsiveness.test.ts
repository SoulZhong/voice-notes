import { describe, expect, it } from "vitest";

type Fs = { readFileSync(path: string, encoding: "utf8"): string };
type Runtime = typeof globalThis & {
  process: { cwd(): string; getBuiltinModule(name: "fs"): Fs };
};

const runtime = globalThis as Runtime;
const fs = runtime.process.getBuiltinModule("fs");
const read = (path: string) => fs.readFileSync(`${runtime.process.cwd()}\\${path}`, "utf8");

describe("AI navigation responsiveness", () => {
  it("runs blocking AI startup commands outside Tauri's command thread", () => {
    const rust = read("src-tauri\\src\\lib.rs");
    const commands = [
      "mcp_agents_status",
      "mcp_manual_snippet",
      "mcp_skill_status",
      "refine_agents_probe",
      "ai_logs_query",
    ];

    for (const command of commands) {
      const start = rust.indexOf(`async fn ${command}`);
      expect(start, `${command} should be async`).toBeGreaterThanOrEqual(0);
      const nextCommand = rust.indexOf("#[tauri::command]", start + 1);
      const body = rust.slice(start, nextCommand < 0 ? undefined : nextCommand);
      expect(body, `${command} should use spawn_blocking`).toContain(
        "tauri::async_runtime::spawn_blocking",
      );
    }
  });

  it("centers every sidebar tab in the same full-width rail", () => {
    const sidebar = read("src\\lib\\Sidebar.svelte");
    const tabStart = sidebar.indexOf(".vtab {");
    const tabEnd = sidebar.indexOf("}", tabStart);
    const tabRule = sidebar.slice(tabStart, tabEnd);

    expect(tabRule).toContain("box-sizing: border-box");
    expect(tabRule).toContain("display: flex");
    expect(tabRule).toContain("align-items: center");
    expect(tabRule).toContain("justify-content: center");
    expect(tabRule).toContain("width: 100%");

    const uprightStart = sidebar.indexOf(".vtab-upright {");
    const uprightEnd = sidebar.indexOf("}", uprightStart);
    expect(sidebar.slice(uprightStart, uprightEnd)).toContain("writing-mode: horizontal-tb");
  });
});
