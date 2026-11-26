//! Persistent JSON settings for Vibo (`~/Library/Application Support/Vibo/settings.json`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "Vibo";
pub const DEFAULT_GEMINI_LIVE_ENDPOINT: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
pub const DEFAULT_SHORTCUT: &str = "option_right+command_right";
pub const DEFAULT_HOLD_THRESHOLD_MS: u64 = 300;
pub const DEFAULT_SOUND_VOLUME: f32 = 0.7;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutActivation {
    /// Hold longer than `hold_threshold_ms` to push-to-talk, or tap to toggle.
    #[default]
    HoldOrToggle,
    /// Hold to record, release to stop.
    PushToTalk,
    /// Press once to start, press again to stop.
    Toggle,
}

impl ShortcutActivation {
    pub fn label(self) -> &'static str {
        match self {
            Self::HoldOrToggle => "Hold or Toggle",
            Self::PushToTalk => "Push to Talk",
            Self::Toggle => "Toggle",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayPosition {
    #[default]
    Bottom,
    Top,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoundTheme {
    #[default]
    Pop,
    Marimba,
}

impl SoundTheme {
    pub fn start_filename(self) -> &'static str {
        match self {
            Self::Pop => "pop_start.wav",
            Self::Marimba => "marimba_start.wav",
        }
    }

    pub fn stop_filename(self) -> &'static str {
        match self {
            Self::Pop => "pop_stop.wav",
            Self::Marimba => "marimba_stop.wav",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub gemini_api_key: String,
    pub gemini_live_endpoint: String,
    pub smart_mode: bool,
    /// BCP-47 language hints (e.g. `["en-US"]`). Empty = automatic detection across 85+ languages.
    pub language_codes: Vec<String>,
    /// Up to 1,000 custom vocabulary phrases for recognition biasing.
    pub custom_vocabulary: Vec<String>,
    /// `handy-keys` binding string, e.g. `"option_right+command_right"` or `"option+space"`.
    pub shortcut: String,
    pub shortcut_activation: ShortcutActivation,
    pub hold_threshold_ms: u64,
    /// Selected input device name (`None` = system default).
    pub microphone: Option<String>,
    pub overlay_position: OverlayPosition,
    pub sound_feedback: bool,
    pub sound_theme: SoundTheme,
    pub sound_volume: f32,
    pub append_trailing_space: bool,
    pub preserve_clipboard: bool,
    pub launch_at_login: bool,
    pub log_transcript_text: bool,
    pub log_level: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            gemini_api_key: String::new(),
            gemini_live_endpoint: DEFAULT_GEMINI_LIVE_ENDPOINT.to_string(),
            smart_mode: true,
            language_codes: Vec::new(),
            custom_vocabulary: Vec::new(),
            shortcut: DEFAULT_SHORTCUT.to_string(),
            shortcut_activation: ShortcutActivation::HoldOrToggle,
            hold_threshold_ms: DEFAULT_HOLD_THRESHOLD_MS,
            microphone: None,
            overlay_position: OverlayPosition::Bottom,
            sound_feedback: true,
            sound_theme: SoundTheme::Pop,
            sound_volume: DEFAULT_SOUND_VOLUME,
            append_trailing_space: true,
            preserve_clipboard: true,
            launch_at_login: false,
            log_transcript_text: true,
            log_level: "info".to_string(),
        }
    }
}

impl Settings {
    /// Returns the saved Gemini API key, falling back to `$GEMINI_API_KEY` if empty.
    pub fn effective_api_key(&self) -> String {
        let saved = self.gemini_api_key.trim();
        if !saved.is_empty() {
            return saved.to_string();
        }
        std::env::var("GEMINI_API_KEY")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_default()
    }

    /// Returns the effective Gemini Live endpoint URL.
    pub fn effective_live_endpoint(&self) -> &str {
        let trimmed = self.gemini_live_endpoint.trim();
        if trimmed.is_empty() {
            DEFAULT_GEMINI_LIVE_ENDPOINT
        } else {
            trimmed
        }
    }

    pub fn load() -> Self {
        let path = settings_path();
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(contents) => match serde_json::from_str::<Settings>(&contents) {
                Ok(mut parsed) => {
                    if parsed.shortcut.trim().is_empty() {
                        parsed.shortcut = DEFAULT_SHORTCUT.to_string();
                    }
                    parsed.sound_volume = parsed.sound_volume.clamp(0.0, 1.0);
                    parsed
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        path = %path.display(),
                        "Failed to parse settings.json; using defaults"
                    );
                    Self::default()
                }
            },
            Err(_) => {
                let defaults = Self::default();
                let _ = defaults.save_to(path);
                defaults
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&settings_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create settings dir {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("Failed to serialize settings")?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, format!("{json}\n"))
            .with_context(|| format!("Failed to write temp settings {}", tmp.display()))?;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        fs::rename(&tmp, path)
            .with_context(|| format!("Failed to replace settings file {}", path.display()))?;
        Ok(())
    }
}

pub fn app_support_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
}

pub fn settings_path() -> PathBuf {
    app_support_dir().join("settings.json")
}

pub fn logs_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join("Library").join("Logs").join(APP_NAME))
        .unwrap_or_else(|| app_support_dir().join("logs"))
}

/// Normalizes custom vocabulary phrases: trims whitespace, drops empties, deduplicates case-insensitively.
pub fn normalize_custom_vocabulary(words: &[String], max_terms: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in words {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            out.push(trimmed.to_string());
            if out.len() >= max_terms {
                break;
            }
        }
    }
    out
}

/// Parses freeform custom vocabulary text (newline- or comma-separated) and normalizes it.
pub fn parse_custom_vocabulary_input(raw: &str, max_terms: usize) -> Vec<String> {
    let parts: Vec<String> = raw.split(['\n', '\r', ',']).map(str::to_string).collect();
    normalize_custom_vocabulary(&parts, max_terms)
}

/// Formats saved custom vocabulary terms for multi-line editing (one phrase per line).
pub fn format_custom_vocabulary_input(words: &[String]) -> String {
    words.join("\n")
}

/// Formats a `handy-keys` binding string for human display on macOS (e.g., `"Right ⌥ + Right ⌘"`).
pub fn format_shortcut_display(binding: &str) -> String {
    binding
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| match part.to_lowercase().as_str() {
            "option_right" | "opt_right" | "alt_right" | "ropt" | "ralt" => "Right ⌥".to_string(),
            "option_left" | "opt_left" | "alt_left" | "lopt" | "lalt" => "Left ⌥".to_string(),
            "option" | "opt" | "alt" => "⌥".to_string(),
            "command_right" | "cmd_right" | "super_right" | "rcmd" => "Right ⌘".to_string(),
            "command_left" | "cmd_left" | "super_left" | "lcmd" => "Left ⌘".to_string(),
            "command" | "cmd" | "super" => "⌘".to_string(),
            "shift_right" | "rshift" => "Right ⇧".to_string(),
            "shift_left" | "lshift" => "Left ⇧".to_string(),
            "shift" => "⇧".to_string(),
            "ctrl_right" | "control_right" | "rctrl" => "Right ⌃".to_string(),
            "ctrl_left" | "control_left" | "lctrl" => "Left ⌃".to_string(),
            "ctrl" | "control" => "⌃".to_string(),
            "fn" => "Fn".to_string(),
            "space" => "Space".to_string(),
            "escape" | "esc" => "Esc".to_string(),
            other => {
                let mut chars = other.chars();
                match chars.next() {
                    Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_right_option_command_and_smart_mode() {
        let s = Settings::default();
        assert_eq!(s.shortcut, "option_right+command_right");
        assert!(s.smart_mode);
        assert!(s.language_codes.is_empty());
        assert_eq!(s.shortcut_activation, ShortcutActivation::HoldOrToggle);
    }

    #[test]
    fn roundtrips_json_and_sets_0600_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let s = Settings {
            gemini_api_key: "test-secret".into(),
            shortcut: "option+space".into(),
            ..Default::default()
        };
        s.save_to(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.gemini_api_key, "test-secret");
        assert_eq!(loaded.shortcut, "option+space");
    }

    #[test]
    fn formats_macos_shortcut_display() {
        assert_eq!(
            format_shortcut_display("option_right+command_right"),
            "Right ⌥ + Right ⌘"
        );
        assert_eq!(format_shortcut_display("option+space"), "⌥ + Space");
    }

    #[test]
    fn parses_and_formats_custom_vocabulary_input() {
        let parsed = parse_custom_vocabulary_input(
            "  Vibo , Antigravity\r\nobjc2\n\n  vibo , Google Cloud , ",
            10,
        );
        assert_eq!(parsed, vec!["Vibo", "Antigravity", "objc2", "Google Cloud"]);
        assert_eq!(
            format_custom_vocabulary_input(&parsed),
            "Vibo\nAntigravity\nobjc2\nGoogle Cloud"
        );

        let capped = parse_custom_vocabulary_input("one, two, three", 2);
        assert_eq!(capped, vec!["one", "two"]);
    }
}
