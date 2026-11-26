//! Native macOS `NSStatusItem` menu bar + native `NSAlert` dialogs for API key,
//! interactive shortcut capture, and permissions.

use crate::audio::capture;
use crate::hotkeys;
use crate::macos::permissions;
use crate::settings::{self, OverlayPosition, Settings, ShortcutActivation, SoundTheme};
use crate::telemetry;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSImage, NSMenu, NSMenuItem, NSSecureTextField, NSStatusBar,
    NSStatusItem, NSTextAlignment, NSTextField, NSVariableStatusItemLength,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::sync::{Arc, mpsc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIconState {
    Idle,
    Listening,
    Polishing,
}

#[derive(Debug, Clone)]
pub enum TrayAction {
    Cancel,
    CopyLastTranscript,
    ToggleSmartMode,
    SelectMicrophone(Option<String>),
    SetShortcut(String),
    RecordShortcutInteractive,
    SetActivation(ShortcutActivation),
    SetOverlayPosition(OverlayPosition),
    ToggleSoundFeedback,
    SetSoundTheme(SoundTheme),
    ToggleLaunchAtLogin,
    ToggleTrailingSpace,
    OpenApiKeyDialog,
    OpenPermissionsDialog,
    OpenSettingsFile,
    OpenLogsFolder,
    Quit,
}

pub struct TraySnapshot {
    pub icon_state: TrayIconState,
    pub settings: Settings,
    pub has_last_transcript: bool,
    pub permissions_ok: bool,
    pub secure_input_warning: bool,
    pub api_key_missing: bool,
}

struct MenuTargetIvars {
    on_action: Arc<dyn Fn(TrayAction) + Send + Sync>,
    mic_names: RefCell<Vec<String>>,
}

const TAG_CANCEL: isize = 1;
const TAG_COPY_LAST: isize = 2;
const TAG_TOGGLE_SMART: isize = 3;
const TAG_MIC_DEFAULT: isize = 5;
const TAG_RECORD_SHORTCUT: isize = 6;
const TAG_PRESET_R_OPT_CMD: isize = 7;
const TAG_PRESET_OPT_SPACE: isize = 8;
const TAG_PRESET_R_OPT: isize = 9;
const TAG_PRESET_R_CMD: isize = 10;
const TAG_ACT_HOLD_OR_TOGGLE: isize = 11;
const TAG_ACT_PTT: isize = 12;
const TAG_ACT_TOGGLE: isize = 13;
const TAG_POS_BOTTOM: isize = 14;
const TAG_POS_TOP: isize = 15;
const TAG_TOGGLE_SOUND: isize = 16;
const TAG_THEME_POP: isize = 17;
const TAG_THEME_MARIMBA: isize = 18;
const TAG_TOGGLE_LOGIN: isize = 19;
const TAG_TOGGLE_TRAILING_SPACE: isize = 25;
const TAG_API_KEY: isize = 20;
const TAG_PERMISSIONS: isize = 21;
const TAG_OPEN_SETTINGS: isize = 22;
const TAG_OPEN_LOGS: isize = 23;
const TAG_QUIT: isize = 24;
const TAG_MIC_BASE: isize = 1000;

define_class!(
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LiviMenuTarget"]
    #[ivars = MenuTargetIvars]
    struct LiviMenuTarget;

    impl LiviMenuTarget {
        #[unsafe(method(onMenuItem:))]
        fn on_menu_item(&self, sender: *mut AnyObject) {
            if sender.is_null() {
                return;
            }
            let item: &NSMenuItem = unsafe { &*(sender as *const NSMenuItem) };
            let tag = item.tag();
            let action = match tag {
                TAG_CANCEL => Some(TrayAction::Cancel),
                TAG_COPY_LAST => Some(TrayAction::CopyLastTranscript),
                TAG_TOGGLE_SMART => Some(TrayAction::ToggleSmartMode),
                TAG_MIC_DEFAULT => Some(TrayAction::SelectMicrophone(None)),
                TAG_RECORD_SHORTCUT => Some(TrayAction::RecordShortcutInteractive),
                TAG_PRESET_R_OPT_CMD => Some(TrayAction::SetShortcut("option_right+command_right".into())),
                TAG_PRESET_OPT_SPACE => Some(TrayAction::SetShortcut("option+space".into())),
                TAG_PRESET_R_OPT => Some(TrayAction::SetShortcut("option_right".into())),
                TAG_PRESET_R_CMD => Some(TrayAction::SetShortcut("command_right".into())),
                TAG_ACT_HOLD_OR_TOGGLE => Some(TrayAction::SetActivation(ShortcutActivation::HoldOrToggle)),
                TAG_ACT_PTT => Some(TrayAction::SetActivation(ShortcutActivation::PushToTalk)),
                TAG_ACT_TOGGLE => Some(TrayAction::SetActivation(ShortcutActivation::Toggle)),
                TAG_POS_BOTTOM => Some(TrayAction::SetOverlayPosition(OverlayPosition::Bottom)),
                TAG_POS_TOP => Some(TrayAction::SetOverlayPosition(OverlayPosition::Top)),
                TAG_TOGGLE_SOUND => Some(TrayAction::ToggleSoundFeedback),
                TAG_THEME_POP => Some(TrayAction::SetSoundTheme(SoundTheme::Pop)),
                TAG_THEME_MARIMBA => Some(TrayAction::SetSoundTheme(SoundTheme::Marimba)),
                TAG_TOGGLE_LOGIN => Some(TrayAction::ToggleLaunchAtLogin),
                TAG_TOGGLE_TRAILING_SPACE => Some(TrayAction::ToggleTrailingSpace),
                TAG_API_KEY => Some(TrayAction::OpenApiKeyDialog),
                TAG_PERMISSIONS => Some(TrayAction::OpenPermissionsDialog),
                TAG_OPEN_SETTINGS => Some(TrayAction::OpenSettingsFile),
                TAG_OPEN_LOGS => Some(TrayAction::OpenLogsFolder),
                TAG_QUIT => Some(TrayAction::Quit),
                t if t >= TAG_MIC_BASE => {
                    let idx = (t - TAG_MIC_BASE) as usize;
                    self.ivars()
                        .mic_names
                        .borrow()
                        .get(idx)
                        .cloned()
                        .map(|name| TrayAction::SelectMicrophone(Some(name)))
                }
                _ => None,
            };
            if let Some(action) = action {
                (self.ivars().on_action)(action);
            }
        }
    }
);

impl LiviMenuTarget {
    fn new(
        mtm: MainThreadMarker,
        on_action: Arc<dyn Fn(TrayAction) + Send + Sync>,
    ) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(MenuTargetIvars {
            on_action,
            mic_names: RefCell::new(Vec::new()),
        });
        unsafe { msg_send![super(this), init] }
    }
}

struct TrayInner {
    status_item: Retained<NSStatusItem>,
    target: Retained<LiviMenuTarget>,
}

thread_local! {
    static TRAY: RefCell<Option<TrayInner>> = const { RefCell::new(None) };
}

fn make_item(
    mtm: MainThreadMarker,
    title: &str,
    key_equiv: &str,
    tag: isize,
    target: &LiviMenuTarget,
    enabled: bool,
    checked: bool,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str(title),
            if enabled {
                Some(sel!(onMenuItem:))
            } else {
                None
            },
            &NSString::from_str(key_equiv),
        )
    };
    item.setTag(tag);
    item.setEnabled(enabled);
    if enabled {
        unsafe { item.setTarget(Some(target)) };
    }
    item.setState(if checked {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    item
}

fn make_header(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str(title),
            None,
            &NSString::from_str(""),
        )
    };
    item.setEnabled(false);
    item
}

fn set_status_icon(status_item: &NSStatusItem, state: TrayIconState, warning: bool) {
    let Some(button) = status_item.button(MainThreadMarker::new().unwrap()) else {
        return;
    };
    let symbol = match (state, warning) {
        (TrayIconState::Listening, _) => "mic.fill",
        (TrayIconState::Polishing, _) => "sparkles",
        (TrayIconState::Idle, true) => "exclamationmark.triangle",
        (TrayIconState::Idle, false) => "waveform",
    };
    if let Some(img) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(symbol),
        Some(&NSString::from_str("LIVI")),
    ) {
        img.setTemplate(true);
        button.setImage(Some(&img));
        button.setTitle(&NSString::from_str(""));
    } else {
        button.setTitle(&NSString::from_str("LIVI"));
    }
}

pub fn init_on_main(mtm: MainThreadMarker, on_action: Arc<dyn Fn(TrayAction) + Send + Sync>) {
    TRAY.with(|slot| {
        if slot.borrow().is_some() {
            return;
        }
        let bar = NSStatusBar::systemStatusBar();
        let status_item = bar.statusItemWithLength(NSVariableStatusItemLength);
        let target = LiviMenuTarget::new(mtm, on_action);
        set_status_icon(&status_item, TrayIconState::Idle, false);
        *slot.borrow_mut() = Some(TrayInner {
            status_item,
            target,
        });
    });
}

pub fn update(snapshot: TraySnapshot) {
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        TRAY.with(|slot| {
            let borrow = slot.borrow();
            let Some(inner) = borrow.as_ref() else { return };
            rebuild_menu(mtm, inner, &snapshot);
        });
    });
}

fn rebuild_menu(mtm: MainThreadMarker, inner: &TrayInner, snap: &TraySnapshot) {
    let warning = !snap.permissions_ok || snap.secure_input_warning || snap.api_key_missing;
    set_status_icon(&inner.status_item, snap.icon_state, warning);

    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);

    let t = &inner.target;
    let s = &snap.settings;

    menu.addItem(&make_header(
        mtm,
        &format!("LIVI v{} — Gemini 3.5 Live", env!("CARGO_PKG_VERSION")),
    ));
    if let Some(summary) = telemetry::last_latency_summary() {
        menu.addItem(&make_header(mtm, &summary.menu_label()));
    }

    if snap.api_key_missing {
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&make_item(
            mtm,
            "⚠ Set Gemini API Key…",
            "",
            TAG_API_KEY,
            t,
            true,
            false,
        ));
    }
    if !snap.permissions_ok {
        menu.addItem(&make_item(
            mtm,
            "⚠ Permissions Required…",
            "",
            TAG_PERMISSIONS,
            t,
            true,
            false,
        ));
    }
    if snap.secure_input_warning {
        menu.addItem(&make_header(
            mtm,
            "⚠ Secure Input active (keyed shortcuts paused)",
        ));
    }

    menu.addItem(&NSMenuItem::separatorItem(mtm));

    if snap.icon_state != TrayIconState::Idle {
        menu.addItem(&make_item(
            mtm,
            "Cancel Dictation (Esc)",
            "",
            TAG_CANCEL,
            t,
            true,
            false,
        ));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
    }

    menu.addItem(&make_item(
        mtm,
        "Copy Last Transcript",
        "",
        TAG_COPY_LAST,
        t,
        snap.has_last_transcript,
        false,
    ));

    menu.addItem(&NSMenuItem::separatorItem(mtm));

    // Mode toggle
    menu.addItem(&make_item(
        mtm,
        "Smart Mode (Gemini 3.5)",
        "",
        TAG_TOGGLE_SMART,
        t,
        true,
        s.smart_mode,
    ));

    // Microphone submenu
    let mic_menu = NSMenu::new(mtm);
    mic_menu.setAutoenablesItems(false);
    mic_menu.addItem(&make_item(
        mtm,
        "System Default",
        "",
        TAG_MIC_DEFAULT,
        t,
        true,
        s.microphone.is_none(),
    ));
    let devices = if snap.icon_state == TrayIconState::Idle {
        capture::list_input_devices()
    } else {
        t.ivars().mic_names.borrow().clone()
    };
    *t.ivars().mic_names.borrow_mut() = devices.clone();
    if !devices.is_empty() {
        mic_menu.addItem(&NSMenuItem::separatorItem(mtm));
        for (idx, name) in devices.iter().enumerate() {
            let checked = s.microphone.as_deref() == Some(name.as_str());
            mic_menu.addItem(&make_item(
                mtm,
                name,
                "",
                TAG_MIC_BASE + idx as isize,
                t,
                true,
                checked,
            ));
        }
    }
    let mic_root = make_header(
        mtm,
        &format!(
            "Microphone ({})",
            s.microphone.as_deref().unwrap_or("Default")
        ),
    );
    mic_root.setEnabled(true);
    mic_root.setSubmenu(Some(&mic_menu));
    menu.addItem(&mic_root);

    // Shortcut submenu
    let sc_menu = NSMenu::new(mtm);
    sc_menu.setAutoenablesItems(false);
    sc_menu.addItem(&make_item(
        mtm,
        "Record New Shortcut…",
        "",
        TAG_RECORD_SHORTCUT,
        t,
        true,
        false,
    ));
    sc_menu.addItem(&NSMenuItem::separatorItem(mtm));
    sc_menu.addItem(&make_header(mtm, "Presets"));
    sc_menu.addItem(&make_item(
        mtm,
        "Right ⌥ + Right ⌘ (Default)",
        "",
        TAG_PRESET_R_OPT_CMD,
        t,
        true,
        s.shortcut
            .eq_ignore_ascii_case("option_right+command_right"),
    ));
    sc_menu.addItem(&make_item(
        mtm,
        "⌥ + Space",
        "",
        TAG_PRESET_OPT_SPACE,
        t,
        true,
        s.shortcut.eq_ignore_ascii_case("option+space"),
    ));
    sc_menu.addItem(&make_item(
        mtm,
        "Right ⌥",
        "",
        TAG_PRESET_R_OPT,
        t,
        true,
        s.shortcut.eq_ignore_ascii_case("option_right"),
    ));
    sc_menu.addItem(&make_item(
        mtm,
        "Right ⌘",
        "",
        TAG_PRESET_R_CMD,
        t,
        true,
        s.shortcut.eq_ignore_ascii_case("command_right"),
    ));
    sc_menu.addItem(&NSMenuItem::separatorItem(mtm));
    sc_menu.addItem(&make_header(mtm, "Activation"));
    sc_menu.addItem(&make_item(
        mtm,
        ShortcutActivation::HoldOrToggle.label(),
        "",
        TAG_ACT_HOLD_OR_TOGGLE,
        t,
        true,
        s.shortcut_activation == ShortcutActivation::HoldOrToggle,
    ));
    sc_menu.addItem(&make_item(
        mtm,
        ShortcutActivation::PushToTalk.label(),
        "",
        TAG_ACT_PTT,
        t,
        true,
        s.shortcut_activation == ShortcutActivation::PushToTalk,
    ));
    sc_menu.addItem(&make_item(
        mtm,
        ShortcutActivation::Toggle.label(),
        "",
        TAG_ACT_TOGGLE,
        t,
        true,
        s.shortcut_activation == ShortcutActivation::Toggle,
    ));
    let sc_root = make_header(
        mtm,
        &format!(
            "Shortcut ({})",
            settings::format_shortcut_display(&s.shortcut)
        ),
    );
    sc_root.setEnabled(true);
    sc_root.setSubmenu(Some(&sc_menu));
    menu.addItem(&sc_root);

    // Overlay & Sound submenu
    let prefs_menu = NSMenu::new(mtm);
    prefs_menu.setAutoenablesItems(false);
    prefs_menu.addItem(&make_header(mtm, "Overlay Position"));
    prefs_menu.addItem(&make_item(
        mtm,
        "Bottom",
        "",
        TAG_POS_BOTTOM,
        t,
        true,
        s.overlay_position == OverlayPosition::Bottom,
    ));
    prefs_menu.addItem(&make_item(
        mtm,
        "Top",
        "",
        TAG_POS_TOP,
        t,
        true,
        s.overlay_position == OverlayPosition::Top,
    ));
    prefs_menu.addItem(&NSMenuItem::separatorItem(mtm));
    prefs_menu.addItem(&make_item(
        mtm,
        "Sound Chimes",
        "",
        TAG_TOGGLE_SOUND,
        t,
        true,
        s.sound_feedback,
    ));
    prefs_menu.addItem(&make_item(
        mtm,
        "Chime: Pop",
        "",
        TAG_THEME_POP,
        t,
        s.sound_feedback,
        s.sound_theme == SoundTheme::Pop,
    ));
    prefs_menu.addItem(&make_item(
        mtm,
        "Chime: Marimba",
        "",
        TAG_THEME_MARIMBA,
        t,
        s.sound_feedback,
        s.sound_theme == SoundTheme::Marimba,
    ));
    prefs_menu.addItem(&NSMenuItem::separatorItem(mtm));
    prefs_menu.addItem(&make_item(
        mtm,
        "Add Space After Transcription",
        "",
        TAG_TOGGLE_TRAILING_SPACE,
        t,
        true,
        s.append_trailing_space,
    ));
    prefs_menu.addItem(&make_item(
        mtm,
        "Launch at Login",
        "",
        TAG_TOGGLE_LOGIN,
        t,
        true,
        s.launch_at_login,
    ));
    let prefs_root = make_header(mtm, "Preferences");
    prefs_root.setEnabled(true);
    prefs_root.setSubmenu(Some(&prefs_menu));
    menu.addItem(&prefs_root);

    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&make_item(
        mtm,
        "Set Gemini API Key…",
        "",
        TAG_API_KEY,
        t,
        true,
        false,
    ));
    menu.addItem(&make_item(
        mtm,
        "Permissions…",
        "",
        TAG_PERMISSIONS,
        t,
        true,
        false,
    ));
    menu.addItem(&make_item(
        mtm,
        "Open settings.json",
        "",
        TAG_OPEN_SETTINGS,
        t,
        true,
        false,
    ));
    menu.addItem(&make_item(
        mtm,
        "Open Logs Folder",
        "",
        TAG_OPEN_LOGS,
        t,
        true,
        false,
    ));

    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&make_item(mtm, "Quit LIVI", "q", TAG_QUIT, t, true, false));

    inner.status_item.setMenu(Some(&menu));
}

/// Shows a native modal alert to enter/update the Gemini API key.
pub fn prompt_api_key_on_main(current_key: String, on_save: impl FnOnce(String) + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);

        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Gemini API Key"));
        alert.setInformativeText(&NSString::from_str(
            "Enter your Google Gemini API key for Gemini 3.5 Transcribe Live.\nStored locally in ~/Library/Application Support/LIVI/settings.json.",
        ));
        alert.addButtonWithTitle(&NSString::from_str("Save & Test"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));

        let field = NSSecureTextField::initWithFrame(
            mtm.alloc(),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(320.0, 24.0)),
        );
        field.setStringValue(&NSString::from_str(&current_key));
        alert.setAccessoryView(Some(&field));
        alert.layout();
        let _ = alert.window().makeFirstResponder(Some(&field));

        let response = alert.runModal();
        if response == NSAlertFirstButtonReturn {
            let new_key = field.stringValue().to_string().trim().to_string();
            on_save(new_key);
        }
    });
}

/// Shows a native modal alert explaining Microphone & Accessibility permissions with one-click setup.
pub fn prompt_permissions_on_main(on_refresh: impl Fn() + Send + Sync + 'static) {
    let on_refresh = Arc::new(on_refresh);
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);

        let snap = permissions::snapshot();
        let mic_status = if snap.microphone {
            "✓ Granted"
        } else {
            "✗ Missing"
        };
        let acc_status = if snap.accessibility {
            "✓ Granted"
        } else {
            "✗ Missing"
        };

        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("LIVI Permissions"));
        alert.setInformativeText(&NSString::from_str(&format!(
            "Microphone: {mic_status}\nAccessibility: {acc_status}\n\nMicrophone is needed to record speech; Accessibility is needed for global shortcuts and pasting at your cursor."
        )));
        alert.addButtonWithTitle(&NSString::from_str("Grant Missing Permissions"));
        alert.addButtonWithTitle(&NSString::from_str("Close"));

        let response = alert.runModal();
        if response == NSAlertFirstButtonReturn {
            if !snap.microphone {
                let cb = Arc::clone(&on_refresh);
                permissions::request_microphone(move |_| cb());
            }
            if !snap.accessibility {
                let _ = permissions::prompt_accessibility();
                permissions::open_accessibility_settings();
            }
            on_refresh();
        }
    });
}

/// Shows a native modal dialog that listens via `handy-keys::KeyboardListener` to capture a
/// new shortcut live (including side-specific modifier combos like Right ⌥ + Right ⌘).
pub fn prompt_record_shortcut_on_main(
    current: String,
    on_captured: impl FnOnce(Option<String>) + Send + 'static,
) {
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);

        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Record New Shortcut"));
        alert.setInformativeText(&NSString::from_str(&format!(
            "Press and release your desired key or modifier combination (e.g. Right ⌥ + Right ⌘).\nCurrent: {}\nPress Esc to cancel.",
            settings::format_shortcut_display(&current)
        )));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));

        let preview_label = NSTextField::initWithFrame(
            mtm.alloc(),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(280.0, 28.0)),
        );
        preview_label.setBezeled(true);
        preview_label.setEditable(false);
        preview_label.setSelectable(false);
        preview_label.setAlignment(NSTextAlignment::Center);
        preview_label.setFont(Some(&NSFont::boldSystemFontOfSize(14.0)));
        preview_label.setStringValue(&NSString::from_str("Listening for keys…"));
        alert.setAccessoryView(Some(&preview_label));

        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (result_tx, result_rx) = mpsc::channel::<Option<String>>();

        // Capture in background thread; stop the modal automatically when a combo commits.
        std::thread::spawn(move || {
            let captured = hotkeys::capture_shortcut_blocking(stop_rx, |preview| {
                let formatted = settings::format_shortcut_display(preview);
                tracing::debug!(preview = %formatted, "Shortcut capture preview");
            })
            .ok()
            .flatten();
            let _ = result_tx.send(captured);
            DispatchQueue::main().exec_async(|| {
                if let Some(mtm) = MainThreadMarker::new() {
                    NSApplication::sharedApplication(mtm).abortModal();
                }
            });
        });

        let _ = alert.runModal();
        let _ = stop_tx.send(());
        let captured = result_rx.try_recv().ok().flatten();
        on_captured(captured);
    });
}
