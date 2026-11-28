# Changelog

All notable changes to Vibo are documented in this file.

## [0.1.1] - 2026-11-27

### Performance & Reliability

- **Overlapped 45 s Standby Pool rotation & peer-close recovery** — Proactively rotates pre-warmed Gemini Live sockets at 45 s (with overlap and a 54 s max claim age) and reconnects on peer close during the 15-minute active window, eliminating the 60 s server idle cliff (`code 1008`) and keeping standby warm hits near 100%.
- **~20 ms lower stop-to-EOS latency** — Gates CoreAudio capture atomically, unparks the audio consumer immediately to flush the resampler in <1 ms, detaches synchronous CoreAudio stream teardown to a background thread, and measures `stop_to_eos_ms` from the true stop request timestamp.
- **Zero idle polling wakeups** — Replaced the 10 ms hotkey polling loop with blocking event-driven `KeyboardListener::recv()`, and parked the background monitor on `tokio::sync::Notify` when Accessibility is granted and a modifier-only shortcut is active (`0.0%` idle CPU, `0` idle wakeups/s).
- **Faster pasting & lazy tray updates** — Pre-snapshots the clipboard in parallel while Gemini finalizes (eliminating 100+ ms paste tail spikes on heavy/promised clipboard items), caches Carbon TIS `Cmd+V` keycode resolution per keyboard layout, and defers CoreAudio input device enumeration to `menuWillOpen:`.

## [0.1.0] - 2026-11-26

### Added

- Initial pure-Rust native macOS menu bar dictation app powered by Gemini 3.5 Transcribe Live (`gemini-3.5-transcribe-live`).
- Smart Mode & Verbatim Mode, custom vocabulary biasing (up to 1,000 terms), and 85+ automatic language detection.
- Lock-free `cpal` + `rtrb` + `rubato` 16 kHz audio capture pipeline and 9-bar Hann-window FFT floating pill overlay (`NSPanel`).
- Global modifier-only and keyed shortcuts via `handy-keys`, receipt-sequenced `NSPasteboard` + `CGEvent` cursor pasting, daily/total word counts, and structured JSONL latency telemetry.
