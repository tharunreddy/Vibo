//! Global keyboard shortcuts via `handy-keys::KeyboardListener` (event-driven macOS
//! `CGEventTap` on a `CFRunLoop` with zero periodic polling wakeups, supporting side-specific
//! modifier-only combos like `"option_right+command_right"`, dynamic Escape cancel, and
//! interactive live capture).

use handy_keys::{BlockingHotkeys, Hotkey, KeyEvent, KeyboardListener, Modifiers};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    DictatePressed,
    DictateReleased,
    CancelPressed,
}

#[derive(Default)]
struct HotkeyState {
    dictate: Option<Hotkey>,
    cancel: Option<Hotkey>,
    dictate_pressed: bool,
    cancel_pressed: bool,
    suspended: bool,
}

impl HotkeyState {
    fn process_event(&mut self, event: &KeyEvent) -> Vec<HotkeyAction> {
        let mut actions = Vec::new();
        if self.suspended {
            return actions;
        }
        if event.is_key_down {
            if let Some(hk) = self.dictate
                && !self.dictate_pressed
                && hk.modifiers.matches(event.modifiers)
                && hk.key == event.key
            {
                self.dictate_pressed = true;
                actions.push(HotkeyAction::DictatePressed);
            }
            if let Some(hk) = self.cancel
                && !self.cancel_pressed
                && hk.modifiers.matches(event.modifiers)
                && hk.key == event.key
            {
                self.cancel_pressed = true;
                actions.push(HotkeyAction::CancelPressed);
            }
        } else {
            if self.dictate_pressed
                && let Some(hk) = self.dictate
                && is_hotkey_released(&hk, event)
            {
                self.dictate_pressed = false;
                actions.push(HotkeyAction::DictateReleased);
            }
            if self.cancel_pressed
                && let Some(hk) = self.cancel
                && is_hotkey_released(&hk, event)
            {
                self.cancel_pressed = false;
            }
        }
        actions
    }
}

fn is_hotkey_released(hotkey: &Hotkey, event: &KeyEvent) -> bool {
    (event.key.is_some() && hotkey.key == event.key)
        || (event.key.is_none() && !hotkey.modifiers.matches(event.modifiers))
}

fn sync_blocking_set(state: &HotkeyState, blocking: &BlockingHotkeys) {
    let mut set = blocking.lock().unwrap_or_else(|e| e.into_inner());
    set.clear();
    if !state.suspended {
        if let Some(hk) = state.dictate {
            set.insert(hk);
        }
        if let Some(hk) = state.cancel {
            set.insert(hk);
        }
    }
}

pub struct HotkeyService {
    state: Arc<Mutex<HotkeyState>>,
    blocking: BlockingHotkeys,
    shutdown: Arc<AtomicBool>,
}

impl HotkeyService {
    pub fn start(
        initial_binding: &str,
        on_action: Arc<dyn Fn(HotkeyAction) + Send + Sync + 'static>,
    ) -> Result<Self, String> {
        let initial_hotkey = match validate_shortcut(initial_binding) {
            Ok(hk) => Some(hk),
            Err(err) => {
                tracing::error!(binding = %initial_binding, error = %err, "Failed to parse initial hotkey");
                None
            }
        };

        let blocking: BlockingHotkeys = Arc::new(Mutex::new(HashSet::new()));
        let state = Arc::new(Mutex::new(HotkeyState {
            dictate: initial_hotkey,
            ..Default::default()
        }));
        {
            let st = state.lock().unwrap_or_else(|e| e.into_inner());
            sync_blocking_set(&st, &blocking);
        }

        let listener = KeyboardListener::new_with_blocking(Arc::clone(&blocking))
            .map_err(|e| format!("Failed to initialize global hotkey listener: {e}"))?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_shutdown = Arc::clone(&shutdown);
        let binding_log = initial_binding.to_string();

        thread::Builder::new()
            .name("vibo-hotkeys".into())
            .spawn(move || {
                tracing::info!(binding = %binding_log, "Event-driven hotkey listener thread started");
                while let Ok(event) = listener.recv() {
                    if thread_shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    let actions = {
                        let mut st = thread_state.lock().unwrap_or_else(|e| e.into_inner());
                        st.process_event(&event)
                    };
                    for action in actions {
                        on_action(action);
                    }
                }
                tracing::info!("Event-driven hotkey listener thread stopped");
            })
            .map_err(|e| format!("Failed to spawn hotkey listener thread: {e}"))?;

        Ok(Self {
            state,
            blocking,
            shutdown,
        })
    }

    pub fn set_dictate_binding(&self, binding: &str) -> Result<(), String> {
        let hk = validate_shortcut(binding)?;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.dictate = Some(hk);
        st.dictate_pressed = false;
        sync_blocking_set(&st, &self.blocking);
        tracing::info!(binding, "Updated dictate shortcut");
        Ok(())
    }

    pub fn set_cancel_enabled(&self, enabled: bool) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let next_cancel = if enabled {
            "escape".parse::<Hotkey>().ok()
        } else {
            None
        };
        if st.cancel == next_cancel {
            return;
        }
        st.cancel = next_cancel;
        st.cancel_pressed = false;
        sync_blocking_set(&st, &self.blocking);
    }

    pub fn set_suspended(&self, suspended: bool) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.suspended == suspended {
            return;
        }
        st.suspended = suspended;
        st.dictate_pressed = false;
        st.cancel_pressed = false;
        sync_blocking_set(&st, &self.blocking);
    }
}

impl Drop for HotkeyService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.suspended = true;
        st.dictate = None;
        st.cancel = None;
        sync_blocking_set(&st, &self.blocking);
    }
}

pub fn validate_shortcut(raw: &str) -> Result<Hotkey, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Shortcut cannot be empty".into());
    }
    trimmed
        .parse::<Hotkey>()
        .map_err(|e| format!("Invalid shortcut '{trimmed}': {e}"))
}

/// True if the shortcut uses only modifier keys (immune to macOS Secure Input).
pub fn is_modifier_only_shortcut(raw: &str) -> bool {
    raw.parse::<Hotkey>()
        .map(|h| h.key.is_none())
        .unwrap_or(false)
}

/// Captures a single shortcut press interactively (supports side-specific modifiers like
/// `option_right+command_right` as well as modifier+key combos). Blocks until a combo is
/// committed (all keys released after pressing) or cancelled (`Esc` alone) or `stop_rx` fires.
pub fn capture_shortcut_blocking(
    stop_rx: Receiver<()>,
    on_preview: impl Fn(&str),
) -> Result<Option<String>, String> {
    let listener = KeyboardListener::new()
        .map_err(|e| format!("Failed to start keyboard capture listener: {e}"))?;

    let mut peak_modifiers = Modifiers::empty();
    let mut peak_key: Option<handy_keys::Key> = None;
    let mut saw_press = false;

    loop {
        if stop_rx.try_recv().is_ok() {
            return Ok(None);
        }

        match listener.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => {
                if event.is_key_down {
                    // Pressing Escape alone cancels capture.
                    if event.key == Some(handy_keys::Key::Escape) && event.modifiers.is_empty() {
                        return Ok(None);
                    }
                    saw_press = true;
                    peak_modifiers |= event.modifiers;
                    if let Some(k) = event.key {
                        peak_key = Some(k);
                    }
                    let preview = format_captured_binding(peak_modifiers, peak_key);
                    if !preview.is_empty() {
                        on_preview(&preview);
                    }
                } else if saw_press && event.modifiers.is_empty() {
                    let captured = format_captured_binding(peak_modifiers, peak_key);
                    if !captured.is_empty() && validate_shortcut(&captured).is_ok() {
                        return Ok(Some(captured));
                    }
                    peak_modifiers = Modifiers::empty();
                    peak_key = None;
                    saw_press = false;
                }
            }
            Err(handy_keys::Error::Timeout) => {}
            Err(_) => return Ok(None),
        }
    }
}

fn format_captured_binding(modifiers: Modifiers, key: Option<handy_keys::Key>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if modifiers.contains(Modifiers::CTRL_LEFT) {
        parts.push("ctrl_left".into());
    } else if modifiers.contains(Modifiers::CTRL_RIGHT) {
        parts.push("ctrl_right".into());
    } else if modifiers.contains(Modifiers::CTRL) {
        parts.push("ctrl".into());
    }

    if modifiers.contains(Modifiers::OPT_LEFT) {
        parts.push("option_left".into());
    } else if modifiers.contains(Modifiers::OPT_RIGHT) {
        parts.push("option_right".into());
    } else if modifiers.contains(Modifiers::OPT) {
        parts.push("option".into());
    }

    if modifiers.contains(Modifiers::SHIFT_LEFT) {
        parts.push("shift_left".into());
    } else if modifiers.contains(Modifiers::SHIFT_RIGHT) {
        parts.push("shift_right".into());
    } else if modifiers.contains(Modifiers::SHIFT) {
        parts.push("shift".into());
    }

    if modifiers.contains(Modifiers::CMD_LEFT) {
        parts.push("command_left".into());
    } else if modifiers.contains(Modifiers::CMD_RIGHT) {
        parts.push("command_right".into());
    } else if modifiers.contains(Modifiers::CMD) {
        parts.push("command".into());
    }

    if modifiers.contains(Modifiers::FN) {
        parts.push("fn".into());
    }

    if let Some(k) = key {
        parts.push(k.to_string().to_lowercase());
    }

    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_right_option_command_modifier_only() {
        let hk = validate_shortcut("option_right+command_right").unwrap();
        assert!(hk.key.is_none());
        assert!(hk.modifiers.contains(Modifiers::OPT_RIGHT));
        assert!(hk.modifiers.contains(Modifiers::CMD_RIGHT));
        assert!(is_modifier_only_shortcut("option_right+command_right"));
    }

    #[test]
    fn parses_keyed_shortcut() {
        let hk = validate_shortcut("option+space").unwrap();
        assert!(hk.key.is_some());
        assert!(!is_modifier_only_shortcut("option+space"));
    }

    #[test]
    fn formats_side_specific_capture() {
        let s = format_captured_binding(Modifiers::OPT_RIGHT | Modifiers::CMD_RIGHT, None);
        assert_eq!(s, "option_right+command_right");
        assert!(validate_shortcut(&s).is_ok());
    }

    #[test]
    fn state_processes_modifier_only_and_cancel_events() {
        let mut st = HotkeyState {
            dictate: Some(validate_shortcut("option_right").unwrap()),
            cancel: Some(validate_shortcut("escape").unwrap()),
            ..Default::default()
        };

        let down = KeyEvent {
            modifiers: Modifiers::OPT_RIGHT,
            key: None,
            is_key_down: true,
            changed_modifier: Some(Modifiers::OPT_RIGHT),
        };
        assert_eq!(st.process_event(&down), vec![HotkeyAction::DictatePressed]);
        // Duplicate down while held does not re-trigger.
        assert!(st.process_event(&down).is_empty());

        let up = KeyEvent {
            modifiers: Modifiers::empty(),
            key: None,
            is_key_down: false,
            changed_modifier: Some(Modifiers::OPT_RIGHT),
        };
        assert_eq!(st.process_event(&up), vec![HotkeyAction::DictateReleased]);

        let esc = KeyEvent {
            modifiers: Modifiers::empty(),
            key: Some(handy_keys::Key::Escape),
            is_key_down: true,
            changed_modifier: None,
        };
        assert_eq!(st.process_event(&esc), vec![HotkeyAction::CancelPressed]);
    }
}
