# Task 1 Report: models 模块——目录解析 + 工件清单 + 状态判定 + lib.rs 收敛

(P5 phase — supersedes the older P3 "Segment 带流内样本偏移" report previously stored
at this path under a different task numbering.)

## What was implemented

1. **Created `src-tauri/src/models/mod.rs`** — verbatim transcription of the brief's Step 3
   code, including:
   - `APP_MODELS_ROOT: OnceLock<PathBuf>` + `init_app_root(dir: PathBuf)`.
   - `root() -> PathBuf` resolving VN_MODELS env var → debug-build dev dir
     (`CARGO_MANIFEST_DIR/models`, if it exists) → `APP_MODELS_ROOT` (app_data_dir/models)
     → fallback to `CARGO_MANIFEST_DIR/models`.
   - `FinalFile { rel_path, bytes, sha256 }`, `ArtifactKind::{File, TarBz2{dest_dir}}`,
     `Artifact { id, label, url, kind, approx_mb, required_for_recording, files }`.
   - `pub const ARTIFACTS: &[Artifact]` with the three pinned entries (`vad`, `speaker`,
     `asr`) — URLs, byte counts, and SHA256 hashes transcribed exactly as given in the
     brief (including the intentional "recongition" URL typo preserved, not "fixed").
   - `artifact_present(root, artifact) -> bool` (existence + exact byte-size check, no
     hashing at startup).
   - `ArtifactState`, `ModelsStatus` (both `Serialize`), `status() -> ModelsStatus`,
     `recording_ready() -> bool`.
   - Embedded `#[cfg(test)] mod tests` exactly as specified in Step 1/brief.

2. **Modified `src-tauri/src/lib.rs`**:
   - Added `pub mod models;` to the module declaration block.
   - Deleted `fn models_dir()`.
   - `sense_voice_dir()` / `speaker_model_path()` now call `models::root()` instead of
     the deleted `models_dir()`.
   - `spawn_session`'s `vad_path` now built from `models::root()`.
   - Extracted the setup closure's inline preload thread into a free function
     `fn preload_models(cache, embedder_cache)`, with the lock-order comment from the
     brief (nested recognizer→embedder slot lock, unchanged from the original).
   - `setup` now injects the production models root via `app.path().app_data_dir()`
     (`.join("models")`, `create_dir_all`, `models::init_app_root(...)`) before calling
     `preload_models`.
   - Added the guard `if !models::recording_ready() { return Err("模型缺失：请先在录制页下载模型".into()); }`
     at the top of **both** `start_recording` and `resume_recording`.
   - Added `#[tauri::command] fn models_status() -> models::ModelsStatus { models::status() }`
     and registered `models_status` in `generate_handler!`.

No deviations from the brief's exact code were needed — the existing `lib.rs` content
matched the brief's line-content assumptions closely enough that all edits applied
cleanly (the brief's cited line numbers, e.g. `models_dir()` at lines 53-55 and the
`vad_path` assignment near line 185, matched the actual file).

## TDD evidence

### RED

Command:
```
cd src-tauri && cargo test models:: 2>&1 | tail -30
```

Output (excerpt):
```
running 3 tests
test models::tests::manifest_covers_three_runtime_artifacts ... ok
test models::tests::artifact_present_requires_existence_and_exact_size ... FAILED
test models::tests::root_prefers_env_var ... FAILED

failures:
    models::tests::artifact_present_requires_existence_and_exact_size
    models::tests::root_prefers_env_var

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 82 filtered out
```

**Deviation from brief's stated expectation**: the brief says "Expected: 3 个测试 FAIL"
but only 2 of the 3 tests fail with `todo!()` stubs — `manifest_covers_three_runtime_artifacts`
only inspects `ARTIFACTS` (a fully-implemented `const`), never calling `root()`,
`artifact_present()`, or `status()`, so it passes even before those functions are
implemented. This is a property of the test itself, not a mistake in the stub setup —
2/3 failing via `not yet implemented` panics still constitutes a valid red state
confirming the stubbed functions are exercised and broken. Noted for the requester;
did not change test content since Step 1 explicitly says the tests come from the brief
verbatim.

Before this run, `pub mod models;` had to be added to `lib.rs` first (not yet part of
the brief's Step 1/2, but required for `cargo test models::` to even compile/select the
module) — done as the first part of Step 4 item 1, pulled forward.

### GREEN

Command:
```
cd src-tauri && cargo test models:: 2>&1 | grep -A 10 "Running unittests"
```

Output:
```
running 3 tests
test models::tests::manifest_covers_three_runtime_artifacts ... ok
test models::tests::root_prefers_env_var ... ok
test models::tests::artifact_present_requires_existence_and_exact_size ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 82 filtered out; finished in 0.00s
```

### Full regression

Command:
```
cd src-tauri && cargo test 2>&1 | grep "test result"
```

Output:
```
test result: ok. 84 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.13s
(remaining integration-test binaries: 0 passed / 1 ignored each, as before — sckit_probe,
recognizer_it, segmenter_it, sense_voice_it, embedder_it — unaffected by this change)
```

81 (baseline) + 3 (new) = 84 passed, matching the brief's expectation. Ran again after
commit to confirm stability — same result.

## Files changed

- `src-tauri/src/models/mod.rs` (new, 199 lines incl. tests)
- `src-tauri/src/lib.rs` (module declaration, path-fn call sites, guard additions,
  preload extraction, setup injection, new command + registration)

## Self-review

**Completeness**
- All four `models_dir()` call sites migrated to `models::root()`: `sense_voice_dir()`,
  `speaker_model_path()`, `spawn_session`'s `vad_path`, and (implicitly) nothing else
  referenced `models_dir()` — verified via `grep -n models_dir src/lib.rs` returning
  nothing after the edit.
- Guard added to **both** `start_recording` and `resume_recording` — verified by
  reading the diff; each guard is textually identical per the brief.
- `models_status` command implemented and registered in `generate_handler!` — verified.
- `preload_models` extracted as a free function with the exact lock-order comment
  from the brief; `setup` calls it after injecting `init_app_root`.
- Interfaces produced match the brief's "Produces" list exactly: `models::root()`,
  `models::init_app_root(PathBuf)`, `models::ARTIFACTS`, `FinalFile`,
  `ArtifactKind::{File, TarBz2{dest_dir}}`, `Artifact` fields, `artifact_present`,
  `status() -> ModelsStatus` (with `artifacts/recording_ready/diarization_ready`),
  `recording_ready() -> bool`, `preload_models`, `models_status` command.

**Quality**
- Comments are in Chinese, matching existing file style (module doc comment,
  function doc comments, inline rationale comments transcribed verbatim from brief).
- No nested locking introduced beyond what already existed in `preload_models`
  (recognizer→embedder, unchanged from original setup closure) — the stated lock
  order (running → generation → session_slot) is untouched since `recording_ready()`
  and `models::root()` take no app-state locks at all.

**Discipline**
- No overbuilding: only what the brief specifies. No download logic (explicitly out
  of scope, left for a later task's `download` submodule referenced in the module
  doc comment).
- `pub mod models;` addition was pulled forward from Step 4 into the RED step purely
  because Rust requires the module to be part of the crate to compile/test it; no
  other Step 4 content was pulled forward early.

**Testing**
- `cargo build` and `cargo clippy --lib` both show the same warning set as the
  pre-change baseline (verified via `git stash`/`cargo build`/`git stash pop`
  comparison) — 2 dead-code warnings in unrelated test-support code
  (`MockCapture`, `from_wav`), not introduced by this change. `cargo clippy --lib`
  shows 0 warnings anchored in `models/mod.rs`.
- Tests verify real behavior: `root_prefers_env_var` checks env-var precedence and
  fallback to the real dev models dir; `artifact_present_requires_existence_and_exact_size`
  checks missing-file, wrong-size, and exact-size cases; `manifest_covers_three_runtime_artifacts`
  checks artifact IDs, the vad+asr-required-for-recording count, and that every
  `sha256` field is 64 hex chars.

## Concerns

None blocking. One minor documentation-accuracy note already covered above (brief
says "3 个测试 FAIL" in the red step; actually 2 of 3 fail, which is still a valid red
signal — no code change was needed to address this, it's inherent to what
`manifest_covers_three_runtime_artifacts` asserts).

## Commit

```
1eb33ee feat(models): 模型目录运行时解析 + 工件清单与状态判定
```
