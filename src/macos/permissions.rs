//! Native macOS Microphone (`AVCaptureDevice`) and Accessibility (`AXIsProcessTrusted`)
//! permission checks, requests, and System Settings deep links.

use block2::RcBlock;
use objc2_app_kit::NSWorkspace;
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_foundation::{NSString, NSURL};
use std::ffi::c_void;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionSnapshot {
    pub microphone: bool,
    pub accessibility: bool,
}

impl PermissionSnapshot {
    pub fn all_granted(self) -> bool {
        self.microphone && self.accessibility
    }
}

pub fn check_accessibility() -> bool {
    unsafe { AXIsProcessTrusted() }
}

pub fn prompt_accessibility() -> bool {
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let dict = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const c_void,
        );
        let trusted = AXIsProcessTrustedWithOptions(dict);
        if !dict.is_null() {
            CFRelease(dict);
        }
        trusted
    }
}

pub fn check_microphone() -> bool {
    let status = unsafe {
        let Some(media_type) = AVMediaTypeAudio else {
            return false;
        };
        AVCaptureDevice::authorizationStatusForMediaType(media_type)
    };
    status == AVAuthorizationStatus::Authorized
}

pub fn request_microphone(on_done: impl Fn(bool) + 'static) {
    unsafe {
        let Some(media_type) = AVMediaTypeAudio else {
            on_done(false);
            return;
        };
        let status = AVCaptureDevice::authorizationStatusForMediaType(media_type);
        match status {
            AVAuthorizationStatus::Authorized => on_done(true),
            AVAuthorizationStatus::NotDetermined => {
                let block = RcBlock::new(move |granted: objc2::runtime::Bool| {
                    on_done(granted.as_bool());
                });
                AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &block);
            }
            _ => {
                open_microphone_settings();
                on_done(false);
            }
        }
    }
}

pub fn snapshot() -> PermissionSnapshot {
    PermissionSnapshot {
        microphone: check_microphone(),
        accessibility: check_accessibility(),
    }
}

pub fn open_url(url: &str) {
    let ns_str = NSString::from_str(url);
    if let Some(ns_url) = NSURL::URLWithString(&ns_str) {
        let workspace = NSWorkspace::sharedWorkspace();
        let _ = workspace.openURL(&ns_url);
    }
}

pub fn open_microphone_settings() {
    open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone");
}

pub fn open_accessibility_settings() {
    open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility");
}
