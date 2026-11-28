//! Pluggable cloud ASR provider boundary.
//!
//! v1 ships with [`gemini_live::GeminiLiveProvider`] (`gemini-3.5-transcribe-live`,
//! Smart Mode). Any future streaming or batch cloud ASR (including unary
//! `gemini-3.5-transcribe`) only needs to implement [`AsrProvider`] and [`AsrSession`].

pub mod gemini_live;
pub mod ws_session;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type TextCallback = Arc<dyn Fn(String) + Send + Sync>;

/// Per-session ASR latency milestones measured inside the provider/transport layer.
#[derive(Debug, Default, Clone)]
pub struct AsrTimings {
    /// True when a pre-warmed standby connection was claimed instead of opening a cold socket.
    pub standby_hit: bool,
    /// Session start → ready to accept audio (WebSocket `setupComplete` + `activityStart`).
    pub connect_ms: Option<f64>,
    /// First audio frame sent → first partial transcript received.
    pub first_partial_ms: Option<f64>,
    /// Caller requested finish → final queued audio flushed and end-of-stream sent.
    pub stop_to_eos_ms: Option<f64>,
    /// End-of-stream sent → server delivered the finalized transcript.
    pub eos_to_final_ms: Option<f64>,
}

/// Completed transcription output paired with its measured ASR timings.
#[derive(Debug, Clone)]
pub struct AsrResult {
    pub text: String,
    pub timings: AsrTimings,
}

/// One active cloud transcription session.
pub trait AsrSession: Send {
    /// Feeds 16 kHz mono f32 samples (-1.0..=1.0) into the session.
    fn feed_pcm(&self, samples: &[f32]) -> Result<(), String>;
    /// Flushes remaining audio and awaits the final transcript and latency timings.
    fn finish(
        self: Box<Self>,
        stop_requested_at: std::time::Instant,
    ) -> Pin<Box<dyn Future<Output = Result<AsrResult, String>> + Send>>;
    /// Aborts immediately and suppresses any subsequent callbacks.
    fn cancel(self: Box<Self>);
}

/// Pluggable cloud ASR backend.
pub trait AsrProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn model(&self) -> &str;
    fn mode_label(&self) -> &'static str;

    fn start_session(
        &self,
        on_partial: TextCallback,
        on_error: TextCallback,
    ) -> Result<Box<dyn AsrSession>, String>;

    /// Optionally pre-warms a standby connection for zero-latency start.
    fn warm(&self) {}

    /// Releases any idle pre-warmed resources.
    fn clear_warm(&self) {}

    /// Verifies credentials and endpoint reachability, returning handshake RTT in ms.
    fn test_connection(&self) -> Pin<Box<dyn Future<Output = Result<u64, String>> + Send + '_>>;
}
