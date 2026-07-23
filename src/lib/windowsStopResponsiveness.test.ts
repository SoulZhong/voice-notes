import { describe, expect, it } from "vitest";
import backend from "../../src-tauri/src/lib.rs?raw";

describe("Windows recording stop responsiveness", () => {
  it("runs the blocking durable shutdown outside the Tauri IPC/UI execution path", () => {
    expect(backend).toContain("async fn stop_recording(app: AppHandle) -> Result<(), String>");
    expect(backend).toContain("tauri::async_runtime::spawn_blocking(move ||");
    expect(backend).toContain("lifecycle.command(lifecycle::Cmd::Stop)");
  });
});
