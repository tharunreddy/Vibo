//! Lightweight macOS Secure Event Input check (`IsSecureEventInputEnabled`).

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

/// Returns `true` when any macOS process currently holds Secure Event Input
/// (e.g. a password field or Terminal's "Secure Keyboard Entry").
///
/// Modifier-only shortcuts like `option_right+command_right` still work during
/// Secure Input; keyed shortcuts (like `option+space`) are blocked by macOS.
pub fn is_secure_input_enabled() -> bool {
    unsafe { IsSecureEventInputEnabled() != 0 }
}
