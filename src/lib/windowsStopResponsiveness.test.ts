import { describe, expect, it } from "vitest";
import backend from "../../src-tauri/src/lib.rs?raw";

describe("Windows recording stop responsiveness", () => {
  it("runs the blocking durable shutdown outside the Tauri IPC/UI execution path", () => {
    const start = backend.indexOf("async fn stop_recording(app: AppHandle) -> Result<(), String>");
    expect(start, "stop_recording should be async and fallible").toBeGreaterThanOrEqual(0);
    const nextCommand = backend.indexOf("#[tauri::command]", start + 1);
    const body = backend.slice(start, nextCommand < 0 ? undefined : nextCommand);
    expect(body).toContain("tauri::async_runtime::spawn_blocking(move ||");
    expect(body).toContain("lifecycle.command(lifecycle::Cmd::Stop)");
  });
});
