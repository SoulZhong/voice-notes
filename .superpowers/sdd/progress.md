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
- Minor (Task 8, defer/triage): stop() has no try/catch (backend stop is no-op; low risk).
- ALL 8 TASKS COMPLETE. Ready for final whole-branch review.

## Final whole-branch review (opus): READY to merge
- No must-fix. 4 known minors all OK-to-defer.
- Lifecycle verified sound; "start blocked until restart" is intentional (stop is P1 no-op).
- P2 backlog (forward concerns):
  1. Fast stream re-transcribes ENTIRE cumulative buffer every ~1.5s → O(n²) CPU + unbounded RAM (~64KB/s). Needs sliding fast-window + committed slow-segment model.
  2. bounded(256) sink + blocking send in cpal callback → audio glitches when ASR lags; use try_send/drop-oldest or move ASR off capture path.
  3. Wire real stop/cancel (hook already exists: Microphone stop_tx + AudioCapture::stop; thread a stop handle lib.rs→run_pipeline).
  4. Emit "recording" status only AFTER recognizer init (avoid recording→error flash).
  5. Add Chinese fixture + assertion to exercise 中英混合 path; end-to-end test pushing stereo/non-16k frames through run_pipeline.
  6. Trim likely-unused deps: thiserror, serde_json (verify first).
- LIVE SMOKE still required (human): speak mixed 中文+English, confirm Chinese chars appear.

## P1.5 — VAD 语句分段重构 (on branch p1-walking-skeleton, before merge)
Plan: docs/superpowers/plans/2026-07-01-voice-notes-p1.5-vad-segmentation.md
- T1: complete (head 5dcc0c4) — fetch silero_vad.onnx
- T2: complete (head b8e891d, review clean) — Segmenter trait + MockSegmenter
- T3: complete (head 878445f, review clean; 2 accepted minors) — SileroSegmenter
- T4: complete (head d7c6dfa, review clean; +partial assertion) — rewrite run_pipeline; delete buffer; ipc FinalEvent; lib.rs wired
- T5: complete (head 615bf65, review clean) — frontend final list + partial line (lib.rs wiring was folded into T4)

## P1.5 final review (opus): READY WITH MINOR FIXES → minors applied
- Confirmed: O(n²)+unbounded-memory FIXED (每次 Whisper ≤15s 单句；current/VAD 每句清空；整体线性、内存恒定).
- Applied minors: (a) clear partial on status stopped/error; (b) guard empty final; (c) clear current during silence.
- P2 backlog additions: move Whisper off consume loop (bounded(256) back-pressure); add a non-ignored VAD segmentation test (commit small fixture/model); short (<1s) utterances skip partial by design; stop_recording still no-op.
- ALL P1.5 DONE. Ready for live smoke + merge.

## P1.6 — SenseVoice recognizer (quality fix: base-int8 Chinese was poor)
User smoke: whisper-base int8 中文识别差、组织不成句。Design doc wanted large-v3; we downgraded for speed. Per-utterance segmentation (P1.5) makes a stronger model feasible. User chose SenseVoice-small (fast, strong zh/en).
- DONE: SenseVoiceRecognizer added (920c8ae), lib.rs switched. Chinese test perfect: '今天开会讨论一下项目进度和下一步计划。'. whisper module kept as alt. Model 347MB fp32 gitignored.
- T: complete (commit 394662e..920c8ae, task review clean/approved by sonnet) — SenseVoiceRecognizer + fetch_models guard + lib.rs swap (whisper kept) + Chinese IT.
  - Model: sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/model.onnx (347MB fp32; no int8 in tarball; tarball ~1GB not 100MB).
  - Test PASSED: `sense_voice_transcribes_chinese` → "今天开会讨论一下项目进度和下一步计划。" (Tingting TTS, ITN period). All 8 unit tests green; ignored tests model-gated.
  - API: language="auto" worked verbatim; use_itn=true; ctor eyre→anyhow via map_err. No deviations.
  - Minor (deferred, non-blocking): find_model_onnx doesn't try dir.join("model.onnx") first (unlike find_tokens); harmless (one non-int8 onnx in pkg). Mirror find_tokens pattern for determinism.
  - Report: .superpowers/sdd/p16-sensevoice-report.md
  - ALL P1.6 DONE. Live smoke (human): confirm mixed 中英 transcription quality beats whisper-base-int8.

## P2 — 系统声音采集 (branch p2-system-audio)
Plan: docs/superpowers/plans/2026-07-01-voice-notes-p2-system-audio.md
Base (branch start): 144474f
执行模式：subagent-driven；设备验证（T1 探针运行/T3/T8 冒烟）统一推迟到末尾人工冒烟。

### P2 task ledger
- T1: complete (144474f..8c9fbbe, review clean after Co-Authored-By trailer amend) — screencapturekit 依赖 + 系统声音探针; API 校准: get_audio_buffer_list()→audio_buffer_list(), AudioBuffer::data()->&[u8]. Runtime probe deferred to final smoke.
  - Minors (deferred, throwaway report): task-1-report.md AudioBufferList 表缺 buffer() 方法; audio.rs 行号与 spike 文档差 6 行。spike 文档为 T3 权威参考。
- T2: complete (8c9fbbe..0610ea9, review clean, Approved) — Source 枚举 + planar_to_mono; 12/12 tests。无 issue。
- T3: complete (0610ea9..3b827a9, review clean, Approved by opus) — SystemAudioCapture(SCKit)。bytes_to_f32 用 chunks_exact(4)+from_le_bytes(无 unsafe)，4 个真值单测；bg 线程/停止/握手镜像 microphone.rs；错误前缀 unauthorized:/unavailable: 正确；回调对异常缓冲跳过不 panic。16 tests。
  - Minors(deferred): extract_audio_mono 单缓冲 channels==0 走 passthrough(可加 guard，无害)；device 相关 dispatch 无单测(不可测)。
  - 未验证假设(留待末尾冒烟): planar vs interleaved 布局、f32 LE、SCShareableContent::get() 错误一律归类 unauthorized。
- T4: complete (3b827a9..5069591, review clean, Approved by sonnet) — FinalJob/PartialJob + run_segment_worker(识别外置)。finals 双路径(循环+flush)不丢；partial 覆盖式、finals 前清槽。17 tests。run_pipeline 未动。
  - Minors(deferred→final triage): (1) finals_tx.send 用 let _ 吞错，异常关闭时在途 final 静默丢(正常生命周期无碍)；(2) 节流时 current_partial()==None 未清旧槽→过时 partial 可能残留(建议 slot = current_partial().map(...))。
- T5: complete (5069591..cfb408b, review clean, Approved by sonnet, 0 issues) — run_asr_worker。finals 不丢(recv_timeout 缓冲优先于 Disconnected)；"[识别失败]"占位后 worker 续跑；partial take()清槽+错误吞掉；finals 优先。20 tests。
