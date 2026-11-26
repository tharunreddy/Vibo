//! State-of-the-art Rust observability (`tracing` + `tracing-subscriber` + `tracing-appender`)
//! and structured per-dictation latency telemetry (`~/Library/Logs/LIVI/transcriptions.jsonl`).

use crate::asr::AsrTimings;
use crate::settings;
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

pub const JSONL_FILE_NAME: &str = "transcriptions.jsonl";
const LOG_FILE_PREFIX: &str = "livi.log";
const MAX_ROTATED_LOG_FILES: usize = 7;

static NEXT_RUN_SEQ: AtomicU64 = AtomicU64::new(1);
static LAST_LATENCY: Mutex<Option<LastLatencySummary>> = Mutex::new(None);
static WORD_STATS: Mutex<WordStatsCache> = Mutex::new(WordStatsCache::new());

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WordStats {
    pub today: u64,
    pub total: u64,
}

impl WordStats {
    pub fn menu_label(&self) -> String {
        format!(
            "Words: {} today · {} total",
            format_count(self.today),
            format_count(self.total)
        )
    }
}

struct WordStatsCache {
    loaded: bool,
    today_date: Option<Date>,
    stats: WordStats,
}

impl WordStatsCache {
    const fn new() -> Self {
        Self {
            loaded: false,
            today_date: None,
            stats: WordStats { today: 0, total: 0 },
        }
    }

    fn ensure_current(&mut self, path: &Path, today: Date, tz: &TimeZone) {
        if !self.loaded || self.today_date != Some(today) {
            self.stats = load_word_stats_from_jsonl(path, today, tz);
            self.today_date = Some(today);
            self.loaded = true;
        }
    }

    fn add_record(&mut self, path: &Path, timestamp: &str, words: u64, today: Date, tz: &TimeZone) {
        if words == 0 {
            return;
        }
        self.ensure_current(path, today, tz);
        self.stats.total = self.stats.total.saturating_add(words);
        let record_date = timestamp
            .parse::<Timestamp>()
            .map(|ts| tz.to_datetime(ts).date())
            .unwrap_or(today);
        if record_date == today {
            self.stats.today = self.stats.today.saturating_add(words);
        }
    }
}

#[derive(Deserialize)]
struct JsonlWordRow {
    timestamp: String,
    #[serde(default)]
    transcript_words: u64,
}

fn current_local_tz_and_date() -> (TimeZone, Date) {
    let tz = TimeZone::system();
    let today = tz.to_datetime(Timestamp::now()).date();
    (tz, today)
}

/// Scans `transcriptions.jsonl` to compute words transcribed on `today` (in `tz`) and overall.
pub fn load_word_stats_from_jsonl(path: &Path, today: Date, tz: &TimeZone) -> WordStats {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return WordStats::default(),
        Err(err) => {
            tracing::warn!(error = %err, path = %path.display(), "Failed to read transcriptions.jsonl");
            return WordStats::default();
        }
    };

    let mut stats = WordStats::default();
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<JsonlWordRow>(trimmed) else {
            continue;
        };
        if row.transcript_words == 0 {
            continue;
        }
        stats.total = stats.total.saturating_add(row.transcript_words);
        if let Ok(ts) = row.timestamp.parse::<Timestamp>()
            && tz.to_datetime(ts).date() == today
        {
            stats.today = stats.today.saturating_add(row.transcript_words);
        }
    }
    stats
}

/// Returns today's (local calendar day) and all-time transcribed word counts.
///
/// Backed by an in-memory cache seeded from `transcriptions.jsonl`, automatically
/// rolling over when the local calendar date changes.
pub fn word_stats() -> WordStats {
    let (tz, today) = current_local_tz_and_date();
    let path = jsonl_path();
    let mut cache = WORD_STATS.lock().unwrap_or_else(|e| e.into_inner());
    cache.ensure_current(&path, today, &tz);
    cache.stats
}

fn record_word_count(timestamp: &str, words: usize) {
    if words == 0 {
        return;
    }
    let (tz, today) = current_local_tz_and_date();
    let path = jsonl_path();
    let mut cache = WORD_STATS.lock().unwrap_or_else(|e| e.into_inner());
    cache.add_record(&path, timestamp, words as u64, today, &tz);
}

pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + (len.saturating_sub(1) / 3));
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[derive(Debug, Clone)]
pub struct LastLatencySummary {
    pub stop_to_paste_ms: f64,
    pub eos_to_final_ms: Option<f64>,
    pub standby_hit: bool,
    pub outcome: DictationOutcome,
}

impl LastLatencySummary {
    pub fn menu_label(&self) -> String {
        match self.outcome {
            DictationOutcome::Success => {
                let standby = if self.standby_hit { "warm" } else { "cold" };
                if let Some(server_ms) = self.eos_to_final_ms {
                    format!(
                        "Last: {:.0} ms (server {:.0} ms, {standby})",
                        self.stop_to_paste_ms, server_ms
                    )
                } else {
                    format!("Last: {:.0} ms ({standby})", self.stop_to_paste_ms)
                }
            }
            DictationOutcome::Empty => "Last: empty".to_string(),
            DictationOutcome::Silent => "Last: mic silent".to_string(),
            DictationOutcome::Cancelled => "Last: cancelled".to_string(),
            DictationOutcome::Failed => "Last: failed".to_string(),
        }
    }
}

pub fn last_latency_summary() -> Option<LastLatencySummary> {
    LAST_LATENCY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub fn record_last_latency(summary: LastLatencySummary) {
    *LAST_LATENCY.lock().unwrap_or_else(|e| e.into_inner()) = Some(summary);
}

pub fn next_run_seq() -> u64 {
    NEXT_RUN_SEQ.fetch_add(1, Ordering::Relaxed)
}

pub fn ms(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1_000.0 * 10.0).round() / 10.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DictationOutcome {
    Success,
    Empty,
    Silent,
    Failed,
    Cancelled,
}

impl DictationOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Empty => "empty",
            Self::Silent => "silent",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Structured per-dictation latency record appended to `~/Library/Logs/LIVI/transcriptions.jsonl`.
#[derive(Debug, Clone, Serialize)]
pub struct DictationRecord {
    pub schema_version: u8,
    pub run_id: String,
    pub timestamp: String,
    pub app_version: &'static str,
    pub provider_id: String,
    pub model: String,
    pub mode: String,
    pub outcome: DictationOutcome,
    pub standby_hit: bool,
    pub audio_duration_ms: f64,
    pub mic_ready_ms: Option<f64>,
    pub connect_ms: Option<f64>,
    pub first_partial_ms: Option<f64>,
    pub stop_to_eos_ms: Option<f64>,
    pub eos_to_final_ms: Option<f64>,
    pub stop_to_final_ms: Option<f64>,
    pub paste_ms: Option<f64>,
    pub stop_to_paste_ms: Option<f64>,
    pub transcript_chars: usize,
    pub transcript_words: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DictationRecord {
    pub fn new(seq: u64, provider_id: &str, model: &str, mode: &str) -> Self {
        let now = Timestamp::now();
        let ts = now.to_string();
        Self {
            schema_version: 1,
            run_id: format!("{ts}-{}-{seq}", std::process::id()),
            timestamp: ts,
            app_version: env!("CARGO_PKG_VERSION"),
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            mode: mode.to_string(),
            outcome: DictationOutcome::Failed,
            standby_hit: false,
            audio_duration_ms: 0.0,
            mic_ready_ms: None,
            connect_ms: None,
            first_partial_ms: None,
            stop_to_eos_ms: None,
            eos_to_final_ms: None,
            stop_to_final_ms: None,
            paste_ms: None,
            stop_to_paste_ms: None,
            transcript_chars: 0,
            transcript_words: 0,
            transcript: None,
            error: None,
        }
    }

    pub fn apply_asr_timings(&mut self, timings: &AsrTimings) {
        self.standby_hit = timings.standby_hit;
        self.connect_ms = timings.connect_ms;
        self.first_partial_ms = timings.first_partial_ms;
        self.stop_to_eos_ms = timings.stop_to_eos_ms;
        self.eos_to_final_ms = timings.eos_to_final_ms;
        if let (Some(a), Some(b)) = (timings.stop_to_eos_ms, timings.eos_to_final_ms) {
            self.stop_to_final_ms = Some(((a + b) * 10.0).round() / 10.0);
        }
    }

    pub fn set_transcript(&mut self, text: &str, include_text: bool) {
        self.transcript_chars = text.chars().count();
        self.transcript_words = text.split_whitespace().count();
        self.transcript = include_text.then(|| text.to_string());
    }

    /// Emits a structured `tracing` summary (never containing transcript text) and appends
    /// the full record to `transcriptions.jsonl` without blocking the caller.
    pub fn log_and_persist(self) {
        tracing::info!(
            run_id = %self.run_id,
            provider = %self.provider_id,
            model = %self.model,
            mode = %self.mode,
            outcome = self.outcome.as_str(),
            standby_hit = self.standby_hit,
            audio_ms = self.audio_duration_ms,
            mic_ready_ms = ?self.mic_ready_ms,
            connect_ms = ?self.connect_ms,
            first_partial_ms = ?self.first_partial_ms,
            stop_to_eos_ms = ?self.stop_to_eos_ms,
            eos_to_final_ms = ?self.eos_to_final_ms,
            stop_to_final_ms = ?self.stop_to_final_ms,
            paste_ms = ?self.paste_ms,
            stop_to_paste_ms = ?self.stop_to_paste_ms,
            chars = self.transcript_chars,
            words = self.transcript_words,
            error = ?self.error,
            "dictation completed"
        );

        let summary = LastLatencySummary {
            stop_to_paste_ms: self
                .stop_to_paste_ms
                .or(self.stop_to_final_ms)
                .unwrap_or(0.0),
            eos_to_final_ms: self.eos_to_final_ms,
            standby_hit: self.standby_hit,
            outcome: self.outcome,
        };
        record_last_latency(summary);
        record_word_count(&self.timestamp, self.transcript_words);

        let path = jsonl_path();
        std::thread::spawn(move || {
            if let Err(err) = append_jsonl(&path, &self) {
                tracing::warn!(error = %err, "Failed to append transcription latency record");
            }
        });
    }
}

pub fn jsonl_path() -> PathBuf {
    settings::logs_dir().join(JSONL_FILE_NAME)
}

pub fn append_jsonl(path: &Path, record: &DictationRecord) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line =
        serde_json::to_vec(record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');

    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    file.lock()?;
    file.write_all(&line)?;
    file.flush()
}

/// Initializes the global `tracing` subscriber with non-blocking daily-rolling log files
/// under `~/Library/Logs/LIVI/` (7-day retention) plus stderr when attached to a TTY.
///
/// The returned `WorkerGuard` MUST be kept alive for the lifetime of the process so
/// buffered log lines flush cleanly on exit.
pub fn init_logging(default_level: &str) -> Option<WorkerGuard> {
    let log_dir = settings::logs_dir();
    let _ = fs::create_dir_all(&log_dir);

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{default_level},tungstenite=warn")));

    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .max_log_files(MAX_ROTATED_LOG_FILES)
        .build(&log_dir)
        .ok();

    let (file_layer, guard) = match file_appender {
        Some(appender) => {
            let (non_blocking, guard) = tracing_appender::non_blocking(appender);
            let layer = fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_writer(non_blocking);
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };

    let stderr_layer = io::stderr().is_terminal().then(|| {
        fmt::layer()
            .compact()
            .with_target(true)
            .with_writer(io::stderr)
    });

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init();

    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_valid_jsonl_with_latency_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(JSONL_FILE_NAME);

        let mut rec = DictationRecord::new(1, "gemini-live", "gemini-3.5-transcribe-live", "smart");
        rec.outcome = DictationOutcome::Success;
        rec.audio_duration_ms = 3200.0;
        rec.mic_ready_ms = Some(15.2);
        rec.apply_asr_timings(&AsrTimings {
            standby_hit: true,
            connect_ms: Some(1.8),
            first_partial_ms: Some(290.0),
            stop_to_eos_ms: Some(12.0),
            eos_to_final_ms: Some(210.5),
        });
        rec.paste_ms = Some(28.0);
        rec.stop_to_paste_ms = Some(250.5);
        rec.set_transcript("Hello world.", true);

        append_jsonl(&path, &rec).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(parsed["outcome"], "success");
        assert_eq!(parsed["standby_hit"], true);
        assert_eq!(parsed["stop_to_final_ms"], 222.5);
        assert_eq!(parsed["stop_to_paste_ms"], 250.5);
        assert_eq!(parsed["transcript_words"], 2);
        assert_eq!(parsed["transcript"], "Hello world.");
    }

    #[test]
    fn omits_transcript_when_disabled() {
        let mut rec = DictationRecord::new(2, "gemini-live", "gemini-3.5-transcribe-live", "smart");
        rec.set_transcript("Secret note", false);
        let json = serde_json::to_value(&rec).unwrap();
        assert!(json.get("transcript").is_none());
        assert_eq!(json["transcript_words"], 2);
    }

    #[test]
    fn formats_word_counts_with_commas_and_menu_label() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(7), "7");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(12_345), "12,345");
        assert_eq!(format_count(1_234_567), "1,234,567");

        let stats = WordStats {
            today: 42,
            total: 12_345,
        };
        assert_eq!(stats.menu_label(), "Words: 42 today · 12,345 total");
    }

    #[test]
    fn loads_today_and_total_words_using_local_timezone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(JSONL_FILE_NAME);

        // Fixed UTC-8 offset (e.g. PST) so we verify UTC timestamps crossing local midnight.
        let tz = TimeZone::fixed(jiff::tz::offset(-8));
        let today = jiff::civil::date(2026, 11, 26);

        // 2026-11-26T07:59:00Z is 2026-11-25 23:59 PST -> yesterday!
        // 2026-11-26T08:05:00Z is 2026-11-26 00:05 PST -> today!
        // 2026-11-27T04:00:00Z is 2026-11-26 20:00 PST -> still today!
        let lines = [
            r#"{"timestamp":"2026-11-26T07:59:00Z","outcome":"success","transcript_words":100}"#,
            r#"{"timestamp":"2026-11-26T08:05:00Z","outcome":"success","transcript_words":25}"#,
            r#"{"timestamp":"2026-11-27T04:00:00Z","outcome":"success","transcript_words":15}"#,
            r#"{"timestamp":"2026-11-26T18:00:00Z","outcome":"empty","transcript_words":0}"#,
            "not valid json line",
            "",
        ];
        fs::write(&path, lines.join("\n") + "\n").unwrap();

        let stats = load_word_stats_from_jsonl(&path, today, &tz);
        assert_eq!(
            stats,
            WordStats {
                today: 40,
                total: 140
            }
        );

        // Missing file safely returns zeros.
        let missing = load_word_stats_from_jsonl(&dir.path().join("missing.jsonl"), today, &tz);
        assert_eq!(missing, WordStats::default());
    }

    #[test]
    fn word_stats_cache_increments_and_rolls_over_at_midnight() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(JSONL_FILE_NAME);
        let tz = TimeZone::fixed(jiff::tz::offset(-8));
        let day1 = jiff::civil::date(2026, 11, 26);
        let day2 = jiff::civil::date(2026, 11, 27);

        let initial = r#"{"timestamp":"2026-11-26T19:00:00Z","outcome":"success","transcript_words":10}"#;
        fs::write(&path, format!("{initial}\n")).unwrap();

        let mut cache = WordStatsCache::new();
        cache.ensure_current(&path, day1, &tz);
        assert_eq!(cache.stats, WordStats { today: 10, total: 10 });

        // Live dictation arrives on day1.
        cache.add_record(&path, "2026-11-26T20:00:00Z", 7, day1, &tz);
        assert_eq!(cache.stats, WordStats { today: 17, total: 17 });

        // Persist the second record to disk as log_and_persist would.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            f,
            r#"{{"timestamp":"2026-11-26T20:00:00Z","outcome":"success","transcript_words":7}}"#
        )
        .unwrap();

        // After midnight rollover to day2, today resets to 0 while total stays 17.
        cache.ensure_current(&path, day2, &tz);
        assert_eq!(cache.stats, WordStats { today: 0, total: 17 });
    }
}
