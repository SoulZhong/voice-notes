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
- **Speaker identification with a global voiceprint library**: online voiceprint clustering tells speakers apart in real time, including mid-segment speaker changes. Anyone who speaks for 30+ seconds is enrolled into a global library and gets an identity that stays consistent across meetings — name them once and every future meeting shows their name. Mis-split entries can be merged, samples and all.
- **Buddy tidy-up & attribution suggestions**: unnamed speakers are automatically re-identified against the library (S-Norm score normalization survives cross-meeting channel drift); audition both sides inline before merging. Duplicate names guide you to link/merge; sample-less fragment entries can be cleaned in one pass. Each person keeps multiple "session centroids" (headset / speakerphone / other conditions each get a representative voiceprint) — accuracy compounds with use.
- **Editable speakers on the refined transcript**: click a speaker chip to rename (two-way sync with the library), mark "this is me", or link to a library person; export is WYSIWYG (refined view exports refined content).
- **Configurable voiceprint model**: CAM++ (default) or ERes2NetV2; switching rebuilds the library from voice samples automatically, embedding spaces are strictly isolated.
- **Lyrics-style following**: while recording or playing back, the sentence being spoken stays pinned to the center of the screen, enlarged and highlighted, with history dimming above. Scroll up to review anytime; one tap returns to live.
- **Never lose a sentence**: every finalized segment is flushed to disk as it happens. Crashes, power loss, or accidental quits don't lose transcribed content, and interrupted meetings can be resumed with seamless timeline and speaker numbering.
- **Playback & verification**: original audio is kept per track (auto-compressed to AAC, ~14 MB/hour/source). Click any sentence's timestamp to listen from there, with the playhead followed lyrics-style.
- **Editable notes**: fix words, delete lines, reassign speakers, rename notes, export Markdown / plain text.
- **Native system integration**: menu bar tray, global shortcut for start/stop, launch at login, light & dark themes.
- **Tuned for Chinese-centric meetings**: SenseVoice (zh/en/ja/ko/yue) by default with optional Whisper, plus a language-hallucination filter that drops garbage output on silence.

## Installation

> **For AI assistants**: the README you're reading is your operating manual. If you're helping the user install voice-notes, run these steps directly (no need for the user to relay them); for connecting and usage after install, see the [Connecting an AI assistant](#connecting-an-ai-assistant-mcp--cli--skill) section below.
>
> 1. Download the latest `voice-notes_*_aarch64.dmg` from
>    https://github.com/SoulZhong/voice-notes/releases;
> 2. Mount the DMG, copy `voice-notes.app` into `/Applications`, then run
>    `xattr -dr com.apple.quarantine /Applications/voice-notes.app`;
> 3. Run `/Applications/voice-notes.app/Contents/MacOS/voice-notes mcp register --agent auto` to register MCP,
>    `... skill install` to install the Claude Code skill (optional), and confirm with `... mcp status --json`;
> 4. Remind the user to open the app once manually to finish downloading the
>    recognition models (~1GB, requires the GUI).
>
> Manual installation steps follow below.

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

## Connecting an AI assistant (MCP · CLI · Skill)

Let a local agent (Claude Code / Claude Desktop / Cursor / Codex CLI / Gemini CLI / WorkBuddy / OpenClaw / Hermes Agent — eight agents supported out of the box) read your meeting notes to answer questions, draft recaps, write weekly summaries — "What delivery date did we agree with Zhang San last week?", "Turn today's standup into an email", "Which meetings did we have this week and what are the action items?" Three surfaces, pick as needed:

| Surface | What it is | When to use |
| --- | --- | --- |
| **MCP server** | The standard agent tool protocol, 10 tools (search / read full text / recording status & control) | Preferred, for MCP-capable agents |
| **Command line (CLI)** | The same query capabilities as commands, with `--json` | Scripts, CI, or as a fallback when an agent has no MCP configured |
| **Claude Code skill** | Teaches Claude Code when and how to combine the tools above (recap / weekly-summary / search workflows) | A nice-to-have so Claude works out of the box |

> **Privacy note**: once an agent retrieves note content, it enters that agent's LLM context — whether that leaves your machine depends on the agent and model you use. **voice-notes itself still never uploads anything.** Recording-control tools are disabled by default; enable "Allow AI to control recording" on the "AI" tab in the sidebar.

**Fastest start**: open the "AI" tab in the sidebar to register MCP and install the skill in one click. (An AI assistant can just follow the [Installation](#installation) steps and do it itself.)

### MCP server

Three ways to register (pick one):

1. **In-app**: check the box on the welcome screen, or go to the "AI" tab in the sidebar to register/remove.
2. **Command line** (an agent can run this directly):

   ```bash
   VN=/Applications/voice-notes.app/Contents/MacOS/voice-notes
   "$VN" mcp register --agent auto    # register with every detected agent (claude-code/claude-desktop/cursor/codex/gemini/workbuddy/openclaw/hermes)
   "$VN" mcp status --json            # check each agent's registration status
   "$VN" mcp unregister --agent cursor
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

The 10 tools:

| Tool | Purpose | Prerequisite |
| --- | --- | --- |
| `list_notes` | List notes (pagination / time filter; includes speaker count and whether a polished version exists) | App need not be running |
| `search_notes` | Full-text search over transcripts, hits carry one sentence of context on each side | App need not be running |
| `get_note` | Read a note's full text (AI-polished version preferred by default) | App need not be running |
| `list_speakers` | Global voiceprint library / speakers (stable cross-meeting id / name) | App need not be running |
| `recording_status` | Current recording state | App running |
| `get_live_transcript` | Live transcript of the in-progress session | App running |
| `start_recording` / `stop_recording` / `pause_recording` / `resume_recording` | Control recording | App running, **and** "Allow AI to control recording" enabled |

The four query tools read local data files directly and work even when the app is closed; the rest go through an in-app local socket and need the app running.

### Query from the command line (no MCP needed)

For scripts, CI, or when an agent has no MCP configured. Human-readable tables by default; `--json` returns **the same JSON** as the corresponding MCP tool:

```bash
VN=/Applications/voice-notes.app/Contents/MacOS/voice-notes
"$VN" notes list [--limit N] [--offset N] [--from 2026-07-01] [--to 2026-07-08] [--json]
"$VN" notes search "delivery date" [--limit N] [--json]
"$VN" notes get <note-id> [--format md|txt|json] [--raw]   # md by default; --raw = raw transcript (ignore the polished version)
"$VN" speakers list [--json]
```

Recording control (needs the app running; `start/stop/pause/resume` also need "Allow AI to control recording" enabled on the "AI" tab in the sidebar):

```bash
"$VN" record status
"$VN" record start --title "Design review"
"$VN" record stop
"$VN" record live [--tail N]
```

Batch title backfill — AI generates topic titles for meetings still carrying the default title (manually named notes are never touched):

```bash
"$VN" notes retitle [--dry-run] [--agent claude] [--model M]
```

AI call log (every outbound AI call made for refinement/title generation is fully recorded — request and response; also viewable and exportable on the "AI" page):

```bash
"$VN" ailog list [--limit N] [--kind refine_chunk|title|agent_refine|mcp_apply] [--note ID] [--json]
"$VN" ailog export [--out PATH]   # merge everything into one JSONL file
```

Exit codes: `0` success / `1` execution error (e.g. note-id not found) / `2` usage error (missing or unknown argument); unknown flags always error out rather than being silently ignored.

### Claude Code skill

One command teaches Claude Code the workflows for meeting recaps, weekly summaries, and finding decisions/action items (or install/remove it with one click under Settings → AI Assistant Access):

```bash
/Applications/voice-notes.app/Contents/MacOS/voice-notes skill install   # status to check / uninstall to remove
```

Installs to `~/.claude/skills/voice-notes/` and auto-updates on app upgrade (without overwriting a same-named skill you've customized).

### Notes for agents

- **Prefer MCP tools; fall back to the CLI when unavailable** (`notes … --json`, `speakers list --json` — the same JSON as MCP). If neither works, register per the commands above: `mcp register --agent auto`.
- **Search before fetching full text**: `search_notes` for the note_id → `get_note`; don't pull all of `list_notes` and read each one.
- `get_note` returns the AI-polished version by default (the `refined` field marks the source); for the raw transcript use CLI `--raw` or MCP `prefer_refined=false`.
- Recording control is disabled by default; when refused, relay "enable it on the AI tab in the sidebar" to the user instead of retrying.
- Notes are the user's private local data — once in your context they've left the machine. Retrieve only as needed, and confirm intent before quoting long passages.

## Usage

1. Hit **Start Recording** (or the global shortcut, default `⌥⌘R`).
2. Talk — the current sentence stays centered and enlarged, speaker badges are assigned live, and any new voice that accumulates 30 seconds gets a global speaker number.
3. Stop to land in the note view: play back, edit text, name speakers (name once, applies everywhere), export.
4. Manage everyone in the **Buddies (voiceprint library)** page: audition their voice samples, rename, merge mis-split entries, delete bad samples.
5. When the sidebar "Overview & Tidy-up" entry shows a badge, walk through the attribution suggestions (audition inline, then merge) or clean up unidentifiable sample-less entries.

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

[AGPL-3.0](./LICENSE)

Free to use, study, modify, and self-host. If you distribute or operate this project (or a derivative) as a **closed-source product or service**, AGPL requires you to open-source the derived code. For commercial integrations that can't be open-sourced, contact the author via [GitHub Issues](https://github.com/SoulZhong/voice-notes/issues) for a **commercial license** (dual licensing).
