# P1 行走骨架 — 进度账本

Plan: docs/superpowers/plans/2026-06-30-voice-notes-p1-walking-skeleton.md
Branch: p1-walking-skeleton

- Task 1: complete (commits 54550b7..168fe8b, review clean after fix) — 脚手架
- Task 2: complete (head 9a824b0, review clean) — AudioFrame/AudioCapture/to_mono
- Task 3: complete (head d6e9270, review clean) — 重采样
- Task 4: complete (head 61d9ff8, review clean; Important+2Minor fixed w/ burst test) — AccumulatingBuffer
- Task 5: complete (head 6de0717, review clean after fix) — Recognizer + Whisper
- Task 6: complete (head aa7db0f, review clean; Important silent-fail fixed via ready handshake) — 麦克风采集
- Task 7: complete (head 2ea25ac, review clean; race+drain-stop fixed) — 会话编排 + IPC + Mock
- Task 8: complete (head 6f50907, build clean) — 录制 UI

## Minor findings (for final review)
Base (branch start): c50efb7e9a8a4d3001d63d286be1e83095a7429f

### Notes
- Task 1: frontend template is SvelteKit (src/routes/+page.svelte, src/app.html), NOT App.svelte/main.ts — Task 8 must adapt UI to SvelteKit routes.
- Task 1 Minor (deferred): src-tauri/models/ not created; Task 5 fetch_models.sh creates it. (gitignored anyway)
- crate [lib] name = app_lib; sherpa-rs 0.6.8; time pinned 0.3.47 (Tauri/cookie workaround).
- Minor (Task 3, defer to final): resample.rs s0 unwrap_or(0.0) is dead branch — add `// idx < len by construction` comment; no upsample-path test (boundary guard untested).
- Task 5 fix: language must be "" (empty=auto-detect) in sherpa-onnx; plan brief's "auto" is INVALID ("Invalid language: auto"). Integration test passes with "".
- Minor (defer to final): test fixture is English-only ("Hello"); once multilingual confirmed, add a Chinese-containing fixture+assertion to exercise 中英混合 path. recognizer_it.rs comment dropped VN_MODELS hint (harmless, #[ignore] gates it).
- API note: sherpa-rs 0.6.8 WhisperRecognizer::transcribe(u32, &[f32]) -> WhisperRecognizerResult (not Result); ctor uses eyre. model tokens file is base-tokens.txt.
- Task 6 intake for Task 7: Microphone::start now BLOCKS until stream confirmed open, returns Err on failure. cpal::Stream is !Send (owned on bg thread). In lib.rs start_recording, run_pipeline returning Err must emit a "status" error event (don't swallow). Keep `pub mod asr;` so tests/recognizer_it.rs (app_lib::asr) still compiles.
