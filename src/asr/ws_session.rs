//! Provider-neutral WebSocket streaming session for cloud speech-to-text.
//!
//! Owns one WebSocket task: uploads queued 16 kHz PCM16LE frames at a bounded rate,
//! coalesces small mic chunks, forwards partial text, instruments per-phase latency
//! milestones, and returns the finalized transcript as soon as the server delivers it.

use crate::asr::{AsrResult, AsrSession, AsrTimings, TextCallback};
use crate::telemetry::ms;
use futures_util::{SinkExt, StreamExt};
use std::collections::hash_map::RandomState;
use std::future::Future;
use std::hash::BuildHasher;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinError, JoinHandle};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT_RETRY_DELAY: Duration = Duration::from_millis(50);
const CONNECT_ERROR_RETRY_DELAY: Duration = Duration::from_millis(250);
const CONNECT_RETRY_JITTER_MS: u64 = 300;
const MAX_CONNECT_ATTEMPTS: usize = 5;
const MIN_ATTEMPT_WINDOW: Duration = Duration::from_millis(250);
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_AFTER_FINAL_TIMEOUT: Duration = Duration::from_secs(5);
const PCM_BYTES_PER_SECOND: f64 = 32_000.0;

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Converts 16 kHz mono f32 samples (-1.0..=1.0) into signed PCM16LE bytes.
pub fn f32_to_pcm16_le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let clamped = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&clamped.to_le_bytes());
    }
    out
}

/// Endpoint and secret of the last connection a provider accepted.
pub struct VerifiedConnection(Mutex<Option<(String, String)>>);

impl Default for VerifiedConnection {
    fn default() -> Self {
        Self::new()
    }
}

impl VerifiedConnection {
    pub const fn new() -> Self {
        Self(Mutex::new(None))
    }

    pub fn record(&self, endpoint: &str, secret: &str, ok: bool) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) =
            ok.then(|| (endpoint.to_string(), secret.trim().to_string()));
    }

    pub fn forget(&self, endpoint: &str, secret: &str) {
        let mut verified = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if verified
            .as_ref()
            .is_some_and(|(e, s)| e == endpoint && s == secret.trim())
        {
            *verified = None;
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum ProtocolEvent {
    Ignore,
    Partial(String),
    Final(String),
}

pub trait WsProtocol: Send + 'static {
    const NAME: &'static str;
    const MAX_AUDIO_LEAD: Duration;
    const CATCH_UP_RATE: f64;
    const IDLE_TIMEOUT: Duration;
    const MAX_AUDIO_FRAME_BYTES: usize;
    const TARGET_AUDIO_FRAME_BYTES: usize = 0;
    const MAX_COALESCE_WAIT: Duration = Duration::from_millis(120);
    const CLIENT_CLOSES_AFTER_FINAL: bool;
    const BINARY_JSON_MESSAGES: bool = false;

    fn audio_messages(&self, pcm16_le: Vec<u8>) -> Vec<Message> {
        chunk_binary_audio(pcm16_le, Self::MAX_AUDIO_FRAME_BYTES)
    }

    fn finish_messages(&self) -> Vec<Message>;
    fn on_text(&mut self, text: &str, ended: bool) -> Result<ProtocolEvent, String>;
    fn on_close(&mut self, frame: Option<CloseFrame>, ended: bool) -> Result<String, String>;
}

pub struct ConnectedSession<P> {
    pub socket: Socket,
    pub protocol: P,
    pub standby_hit: bool,
}

enum StreamCommand {
    Audio(Vec<u8>),
    Finish(tokio::time::Instant),
}

struct Pacer {
    start: tokio::time::Instant,
    sent: Duration,
    free_at: tokio::time::Instant,
    max_lead: Duration,
    catch_up_rate: f64,
}

impl Pacer {
    fn new(start: tokio::time::Instant, max_lead: Duration, catch_up_rate: f64) -> Self {
        Self {
            start,
            sent: Duration::ZERO,
            free_at: start,
            max_lead,
            catch_up_rate,
        }
    }

    fn reserve(&mut self, now: tokio::time::Instant, audio: Duration) -> tokio::time::Instant {
        self.sent += audio;
        let at = (self.start + self.sent.saturating_sub(self.max_lead))
            .max(self.free_at)
            .max(now);
        self.free_at = at + audio.div_f64(self.catch_up_rate);
        at
    }
}

#[derive(Clone)]
pub struct SessionControl {
    callbacks_enabled: Arc<Mutex<bool>>,
    abort: AbortHandle,
}

impl SessionControl {
    pub fn cancel(&self) {
        self.disable_callbacks();
        self.abort.abort();
    }

    fn disable_callbacks(&self) {
        *self
            .callbacks_enabled
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = false;
    }
}

pub struct WsStreamSession {
    name: &'static str,
    tx: mpsc::UnboundedSender<StreamCommand>,
    task: JoinHandle<Result<AsrResult, String>>,
    control: SessionControl,
    final_rx: oneshot::Receiver<AsrResult>,
    detached: bool,
}

enum Finished {
    Final(AsrResult),
    Task(Result<Result<AsrResult, String>, JoinError>),
}

impl WsStreamSession {
    pub fn spawn<P, F>(
        connect: F,
        secret: String,
        on_partial: TextCallback,
        on_error: TextCallback,
    ) -> Self
    where
        P: WsProtocol,
        F: Future<Output = Result<ConnectedSession<P>, String>> + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let (final_tx, final_rx) = oneshot::channel();
        let callbacks_enabled = Arc::new(Mutex::new(true));
        let enabled = Arc::clone(&callbacks_enabled);
        let task = tokio::spawn(async move {
            let mut final_tx = Some(final_tx);
            let result = async {
                let connect_start = Instant::now();
                let connected = connect.await?;
                let connect_ms = ms(connect_start.elapsed());
                tracing::info!(
                    provider = P::NAME,
                    standby_hit = connected.standby_hit,
                    connect_ms,
                    "WebSocket ASR session ready"
                );
                let mut timings = AsrTimings {
                    standby_hit: connected.standby_hit,
                    connect_ms: Some(connect_ms),
                    ..Default::default()
                };
                run_stream(
                    connected.socket,
                    connected.protocol,
                    rx,
                    &enabled,
                    on_partial,
                    P::IDLE_TIMEOUT,
                    &mut timings,
                    &mut final_tx,
                )
                .await
            }
            .await
            .map_err(|e: String| redact(e, &secret));

            if final_tx.is_none() {
                if let Err(error) = &result {
                    tracing::warn!(
                        provider = P::NAME,
                        error = %error,
                        "Session failed after final transcript was already delivered"
                    );
                }
                return result;
            }
            if let Err(error) = &result {
                let guard = enabled.lock().unwrap_or_else(|e| e.into_inner());
                if *guard {
                    on_error(error.clone());
                }
            }
            result
        });
        let control = SessionControl {
            callbacks_enabled,
            abort: task.abort_handle(),
        };
        Self {
            name: P::NAME,
            tx,
            task,
            control,
            final_rx,
            detached: false,
        }
    }

    pub async fn finish_with_timeout(mut self, timeout: Duration) -> Result<AsrResult, String> {
        let _ = self
            .tx
            .send(StreamCommand::Finish(tokio::time::Instant::now()));
        let finished = tokio::time::timeout(timeout, async {
            tokio::select! {
                biased;
                Ok(res) = &mut self.final_rx => Finished::Final(res),
                joined = &mut self.task => Finished::Task(joined),
            }
        })
        .await;

        let result = match finished {
            Ok(Finished::Final(res)) => {
                self.control.disable_callbacks();
                self.detached = true;
                return Ok(res);
            }
            Ok(Finished::Task(Ok(result))) => result,
            Ok(Finished::Task(Err(_))) => Err(format!(
                "{} stream was cancelled or terminated unexpectedly",
                self.name
            )),
            Err(_) => {
                self.control.cancel();
                let _ = (&mut self.task).await;
                Err(format!(
                    "Timed out waiting for {} final transcript",
                    self.name
                ))
            }
        };
        self.control.cancel();
        result
    }
}

impl AsrSession for WsStreamSession {
    fn feed_pcm(&self, samples: &[f32]) -> Result<(), String> {
        self.tx
            .send(StreamCommand::Audio(f32_to_pcm16_le(samples)))
            .map_err(|_| format!("{} stream is no longer accepting audio", self.name))
    }

    fn finish(self: Box<Self>) -> Pin<Box<dyn Future<Output = Result<AsrResult, String>> + Send>> {
        Box::pin(async move { self.finish_with_timeout(FINALIZE_TIMEOUT).await })
    }

    fn cancel(self: Box<Self>) {
        self.control.cancel();
    }
}

impl Drop for WsStreamSession {
    fn drop(&mut self) {
        if !self.detached {
            self.control.cancel();
        }
    }
}

pub fn redact(error: String, secret: &str) -> String {
    let secret = secret.trim();
    if secret.is_empty() {
        error
    } else {
        error.replace(secret, "[REDACTED]")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConnectError {
    Retryable(String),
    Fatal(String),
}

pub(crate) fn is_retryable_ws_error(error: &WsError) -> bool {
    match error {
        WsError::Http(response) => {
            response.status().is_server_error() || response.status() == StatusCode::REQUEST_TIMEOUT
        }
        WsError::Io(_)
        | WsError::Tls(_)
        | WsError::ConnectionClosed
        | WsError::AlreadyClosed
        | WsError::Protocol(_) => true,
        _ => false,
    }
}

fn jitter_duration(max_ms: u64) -> Duration {
    if max_ms == 0 {
        return Duration::ZERO;
    }
    let sample = RandomState::new().hash_one(Instant::now());
    Duration::from_millis(sample % (max_ms + 1))
}

pub(crate) async fn connect_with_retry<T, F, Fut>(
    name: &'static str,
    secret: &str,
    timeout_message: &str,
    mut attempt_fn: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ConnectError>>,
{
    let max_attempts = MAX_CONNECT_ATTEMPTS.max(1);
    let deadline = tokio::time::Instant::now() + CONNECTION_TIMEOUT;
    let min_window = MIN_ATTEMPT_WINDOW.min(CONNECT_ATTEMPT_TIMEOUT);
    let mut last_error: Option<String> = None;

    for attempt in 1..=max_attempts {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            break;
        };
        if attempt > 1 && remaining < min_window {
            break;
        }
        let budget = remaining.min(CONNECT_ATTEMPT_TIMEOUT);
        let retry_delay = match tokio::time::timeout(budget, attempt_fn()).await {
            Ok(Ok(value)) => {
                if attempt > 1 {
                    tracing::info!(provider = name, attempt, "Connected on retry");
                }
                return Ok(value);
            }
            Ok(Err(ConnectError::Fatal(error))) => return Err(redact(error, secret)),
            Ok(Err(ConnectError::Retryable(error))) => {
                let error = redact(error, secret);
                let backoff = CONNECT_ERROR_RETRY_DELAY
                    .saturating_mul(attempt as u32)
                    .min(CONNECT_ATTEMPT_TIMEOUT)
                    + jitter_duration(CONNECT_RETRY_JITTER_MS);
                if attempt < max_attempts {
                    tracing::warn!(
                        provider = name,
                        attempt,
                        max_attempts,
                        error = %error,
                        retry_ms = backoff.as_millis() as u64,
                        "Connect attempt failed; retrying"
                    );
                }
                last_error = Some(error);
                backoff
            }
            Err(_) => {
                let pause = CONNECT_TIMEOUT_RETRY_DELAY + jitter_duration(CONNECT_RETRY_JITTER_MS);
                if attempt < max_attempts {
                    tracing::warn!(
                        provider = name,
                        attempt,
                        max_attempts,
                        timeout_ms = budget.as_millis() as u64,
                        retry_ms = pause.as_millis() as u64,
                        "Connect attempt timed out; retrying"
                    );
                }
                last_error = None;
                pause
            }
        };

        if attempt == max_attempts {
            break;
        }
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            break;
        };
        if remaining <= retry_delay + min_window {
            break;
        }
        tokio::time::sleep(retry_delay).await;
    }

    Err(redact(
        last_error.unwrap_or_else(|| timeout_message.to_string()),
        secret,
    ))
}

fn chunk_binary_audio(bytes: Vec<u8>, max_bytes: usize) -> Vec<Message> {
    if bytes.is_empty() {
        return Vec::new();
    }
    if bytes.len() <= max_bytes {
        return vec![Message::Binary(bytes.into())];
    }
    bytes
        .chunks(max_bytes.max(1))
        .map(|chunk| Message::Binary(chunk.to_vec().into()))
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn handle_protocol_text<P: WsProtocol>(
    protocol: &mut P,
    text: &str,
    ended: bool,
    enabled: &Arc<Mutex<bool>>,
    on_partial: &TextCallback,
    timings: &mut AsrTimings,
    first_audio_sent_at: Option<tokio::time::Instant>,
    final_tx: &mut Option<oneshot::Sender<AsrResult>>,
    delivered_at: &mut Option<tokio::time::Instant>,
    finish_timing: Option<(tokio::time::Instant, tokio::time::Instant)>,
    audio_sent: Duration,
    write: &mut futures_util::stream::SplitSink<Socket, Message>,
) -> Result<Option<AsrResult>, String> {
    let name = P::NAME;
    match protocol.on_text(text, ended)? {
        ProtocolEvent::Ignore => Ok(None),
        ProtocolEvent::Partial(text) => {
            if timings.first_partial_ms.is_none()
                && let Some(sent_at) = first_audio_sent_at
            {
                timings.first_partial_ms = Some(ms(sent_at.elapsed()));
            }
            let guard = enabled.lock().unwrap_or_else(|e| e.into_inner());
            if *guard && delivered_at.is_none() {
                on_partial(text);
            }
            Ok(None)
        }
        ProtocolEvent::Final(text) => {
            if ended {
                let now = tokio::time::Instant::now();
                if let Some((requested, end_sent)) = finish_timing {
                    timings.stop_to_eos_ms =
                        Some(ms(end_sent.saturating_duration_since(requested)));
                    timings.eos_to_final_ms = Some(ms(now.saturating_duration_since(end_sent)));
                    tracing::debug!(
                        provider = name,
                        audio_secs = audio_sent.as_secs_f64(),
                        stop_to_eos_ms = ?timings.stop_to_eos_ms,
                        eos_to_final_ms = ?timings.eos_to_final_ms,
                        "Final transcript received"
                    );
                }
                let result = AsrResult {
                    text,
                    timings: timings.clone(),
                };
                if let Some(tx) = final_tx.take()
                    && tx.send(result.clone()).is_ok()
                {
                    *delivered_at = Some(now);
                }
                if P::CLIENT_CLOSES_AFTER_FINAL {
                    let _ = tokio::time::timeout(
                        CLOSE_AFTER_FINAL_TIMEOUT,
                        write.send(Message::Close(Some(CloseFrame {
                            code: CloseCode::Normal,
                            reason: "".into(),
                        }))),
                    )
                    .await;
                    return Ok(Some(result));
                }
            }
            Ok(None)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_stream<P: WsProtocol>(
    socket: Socket,
    mut protocol: P,
    mut rx: mpsc::UnboundedReceiver<StreamCommand>,
    enabled: &Arc<Mutex<bool>>,
    on_partial: TextCallback,
    idle_timeout: Duration,
    timings: &mut AsrTimings,
    final_tx: &mut Option<oneshot::Sender<AsrResult>>,
) -> Result<AsrResult, String> {
    let name = P::NAME;
    let (mut write, mut read) = socket.split();
    let mut ended = false;
    let mut pending: Option<(StreamCommand, tokio::time::Instant)> = None;
    let mut coalesce_buf: Vec<u8> = Vec::new();
    let mut coalesce_deadline: Option<tokio::time::Instant> = None;
    let mut finish_after_flush: Option<tokio::time::Instant> = None;
    let mut first_audio_sent_at: Option<tokio::time::Instant> = None;
    let mut pacer = Pacer::new(
        tokio::time::Instant::now(),
        P::MAX_AUDIO_LEAD,
        P::CATCH_UP_RATE,
    );
    let mut finish_timing = None;
    let mut delivered_at: Option<tokio::time::Instant> = None;
    let mut last_received = tokio::time::Instant::now();

    loop {
        let send_at = pending
            .as_ref()
            .map_or_else(tokio::time::Instant::now, |(_, at)| *at);
        let flush_at = coalesce_deadline.unwrap_or_else(tokio::time::Instant::now);
        let receive_deadline = match delivered_at {
            Some(at) => (last_received + idle_timeout).min(at + CLOSE_AFTER_FINAL_TIMEOUT),
            None => last_received + idle_timeout,
        };

        tokio::select! {
            command = rx.recv(), if !ended && pending.is_none() && finish_after_flush.is_none() => {
                let command = command.ok_or_else(|| format!("{name} audio queue closed unexpectedly"))?;
                let now = tokio::time::Instant::now();
                if P::TARGET_AUDIO_FRAME_BYTES == 0 {
                    let at = match &command {
                        StreamCommand::Audio(bytes) => pacer.reserve(
                            now,
                            Duration::from_secs_f64(bytes.len() as f64 / PCM_BYTES_PER_SECOND),
                        ),
                        StreamCommand::Finish(_) => now,
                    };
                    pending = Some((command, at));
                } else {
                    match command {
                        StreamCommand::Audio(bytes) => {
                            if !bytes.is_empty() {
                                if coalesce_buf.is_empty() {
                                    coalesce_deadline = Some(now + P::MAX_COALESCE_WAIT);
                                }
                                coalesce_buf.extend_from_slice(&bytes);
                            }
                            while coalesce_buf.len() < P::TARGET_AUDIO_FRAME_BYTES {
                                match rx.try_recv() {
                                    Ok(StreamCommand::Audio(more)) => {
                                        if !more.is_empty() {
                                            if coalesce_buf.is_empty() {
                                                coalesce_deadline = Some(now + P::MAX_COALESCE_WAIT);
                                            }
                                            coalesce_buf.extend_from_slice(&more);
                                        }
                                    }
                                    Ok(StreamCommand::Finish(requested)) => {
                                        finish_after_flush = Some(requested);
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }
                            if finish_after_flush.is_some()
                                || coalesce_buf.len() >= P::TARGET_AUDIO_FRAME_BYTES
                            {
                                coalesce_deadline = None;
                                if !coalesce_buf.is_empty() {
                                    let bytes = std::mem::take(&mut coalesce_buf);
                                    let at = pacer.reserve(
                                        now,
                                        Duration::from_secs_f64(
                                            bytes.len() as f64 / PCM_BYTES_PER_SECOND,
                                        ),
                                    );
                                    pending = Some((StreamCommand::Audio(bytes), at));
                                } else if let Some(requested) = finish_after_flush.take() {
                                    pending = Some((StreamCommand::Finish(requested), now));
                                }
                            }
                        }
                        StreamCommand::Finish(requested) => {
                            coalesce_deadline = None;
                            if !coalesce_buf.is_empty() {
                                let bytes = std::mem::take(&mut coalesce_buf);
                                let at = pacer.reserve(
                                    now,
                                    Duration::from_secs_f64(
                                        bytes.len() as f64 / PCM_BYTES_PER_SECOND,
                                    ),
                                );
                                finish_after_flush = Some(requested);
                                pending = Some((StreamCommand::Audio(bytes), at));
                            } else {
                                pending = Some((StreamCommand::Finish(requested), now));
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(flush_at), if !ended && pending.is_none() && coalesce_deadline.is_some() => {
                coalesce_deadline = None;
                if !coalesce_buf.is_empty() {
                    let now = tokio::time::Instant::now();
                    let bytes = std::mem::take(&mut coalesce_buf);
                    let at = pacer.reserve(
                        now,
                        Duration::from_secs_f64(bytes.len() as f64 / PCM_BYTES_PER_SECOND),
                    );
                    pending = Some((StreamCommand::Audio(bytes), at));
                }
            }
            _ = tokio::time::sleep_until(send_at), if pending.is_some() => {
                let Some((command, _)) = pending.take() else { continue; };
                let (messages, requested, is_audio) = match command {
                    StreamCommand::Audio(bytes) => (protocol.audio_messages(bytes), None, true),
                    StreamCommand::Finish(requested) => (protocol.finish_messages(), Some(requested), false),
                };
                for message in messages {
                    tokio::time::timeout(CONNECTION_TIMEOUT, write.send(message))
                        .await
                        .map_err(|_| format!("Timed out sending audio/end of stream to {name}"))?
                        .map_err(|e| format!("Failed to send audio/end of stream to {name}: {e}"))?;
                }
                if is_audio && first_audio_sent_at.is_none() {
                    first_audio_sent_at = Some(tokio::time::Instant::now());
                }
                if let Some(requested) = requested {
                    ended = true;
                    finish_timing = Some((requested, tokio::time::Instant::now()));
                } else if let Some(requested) = finish_after_flush.take() {
                    pending = Some((StreamCommand::Finish(requested), tokio::time::Instant::now()));
                }
            }
            message = read.next() => {
                last_received = tokio::time::Instant::now();
                match message {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(result) = handle_protocol_text(
                            &mut protocol,
                            &text,
                            ended,
                            enabled,
                            &on_partial,
                            timings,
                            first_audio_sent_at,
                            final_tx,
                            &mut delivered_at,
                            finish_timing,
                            pacer.sent,
                            &mut write,
                        )
                        .await?
                        {
                            return Ok(result);
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) if P::BINARY_JSON_MESSAGES => {
                        let text = std::str::from_utf8(&bytes).map_err(|e| {
                            format!("Invalid UTF-8 in {name} response frame: {e}")
                        })?;
                        if let Some(result) = handle_protocol_text(
                            &mut protocol,
                            text,
                            ended,
                            enabled,
                            &on_partial,
                            timings,
                            first_audio_sent_at,
                            final_tx,
                            &mut delivered_at,
                            finish_timing,
                            pacer.sent,
                            &mut write,
                        )
                        .await?
                        {
                            return Ok(result);
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = tokio::time::timeout(CONNECTION_TIMEOUT, write.send(Message::Pong(data))).await;
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let text = protocol.on_close(frame, ended)?;
                        if let Some((requested, end_sent)) = finish_timing {
                            let now = tokio::time::Instant::now();
                            timings.stop_to_eos_ms.get_or_insert_with(|| ms(end_sent.saturating_duration_since(requested)));
                            timings.eos_to_final_ms.get_or_insert_with(|| ms(now.saturating_duration_since(end_sent)));
                        }
                        return Ok(AsrResult {
                            text,
                            timings: timings.clone(),
                        });
                    }
                    Some(Err(e)) => {
                        return Err(format!("{name} WebSocket receive error: {e}"));
                    }
                    None => {
                        let text = protocol.on_close(None, ended)?;
                        return Ok(AsrResult {
                            text,
                            timings: timings.clone(),
                        });
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep_until(receive_deadline) => {
                if delivered_at.is_some() {
                    return Ok(AsrResult {
                        text: String::new(),
                        timings: timings.clone(),
                    });
                }
                return Err(format!("{name} timed out waiting for server response"));
            }
        }
    }
}
