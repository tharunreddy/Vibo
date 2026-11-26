//! Global keyboard shortcuts via `handy-keys` (supports side-specific modifier-only combos
//! like `"option_right+command_right"`, dynamic Escape cancel, and interactive live capture).

use handy_keys::{Hotkey, HotkeyId, HotkeyManager, HotkeyState, KeyboardListener, Modifiers};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    DictatePressed,
    DictateReleased,
    CancelPressed,
}

enum ManagerCommand {
    SetDictate {
        binding: String,
        reply: Sender<Result<(), String>>,
    },
    SetCancelEnabled {
        enabled: bool,
    },
    SetSuspended {
        suspended: bool,
    },
    Shutdown,
}

pub struct HotkeyService {
    cmd_tx: Mutex<Sender<ManagerCommand>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl HotkeyService {
    pub fn start(
        initial_binding: &str,
        on_action: Arc<dyn Fn(HotkeyAction) + Send + Sync + 'static>,
    ) -> Result<Self, String> {
        let manager = HotkeyManager::new_with_blocking()
            .map_err(|e| format!("Failed to initialize global hotkey listener: {e}"))?;
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let binding = initial_binding.to_string();

        let thread = thread::spawn(move || {
            run_manager_thread(manager, cmd_rx, &binding, on_action);
        });

        Ok(Self {
            cmd_tx: Mutex::new(cmd_tx),
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn set_dictate_binding(&self, binding: &str) -> Result<(), String> {
        validate_shortcut(binding)?;
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(ManagerCommand::SetDictate {
                binding: binding.to_string(),
                reply: tx,
            })
            .map_err(|_| "Hotkey thread stopped".to_string())?;
        rx.recv().map_err(|_| "Hotkey thread stopped".to_string())?
    }

    pub fn set_cancel_enabled(&self, enabled: bool) {
        let _ = self
            .cmd_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(ManagerCommand::SetCancelEnabled { enabled });
    }

    pub fn set_suspended(&self, suspended: bool) {
        let _ = self
            .cmd_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(ManagerCommand::SetSuspended { suspended });
    }
}

impl Drop for HotkeyService {
    fn drop(&mut self) {
        let _ = self
            .cmd_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(ManagerCommand::Shutdown);
        if let Some(handle) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
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

fn run_manager_thread(
    manager: HotkeyManager,
    cmd_rx: Receiver<ManagerCommand>,
    initial_binding: &str,
    on_action: Arc<dyn Fn(HotkeyAction) + Send + Sync + 'static>,
) {
    tracing::info!(binding = initial_binding, "Hotkey manager thread started");

    let mut dictate_id: Option<HotkeyId> = None;
    let mut cancel_id: Option<HotkeyId> = None;
    let mut current_binding = initial_binding.to_string();
    let mut suspended = false;
    let mut id_roles: HashMap<HotkeyId, bool> = HashMap::new(); // true = dictate, false = cancel

    if let Ok(hk) = validate_shortcut(&current_binding) {
        match manager.register(hk) {
            Ok(id) => {
                dictate_id = Some(id);
                id_roles.insert(id, true);
            }
            Err(err) => {
                tracing::error!(binding = %current_binding, error = %err, "Failed to register initial hotkey");
            }
        }
    }

    loop {
        while let Some(event) = manager.try_recv() {
            if suspended {
                continue;
            }
            if let Some(&is_dictate) = id_roles.get(&event.id) {
                if is_dictate {
                    match event.state {
                        HotkeyState::Pressed => on_action(HotkeyAction::DictatePressed),
                        HotkeyState::Released => on_action(HotkeyAction::DictateReleased),
                    }
                } else if event.state == HotkeyState::Pressed {
                    on_action(HotkeyAction::CancelPressed);
                }
            }
        }

        match cmd_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(ManagerCommand::SetDictate { binding, reply }) => {
                let result = (|| -> Result<(), String> {
                    let hk = validate_shortcut(&binding)?;
                    if let Some(old) = dictate_id.take() {
                        id_roles.remove(&old);
                        let _ = manager.unregister(old);
                    }
                    if !suspended {
                        let id = manager
                            .register(hk)
                            .map_err(|e| format!("Failed to register '{binding}': {e}"))?;
                        dictate_id = Some(id);
                        id_roles.insert(id, true);
                    }
                    current_binding = binding.clone();
                    tracing::info!(binding = %current_binding, "Updated dictate shortcut");
                    Ok(())
                })();
                let _ = reply.send(result);
            }
            Ok(ManagerCommand::SetCancelEnabled { enabled }) => {
                if enabled && cancel_id.is_none() && !suspended {
                    if let Ok(esc) = "escape".parse::<Hotkey>()
                        && let Ok(id) = manager.register(esc)
                    {
                        cancel_id = Some(id);
                        id_roles.insert(id, false);
                    }
                } else if !enabled && let Some(old) = cancel_id.take() {
                    id_roles.remove(&old);
                    let _ = manager.unregister(old);
                }
            }
            Ok(ManagerCommand::SetSuspended { suspended: s }) => {
                if s == suspended {
                    continue;
                }
                suspended = s;
                if suspended {
                    if let Some(old) = dictate_id.take() {
                        id_roles.remove(&old);
                        let _ = manager.unregister(old);
                    }
                    if let Some(old) = cancel_id.take() {
                        id_roles.remove(&old);
                        let _ = manager.unregister(old);
                    }
                } else if let Ok(hk) = validate_shortcut(&current_binding)
                    && let Ok(id) = manager.register(hk)
                {
                    dictate_id = Some(id);
                    id_roles.insert(id, true);
                }
            }
            Ok(ManagerCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    tracing::info!("Hotkey manager thread stopped");
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

        if let Some(event) = listener.try_recv() {
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
        } else {
            thread::sleep(Duration::from_millis(12));
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
}
