# Vibo

**Vibo** is a **low-latency, low-RAM**, pure-Rust native macOS menu bar dictation app powered by **Gemini 3.5 Transcribe Live** (`gemini-3.5-transcribe-live`).

Built directly on AppKit, CoreGraphics, and CoreAudio with zero Electron or WebView overhead, Vibo uses **~31 MB of RAM**, streams microphone audio over a pre-warmed WebSocket connection, and pastes finalized transcripts straight at your cursor in **~315 ms** while preserving your clipboard.

---

## Performance & Footprint

Designed from the ground up to stay fast and out of your way:

- **Low RAM Footprint**: **~31 MB** physical memory footprint (~44 MB RSS) in Activity Monitor — no Electron, WebView, or browser runtime.
- **Tiny App Bundle**: **~3.8 MB** `/Applications/Vibo.app` (**3.3 MB** native release binary).
- **Sub-Second Dictation**: Pre-warmed standby WebSockets, a lock-free audio capture ring buffer, and receipt-sequenced native pasting keep local overhead to a few milliseconds.

---

## Features

- **Low-Latency Streaming Transcription** — Connects to `gemini-3.5-transcribe-live` over bidirectional WebSockets with manual VAD (`activityStart` / `activityEnd`) and a pre-warmed **Standby Pool** (45 s overlapped socket rotation with 20 s keepalive pings) for near-instant dictation start.
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
- **Xcode Command Line Tools** (`codesign`, `sips`, `iconutil`) and `python3` (used by the install script to generate `Vibo.icns`)
- A **Google Gemini API key** with access to `gemini-3.5-transcribe-live` ([how to create a Gemini API key](https://aistudio.google.com/docs/api-key))
  - **Tip:** **Google AI Pro** subscribers receive **$10/month** in Google Cloud & Gemini API credits via the [Google Developer Program](https://developers.google.com/program) — see [how to claim monthly AI plan credits in Google AI Studio](https://aistudio.google.com/docs/google-ai-plans#claim-api-credits).

---

## Observed Latencies

Measured across 53 real-world **Smart Mode** dictation sessions on macOS with `gemini-3.5-transcribe-live` (44 warm standby pool hits, 9 cold connects):

| Stage | Metric | Median (p50) | p90 | Range (min–max) | What it measures |
| --- | --- | ---: | ---: | ---: | --- |
| **Stop → Paste (End-to-End)** | `stop_to_paste_ms` | **314 ms** | 402 ms | 247–463 ms | Releasing the hotkey to text pasted at your cursor |
| **Stop → Final Transcript** | `stop_to_final_ms` | **297 ms** | 380 ms | 229–446 ms | Hotkey release to finalized transcript received |
| **Server Finalization** | `eos_to_final_ms` | **294 ms** | 377 ms | 226–441 ms | `activityEnd` sent to final Gemini response |
| **Audio Flush** | `stop_to_eos_ms` | **2.6 ms** | 2.8 ms | 0.8–9.8 ms | Stop signal to final PCM chunk + `activityEnd` sent |
| **WebSocket Attach (Warm)** | `connect_ms` | **0.2 ms** | 0.3 ms | 0.1–0.3 ms | Claiming a pre-warmed socket from the Standby Pool |
| **WebSocket Attach (Cold)** | `connect_ms` | **194 ms** | — | 179–225 ms | Fresh TLS + WebSocket handshake + setup |
| **Microphone Startup** | `mic_ready_ms` | **70 ms** | 73 ms | 58–94 ms | Opening CoreAudio input stream |
| **Cursor Paste** | `paste_ms` | **1.7 ms** | 2.8 ms | 0.7–10.9 ms | `NSPasteboard` handoff and `Cmd+V` injection |

> Network latency varies by connection and distance to Gemini API endpoints. Live per-run latency is always visible in the Vibo menu bar and logged to `~/Library/Logs/Vibo/transcriptions.jsonl`.

---

## Installation & Quick Start

### 1. Build & Install `/Applications/Vibo.app`

```bash
git clone https://github.com/tharunreddy/Vibo.git
cd Vibo
./scripts/install-macos.sh
```

The install script builds the release binary, generates the `Vibo.icns` app icon, assembles `/Applications/Vibo.app`, ad-hoc signs it with `macos/Entitlements.plist`, resets TCC permissions so macOS prompts cleanly for the new signature, and launches Vibo.

When rebuilding after code changes without wanting to re-grant macOS permissions:

```bash
./scripts/install-macos.sh --keep-permissions
```

### 2. Configure API Key & Permissions

1. Click the **Vibo** icon in the macOS menu bar and select **Set Gemini API Key…** (Vibo immediately verifies the key and displays the connection RTT). If you don't have a key yet, see the [official Gemini API key documentation](https://aistudio.google.com/docs/api-key).
   - **Google AI Pro subscribers:** You get **$10/month** in Google Cloud & Gemini API credits through the [Google Developer Program](https://developers.google.com/program) that apply to your paid Gemini API key ([claim your monthly credits in AI Studio](https://aistudio.google.com/docs/google-ai-plans#claim-api-credits)).
   - Alternatively, set the `GEMINI_API_KEY` environment variable; Vibo falls back to it whenever `gemini_api_key` in `settings.json` is empty.
2. Select **Permissions…** in the menu bar and grant:
   - **Microphone** — required to capture voice input.
   - **Accessibility** — required for global hotkeys and `Cmd+V` paste injection. Vibo automatically starts its hotkey listener as soon as Accessibility is granted.

### 3. Dictate Anywhere

Focus any text field and press **`Right ⌥ + Right ⌘`**:

- **Hold** (> 300 ms) and speak, then **release** to transcribe and paste.
- **Tap** to start hands-free recording, then **tap again** to stop and paste.
- Press **`Esc`** (or click **`×`** on the pill) to cancel.

---

## Configuration

Settings are persisted atomically (with `0600` file permissions) at:

```text
~/Library/Application Support/Vibo/settings.json
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

Choose **Open Logs Folder** from the menu bar or inspect `~/Library/Logs/Vibo/`:

- **`vibo.log.YYYY-MM-DD`** — Daily-rotating structured logs via `tracing` (7-day retention). Transcript text is never written to application logs.
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

---

## License

MIT — see [LICENSE](LICENSE).

---

*Vibo is an independent open-source project and is not affiliated with or endorsed by Google. Gemini is a trademark of Google LLC.*
