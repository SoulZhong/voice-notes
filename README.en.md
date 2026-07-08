<div align="center">

# voice-notes

**Local, real-time meeting transcription for macOS · speaker identification · fully offline**

[中文](./README.md) | English

[![platform](https://img.shields.io/badge/platform-macOS%2013%2B-black)](#requirements)
[![license](https://img.shields.io/badge/license-AGPL--3.0-blue)](#license)
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

## Installation

> **Let an AI assistant install it for you**: paste the block below verbatim to your AI coding assistant (Claude Code / Codex, etc.) and it will download, install, and register the MCP integration for you:
>
> ```text
> Please install voice-notes and set up its MCP integration for me:
> 1. Download the latest voice-notes_*_aarch64.dmg from
>    https://github.com/SoulZhong/voice-notes/releases;
> 2. Mount the DMG, copy voice-notes.app into /Applications, then run
>    xattr -dr com.apple.quarantine /Applications/voice-notes.app;
> 3. Run /Applications/voice-notes.app/Contents/MacOS/voice-notes mcp register --agent auto
>    to register the MCP server, and confirm with ... mcp status --json;
> 4. Remind me to open the app once manually to finish downloading the
>    recognition models (~1GB, requires the GUI).
> ```

### Requirements

- **macOS 13 or later**, Apple Silicon (M-series) Mac — system-audio capture relies on ScreenCaptureKit, and only arm64 packages are provided for now
- Disk space: ~60 MB for the app, ~1 GB for recognition models (downloaded on first launch)

### Steps

1. Download the latest `voice-notes_x.y.z_aarch64.dmg` from [Releases](https://github.com/SoulZhong/voice-notes/releases).
2. Open the DMG and drag **voice-notes** into Applications.
3. **First open**: the package is not code-signed yet, so double-clicking gets blocked by macOS. **Right-click the app → Open → Open** (one time only), or run:
   ```bash
   xattr -d com.apple.quarantine /Applications/voice-notes.app
   ```
4. On first launch you'll see a **welcome screen**: hit "Get Started" to download the recognition models (~1 GB, mirrored, resumable). When it finishes you land on the recording page, ready to go.
   Want models/data on a custom location (e.g. an external drive)? Click "Advanced settings →" on the welcome screen and set the directories under **Settings → Storage** *before* downloading.

### Grant two system permissions

Both are on-demand; denying them degrades gracefully:

| Permission | When it appears | Used for | If denied |
| --- | --- | --- | --- |
| Microphone | First time you start recording | Transcribing your speech | System audio only |
| Screen Recording | First time system audio is captured | Audio stream of other apps only — **no frames are read** | Microphone only (with an in-app notice) |

If you denied one, re-enable it later under **System Settings → Privacy & Security**.

## Configuration

Works out of the box — every setting has a sensible default. Adjust as needed (all in **Settings**):

| Group | Item | Notes |
| --- | --- | --- |
| General | Appearance / launch at login / menu bar icon / global shortcut | Shortcut defaults to `⌥⌘R`, opt-in |
| Storage | **Data directory / models directory** | Relocatable anywhere (iCloud / external drive); existing content is migrated automatically |
| Storage | Audio disk usage & cleanup | Deletes audio only; transcripts and speakers are kept |
| Recording | System audio only / keep output volume / garbage filter / keep audio | Scenario guide below under "Recording options" |
| AI polish | LLM post-processing (optional) | Any OpenAI-compatible API (DeepSeek / Qwen / Doubao / Kimi presets); fixes typos and merges speakers after the meeting. Works fine without it |
| Speech models | SenseVoice (default) / Whisper / Paraformer | The default is best for Chinese-centric meetings |

### Run from source (developers)

- [Rust](https://rustup.rs) (stable) and Node.js 18+
- meson and ninja (to build the vendored WebRTC echo-cancellation module): `pip3 install --user meson ninja`

```bash
git clone https://github.com/SoulZhong/voice-notes.git
cd voice-notes
npm install
npm run tauri dev      # development
npm run tauri build    # build the .app + .dmg
```

Models can also be prefetched outside the app: `./scripts/fetch_models.sh`

| Model | Purpose | Notes |
| --- | --- | --- |
| Silero VAD | Voice activity detection / segmentation | Required, tiny |
| SenseVoice | Speech recognition (zh/en/ja/ko/yue) | Default ASR |
| Whisper base | Speech recognition (multilingual) | Optional, switchable in Settings |
| CAM++ (3D-Speaker) | Speaker embeddings | Optional; without it you get transcription only |

## Connecting an AI assistant (MCP)

voice-notes ships a built-in MCP (Model Context Protocol) server. Once registered, Claude Code / Claude Desktop / Cursor / Codex CLI / Gemini CLI can query your meeting notes directly — "What delivery date did we agree with Zhang San last week?", "Turn today's standup into an email."

> **Privacy note**: once an agent retrieves note content, it enters that agent's LLM context — whether that leaves your machine depends on the agent and model you use. **voice-notes itself still never uploads anything.** "Allow AI to control recording" is off by default; enable it under Settings → AI Assistant Access.

Three ways to connect (pick one):

1. **In-app**: check the box on the first-launch welcome screen, or go to Settings → AI Assistant Access anytime to register/remove.
2. **Command line** (no UI needed; an agent can run this directly):

   ```bash
   /Applications/voice-notes.app/Contents/MacOS/voice-notes mcp register --agent auto   # register with every detected agent
   /Applications/voice-notes.app/Contents/MacOS/voice-notes mcp status --json           # check registration status
   ```

3. **Manual configuration** (for agents not auto-detected): add this to its MCP config:

   ```json
   { "mcpServers": { "voice-notes": {
       "command": "/Applications/voice-notes.app/Contents/MacOS/voice-notes",
       "args": ["mcp", "serve"] } } }
   ```

   Codex CLI (`~/.codex/config.toml`):

   ```toml
   [mcp_servers.voice-notes]
   command = "/Applications/voice-notes.app/Contents/MacOS/voice-notes"
   args = ["mcp", "serve"]
   ```

Tools provided:

| Tool | Purpose | Requires the app running |
| --- | --- | --- |
| `list_notes` | List notes (pagination / time filter) | No |
| `search_notes` | Full-text search over transcripts | No |
| `get_note` | Read a note's full text (AI-polished version preferred) | No |
| `list_speakers` | Global voiceprint library / speakers | No |
| `recording_status` / `get_live_transcript` | Recording status / live transcript | Yes |
| `start/stop/pause/resume_recording` | Control recording (disabled by default, enable in Settings) | Yes |

## Usage

1. Hit **Start Recording** (or the global shortcut, default `⌥⌘R`).
2. Talk — the current sentence stays centered and enlarged, speaker badges are assigned live, and any new voice that accumulates 10 seconds gets a global speaker number.
3. Stop to land in the note view: play back, edit text, name speakers (name once, applies everywhere), export.
4. Manage everyone in the **Buddies (voiceprint library)** page: audition their voice sample, rename, merge mis-split entries.

### Recording options (Settings → Recording)

| Scenario | Recommendation |
| --- | --- |
| Listening only (you don't speak) | Enable **System audio only**: the mic never starts, playback volume and quality are untouched |
| Speaker-phone meeting where your own speech must be recorded | Enable **Keep output volume while recording**: bypasses macOS voice-processing's volume ducking; echo is removed by the built-in software canceller (WebRTC AEC3) |
| Wearing headphones | Leave both off: system echo cancellation stays on for the cleanest transcript |

## FAQ

**"App is damaged" / "cannot be opened" on double-click?**
That's macOS Gatekeeper blocking the unsigned package, not corruption. Right-click → Open → Open, or run `xattr -d com.apple.quarantine /Applications/voice-notes.app` and open normally.

**"Start Recording" does nothing / says models are missing?**
The recognition models haven't finished downloading. Return to the welcome screen or **Settings → Speech models** to continue (downloads resume); you can also switch the download mirror in Settings.

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

[AGPL-3.0](./LICENSE) © zhongweijian (SoulZhong)

Free to use, study, modify, and self-host. If you distribute or operate this project (or a derivative) as a **closed-source product or service**, AGPL requires you to open-source the derived code. For commercial integrations that can't be open-sourced, contact the author via [GitHub Issues](https://github.com/SoulZhong/voice-notes/issues) for a **commercial license** (dual licensing).

> Note: v0.1.0 and earlier were released under MIT, which remains valid for copies already obtained; versions from this change onward are AGPL-3.0.
