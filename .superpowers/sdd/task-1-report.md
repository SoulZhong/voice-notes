# P2 Task 1 Report: screencapturekit 依赖 + 系统声音 spike

## Status: DONE

---

## What Was Done

### Step 1: Added dependency (macOS-only)

Added to `src-tauri/Cargo.toml`:
```toml
[target.'cfg(target_os = "macos")'.dependencies]
screencapturekit = { version = "8", features = ["macos_13_0"] }
```

Resolved to `screencapturekit = "8.0.0"`.

### Step 2: Wrote probe (`src-tauri/tests/sckit_probe.rs`)

Created `#[ignore]` probe with `#![cfg(target_os = "macos")]` gate. One API correction vs. the brief's illustrative code: `sample.get_audio_buffer_list()` → `sample.audio_buffer_list()` (see API Deviations below).

### Step 3: Compiled probe

```
cd src-tauri && cargo test --test sckit_probe --no-run
```

Result:
```
   Compiling screencapturekit v8.0.0
   Compiling voice-notes v0.1.0 (...)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 19.30s
  Executable tests/sckit_probe.rs (target/debug/deps/sckit_probe-9887d28a6744db1c)
```

Zero errors. Two pre-existing warnings (unrelated to this task).

### Step 4 (Deferred)

Runtime probe run deferred to final manual smoke session (requires screen-recording permission + audio playback).

### Step 5: Wrote spike findings

Created `.superpowers/sdd/p2-sckit-spike.md` with confirmed API signatures and sources (file:line into crate source), deviation table, and runtime items to confirm at smoke.

### Step 6: Committed

```
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tests/sckit_probe.rs .superpowers/sdd/p2-sckit-spike.md
git commit -m "spike(p2): screencapturekit 依赖 + 系统声音探针与 findings"
```

---

## Confirmed Crate API (compile-verified, with file:line)

### `SCStreamOutputTrait`

```rust
trait SCStreamOutputTrait: Send + Sync + 'static {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType);
}
```
Source: `screencapturekit-8.0.0/src/stream/output_trait.rs`

### `SCStreamOutputType::Audio`

```rust
pub enum SCStreamOutputType { Screen, Audio, Microphone }
```
Source: `screencapturekit-8.0.0/src/stream/output_type.rs:27`

### `CMSampleBuffer::format_description()`

```rust
// On CMSampleBuffer struct directly (apple_cf)
pub fn format_description(&self) -> Option<CMFormatDescription>
```
Source: `apple-cf-0.9.3/src/cm/sample_buffer.rs:179`

### `CMSampleBufferExt::audio_buffer_list()`

```rust
// Trait in prelude — NOT get_audio_buffer_list()
fn audio_buffer_list(&self) -> Option<AudioBufferList>
```
Source: `screencapturekit-8.0.0/src/cm/sample_buffer.rs:331`

### `AudioBufferList`

```rust
impl AudioBufferList {
    pub fn num_buffers(&self) -> usize
    pub fn get(&self, index: usize) -> Option<&AudioBuffer>
    pub fn iter(&self) -> AudioBufferListIter<'_>
}
```
Source: `screencapturekit-8.0.0/src/cm/audio.rs:159-195`

### `AudioBuffer`

```rust
#[repr(C)]
pub struct AudioBuffer {
    pub number_channels: u32,
    pub data_bytes_size: u32,
    // data_ptr private
}
impl AudioBuffer {
    pub fn data(&self) -> &[u8]   // raw bytes; cast to &[f32] for audio
}
```
Source: `screencapturekit-8.0.0/src/cm/audio.rs:19-98`

### `SCContentFilter` builder

```rust
SCContentFilter::create()
    .with_display(&display)
    .with_excluding_windows(&[])
    .build()
```
Source: `screencapturekit-8.0.0/src/stream/content_filter.rs`

### `SCStreamConfiguration` builder

```rust
SCStreamConfiguration::new()
    .with_width(2_u32)
    .with_height(2_u32)
    .with_captures_audio(true)
    .with_sample_rate(48_000_i32)
    .with_channel_count(2_i32)
```
Sources: `screencapturekit-8.0.0/src/stream/configuration/dimensions.rs:45,103` and `audio.rs:187,232,288`

### `SCStream`

```rust
pub fn new(filter: &SCContentFilter, config: &SCStreamConfiguration) -> SCStream
pub fn add_output_handler<H: SCStreamOutputTrait>(&mut self, handler: H, output_type: SCStreamOutputType)
pub fn start_capture(&mut self) -> SCResult<()>
pub fn stop_capture(&mut self) -> SCResult<()>
```

---

## API Deviations from Brief

| Brief (illustrative) | Real crate v8 API | Impact |
|---|---|---|
| `sample.get_audio_buffer_list()` | `sample.audio_buffer_list()` | Fixed in probe; T3 must use `audio_buffer_list()` |
| All other calls | Match exactly | — |

---

## Files Changed

- `src-tauri/Cargo.toml` — macOS-gated screencapturekit dep
- `src-tauri/Cargo.lock` — new crate + transitive deps
- `src-tauri/tests/sckit_probe.rs` — new ignored probe
- `.superpowers/sdd/p2-sckit-spike.md` — spike findings

---

## Self-Review

- Dep correctly gated to macOS only — cross-platform builds unaffected
- Probe is `#[ignore]` + file-level `#![cfg(target_os = "macos")]` — will not run in CI
- No production code modified (trait, audio/mod.rs, etc. all unchanged)
- No new warnings introduced
- Probe uses `use screencapturekit::prelude::*` which brings in `CMSampleBufferExt` (needed for `audio_buffer_list()`)

## Concerns

- **Runtime deferred**: We know the crate API compiles; whether ScreenCaptureKit actually delivers f32 audio frames is confirmed at smoke only. The decision gate (continue vs. Swift shim) stays open until then.
- **f32 interpretation**: `AudioBuffer::data()` returns `&[u8]`. T3 will need `bytemuck::cast_slice` or manual `f32::from_le_bytes` to get `&[f32]`. Format (planar vs. interleaved) also needs runtime confirmation.
