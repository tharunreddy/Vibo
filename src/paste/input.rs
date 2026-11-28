//! Direct macOS `CGEvent` keyboard chord injection with Carbon TIS layout-aware `Cmd+V`
//! resolution (zero `enigo` dependency).

use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
};
use objc2_foundation::NSString;
use std::cell::RefCell;
use std::ffi::c_void;

// kVK_ANSI_V fallback if Carbon TIS cannot expose the active layout.
const ANSI_V_KEYCODE: u16 = 9;
// kVK_Command (Left Command)
const COMMAND_KEYCODE: u16 = 55;
const KEYCODE_COUNT: u16 = 128;
const UC_KEY_ACTION_DISPLAY: u16 = 3;
const UC_KEY_TRANSLATE_NO_DEAD_KEYS_MASK: u32 = 1;
const COMMAND_MODIFIER_STATE: u32 = 1;

type TisInputSourceRef = *const c_void;
type CfDataRef = *const c_void;
type CfStringRef = *const c_void;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> TisInputSourceRef;
    fn TISGetInputSourceProperty(
        input_source: TisInputSourceRef,
        property_key: CfStringRef,
    ) -> CfDataRef;
    static kTISPropertyUnicodeKeyLayoutData: CfStringRef;
    static kTISPropertyInputSourceID: CfStringRef;
    fn UCKeyTranslate(
        key_layout: *const u8,
        virtual_key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        key_translate_options: u32,
        dead_key_state: *mut u32,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut u16,
    ) -> i32;
    fn LMGetKbdType() -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataGetBytePtr(data: CfDataRef) -> *const u8;
    fn CFRelease(value: *const c_void);
}

struct InputSource(TisInputSourceRef);

impl Drop for InputSource {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

thread_local! {
    static CACHED_CMD_V: RefCell<Option<(String, u16)>> = const { RefCell::new(None) };
}

/// Resolves the physical virtual keycode that macOS interprets as `v` while Command is held,
/// caching the lookup by active Carbon TIS keyboard layout ID.
/// Must be called on the macOS main thread.
fn resolve_command_v_keycode() -> u16 {
    let source = InputSource(unsafe { TISCopyCurrentKeyboardLayoutInputSource() });
    if source.0.is_null() {
        return ANSI_V_KEYCODE;
    }
    let source_id = unsafe {
        let id_ref = TISGetInputSourceProperty(source.0, kTISPropertyInputSourceID);
        if id_ref.is_null() {
            None
        } else {
            Some((*(id_ref as *const NSString)).to_string())
        }
    };
    if let Some(ref id) = source_id
        && let Some(cached) = CACHED_CMD_V.with(|c| {
            c.borrow()
                .as_ref()
                .and_then(|(k, v)| (k == id).then_some(*v))
        })
    {
        return cached;
    }
    let layout_data =
        unsafe { TISGetInputSourceProperty(source.0, kTISPropertyUnicodeKeyLayoutData) };
    if layout_data.is_null() {
        return ANSI_V_KEYCODE;
    }
    let layout = unsafe { CFDataGetBytePtr(layout_data) };
    if layout.is_null() {
        return ANSI_V_KEYCODE;
    }
    let keyboard_type = unsafe { LMGetKbdType() } as u32;
    let mut resolved = ANSI_V_KEYCODE;
    for keycode in 0..KEYCODE_COUNT {
        let mut dead_key_state = 0;
        let mut chars = [0_u16; 4];
        let mut length = 0_usize;
        let status = unsafe {
            UCKeyTranslate(
                layout,
                keycode,
                UC_KEY_ACTION_DISPLAY,
                COMMAND_MODIFIER_STATE,
                keyboard_type,
                UC_KEY_TRANSLATE_NO_DEAD_KEYS_MASK,
                &mut dead_key_state,
                chars.len(),
                &mut length,
                chars.as_mut_ptr(),
            )
        };
        if status == 0 && length == 1 && chars[0] == u16::from(b'v') {
            resolved = keycode;
            break;
        }
    }
    if let Some(id) = source_id {
        CACHED_CMD_V.with(|c| {
            *c.borrow_mut() = Some((id, resolved));
        });
    }
    resolved
}

/// Token representing a currently held Command modifier to be released after `CHORD_HOLD_MS`.
#[derive(Debug, Clone, Copy)]
pub struct HeldPasteChord;

/// Presses `Cmd` and taps layout-aware `V`, returning a [`HeldPasteChord`] token so the
/// caller can release `Cmd` after a short hold without blocking the macOS main run loop.
/// Must be called on the macOS main thread.
pub fn press_paste_chord() -> Result<HeldPasteChord, String> {
    let v_keycode = resolve_command_v_keycode();
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok_or_else(|| {
        "Failed to create CGEventSource (check Accessibility permission)".to_string()
    })?;

    let cmd_down = CGEvent::new_keyboard_event(Some(&source), COMMAND_KEYCODE, true)
        .ok_or_else(|| "Failed to create Command key-down event".to_string())?;
    CGEvent::set_flags(Some(&cmd_down), CGEventFlags::MaskCommand);

    let v_down = CGEvent::new_keyboard_event(Some(&source), v_keycode, true)
        .ok_or_else(|| "Failed to create V key-down event".to_string())?;
    CGEvent::set_flags(Some(&v_down), CGEventFlags::MaskCommand);

    let v_up = CGEvent::new_keyboard_event(Some(&source), v_keycode, false)
        .ok_or_else(|| "Failed to create V key-up event".to_string())?;
    CGEvent::set_flags(Some(&v_up), CGEventFlags::MaskCommand);

    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&cmd_down));
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&v_down));
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&v_up));

    Ok(HeldPasteChord)
}

/// Releases the held Command modifier from [`press_paste_chord`].
pub fn release_paste_chord(_held: HeldPasteChord) {
    let Some(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        return;
    };
    if let Some(cmd_up) = CGEvent::new_keyboard_event(Some(&source), COMMAND_KEYCODE, false) {
        CGEvent::set_flags(Some(&cmd_up), CGEventFlags::empty());
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&cmd_up));
    }
}
