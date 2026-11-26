//! Vibo — Minimal pure-Rust native macOS dictation app powered by Gemini 3.5 Transcribe Live.

mod asr;
mod audio;
mod controller;
mod hotkeys;
mod macos;
mod paste;
mod secure_input;
mod settings;
mod telemetry;

use controller::AppController;
use macos::{overlay, tray};
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEventModifierFlags, NSMenu, NSMenuItem,
    NSRunningApplication,
};
use objc2_foundation::{NSBundle, NSString};
use settings::Settings;
use std::sync::Arc;

/// Installs a standard Edit menu on the hidden Accessory `NSApplication` mainMenu so
/// standard macOS shortcuts (`Cmd+V`, `Cmd+C`, `Cmd+X`, `Cmd+A`, `Cmd+Z`) route cleanly
/// to `NSTextField` / `NSSecureTextField` inside native modal alerts.
fn install_standard_edit_menu(mtm: MainThreadMarker, app: &NSApplication) {
    let main_menu = NSMenu::new(mtm);
    let edit_root = NSMenuItem::new(mtm);
    let edit_menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Edit"));

    let entries = [
        ("Undo", sel!(undo:), "z", false),
        ("Redo", sel!(redo:), "z", true),
        ("Cut", sel!(cut:), "x", false),
        ("Copy", sel!(copy:), "c", false),
        ("Paste", sel!(paste:), "v", false),
        ("Select All", sel!(selectAll:), "a", false),
    ];

    for (title, action, key, shift) in entries {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        if shift {
            item.setKeyEquivalentModifierMask(
                NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
            );
        }
        edit_menu.addItem(&item);
    }

    edit_root.setSubmenu(Some(&edit_menu));
    main_menu.addItem(&edit_root);
    app.setMainMenu(Some(&main_menu));
}

fn is_another_instance_running() -> bool {
    let Some(bundle_id) = NSBundle::mainBundle().bundleIdentifier() else {
        return false;
    };
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle_id);
    apps.count() > 1
}

fn main() {
    let settings = Settings::load();
    let _log_guard = telemetry::init_logging(&settings.log_level);

    if is_another_instance_running() {
        tracing::info!("Another Vibo instance is already running; exiting");
        return;
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        shortcut = %settings.shortcut,
        smart_mode = settings.smart_mode,
        "Starting Vibo"
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("vibo-tokio")
        .build()
        .expect("Failed to create Tokio runtime");

    let mtm = MainThreadMarker::new().expect("Vibo must start on the macOS main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    install_standard_edit_menu(mtm, &app);

    let ctrl = AppController::new(rt.handle().clone(), settings);

    let ctrl_cancel = Arc::clone(&ctrl);
    overlay::init_on_main(mtm, Arc::new(move || ctrl_cancel.cancel_current()));

    let ctrl_tray = Arc::clone(&ctrl);
    tray::init_on_main(
        mtm,
        Arc::new(move |action| ctrl_tray.handle_tray_action(action)),
    );

    ctrl.bootstrap();

    app.run();
}
