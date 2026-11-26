# LIVI (`YADA-Livi`)

**LIVI** is a minimal, pure-Rust native macOS menu bar dictation app powered by **Gemini 3.5 Transcribe Live** (`gemini-3.5-transcribe-live`).

Built directly on AppKit, CoreGraphics, and CoreAudio with zero WebView overhead, LIVI streams microphone audio over a pre-warmed WebSocket connection and pastes finalized transcripts straight at your cursor while preserving your clipboard.

---

## Features

- **Low-Latency Streaming Transcription** — Connects to `gemini-3.5-transcribe-live` over bidirectional WebSockets with manual VAD (`activityStart` / `activityEnd`) and a pre-warmed **Standby Pool** (4-minute socket TTL with 25 s keepalive pings) for near-instant dictation start.
- **Smart Mode & Verbatim Mode** — Defaults to Gemini **Smart Mode** for clean, punctuated, self-corrected output; switch to **Verbatim** anytime from the menu bar.
- **Custom Vocabulary & 85+ Languages** — Add up to 1,000 custom vocabulary phrases via the native menu bar dialog for recognition biasing. Automatic multilingual detection & code-switching works out of the box, with optional BCP-47 language hints in `settings.json`.
- **Native macOS UI (Zero WebView)** —
  - Menu bar accessory app (`LSUIElement`) with a dynamic vector status icon (idle sparkle, coral-red listening indicator, cyan-to-violet polishing sparkle, and amber attention badge).
  - Non-activating dark obsidian floating pill overlay (`NSPanel`, forced `DarkAqua`) with a live 9-bar Hann-window FFT waveform, polishing spinner, and auto-dismissing status notices—legible over both Light and Dark windows and positioned on the screen under your cursor.
  - Native `NSSound` start/stop chimes (`Pop` and `Marimba` themes).
- **Flexible Global Hotkeys** — Powered by `handy-keys`, supporting side-specific modifier-only shortcuts (immune to macOS Secure Event Input) as well as standard modifier + key combos:
  - **Default**: `Right ⌥ + Right ⌘` (`option_right+command_right`)
  - **Activation modes**: **Hold or Toggle** (default: hold > 300 ms for push-to-talk, tap to toggle), **Push to Talk**, or **Toggle**.
  - **Interactive Shortcut Recorder** and presets (`Right ⌥ + Right ⌘`, `⌥ + Space`, `Right ⌥`, `Right ⌘`) in the menu bar.
  - Press `Esc` while listening or polishing to cancel immediately.
- **Receipt-Sequenced Cursor Paste** — Publishes transcripts to `NSPasteboard` as a lazy promise tagged with transient/concealed markers (so clipboard managers like Maccy and Paste ignore them), injects a keyboard-layout-aware `Cmd+V` via Carbon TIS + `CGEvent`, waits for the target app's read receipt, and restores your previous clipboard (text, PNG, or TIFF) without clobbering newer copies.
- **Lock-Free Audio Pipeline** — Captures audio via `cpal` into a wait-free SPSC ring buffer (`rtrb`) with zero-allocation mono downmixing and `rubato` FFT resampling to 16 kHz mono PCM16LE.
- **Built-in Stats & Latency Telemetry** — Tracks daily and all-time dictated word counts plus last-run latency (`stop_to_paste_ms`, server latency, warm/cold standby) directly in the menu bar, with structured JSONL telemetry on disk.

---

## Requirements

- **macOS 13.0+** (Ventura or later)
- **Rust** toolchain supporting **Edition 2024** (`rustc 1.85+`)
- **Xcode Command Line Tools** (`codesign`, `sips`, `iconutil`) and `python3` (used by the install script to generate `LIVI.icns`)
- A **Google Gemini API key** with access to `gemini-3.5-transcribe-live`

---

## Installation & Quick Start

### 1. Build & Install `/Applications/LIVI.app`

```bash
git clone https://github.com/tharunreddy/YADA-Livi.git
cd YADA-Livi
./scripts/install-macos.sh
```

The install script builds the release binary, generates `resources/LIVI.icns`, assembles `/Applications/LIVI.app`, ad-hoc signs it with `macos/Entitlements.plist`, resets TCC permissions so macOS prompts cleanly for the new signature, and launches LIVI.

When rebuilding after code changes without wanting to re-grant macOS permissions:

```bash
./scripts/install-macos.sh --keep-permissions
```

### 2. Configure API Key & Permissions

1. Click the **LIVI** icon in the macOS menu bar and select **Set Gemini API Key…** (LIVI immediately verifies the key and displays the connection RTT).
   - Alternatively, set the `GEMINI_API_KEY` environment variable; LIVI falls back to it whenever `gemini_api_key` in `settings.json` is empty.
2. Select **Permissions…** in the menu bar and grant:
   - **Microphone** — required to capture voice input.
   - **Accessibility** — required for global hotkeys and `Cmd+V` paste injection. LIVI automatically starts its hotkey listener as soon as Accessibility is granted.

### 3. Dictate Anywhere

Focus any text field and press **`Right ⌥ + Right ⌘`**:

- **Hold** (> 300 ms) and speak, then **release** to transcribe and paste.
- **Tap** to start hands-free recording, then **tap again** to stop and paste.
- Press **`Esc`** (or click **`×`** on the pill) to cancel.

---

## Configuration

Settings are persisted atomically (with `0600` file permissions) at:

```text
~/Library/Application Support/LIVI/settings.json
```

Most options can be changed directly from the menu bar; advanced options can be edited by choosing **Open settings.json** from the menu bar.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `gemini_api_key` | `string` | `""` | Gemini API key (falls back to `$GEMINI_API_KEY` if empty). Also supports `auth_tokens/...` ephemeral tokens. |
| `gemini_live_endpoint` | `string` | `wss://generativelanguage.googleapis.com/.../BidiGenerateContent` | Custom `ws://` or `wss://` Gemini Live endpoint URL. |
| `smart_mode` | `bool` | `true` | `true` for Gemini Smart Mode (`SMART`), `false` for `VERBATIM`. |
| `language_codes` | `string[]` | `[]` | Optional BCP-47 language hints (e.g. `["en-US"]`). Empty enables automatic 85+ language detection. |
| `custom_vocabulary` | `string[]` | `[]` | Up to 1,000 recognition-biasing terms/phrases. |
| `shortcut` | `string` | `"option_right+command_right"` | `handy-keys` binding string (e.g. `"option+space"`, `"option_right"`, `"command_right"`). |
| `shortcut_activation` | `string` | `"hold_or_toggle"` | `"hold_or_toggle"`, `"push_to_talk"`, or `"toggle"`. |
| `hold_threshold_ms` | `number` | `300` | Hold duration (ms) distinguishing hold vs. tap in `hold_or_toggle` mode. |
| `microphone` | `string \| null` | `null` | Exact input device name, or `null` for System Default. |
| `overlay_position` | `string` | `"bottom"` | Floating pill vertical position: `"bottom"` or `"top"`. |
| `sound_feedback` | `bool` | `true` | Enable/disable start and stop chimes. |
| `sound_theme` | `string` | `"pop"` | Chime sound theme: `"pop"` or `"marimba"`. |
| `sound_volume` | `number` | `0.7` | Chime playback volume (`0.0`–`1.0`). |
| `append_trailing_space` | `bool` | `true` | Append a trailing space after each pasted transcript. |
| `preserve_clipboard` | `bool` | `true` | Restore previous clipboard contents after target app reads the paste. |
| `launch_at_login` | `bool` | `false` | Register/unregister macOS Login Item via `SMAppService`. |
| `log_transcript_text` | `bool` | `true` | Include transcript text in `transcriptions.jsonl` (app logs never include transcript text). |
| `log_level` | `string` | `"info"` | Tracing log level (overridden by `RUST_LOG` when set). |

> **Note on Secure Event Input**: When macOS Secure Keyboard Entry is active (for example, in a password field or Terminal with Secure Keyboard Entry enabled), macOS blocks global shortcuts that include regular keys (like `⌥ + Space`). Modifier-only shortcuts such as the default `Right ⌥ + Right ⌘` continue working normally.

---

## Logs & Telemetry

Choose **Open Logs Folder** from the menu bar or inspect `~/Library/Logs/LIVI/`:

- **`livi.log.YYYY-MM-DD`** — Daily-rotating structured logs via `tracing` (7-day retention). Transcript text is never written to application logs.
- **`transcriptions.jsonl`** — Append-only per-dictation latency records (`schema_version: 1`) containing:
  - `outcome` (`success`, `empty`, `silent`, `failed`, `cancelled`), `mode`, `model`, `standby_hit`
  - Latency milestones in ms: `audio_duration_ms`, `mic_ready_ms`, `connect_ms`, `first_partial_ms`, `stop_to_eos_ms`, `eos_to_final_ms`, `stop_to_final_ms`, `paste_ms`, `stop_to_paste_ms`
  - `transcript_chars`, `transcript_words`, and (when `log_transcript_text` is `true`) `transcript`

---

## Development

```bash
# Run unit tests
cargo test

# Run locally in debug mode (logs also print to stderr when attached to a TTY)
RUST_LOG=debug cargo run
```

### Project Layout

```text
├── Cargo.toml
├── macos/
│   ├── Info.plist              # Accessory bundle metadata (macOS 13+, LSUIElement)
│   └── Entitlements.plist      # Microphone / audio-input entitlements
├── resources/                  # Start/stop chime WAVs and generated LIVI.icns
├── scripts/
│   └── install-macos.sh        # Release build, icon generation, signing & /Applications install
└── src/
    ├── main.rs                 # NSApplication bootstrap, Edit menu, single-instance guard
    ├── controller.rs           # Central dictation state machine & tray action coordinator
    ├── settings.rs             # Persistent JSON config, shortcut formatting, vocab normalization
    ├── hotkeys.rs              # Global shortcuts, dynamic Esc cancel, live capture (handy-keys)
    ├── secure_input.rs         # Carbon IsSecureEventInputEnabled detection
    ├── telemetry.rs            # tracing-appender logs, JSONL latency telemetry, word stats
    ├── asr/
    │   ├── mod.rs              # Pluggable AsrProvider / AsrSession traits & timing structs
    │   ├── ws_session.rs       # Provider-neutral paced/coalesced WebSocket streaming session
    │   └── gemini_live.rs      # Gemini 3.5 Transcribe Live protocol & pre-warmed StandbyPool
    ├── audio/
    │   ├── capture.rs          # Lock-free cpal + rtrb + rubato 16 kHz mono capture pipeline
    │   ├── visualizer.rs       # 512-sample Hann-window FFT spectrum analyzer
    │   └── sound.rs            # Native NSSound chime playback
    ├── paste/
    │   ├── mod.rs              # Receipt-sequenced paste state machine
    │   ├── macos.rs            # NSPasteboard lazy promise provider & clipboard restoration
    │   └── input.rs            # Carbon TIS layout-aware Cmd+V CGEvent injection
    └── macos/
        ├── tray.rs             # NSStatusItem custom vector icon, menu, and native NSAlert dialogs
        ├── overlay.rs          # Floating DarkAqua NSPanel pill overlay & 9-bar waveform view
        ├── permissions.rs      # AVFoundation Microphone & AX Accessibility checks/deep links
        └── autostart.rs        # SMAppService Launch at Login integration
```

---

## License

MIT — see [LICENSE](LICENSE).
