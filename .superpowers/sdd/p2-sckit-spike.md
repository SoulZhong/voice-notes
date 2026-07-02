# P2 screencapturekit Spike — Findings

## Status

**Compile**: PASS (`cargo test --test sckit_probe --no-run` succeeded — see below)
**Runtime**: DEFERRED — device-dependent run requires screen-recording permission + active audio playback. Scheduled for the final manual smoke session at the end of P2.

---

## Decision Gate

- Crate compiles and the API surface is confirmed → **continue with T3 `SystemAudioCapture` using `screencapturekit = "8"`**
- If runtime probe fails (no audio callback, or format doesn't yield f32 samples) → escalate and consider Swift shim fallback (would need a revised T3 plan; all other P2 tasks remain unaffected since they sit behind `AudioCapture` trait)

---

## Confirmed Crate API (compile-verified)

### Crate version

`screencapturekit = "8.0.0"`, feature `macos_13_0`

### Imports (`use screencapturekit::prelude::*`)

The prelude re-exports everything T3 will need:
- `CMSampleBuffer`, `CMSampleBufferExt`, `CMSampleBufferSCExt`, `CMTime`
- `SCShareableContent`, `SCDisplay`, `SCWindow`, `SCRunningApplication`
- `SCContentFilter`
- `SCStreamConfiguration`
- `SCStreamOutputTrait`
- `SCStreamOutputType`
- `SCStream`
- `SCError`, `SCResult`

Source: `screencapturekit-8.0.0/src/lib.rs:799-817`

---

### `SCShareableContent::get()`

```rust
pub fn get() -> SCResult<SCShareableContent>
```

Blocking synchronous call; returns `Err(SCError)` when screen-recording permission is denied.

`SCShareableContent::displays()` → `Vec<SCDisplay>`

Source: `screencapturekit-8.0.0/src/shareable_content/mod.rs:203`

---

### `SCContentFilter`

Builder pattern — entry point `SCContentFilter::create()`:

```rust
SCContentFilter::create()
    .with_display(&display)          // SCDisplay borrow
    .with_excluding_windows(&[])     // &[&SCWindow] or &[SCWindow]
    .build()
    -> SCContentFilter
```

Source: `screencapturekit-8.0.0/src/stream/content_filter.rs`

---

### `SCStreamConfiguration`

Builder pattern:

```rust
SCStreamConfiguration::new()
    .with_width(2_u32)
    .with_height(2_u32)
    .with_captures_audio(true)
    .with_sample_rate(48_000_i32)   // impl Into<i32>
    .with_channel_count(2_i32)      // impl Into<i32>
```

Sources:
- `with_width` / `with_height`: `screencapturekit-8.0.0/src/stream/configuration/dimensions.rs:45,103`
- `with_captures_audio`: `screencapturekit-8.0.0/src/stream/configuration/audio.rs:187`
- `with_sample_rate`: `screencapturekit-8.0.0/src/stream/configuration/audio.rs:232`
- `with_channel_count`: `screencapturekit-8.0.0/src/stream/configuration/audio.rs:288`

---

### `SCStreamOutputTrait`

```rust
pub trait SCStreamOutputTrait: Send + Sync + 'static {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType);
}
```

Source: `screencapturekit-8.0.0/src/stream/output_trait.rs`

---

### `SCStreamOutputType`

```rust
pub enum SCStreamOutputType {
    Screen,
    Audio,          // system audio (macOS 13.0+)
    Microphone,     // macOS 15.0+
}
```

Source: `screencapturekit-8.0.0/src/stream/output_type.rs:27-38`

---

### `SCStream`

```rust
pub fn new(filter: &SCContentFilter, config: &SCStreamConfiguration) -> SCStream
pub fn add_output_handler<H: SCStreamOutputTrait>(&mut self, handler: H, output_type: SCStreamOutputType)
pub fn start_capture(&mut self) -> SCResult<()>
pub fn stop_capture(&mut self) -> SCResult<()>
```

Source: `screencapturekit-8.0.0/src/stream/sc_stream.rs`

---

### `CMSampleBuffer` — audio accessors

#### `format_description()`

```rust
// Directly on CMSampleBuffer struct (from apple_cf::cm::CMSampleBuffer)
pub fn format_description(&self) -> Option<CMFormatDescription>
```

Source: `apple-cf-0.9.3/src/cm/sample_buffer.rs:179`

Returns `None` for audio buffers in practice in some cases; format info can also come from `CMFormatDescription` methods (sample rate, channel count, bit depth).

#### `audio_buffer_list()`

```rust
// From CMSampleBufferExt trait (in prelude)
fn audio_buffer_list(&self) -> Option<AudioBufferList>
```

**DEVIATION FROM BRIEF**: The brief used `sample.get_audio_buffer_list()` — the real method is `audio_buffer_list()` (no `get_` prefix). The trait is `CMSampleBufferExt`, which is re-exported in `screencapturekit::prelude::*`.

Source: `screencapturekit-8.0.0/src/cm/sample_buffer.rs:331,427`

---

### `AudioBufferList`

```rust
pub struct AudioBufferList { /* private fields */ }

impl AudioBufferList {
    pub fn num_buffers(&self) -> usize
    pub fn get(&self, index: usize) -> Option<&AudioBuffer>
    pub fn buffer(&self, index: usize) -> Option<AudioBufferRef<'_>>
    pub fn iter(&self) -> AudioBufferListIter<'_>
}
```

Source: `screencapturekit-8.0.0/src/cm/audio.rs:153-195`

---

### `AudioBuffer`

```rust
#[repr(C)]
pub struct AudioBuffer {
    pub number_channels: u32,
    pub data_bytes_size: u32,
    // data_ptr: *mut c_void (private)
}

impl AudioBuffer {
    pub fn data(&self) -> &[u8]           // raw bytes (NOT f32 slice)
    pub fn data_byte_size(&self) -> usize
}
```

Source: `screencapturekit-8.0.0/src/cm/audio.rs:19-98`

**Key for T3**: To interpret as `f32` samples:
```rust
let bytes = buf.data();
let floats: &[f32] = bytemuck::cast_slice(bytes);
// or without bytemuck:
let floats: Vec<f32> = bytes.chunks_exact(4)
    .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
    .collect();
```

The CoreAudio / ScreenCaptureKit convention is **IEEE 754 float32, little-endian, planar** (one `AudioBuffer` per channel when planar). This is to be **confirmed at runtime** in the deferred smoke session.

---

## API Deviations from Brief

| Brief (illustrative) | Real crate v8 API | Notes |
|---|---|---|
| `sample.get_audio_buffer_list()` | `sample.audio_buffer_list()` | No `get_` prefix; from `CMSampleBufferExt` in prelude |
| `sample.format_description()` | `sample.format_description()` | Correct; returns `Option<CMFormatDescription>` |
| `SCStreamOutputTrait::did_output_sample_buffer(_, sample, _t)` | `did_output_sample_buffer(&self, sample, of_type)` | Matches; param name difference only |
| `SCContentFilter::create().with_display(display).with_excluding_windows(&[]).build()` | Same | Confirmed correct |
| `SCStreamConfiguration::new().with_captures_audio(true).with_sample_rate(48_000).with_channel_count(2)` | Same | Confirmed correct |

---

## Compile Command + Output

```
cd src-tauri && cargo test --test sckit_probe --no-run
```

Output (trimmed):
```
   Compiling screencapturekit v8.0.0
   Compiling voice-notes v0.1.0 (...)
warning: struct `MockCapture` is never constructed   [pre-existing, benign]
warning: associated function `from_wav` is never used  [pre-existing, benign]
warning: `voice-notes` (lib) generated 2 warnings
    Finished `test` profile [unoptimized + debuginfo] target(s) in 19.30s
  Executable tests/sckit_probe.rs (target/debug/deps/sckit_probe-9887d28a6744db1c)
```

Zero errors. Two pre-existing warnings (not introduced by this task).

---

## Files Changed

- `src-tauri/Cargo.toml` — added `[target.'cfg(target_os = "macos")'.dependencies]` with `screencapturekit = { version = "8", features = ["macos_13_0"] }`
- `src-tauri/Cargo.lock` — updated with new crate + transitive deps
- `src-tauri/tests/sckit_probe.rs` — new probe test (ignored, manual)
- `.superpowers/sdd/p2-sckit-spike.md` — this file

---

## Runtime Items to Confirm at Smoke

- Actual sample rate delivered (should be 48 000 Hz as configured)
- Number of `AudioBuffer`s per `AudioBufferList` (1 = interleaved stereo; 2 = planar L+R)
- `number_channels` field on each `AudioBuffer`
- Whether `data()` bytes are f32 LE (`mFormatFlags & kAudioFormatFlagIsFloat`)
- What `SCShareableContent::get()` returns / panics with when permission is denied

---

## Self-Review Notes

- Probe is `#[ignore]` — will not run in CI
- `#![cfg(target_os = "macos")]` at crate level gates the entire file
- No changes to the `AudioCapture` trait or existing modules
- The `screencapturekit` dep is gated with `[target.'cfg(target_os = "macos")'.dependencies]` — cross-platform build is unaffected
- Two pre-existing warnings (`MockCapture`) are unchanged — not our concern

## 运行时实锤（2026-07-02 冒烟，macOS 26 Apple Silicon）
探针实测（授权屏幕录制 + TTS 放音）：
- **采样率**：`format_description().audio_sample_rate()` = `Some(48000.0)`（48k 回退分支用不上，但保留无害）。
- **布局**：`num_buffers=2`，每 buffer `number_channels=1`、3840 bytes（=960 样本/20ms）→ **PLANAR**，extract_audio_mono 走 `n>1` → planar_to_mono 分支。两平面内容相同（系统把 mono 源上混为双声道）。
- **样本格式**：f32-LE 确认——有声帧解码为平滑波形（head=[0.0516, 0.1088, 0.0751]，peak≈0.24 随语音起伏，静音=0）；错误编码会解出乱码级数值。codec="lpcm"。
- **权限拒绝**：未授权时 `SCShareableContent::get()` → `NoShareableContent("Content unavailable: 用户拒绝了…TCC")` → T3 的 `unauthorized:` 前缀 → `denied` 分类正确。
- **静音行为**：无声时 SCKit 仍持续投递全零 buffer（回调不断流）。
- 结论：T3 `extract_audio_mono`/`bytes_to_f32`/`audio_sample_rate` 全部假设与实测一致，无需修改。
