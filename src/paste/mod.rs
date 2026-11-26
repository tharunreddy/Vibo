//! Receipt-sequenced reliable cursor paste on macOS.
//!
//! Publishes the transcript to `NSPasteboard` as a lazy promise (`declareTypes:owner:`),
//! injects a layout-aware `Cmd+V` via `CGEvent`, observes the target app's read receipt
//! (`pasteboard:provideDataForType:`), and restores the user's previous clipboard once
//! reads go quiet (never clobbering a newer external copy).

pub mod input;
pub mod macos;

use std::time::{Duration, Instant};

pub const QUIET_PERIOD: Duration = Duration::from_millis(200);
pub const RESTORE_TIMEOUT: Duration = Duration::from_secs(8);
pub const FAILED_INJECTION_TIMEOUT: Duration = Duration::from_millis(500);
pub const CHORD_HOLD_MS: u64 = 60;

#[derive(Debug)]
pub(crate) struct TxState {
    pub published_at: Instant,
    pub injected_at: Option<Instant>,
    pub injection_failed: bool,
    pub receipts: Vec<Instant>,
    pub ownership_lost: bool,
    pub cancelled: bool,
    pub logged_receipt: bool,
}

impl TxState {
    pub fn new() -> Self {
        Self {
            published_at: Instant::now(),
            injected_at: None,
            injection_failed: false,
            receipts: Vec::new(),
            ownership_lost: false,
            cancelled: false,
            logged_receipt: false,
        }
    }

    pub fn record_receipt(&mut self, at: Instant) {
        self.receipts.push(at);
        if !self.logged_receipt
            && let Some(injected) = self.injected_at
            && at >= injected
        {
            self.logged_receipt = true;
            tracing::debug!(
                after_chord_ms = at.duration_since(injected).as_millis() as u64,
                "Target app read promised clipboard transcript"
            );
        }
    }

    pub fn last_receipt_after_injection(&self) -> Option<Instant> {
        let injected = self.injected_at?;
        self.receipts.iter().copied().rev().find(|t| *t >= injected)
    }

    #[cfg(test)]
    pub fn any_receipt_after_injection(&self) -> bool {
        self.last_receipt_after_injection().is_some()
    }
}

pub(crate) enum WaitDecision {
    KeepWaiting,
    Finish,
}

pub(crate) fn evaluate(state: &TxState, now: Instant) -> WaitDecision {
    if state.ownership_lost || state.cancelled {
        return WaitDecision::Finish;
    }
    if let Some(last) = state.last_receipt_after_injection()
        && now.duration_since(last) >= QUIET_PERIOD
    {
        return WaitDecision::Finish;
    }
    let deadline = if state.injection_failed {
        FAILED_INJECTION_TIMEOUT
    } else {
        RESTORE_TIMEOUT
    };
    if now.duration_since(state.published_at) >= deadline {
        return WaitDecision::Finish;
    }
    WaitDecision::KeepWaiting
}

/// Pastes `text` at the current cursor position on the macOS main thread.
/// Must be called from any thread; dispatches to main queue and waits until the
/// `Cmd+V` chord has been posted (clipboard restoration finishes asynchronously).
pub fn paste_at_cursor(text: &str, preserve_clipboard: bool) -> Result<(), String> {
    macos::paste_on_main_sync(text, preserve_clipboard)
}

/// Copies `text` plainly to the general macOS pasteboard (for "Copy Last Transcript").
pub fn copy_to_clipboard(text: &str) {
    macos::copy_plain_text_async(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_after(ago: Duration) -> TxState {
        let mut s = TxState::new();
        s.published_at = Instant::now() - ago;
        s
    }

    #[test]
    fn waits_until_quiet_period_after_post_injection_receipt() {
        let mut s = state_after(Duration::from_millis(300));
        s.injected_at = Some(Instant::now() - Duration::from_millis(250));
        s.receipts.push(Instant::now() - Duration::from_millis(50));
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));

        s.receipts.clear();
        s.receipts.push(Instant::now() - QUIET_PERIOD);
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn ignores_pre_injection_receipt() {
        let mut s = state_after(Duration::from_millis(300));
        s.receipts.push(Instant::now() - Duration::from_millis(200));
        s.injected_at = Some(Instant::now() - Duration::from_millis(100));
        assert!(!s.any_receipt_after_injection());
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }
}
