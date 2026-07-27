# AI Navigation Responsiveness Implementation Plan

> **For Codex:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task.

**Goal:** Keep the window responsive when entering or leaving the AI page, and make every sidebar tab occupy and align within the same rail width.

**Architecture:** Preserve the existing Svelte/Tauri API contract while moving blocking filesystem, executable discovery, and log-query work off Tauri's command thread with `tauri::async_runtime::spawn_blocking`. Normalize the shared `.vtab` box model so buttons and links use the same full-width flex alignment, while retaining vertical Chinese labels and the horizontal `AI` label.

**Tech Stack:** Svelte 5, TypeScript, Vitest, Tauri 2, Rust, Windows UI Automation

---

### Task 1: Add regression coverage for blocking AI commands

**Files:**
- Create: `src/lib/aiNavigationResponsiveness.test.ts`
- Test: `src/lib/aiNavigationResponsiveness.test.ts`

**Step 1: Write the failing test**

Add a source-level regression test that reads `src-tauri/src/lib.rs` and verifies the AI-page startup commands that perform filesystem, process, or log I/O are asynchronous and dispatch their blocking bodies through `tauri::async_runtime::spawn_blocking`.

Cover:
- `mcp_agents_status`
- `mcp_manual_snippet`
- `mcp_skill_status`
- `refine_agents_probe`
- `ai_logs_query`

Use the repository's existing `globalThis.process.getBuiltinModule("fs")` test helper pattern so the test does not require Node type declarations.

**Step 2: Run test to verify it fails**

Run: `npm.cmd test -- src/lib/aiNavigationResponsiveness.test.ts`

Expected: FAIL because the commands are still synchronous and do not use `spawn_blocking`.

**Step 3: Implement the smallest asynchronous command wrappers**

Modify `src-tauri/src/lib.rs`:
- Change each covered command to `async fn`.
- Move its existing synchronous body into an owned `move ||` closure passed to `tauri::async_runtime::spawn_blocking`.
- Map join failures to a descriptive `String`.
- Preserve each command name, arguments, successful serialized payload, and normal error text.
- Leave pure in-memory commands such as `mcp_capabilities` and the atomic healed counter synchronous.

**Step 4: Run test to verify it passes**

Run: `npm.cmd test -- src/lib/aiNavigationResponsiveness.test.ts`

Expected: PASS.

### Task 2: Add regression coverage for sidebar alignment

**Files:**
- Modify: `src/lib/aiNavigationResponsiveness.test.ts`
- Modify: `src/lib/Sidebar.svelte`

**Step 1: Write the failing alignment test**

Extend the regression test to read `src/lib/Sidebar.svelte` and verify `.vtab` establishes:
- `box-sizing: border-box`
- `display: flex`
- `align-items: center`
- `justify-content: center`
- `width: 100%`

Also verify `.vtab-upright` keeps the `AI` label horizontal.

**Step 2: Run test to verify it fails**

Run: `npm.cmd test -- src/lib/aiNavigationResponsiveness.test.ts`

Expected: FAIL because `.vtab` currently relies on intrinsic button/link widths.

**Step 3: Implement the shared tab box model**

Modify `src/lib/Sidebar.svelte` so `.vtab` uses the full rail width and a centered flex layout with a consistent box model and typography. Preserve the current vertical writing mode for Chinese tabs and override it only for `.vtab-upright`.

**Step 4: Run test to verify it passes**

Run: `npm.cmd test -- src/lib/aiNavigationResponsiveness.test.ts`

Expected: PASS.

### Task 3: Run repository-level verification

**Files:**
- Verify: `src-tauri/src/lib.rs`
- Verify: `src/lib/Sidebar.svelte`
- Verify: `src/lib/aiNavigationResponsiveness.test.ts`

**Step 1: Run the focused regression test**

Run: `npm.cmd test -- src/lib/aiNavigationResponsiveness.test.ts`

Expected: PASS.

**Step 2: Run all frontend tests**

Run: `npm.cmd test`

Expected: all tests PASS.

**Step 3: Run Svelte and TypeScript checks**

Run: `npm.cmd run check`

Expected: zero errors.

**Step 4: Run the Windows Rust check**

Set the known LLVM, bindgen, Cargo target, and runtime-library environment for this worktree, then run:

`cargo check --manifest-path src-tauri/Cargo.toml`

Expected: exit code 0.

### Task 4: Repackage and perform a Windows smoke test

**Files:**
- Build input: `src-tauri/tauri.windows.conf.json`
- Build output: `C:\tmp\voice-notes-icon-target\release\bundle\nsis\voice-notes_0.5.0_x64-setup.exe`

**Step 1: Build the Windows installer**

With the verified Windows build environment, run:

`npm.cmd run tauri -- build --config src-tauri/tauri.windows.conf.json`

Expected: the executable and NSIS installer are produced successfully.

**Step 2: Install the package silently**

Close the running installed app if necessary, then run the generated installer with `/S`.

Expected: `C:\Users\lishuyuan\AppData\Local\voice-notes\voice-notes.exe` is updated.

**Step 3: Launch and automate navigation**

Launch the installed app. Use Windows UI Automation to invoke the `AI` tab and then at least two other sidebar tabs. After every transition, confirm:
- the process remains alive;
- `Process.Responding` remains true;
- another tab can be invoked without waiting for AI startup work to finish.

**Step 4: Verify alignment evidence**

Confirm the installed build contains the tested full-width centered `.vtab` rules. If practical, capture or visually inspect the running sidebar to ensure the horizontal `AI` label and vertical Chinese labels share the same centered rail.

### Task 5: Review and commit

**Files:**
- Review all files changed by Tasks 1–4.

**Step 1: Inspect the diff**

Run: `git diff --check` and `git diff --stat`.

Expected: no whitespace errors and only the planned source, test, and documentation changes.

**Step 2: Run final verification**

Re-run the focused test and any check affected by final cleanup.

**Step 3: Commit**

Stage only the intentional files and commit with:

`git commit -m "fix: keep AI navigation responsive"`

Expected: a clean implementation commit on `codex/windows-icon-unification`, excluding unrelated stat-only worktree changes.
