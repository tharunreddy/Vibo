//! Gemini 3.5 Transcribe Live (`gemini-3.5-transcribe-live`) streaming ASR provider.
//!
//! Features:
//! - Smart Mode (`mode: "SMART"`) cumulative transcript replacement & Verbatim mode
//! - Optional `languageCodes` (omitted when empty for automatic 85+ language detection)
//! - Up to 1,000 `customVocabulary` biasing terms
//! - Manual VAD (`activityStart` → 100–200 ms coalesced PCM16LE frames → `activityEnd`)
//! - Pre-warmed 4-minute `StandbyPool` with 25s pings & instant claim (`standby_hit`)

use crate::asr::ws_session::{
    CONNECTION_TIMEOUT, ConnectError, ConnectedSession, ProtocolEvent, Socket, VerifiedConnection,
    WsProtocol, WsStreamSession, connect_with_retry, is_retryable_ws_error, redact,
};
use crate::asr::{AsrProvider, AsrSession, TextCallback};
use crate::settings::{self, DEFAULT_GEMINI_LIVE_ENDPOINT, Settings};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderName};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use url::Url;

pub const GEMINI_MODEL_NAME: &str = "gemini-3.5-transcribe-live";
pub const GEMINI_MAX_CUSTOM_VOCABULARY: usize = 1_000;

/// Coalesce 16 kHz mono PCM16LE mic frames to ~100 ms (3,200 bytes) per WebSocket frame.
const GEMINI_TARGET_AUDIO_FRAME_BYTES: usize = 3_200;
/// Cap each base64 JSON audio payload at 200 ms (6,400 raw bytes).
const GEMINI_MAX_AUDIO_FRAME_BYTES: usize = 6_400;
const GEMINI_MAX_COALESCE_WAIT: Duration = Duration::from_millis(120);
const GEMINI_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

const STANDBY_SOCKET_TTL: Duration = Duration::from_secs(4 * 60);
const STANDBY_ACTIVE_WINDOW: Duration = Duration::from_secs(15 * 60);
const STANDBY_PING_INTERVAL: Duration = Duration::from_secs(25);
const STANDBY_MIN_ROTATE_AGE: Duration = Duration::from_secs(5);
const STANDBY_ACTIVITY_START_TIMEOUT: Duration = Duration::from_secs(2);
const PCM_MIME_TYPE: &str = "audio/pcm;rate=16000";

static VERIFIED_CONNECTION: VerifiedConnection = VerifiedConnection::new();
static STANDBY: OnceLock<Arc<StandbyPool>> = OnceLock::new();

fn standby_pool() -> &'static Arc<StandbyPool> {
    STANDBY.get_or_init(|| Arc::new(StandbyPool::new()))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TranscriptionMode {
    Verbatim,
    #[default]
    Smart,
}

impl TranscriptionMode {
    pub fn from_smart_enabled(enabled: bool) -> Self {
        if enabled { Self::Smart } else { Self::Verbatim }
    }

    pub fn as_api_str(self) -> &'static str {
        match self {
            Self::Verbatim => "VERBATIM",
            Self::Smart => "SMART",
        }
    }

    pub fn as_label(self) -> &'static str {
        match self {
            Self::Verbatim => "verbatim",
            Self::Smart => "smart",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GeminiSessionConfig {
    pub mode: TranscriptionMode,
    /// Optional BCP-47 hints. Empty = automatic 85+ language detection & code-switching.
    pub language_codes: Vec<String>,
    pub custom_vocabulary: Vec<String>,
}

/// Concrete [`AsrProvider`] backed by `gemini-3.5-transcribe-live`.
#[derive(Clone, Debug)]
pub struct GeminiLiveProvider {
    endpoint: String,
    api_key: String,
    config: GeminiSessionConfig,
}

impl GeminiLiveProvider {
    pub fn from_settings(settings: &Settings) -> Self {
        let custom_vocabulary = settings::normalize_custom_vocabulary(
            &settings.custom_vocabulary,
            GEMINI_MAX_CUSTOM_VOCABULARY,
        );
        let language_codes = settings
            .language_codes
            .iter()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect();
        Self {
            endpoint: settings.effective_live_endpoint().to_string(),
            api_key: settings.effective_api_key(),
            config: GeminiSessionConfig {
                mode: TranscriptionMode::from_smart_enabled(settings.smart_mode),
                language_codes,
                custom_vocabulary,
            },
        }
    }
}

impl AsrProvider for GeminiLiveProvider {
    fn id(&self) -> &'static str {
        "gemini-live"
    }

    fn model(&self) -> &str {
        GEMINI_MODEL_NAME
    }

    fn mode_label(&self) -> &'static str {
        self.config.mode.as_label()
    }

    fn start_session(
        &self,
        on_partial: TextCallback,
        on_error: TextCallback,
    ) -> Result<Box<dyn AsrSession>, String> {
        let session = start_session_with_pool(
            Arc::clone(standby_pool()),
            &self.endpoint,
            &self.api_key,
            &self.config,
            on_partial,
            on_error,
        )?;
        Ok(Box::new(session))
    }

    fn warm(&self) {
        warm_standby(&self.endpoint, &self.api_key, &self.config);
    }

    fn clear_warm(&self) {
        clear_standby();
    }

    fn test_connection(&self) -> Pin<Box<dyn Future<Output = Result<u64, String>> + Send + '_>> {
        Box::pin(test_gemini_connection(&self.endpoint, &self.api_key))
    }
}

fn normalize_endpoint(endpoint: &str) -> &str {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        DEFAULT_GEMINI_LIVE_ENDPOINT
    } else {
        trimmed
    }
}

fn validate_endpoint(endpoint: &str) -> Result<Url, String> {
    let url = Url::parse(endpoint).map_err(|e| format!("Invalid Gemini Live endpoint: {e}"))?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(format!(
            "Invalid Gemini Live endpoint scheme '{}': expected ws or wss",
            url.scheme()
        ));
    }
    Ok(url)
}

fn setup_message(config: &GeminiSessionConfig) -> Value {
    let mut transcription = json!({
        "mode": config.mode.as_api_str(),
    });
    if !config.language_codes.is_empty() {
        transcription["languageCodes"] = json!(config.language_codes);
    }
    if !config.custom_vocabulary.is_empty() {
        transcription["customVocabulary"] = json!(config.custom_vocabulary);
    }
    json!({
        "setup": {
            "model": format!("models/{GEMINI_MODEL_NAME}"),
            "generationConfig": {
                "responseModalities": ["TEXT"],
            },
            "realtimeInputConfig": {
                "automaticActivityDetection": {
                    "disabled": true,
                },
            },
            "inputAudioTranscription": transcription,
        }
    })
}

fn activity_start_message() -> Value {
    json!({ "realtimeInput": { "activityStart": {} } })
}

fn activity_end_message() -> Value {
    json!({ "realtimeInput": { "activityEnd": {} } })
}

fn audio_chunk_message(pcm16_le: &[u8]) -> Value {
    json!({
        "realtimeInput": {
            "audio": {
                "mimeType": PCM_MIME_TYPE,
                "data": BASE64.encode(pcm16_le),
            }
        }
    })
}

fn extract_json_error(value: &Value) -> Option<(Option<u64>, Option<&str>, String)> {
    let error = value.get("error").filter(|v| !v.is_null())?;
    let code = error.get("code").and_then(Value::as_u64);
    let status = error.get("status").and_then(Value::as_str);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("Unknown server error")
        .to_string();
    Some((code, status, message))
}

fn format_json_error(code: Option<u64>, status: Option<&str>, message: &str) -> String {
    match (code, status) {
        (Some(code), Some(status)) => {
            format!("Gemini Live error {code} ({status}): {message}")
        }
        (Some(code), None) => format!("Gemini Live error {code}: {message}"),
        (None, Some(status)) => format!("Gemini Live error ({status}): {message}"),
        (None, None) => format!("Gemini Live error: {message}"),
    }
}

fn is_auth_error(code: Option<u64>, status: Option<&str>) -> bool {
    matches!(code, Some(401 | 403))
        || matches!(status, Some("UNAUTHENTICATED" | "PERMISSION_DENIED"))
}

fn format_close_error(frame: Option<CloseFrame>) -> (String, bool) {
    let Some(frame) = frame else {
        return ("Gemini Live closed before setupComplete".into(), true);
    };
    let code = u16::from(frame.code);
    let message = if frame.reason.is_empty() {
        format!("Gemini Live closed with code {code}")
    } else {
        format!("Gemini Live closed with code {code}: {}", frame.reason)
    };
    let retryable = !matches!(
        frame.code,
        CloseCode::Protocol | CloseCode::Unsupported | CloseCode::Invalid | CloseCode::Policy
    );
    (message, retryable)
}

fn format_ws_connect_error(error: WsError) -> String {
    match error {
        WsError::Http(response) => {
            let status = response.status();
            let detail = response.body().as_deref().and_then(|body| {
                if let Ok(json) = serde_json::from_slice::<Value>(body) {
                    if let Some((_, _, msg)) = extract_json_error(&json) {
                        return Some(msg);
                    }
                    if let Some(msg) = json.get("message").and_then(Value::as_str) {
                        let trimmed = msg.trim();
                        if !trimmed.is_empty() {
                            return Some(trimmed.to_string());
                        }
                    }
                }
                let text = std::str::from_utf8(body).ok()?.trim();
                if text.is_empty() || text.starts_with('<') {
                    return None;
                }
                Some(text.chars().take(240).collect())
            });
            match detail {
                Some(reason) => format!("Gemini Live rejected connection ({status}): {reason}"),
                None => format!("Gemini Live rejected connection ({status})"),
            }
        }
        other => format!("Failed to connect to Gemini Live WebSocket: {other}"),
    }
}

fn decode_server_frame_text(message: &Message) -> Result<Option<&str>, String> {
    match message {
        Message::Text(text) => Ok(Some(text.as_str())),
        Message::Binary(bytes) => std::str::from_utf8(bytes)
            .map(Some)
            .map_err(|e| format!("Invalid UTF-8 in Gemini Live response frame: {e}")),
        _ => Ok(None),
    }
}

fn validate_setup_ack(text: &str) -> Result<bool, String> {
    let payload: Value = serde_json::from_str(text)
        .map_err(|e| format!("Invalid Gemini Live handshake JSON: {e}"))?;
    if let Some((code, status, message)) = extract_json_error(&payload) {
        return Err(format_json_error(code, status, &message));
    }
    Ok(payload
        .get("setupComplete")
        .is_some_and(|value| !value.is_null()))
}

async fn connect_and_setup(
    endpoint: &str,
    secret: &str,
    config: &GeminiSessionConfig,
    send_activity_start: bool,
) -> Result<Socket, String> {
    let trimmed_secret = secret.trim();
    if trimmed_secret.is_empty() {
        return Err("Gemini API key is not set. Configure it in the Vibo menu bar.".into());
    }
    let base_endpoint = normalize_endpoint(endpoint);
    let parsed_url = validate_endpoint(base_endpoint)?;
    let has_query_auth = parsed_url
        .query_pairs()
        .any(|(key, _)| key == "key" || key == "access_token");
    let setup_payload = setup_message(config).to_string();

    let result = connect_with_retry(
        GeminiProtocol::NAME,
        trimmed_secret,
        "Gemini Live connection timed out after 10 seconds",
        || {
            let setup_payload = setup_payload.clone();
            async move {
                let mut request = base_endpoint.into_client_request().map_err(|e| {
                    ConnectError::Fatal(format!("Invalid Gemini Live endpoint: {e}"))
                })?;
                if !has_query_auth {
                    if trimmed_secret.starts_with("auth_tokens/") {
                        let authorization = HeaderValue::from_str(&format!(
                            "Token {trimmed_secret}"
                        ))
                        .map_err(|_| {
                            ConnectError::Fatal("Gemini API key contains invalid characters".into())
                        })?;
                        request.headers_mut().insert(AUTHORIZATION, authorization);
                    } else {
                        let api_key = HeaderValue::from_str(trimmed_secret).map_err(|_| {
                            ConnectError::Fatal("Gemini API key contains invalid characters".into())
                        })?;
                        request
                            .headers_mut()
                            .insert(HeaderName::from_static("x-goog-api-key"), api_key);
                    }
                }
                let (mut socket, _) = connect_async_with_config(request, None, true)
                    .await
                    .map_err(|e| {
                        let retryable = is_retryable_ws_error(&e);
                        let msg = format_ws_connect_error(e);
                        if retryable {
                            ConnectError::Retryable(msg)
                        } else {
                            ConnectError::Fatal(msg)
                        }
                    })?;

                socket
                    .send(Message::Text(setup_payload.into()))
                    .await
                    .map_err(|e| {
                        ConnectError::Retryable(format!("Failed to send Gemini Live setup: {e}"))
                    })?;

                loop {
                    match socket.next().await {
                        Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) => {
                            let text = decode_server_frame_text(&message)
                                .map_err(ConnectError::Fatal)?
                                .unwrap_or_default();
                            if validate_setup_ack(text).map_err(ConnectError::Fatal)? {
                                break;
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            socket.send(Message::Pong(data)).await.map_err(|e| {
                                ConnectError::Retryable(format!(
                                    "Gemini Live handshake ping failed: {e}"
                                ))
                            })?;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(frame))) => {
                            let (message, retryable) = format_close_error(frame);
                            return Err(if retryable {
                                ConnectError::Retryable(message)
                            } else {
                                ConnectError::Fatal(message)
                            });
                        }
                        Some(Err(e)) => {
                            return Err(ConnectError::Retryable(format!(
                                "Gemini Live handshake failed: {e}"
                            )));
                        }
                        None => {
                            return Err(ConnectError::Retryable(
                                "Gemini Live closed before setupComplete".into(),
                            ));
                        }
                        _ => {}
                    }
                }

                if send_activity_start {
                    socket
                        .send(Message::Text(activity_start_message().to_string().into()))
                        .await
                        .map_err(|e| {
                            ConnectError::Retryable(format!(
                                "Failed to send Gemini Live activityStart: {e}"
                            ))
                        })?;
                }
                Ok(socket)
            }
        },
    )
    .await;

    if result.is_err() {
        VERIFIED_CONNECTION.forget(base_endpoint, trimmed_secret);
    }
    result
}

fn append_utterance(target: &mut String, utterance: &str) {
    let trimmed = utterance.trim();
    if trimmed.is_empty() {
        return;
    }
    if !target.is_empty() {
        target.push(' ');
    }
    target.push_str(trimmed);
}

fn combine_utterances(final_text: &str, interim: &str) -> String {
    let mut combined = final_text.to_string();
    append_utterance(&mut combined, interim);
    combined
}

struct GeminiProtocol {
    credentials: (String, String),
    mode: TranscriptionMode,
    final_text: String,
    received_final: bool,
    last_partial: String,
}

impl GeminiProtocol {
    fn new(credentials: (String, String), mode: TranscriptionMode) -> Self {
        Self {
            credentials,
            mode,
            final_text: String::new(),
            received_final: false,
            last_partial: String::new(),
        }
    }

    fn apply_final_transcription(&mut self, text: &str) {
        match self.mode {
            TranscriptionMode::Verbatim => append_utterance(&mut self.final_text, text),
            // In SMART mode, Gemini sends the cumulative rewritten session transcript.
            TranscriptionMode::Smart => {
                self.final_text = text.trim().to_string();
            }
        }
    }
}

impl WsProtocol for GeminiProtocol {
    const NAME: &'static str = "Gemini Live";
    const MAX_AUDIO_LEAD: Duration = Duration::from_secs(3);
    const CATCH_UP_RATE: f64 = 4.0;
    const IDLE_TIMEOUT: Duration = GEMINI_IDLE_TIMEOUT;
    const MAX_AUDIO_FRAME_BYTES: usize = GEMINI_MAX_AUDIO_FRAME_BYTES;
    const TARGET_AUDIO_FRAME_BYTES: usize = GEMINI_TARGET_AUDIO_FRAME_BYTES;
    const MAX_COALESCE_WAIT: Duration = GEMINI_MAX_COALESCE_WAIT;
    const CLIENT_CLOSES_AFTER_FINAL: bool = true;
    const BINARY_JSON_MESSAGES: bool = true;

    fn audio_messages(&self, pcm16_le: Vec<u8>) -> Vec<Message> {
        if pcm16_le.is_empty() {
            return Vec::new();
        }
        pcm16_le
            .chunks(Self::MAX_AUDIO_FRAME_BYTES.max(1))
            .map(|chunk| Message::Text(audio_chunk_message(chunk).to_string().into()))
            .collect()
    }

    fn finish_messages(&self) -> Vec<Message> {
        vec![Message::Text(activity_end_message().to_string().into())]
    }

    fn on_text(&mut self, text: &str, ended: bool) -> Result<ProtocolEvent, String> {
        let event: Value = serde_json::from_str(text)
            .map_err(|e| format!("Invalid Gemini Live response JSON: {e}"))?;
        if let Some((code, status, message)) = extract_json_error(&event) {
            if is_auth_error(code, status) {
                VERIFIED_CONNECTION.forget(&self.credentials.0, &self.credentials.1);
            }
            return Err(format_json_error(code, status, &message));
        }
        if let Some(go_away) = event.get("goAway").filter(|v| !v.is_null()) {
            tracing::warn!(go_away = %go_away, "Gemini Live sent goAway");
        }
        let Some(server_content) = event.get("serverContent").filter(|v| !v.is_null()) else {
            return Ok(ProtocolEvent::Ignore);
        };

        let mut committed_update = false;
        if let Some(input_transcription) = server_content
            .get("inputTranscription")
            .filter(|v| !v.is_null())
        {
            let transcript = input_transcription
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("");
            self.apply_final_transcription(transcript);
            committed_update = true;
            if ended {
                self.received_final = true;
                return Ok(ProtocolEvent::Final(self.final_text.trim().to_string()));
            }
        }

        let turn_finished = server_content
            .get("generationComplete")
            .and_then(Value::as_bool)
            == Some(true)
            || server_content.get("turnComplete").and_then(Value::as_bool) == Some(true);
        if turn_finished && ended {
            self.received_final = true;
            return Ok(ProtocolEvent::Final(self.final_text.trim().to_string()));
        }

        let interim = server_content
            .get("interimInputTranscription")
            .filter(|v| !v.is_null())
            .and_then(|v| v.get("text"))
            .and_then(Value::as_str);

        if let Some(interim) = interim {
            let partial = combine_utterances(&self.final_text, interim);
            if partial == self.last_partial {
                return Ok(ProtocolEvent::Ignore);
            }
            self.last_partial.clone_from(&partial);
            return Ok(ProtocolEvent::Partial(partial));
        }

        if committed_update {
            let partial = self.final_text.clone();
            if partial == self.last_partial {
                return Ok(ProtocolEvent::Ignore);
            }
            self.last_partial.clone_from(&partial);
            return Ok(ProtocolEvent::Partial(partial));
        }

        Ok(ProtocolEvent::Ignore)
    }

    fn on_close(&mut self, frame: Option<CloseFrame>, ended: bool) -> Result<String, String> {
        let frame =
            frame.ok_or_else(|| "Gemini Live closed without a completion status".to_string())?;
        if frame.code != CloseCode::Normal {
            if frame.code == CloseCode::Policy {
                VERIFIED_CONNECTION.forget(&self.credentials.0, &self.credentials.1);
            }
            return Err(if frame.reason.is_empty() {
                format!("Gemini Live closed with code {}", u16::from(frame.code))
            } else {
                format!(
                    "Gemini Live closed with code {}: {}",
                    u16::from(frame.code),
                    frame.reason
                )
            });
        }
        if !ended {
            return Err("Gemini Live closed before activityEnd".into());
        }
        if !self.received_final {
            return Err("Gemini Live closed without a final transcript".into());
        }
        Ok(self.final_text.trim().to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StandbyKey {
    endpoint: String,
    secret: String,
    config: GeminiSessionConfig,
}

impl StandbyKey {
    fn new(endpoint: &str, secret: &str, config: &GeminiSessionConfig) -> Self {
        Self {
            endpoint: normalize_endpoint(endpoint).to_string(),
            secret: secret.trim().to_string(),
            config: config.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct StandbyTimings {
    socket_ttl: Duration,
    active_window: Duration,
    ping_interval: Duration,
    min_rotate_age: Duration,
}

impl Default for StandbyTimings {
    fn default() -> Self {
        Self {
            socket_ttl: STANDBY_SOCKET_TTL,
            active_window: STANDBY_ACTIVE_WINDOW,
            ping_interval: STANDBY_PING_INTERVAL,
            min_rotate_age: STANDBY_MIN_ROTATE_AGE,
        }
    }
}

enum StandbyCommand {
    Claim(oneshot::Sender<Option<Socket>>),
    Close,
}

struct StandbyEntry {
    id: u64,
    key: StandbyKey,
    cmd_tx: mpsc::Sender<StandbyCommand>,
    warm_until: Arc<Mutex<Instant>>,
}

struct StandbyPool {
    entry: Mutex<Option<StandbyEntry>>,
    next_id: AtomicU64,
}

impl StandbyPool {
    fn new() -> Self {
        Self {
            entry: Mutex::new(None),
            next_id: AtomicU64::new(0),
        }
    }

    fn warm(self: &Arc<Self>, key: StandbyKey, timings: StandbyTimings) {
        if key.secret.is_empty() || validate_endpoint(&key.endpoint).is_err() {
            self.clear();
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let deadline = Instant::now() + timings.active_window;
        let previous = {
            let mut slot = self.entry.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = slot.as_ref()
                && existing.key == key
                && !existing.cmd_tx.is_closed()
            {
                *existing
                    .warm_until
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = deadline;
                return;
            }
            let (cmd_tx, cmd_rx) = mpsc::channel(2);
            let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
            let warm_until = Arc::new(Mutex::new(deadline));
            let previous = slot.replace(StandbyEntry {
                id,
                key: key.clone(),
                cmd_tx,
                warm_until: Arc::clone(&warm_until),
            });
            let pool = Arc::clone(self);
            handle.spawn(async move {
                run_standby_keeper(pool, id, key, cmd_rx, warm_until, timings).await;
            });
            previous
        };
        if let Some(prev) = previous {
            let _ = prev.cmd_tx.try_send(StandbyCommand::Close);
        }
    }

    fn clear(&self) {
        let previous = self.entry.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(prev) = previous {
            let _ = prev.cmd_tx.try_send(StandbyCommand::Close);
        }
    }

    fn clear_if_matches_credentials(&self, endpoint: &str, secret: &str) {
        let endpoint = normalize_endpoint(endpoint);
        let secret = secret.trim();
        let previous = {
            let mut slot = self.entry.lock().unwrap_or_else(|e| e.into_inner());
            if slot
                .as_ref()
                .is_some_and(|e| e.key.endpoint == endpoint && e.key.secret == secret)
            {
                slot.take()
            } else {
                None
            }
        };
        if let Some(prev) = previous {
            let _ = prev.cmd_tx.try_send(StandbyCommand::Close);
        }
    }

    fn remove_if_id(&self, id: u64) {
        let mut slot = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|e| e.id == id) {
            *slot = None;
        }
    }

    async fn claim(&self, key: &StandbyKey) -> Option<Socket> {
        let (matching, mismatched) = {
            let mut slot = self.entry.lock().unwrap_or_else(|e| e.into_inner());
            match slot.take() {
                Some(entry) if entry.key == *key => (Some(entry), None),
                Some(entry) => (None, Some(entry)),
                None => (None, None),
            }
        };
        if let Some(stale) = mismatched {
            let _ = stale.cmd_tx.try_send(StandbyCommand::Close);
        }
        let entry = matching?;
        let (reply_tx, reply_rx) = oneshot::channel();
        if entry
            .cmd_tx
            .send(StandbyCommand::Claim(reply_tx))
            .await
            .is_err()
        {
            return None;
        }
        match tokio::time::timeout(CONNECTION_TIMEOUT, reply_rx).await {
            Ok(Ok(socket)) => socket,
            _ => None,
        }
    }
}

async fn close_socket_gracefully(socket: &mut Socket) {
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        socket.send(Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        }))),
    )
    .await;
}

async fn run_standby_keeper(
    pool: Arc<StandbyPool>,
    id: u64,
    key: StandbyKey,
    mut cmd_rx: mpsc::Receiver<StandbyCommand>,
    warm_until: Arc<Mutex<Instant>>,
    timings: StandbyTimings,
) {
    loop {
        let connect_start = Instant::now();
        let connect_fut = connect_and_setup(&key.endpoint, &key.secret, &key.config, false);
        tokio::pin!(connect_fut);
        let mut pending_claim: Option<oneshot::Sender<Option<Socket>>> = None;

        let socket_result = loop {
            tokio::select! {
                result = &mut connect_fut => break result,
                cmd = cmd_rx.recv(), if pending_claim.is_none() => match cmd {
                    Some(StandbyCommand::Claim(reply_tx)) => {
                        pending_claim = Some(reply_tx);
                    }
                    Some(StandbyCommand::Close) | None => {
                        pool.remove_if_id(id);
                        return;
                    }
                }
            }
        };

        let mut socket = match socket_result {
            Ok(socket) => {
                VERIFIED_CONNECTION.record(&key.endpoint, &key.secret, true);
                tracing::debug!(
                    elapsed_ms = connect_start.elapsed().as_millis() as u64,
                    "Gemini Live standby socket ready"
                );
                socket
            }
            Err(error) => {
                tracing::debug!(
                    error = %redact(error, &key.secret),
                    "Gemini Live standby warmup failed"
                );
                if let Some(reply_tx) = pending_claim {
                    let _ = reply_tx.send(None);
                }
                pool.remove_if_id(id);
                return;
            }
        };

        if let Some(reply_tx) = pending_claim {
            pool.remove_if_id(id);
            if let Err(Some(mut unclaimed)) = reply_tx.send(Some(socket)) {
                close_socket_gracefully(&mut unclaimed).await;
            }
            return;
        }

        let established_at = Instant::now();
        let expires_at = tokio::time::Instant::now() + timings.socket_ttl;
        let mut ping_interval = tokio::time::interval(timings.ping_interval);
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping_interval.tick().await;

        let should_rotate = loop {
            tokio::select! {
                biased;
                cmd = cmd_rx.recv() => match cmd {
                    Some(StandbyCommand::Claim(reply_tx)) => {
                        pool.remove_if_id(id);
                        if let Err(Some(mut unclaimed)) = reply_tx.send(Some(socket)) {
                            close_socket_gracefully(&mut unclaimed).await;
                        }
                        return;
                    }
                    Some(StandbyCommand::Close) | None => {
                        pool.remove_if_id(id);
                        close_socket_gracefully(&mut socket).await;
                        return;
                    }
                },
                message = socket.next() => match message {
                    Some(Ok(Message::Ping(data))) => {
                        let pong = tokio::time::timeout(
                            Duration::from_secs(5),
                            socket.send(Message::Pong(data)),
                        )
                        .await;
                        if !matches!(pong, Ok(Ok(()))) {
                            pool.remove_if_id(id);
                            return;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(msg @ (Message::Text(_) | Message::Binary(_)))) => {
                        if let Ok(Some(text)) = decode_server_frame_text(&msg)
                            && let Ok(payload) = serde_json::from_str::<Value>(text)
                        {
                            if let Some((code, status, err_msg)) = extract_json_error(&payload) {
                                if is_auth_error(code, status) {
                                    VERIFIED_CONNECTION.forget(&key.endpoint, &key.secret);
                                }
                                tracing::warn!(
                                    error = %redact(format_json_error(code, status, &err_msg), &key.secret),
                                    "Gemini Live standby received error"
                                );
                                pool.remove_if_id(id);
                                return;
                            }
                            if payload.get("goAway").is_some_and(|v| !v.is_null()) {
                                tracing::debug!("Gemini Live standby received goAway");
                                close_socket_gracefully(&mut socket).await;
                                break established_at.elapsed() >= timings.min_rotate_age;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                        tracing::debug!("Gemini Live standby closed by peer");
                        pool.remove_if_id(id);
                        return;
                    }
                    _ => {}
                },
                _ = ping_interval.tick() => {
                    if !matches!(
                        tokio::time::timeout(
                            Duration::from_secs(5),
                            socket.send(Message::Ping(Vec::new().into())),
                        )
                        .await,
                        Ok(Ok(()))
                    ) {
                        tracing::debug!("Gemini Live standby ping failed");
                        pool.remove_if_id(id);
                        return;
                    }
                },
                _ = tokio::time::sleep_until(expires_at) => {
                    tracing::debug!("Gemini Live standby reached TTL; rotating");
                    close_socket_gracefully(&mut socket).await;
                    break true;
                },
            }
        };

        let still_active =
            should_rotate && Instant::now() < *warm_until.lock().unwrap_or_else(|e| e.into_inner());
        if !still_active {
            tracing::debug!("Gemini Live standby active window elapsed; going cold");
            pool.remove_if_id(id);
            return;
        }
    }
}

async fn claim_or_connect(
    pool: &StandbyPool,
    endpoint: &str,
    secret: &str,
    config: &GeminiSessionConfig,
) -> Result<(Socket, bool), String> {
    let key = StandbyKey::new(endpoint, secret, config);
    if let Some(mut socket) = pool.claim(&key).await {
        match tokio::time::timeout(
            STANDBY_ACTIVITY_START_TIMEOUT,
            socket.send(Message::Text(activity_start_message().to_string().into())),
        )
        .await
        {
            Ok(Ok(())) => return Ok((socket, true)),
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "Pre-warmed socket failed on activityStart; reconnecting fresh");
            }
            Err(_) => {
                tracing::warn!("Pre-warmed socket timed out on activityStart; reconnecting fresh");
            }
        }
    }
    let socket = connect_and_setup(endpoint, secret, config, true).await?;
    Ok((socket, false))
}

pub fn warm_standby(endpoint: &str, secret: &str, config: &GeminiSessionConfig) {
    standby_pool().warm(
        StandbyKey::new(endpoint, secret, config),
        StandbyTimings::default(),
    );
}

pub fn clear_standby() {
    standby_pool().clear();
}

fn start_session_with_pool(
    pool: Arc<StandbyPool>,
    endpoint: &str,
    secret: &str,
    config: &GeminiSessionConfig,
    on_partial: TextCallback,
    on_error: TextCallback,
) -> Result<WsStreamSession, String> {
    if secret.trim().is_empty() {
        return Err("Gemini API key is not set. Configure it in the Vibo menu bar.".into());
    }
    let endpoint = endpoint.to_string();
    let secret = secret.trim().to_string();
    let connect_secret = secret.clone();
    let config = config.clone();

    Ok(WsStreamSession::spawn(
        async move {
            let mode = config.mode;
            let (socket, standby_hit) =
                claim_or_connect(&pool, &endpoint, &connect_secret, &config).await?;
            let credentials = (normalize_endpoint(&endpoint).to_string(), connect_secret);
            Ok(ConnectedSession {
                socket,
                protocol: GeminiProtocol::new(credentials, mode),
                standby_hit,
            })
        },
        secret,
        on_partial,
        on_error,
    ))
}

pub async fn test_gemini_connection(endpoint: &str, secret: &str) -> Result<u64, String> {
    let start = Instant::now();
    let config = GeminiSessionConfig::default();
    let result = connect_and_setup(endpoint, secret, &config, false)
        .await
        .map_err(|error| redact(error, secret));
    VERIFIED_CONNECTION.record(normalize_endpoint(endpoint), secret, result.is_ok());
    if result.is_err() {
        standby_pool().clear_if_matches_credentials(endpoint, secret);
    }
    let mut socket = result?;
    let elapsed = start.elapsed().as_millis() as u64;
    close_socket_gracefully(&mut socket).await;
    Ok(elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    #[allow(clippy::result_large_err)]
    async fn mock_server<F, Fut>(handler: F) -> (String, JoinHandle<()>)
    where
        F: FnOnce(WebSocketStream<TcpStream>, Value) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_hdr_async(stream, |req: &Request, resp: Response| {
                assert_eq!(req.headers()["x-goog-api-key"], "test-key");
                Ok(resp)
            })
            .await
            .unwrap();
            let setup_frame = socket.next().await.unwrap().unwrap();
            let setup_json: Value = serde_json::from_str(setup_frame.to_text().unwrap()).unwrap();
            socket
                .send(Message::Text(
                    json!({"setupComplete": {}}).to_string().into(),
                ))
                .await
                .unwrap();
            handler(socket, setup_json).await;
        });
        (endpoint, task)
    }

    #[test]
    fn setup_message_omits_empty_language_codes_and_sets_smart_mode() {
        let payload = setup_message(&GeminiSessionConfig::default());
        let cfg = &payload["setup"]["inputAudioTranscription"];
        assert_eq!(cfg["mode"], "SMART");
        assert!(cfg.get("languageCodes").is_none());
        assert!(cfg.get("customVocabulary").is_none());

        let with_hints = setup_message(&GeminiSessionConfig {
            mode: TranscriptionMode::Smart,
            language_codes: vec!["en-US".into()],
            custom_vocabulary: vec!["Vibo".into()],
        });
        assert_eq!(
            with_hints["setup"]["inputAudioTranscription"]["languageCodes"],
            json!(["en-US"])
        );
        assert_eq!(
            with_hints["setup"]["inputAudioTranscription"]["customVocabulary"],
            json!(["Vibo"])
        );
    }

    #[tokio::test]
    async fn smart_mode_replaces_cumulative_transcript_and_reports_timings() {
        let (endpoint, server_task) = mock_server(|mut socket, setup_json| async move {
            assert_eq!(
                setup_json["setup"]["model"],
                format!("models/{GEMINI_MODEL_NAME}")
            );
            // Expect activityStart
            let start = socket.next().await.unwrap().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(start.to_text().unwrap()).unwrap(),
                json!({"realtimeInput": {"activityStart": {}}})
            );
            // Read until activityEnd, sending an interim after first audio chunk
            let mut sent_interim = false;
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let val: Value = serde_json::from_str(&text).unwrap();
                if val["realtimeInput"].get("activityEnd").is_some() {
                    break;
                }
                if !sent_interim {
                    sent_interim = true;
                    socket
                        .send(Message::Text(
                            json!({"serverContent": {"interimInputTranscription": {"text": "um hello"}}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
            }
            socket
                .send(Message::Text(
                    json!({"serverContent": {"inputTranscription": {"text": "Hello."}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        })
        .await;

        let partials = Arc::new(Mutex::new(Vec::<String>::new()));
        let partials_clone = Arc::clone(&partials);
        let pool = Arc::new(StandbyPool::new());
        let session = Box::new(
            start_session_with_pool(
                pool,
                &endpoint,
                "test-key",
                &GeminiSessionConfig::default(),
                Arc::new(move |t| partials_clone.lock().unwrap().push(t)),
                Arc::new(|_| {}),
            )
            .unwrap(),
        );

        session.feed_pcm(&vec![0.1_f32; 2000]).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let result = session.finish().await.unwrap();
        server_task.await.unwrap();

        assert_eq!(result.text, "Hello.");
        assert!(!result.timings.standby_hit);
        assert!(result.timings.connect_ms.is_some());
        assert!(result.timings.stop_to_eos_ms.is_some());
        assert!(result.timings.eos_to_final_ms.is_some());
        assert_eq!(partials.lock().unwrap().as_slice(), &["um hello"]);
    }

    #[tokio::test]
    async fn standby_pool_serves_warm_socket_with_standby_hit_true() {
        let (endpoint, server_task) = mock_server(|mut socket, _| async move {
            let start = socket.next().await.unwrap().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(start.to_text().unwrap()).unwrap(),
                json!({"realtimeInput": {"activityStart": {}}})
            );
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let val: Value = serde_json::from_str(&text).unwrap();
                if val["realtimeInput"].get("activityEnd").is_some() {
                    break;
                }
            }
            socket
                .send(Message::Text(
                    json!({"serverContent": {"inputTranscription": {"text": "Warm hit!"}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        })
        .await;

        let pool = Arc::new(StandbyPool::new());
        let cfg = GeminiSessionConfig::default();
        pool.warm(
            StandbyKey::new(&endpoint, "test-key", &cfg),
            StandbyTimings::default(),
        );
        tokio::time::sleep(Duration::from_millis(80)).await;

        let session = Box::new(
            start_session_with_pool(
                Arc::clone(&pool),
                &endpoint,
                "test-key",
                &cfg,
                Arc::new(|_| {}),
                Arc::new(|_| {}),
            )
            .unwrap(),
        );
        session.feed_pcm(&vec![0.05_f32; 1600]).unwrap();
        let result = session.finish().await.unwrap();
        server_task.await.unwrap();

        assert_eq!(result.text, "Warm hit!");
        assert!(result.timings.standby_hit);
    }
}
