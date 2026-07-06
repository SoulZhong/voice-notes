<div align="center">

# voice-notes

**Local, real-time meeting transcription for macOS · speaker identification · fully offline**

[中文](./README.md) | English

[![platform](https://img.shields.io/badge/platform-macOS%2013%2B-black)](#requirements)
[![license](https://img.shields.io/badge/license-MIT-blue)](#license)
[![tauri](https://img.shields.io/badge/Tauri-2-24C8DB)](https://tauri.app)
[![rust](https://img.shields.io/badge/Rust-stable-orange)](https://www.rust-lang.org)

</div>

Open it when a meeting starts. Every sentence — yours, theirs, whatever comes out of the speakers — becomes a speaker-labeled text note in real time. All recognition runs on your Mac; **no audio or text ever leaves your machine**.

## Features

- **Dual-source live transcription**: captures the microphone and system audio (ScreenCaptureKit) simultaneously, so both what you say and what you hear in online meetings end up in the note. Cross-channel echo dedup keeps speaker bleed-through from being transcribed twice.
- **Fully local & offline**: ASR / VAD / speaker models all run on-device via sherpa-onnx. Works without a network connection; nothing is uploaded, ever.
- **Speaker identification with a global voiceprint library**: online voiceprint clustering tells speakers apart in real time, including mid-segment speaker changes. Anyone who speaks for 10+ seconds is enrolled into a global library and gets an identity that stays consistent across meetings — name them once and every future meeting shows their name. Mis-split entries can be merged, samples and all.
- **Lyrics-style following**: while recording or playing back, the sentence being spoken stays pinned to the center of the screen, enlarged and highlighted, with history dimming above. Scroll up to review anytime; one tap returns to live.
- **Never lose a sentence**: every finalized segment is flushed to disk as it happens. Crashes, power loss, or accidental quits don't lose transcribed content, and interrupted meetings can be resumed with seamless timeline and speaker numbering.
- **Playback & verification**: original audio is kept per track (auto-compressed to AAC, ~14 MB/hour/source). Click any sentence's timestamp to listen from there, with the playhead followed lyrics-style.
- **Editable notes**: fix words, delete lines, reassign speakers, rename notes, export Markdown / plain text.
- **Native system integration**: menu bar tray, global shortcut for start/stop, launch at login, light & dark themes.
- **Tuned for Chinese-centric meetings**: SenseVoice (zh/en/ja/ko/yue) by default with optional Whisper, plus a language-hallucination filter that drops garbage output on silence.

## Quick Start

### Requirements

- macOS 13 or later (system-audio capture relies on ScreenCaptureKit)
- [Rust](https://rustup.rs) (stable) and Node.js 18+
- meson and ninja (to build the vendored WebRTC echo-cancellation module): `pip3 install --user meson ninja`
- Permissions: Microphone (your speech), Screen Recording (used **only** to capture system audio — no frames are read)

### Run from source

```bash
git clone https://github.com/SoulZhong/voice-notes.git
cd voice-notes
npm install
npm run tauri dev      # development
npm run tauri build    # build the .app
```

### Models

Download in-app on first launch (**Settings → Models**, with a mirror for restricted networks), or prefetch:

```bash
./scripts/fetch_models.sh
```

| Model | Purpose | Notes |
| --- | --- | --- |
| Silero VAD | Voice activity detection / segmentation | Required, tiny |
| SenseVoice | Speech recognition (zh/en/ja/ko/yue) | Default ASR |
| Whisper base | Speech recognition (multilingual) | Optional, switchable in Settings |
| CAM++ (3D-Speaker) | Speaker embeddings | Optional; without it you get transcription only |

## Usage

1. Hit **Start Recording** (or the global shortcut, default `⌥⌘R`).
2. Talk — the current sentence stays centered and enlarged, speaker badges are assigned live, and any new voice that accumulates 10 seconds gets a global speaker number.
3. Stop to land in the note view: play back, edit text, name speakers (name once, applies everywhere), export.
4. Manage everyone in the **Voiceprints** page: audition their voice sample, rename, merge mis-split entries.

### Recording options (Settings → Recording)

| Scenario | Recommendation |
| --- | --- |
| Listening only (you don't speak) | Enable **System audio only**: the mic never starts, playback volume and quality are untouched |
| Speaker-phone meeting where your own speech must be recorded | Enable **Keep output volume while recording**: bypasses macOS voice-processing's volume ducking; echo is removed by the built-in software canceller (WebRTC AEC3) |
| Wearing headphones | Leave both off: system echo cancellation stays on for the cleanest transcript |

## FAQ

**Why does it need Screen Recording permission?**
Capturing system audio (sound played by other apps) on macOS is only possible through ScreenCaptureKit, which lives under the Screen Recording permission. Only the audio stream is consumed; no screen content is read.

**Playback volume drops when recording starts / people say my voice got quieter?**
That's macOS voice-processing (VPIO echo cancellation) ducking, a system behavior. Use one of the two toggles above to eliminate it.

**Where is my data?**
In the app data directory by default, relocatable in Settings (e.g. iCloud or an external drive). One folder per meeting: `meta.json` + `segments.jsonl` (sentence-by-sentence transcript) + audio tracks + `speakers.json` — plain text formats any tool can read.

**Windows / Linux?**
macOS only for now (system-audio capture, echo cancellation, and the menu bar all depend on platform APIs). The transcription pipeline itself is cross-platform Rust — contributions of audio-capture layers for other platforms are welcome.

## How it works

```
Microphone ──┐                                  ┌─ live captions (lyrics-style)
             ├─ VAD ── ASR ──── speaker ────────┼─ per-sentence journal (segments.jsonl)
System audio ┘ (Silero) (SenseVoice) clustering └─ global voiceprint library
               echo dedup · language filter · in-segment speaker split (CAM++)
```

Built with [Tauri 2](https://tauri.app) (Rust backend + system integration), [SvelteKit](https://svelte.dev) (UI), and [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) (on-device inference). UI follows the design system in [DESIGN.md](./DESIGN.md).

## Development

```bash
npm run check                 # frontend type checking
cd src-tauri && cargo test    # backend tests
```

## License

[MIT](./LICENSE)
