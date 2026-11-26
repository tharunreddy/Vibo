//! Central dictation state machine coordinating hotkeys, audio capture, pluggable ASR,
//! reliable cursor paste, native overlay, menu bar tray, and latency telemetry.

use crate::asr::gemini_live::GeminiLiveProvider;
use crate::asr::{AsrProvider, AsrSession};
use crate::audio::capture::{ActiveCapture, TARGET_SAMPLE_RATE};
use crate::audio::sound::{self, ChimeKind};
use crate::hotkeys::{self, HotkeyAction, HotkeyService};
use crate::macos::{autostart, overlay, permissions, tray};
use crate::paste;
use crate::secure_input;
use crate::settings::{self, Settings, ShortcutActivation};
use crate::telemetry::{self, DictationOutcome, DictationRecord};
use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct ActiveSession {
    run_seq: u64,
    started_at: Instant,
    capture: ActiveCapture,
    asr: Arc<Mutex<Option<Box<dyn AsrSession>>>>,
    provider_id: &'static str,
    model: String,
    mode: &'static str,
}

#[derive(Default)]
struct State {
    recording: Option<ActiveSession>,
    polishing: bool,
    press_started_at: Option<Instant>,
    toggled_on: bool,
    last_transcript: Option<String>,
    secure_input_active: bool,
}

pub struct AppController {
    rt: tokio::runtime::Handle,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    hotkeys: Mutex<Option<HotkeyService>>,
}

impl AppController {
    pub fn new(rt: tokio::runtime::Handle, initial_settings: Settings) -> Arc<Self> {
        Arc::new(Self {
            rt,
            settings: Mutex::new(initial_settings),
            state: Mutex::new(State::default()),
            hotkeys: Mutex::new(None),
        })
    }

    /// Initializes hotkeys, autostart, StandbyPool warmup, and initial tray snapshot.
    pub fn bootstrap(self: &Arc<Self>) {
        let s = self.settings_snapshot();
        autostart::set_launch_at_login(s.launch_at_login);

        self.try_init_hotkeys();
        self.warm_asr();
        self.refresh_tray();
        self.start_background_monitors();
    }

    pub fn settings_snapshot(&self) -> Settings {
        self.settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn update_settings(&self, mutate: impl FnOnce(&mut Settings)) -> Settings {
        let updated = {
            let mut guard = self.settings.lock().unwrap_or_else(|e| e.into_inner());
            mutate(&mut guard);
            guard.clone()
        };
        if let Err(err) = updated.save() {
            tracing::error!(error = %err, "Failed to persist settings.json");
        }
        updated
    }

    fn build_provider(&self, settings: &Settings) -> Arc<dyn AsrProvider> {
        Arc::new(GeminiLiveProvider::from_settings(settings))
    }

    fn warm_asr(&self) {
        let s = self.settings_snapshot();
        let provider = self.build_provider(&s);
        let has_key = !s.effective_api_key().is_empty();
        self.rt.spawn(async move {
            if has_key {
                provider.warm();
            } else {
                provider.clear_warm();
            }
        });
    }

    fn try_init_hotkeys(self: &Arc<Self>) {
        if self
            .hotkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            return;
        }
        if !permissions::check_accessibility() {
            tracing::warn!("Accessibility permission not granted yet; deferring hotkey listener");
            return;
        }
        let binding = self.settings_snapshot().shortcut;
        let weak = Arc::downgrade(self);
        let on_action = Arc::new(move |action: HotkeyAction| {
            if let Some(ctrl) = weak.upgrade() {
                ctrl.handle_hotkey(action);
            }
        });
        match HotkeyService::start(&binding, on_action) {
            Ok(service) => {
                *self.hotkeys.lock().unwrap_or_else(|e| e.into_inner()) = Some(service);
            }
            Err(err) => {
                tracing::error!(error = %err, "Failed to start HotkeyService");
            }
        }
    }

    fn start_background_monitors(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.rt.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            loop {
                interval.tick().await;
                let Some(ctrl) = weak.upgrade() else { break };

                // Auto-start hotkeys the moment the user grants Accessibility.
                if ctrl
                    .hotkeys
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
                    && permissions::check_accessibility()
                {
                    ctrl.try_init_hotkeys();
                    ctrl.refresh_tray();
                }

                // Monitor Secure Input for keyed shortcuts.
                let sec = secure_input::is_secure_input_enabled();
                let changed = {
                    let mut st = ctrl.state.lock().unwrap_or_else(|e| e.into_inner());
                    if st.secure_input_active != sec {
                        st.secure_input_active = sec;
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    ctrl.refresh_tray();
                }
            }
        });
    }

    pub fn refresh_tray(&self) {
        let s = self.settings_snapshot();
        let perms = permissions::snapshot();
        let (icon_state, has_last, sec_active) = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let icon_state = if st.recording.is_some() {
                tray::TrayIconState::Listening
            } else if st.polishing {
                tray::TrayIconState::Polishing
            } else {
                tray::TrayIconState::Idle
            };
            (
                icon_state,
                st.last_transcript.is_some(),
                st.secure_input_active,
            )
        };
        let secure_input_warning = sec_active && !hotkeys::is_modifier_only_shortcut(&s.shortcut);
        let api_key_missing = s.effective_api_key().is_empty();

        tray::update(tray::TraySnapshot {
            icon_state,
            settings: s,
            has_last_transcript: has_last,
            permissions_ok: perms.all_granted(),
            secure_input_warning,
            api_key_missing,
        });
    }

    pub fn handle_hotkey(self: &Arc<Self>, action: HotkeyAction) {
        match action {
            HotkeyAction::CancelPressed => self.cancel_current(),
            HotkeyAction::DictatePressed => {
                let s = self.settings_snapshot();
                let mut should_start = false;
                let mut should_stop = false;
                {
                    let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    if st.polishing {
                        return;
                    }
                    match s.shortcut_activation {
                        ShortcutActivation::PushToTalk => {
                            if st.recording.is_none() {
                                st.press_started_at = Some(Instant::now());
                                should_start = true;
                            }
                        }
                        ShortcutActivation::Toggle => {
                            if st.recording.is_some() {
                                should_stop = true;
                            } else {
                                st.toggled_on = true;
                                should_start = true;
                            }
                        }
                        ShortcutActivation::HoldOrToggle => {
                            if st.recording.is_some() && st.toggled_on {
                                st.toggled_on = false;
                                should_stop = true;
                            } else if st.recording.is_none() {
                                st.press_started_at = Some(Instant::now());
                                st.toggled_on = false;
                                should_start = true;
                            }
                        }
                    }
                }
                if should_start {
                    self.start_dictation();
                } else if should_stop {
                    self.stop_and_transcribe();
                }
            }
            HotkeyAction::DictateReleased => {
                let s = self.settings_snapshot();
                let threshold = Duration::from_millis(s.hold_threshold_ms);
                let mut should_stop = false;
                {
                    let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    if st.recording.is_none() {
                        st.press_started_at = None;
                        return;
                    }
                    match s.shortcut_activation {
                        ShortcutActivation::PushToTalk => {
                            st.press_started_at = None;
                            should_stop = true;
                        }
                        ShortcutActivation::Toggle => {}
                        ShortcutActivation::HoldOrToggle => {
                            if let Some(pressed_at) = st.press_started_at.take() {
                                if pressed_at.elapsed() >= threshold {
                                    should_stop = true;
                                } else {
                                    // Short tap: stay recording until next press!
                                    st.toggled_on = true;
                                }
                            }
                        }
                    }
                }
                if should_stop {
                    self.stop_and_transcribe();
                }
            }
        }
    }

    fn start_dictation(self: &Arc<Self>) {
        let s = self.settings_snapshot();
        if s.effective_api_key().is_empty() {
            overlay::show_notice(
                "Set Gemini API Key in Vibo menu".into(),
                overlay::NoticeKind::Warning,
                s.overlay_position,
            );
            return;
        }
        if !permissions::check_microphone() {
            overlay::show_notice(
                "Microphone permission required".into(),
                overlay::NoticeKind::Error,
                s.overlay_position,
            );
            permissions::open_microphone_settings();
            return;
        }

        let run_seq = telemetry::next_run_seq();
        let started_at = Instant::now();
        let provider = self.build_provider(&s);
        let provider_id = provider.id();
        let model = provider.model().to_string();
        let mode = provider.mode_label();

        let _span =
            tracing::info_span!("dictation", run_id = run_seq, provider = provider_id, mode)
                .entered();

        let err_position = s.overlay_position;
        let weak_for_err = Arc::downgrade(self);
        let on_partial = Arc::new(|_text: String| {});
        let on_error = Arc::new(move |err: String| {
            tracing::error!(error = %err, "ASR session failed during recording");
            overlay::show_notice(
                short_error_label(&err),
                overlay::NoticeKind::Error,
                err_position,
            );
            if let Some(ctrl) = weak_for_err.upgrade() {
                ctrl.abort_failed_recording(run_seq);
            }
        });

        let session = {
            let _enter = self.rt.enter();
            match provider.start_session(on_partial, on_error) {
                Ok(sess) => sess,
                Err(err) => {
                    tracing::error!(error = %err, "Failed to start ASR session");
                    overlay::show_notice(
                        short_error_label(&err),
                        overlay::NoticeKind::Error,
                        s.overlay_position,
                    );
                    return;
                }
            }
        };

        let asr_slot = Arc::new(Mutex::new(Some(session)));
        let asr_for_audio = Arc::clone(&asr_slot);
        let on_audio = Arc::new(move |frame: &[f32]| {
            if let Ok(guard) = asr_for_audio.lock()
                && let Some(sess) = guard.as_ref()
            {
                let _ = sess.feed_pcm(frame);
            }
        });
        let on_levels = Arc::new(|levels: Vec<f32>| {
            overlay::update_levels(levels);
        });
        let sound_enabled = s.sound_feedback;
        let sound_theme = s.sound_theme;
        let sound_vol = s.sound_volume;
        let on_ready = Arc::new(move |_elapsed: Duration| {
            overlay::mark_capture_ready();
            sound::play_chime(sound_enabled, sound_theme, sound_vol, ChimeKind::Start);
        });

        let capture = match ActiveCapture::start(
            s.microphone.as_deref(),
            started_at,
            on_audio,
            on_levels,
            on_ready,
        ) {
            Ok(cap) => cap,
            Err(err) => {
                tracing::error!(error = %err, "Failed to start microphone capture");
                if let Some(sess) = asr_slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    sess.cancel();
                }
                overlay::show_notice(
                    "Microphone unavailable".into(),
                    overlay::NoticeKind::Error,
                    s.overlay_position,
                );
                return;
            }
        };

        let mic_name = capture.device_name.clone();
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.recording = Some(ActiveSession {
                run_seq,
                started_at,
                capture,
                asr: asr_slot,
                provider_id,
                model,
                mode,
            });
        }

        if let Some(hk) = self
            .hotkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            hk.set_cancel_enabled(true);
        }
        overlay::show_listening(mic_name, s.overlay_position);
        self.refresh_tray();
    }

    fn stop_and_transcribe(self: &Arc<Self>) {
        let stop_requested_at = Instant::now();
        let active = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(active) = st.recording.take() else {
                return;
            };
            st.polishing = true;
            st.toggled_on = false;
            st.press_started_at = None;
            active
        };

        let s = self.settings_snapshot();
        sound::play_chime(
            s.sound_feedback,
            s.sound_theme,
            s.sound_volume,
            ChimeKind::Stop,
        );
        overlay::show_polishing();
        self.refresh_tray();

        let ctrl = Arc::clone(self);
        self.rt.spawn(async move {
            let span = tracing::info_span!(
                "dictation",
                run_id = active.run_seq,
                provider = active.provider_id,
                mode = active.mode
            );
            let _enter = span.enter();

            let mut record = DictationRecord::new(
                active.run_seq,
                active.provider_id,
                &active.model,
                active.mode,
            );

            // Stop capture on a blocking thread so CoreAudio teardown never stalls Tokio.
            let capture = active.capture;
            let cap_summary = tokio::task::spawn_blocking(move || capture.stop())
                .await
                .unwrap_or(crate::audio::capture::CaptureSummary {
                    samples_16k: 0,
                    input_silent: false,
                    mic_ready_ms: None,
                });

            record.audio_duration_ms =
                ((cap_summary.samples_16k as f64 / TARGET_SAMPLE_RATE as f64) * 1_000.0 * 10.0)
                    .round()
                    / 10.0;
            record.mic_ready_ms = cap_summary.mic_ready_ms;

            let session = active.asr.lock().unwrap_or_else(|e| e.into_inner()).take();

            if cap_summary.input_silent || cap_summary.samples_16k == 0 {
                if let Some(sess) = session {
                    sess.cancel();
                }
                if cap_summary.input_silent {
                    record.outcome = DictationOutcome::Silent;
                    overlay::show_notice(
                        "Microphone is silent".into(),
                        overlay::NoticeKind::Warning,
                        s.overlay_position,
                    );
                } else {
                    record.outcome = DictationOutcome::Empty;
                    overlay::hide();
                }
                ctrl.finish_polishing_state();
                record.log_and_persist();
                ctrl.warm_asr();
                ctrl.refresh_tray();
                return;
            }

            let Some(sess) = session else {
                record.outcome = DictationOutcome::Cancelled;
                overlay::hide();
                ctrl.finish_polishing_state();
                record.log_and_persist();
                ctrl.refresh_tray();
                return;
            };

            let asr_outcome = sess.finish().await;
            // Check if the user cancelled while polishing.
            let still_polishing = ctrl
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .polishing;
            if !still_polishing {
                record.outcome = DictationOutcome::Cancelled;
                record.log_and_persist();
                ctrl.warm_asr();
                ctrl.refresh_tray();
                return;
            }

            match asr_outcome {
                Ok(res) => {
                    record.apply_asr_timings(&res.timings);
                    let final_text = res.text.trim().to_string();
                    if final_text.is_empty() {
                        record.outcome = DictationOutcome::Empty;
                        overlay::hide();
                    } else {
                        record.set_transcript(&final_text, s.log_transcript_text);
                        {
                            let mut st = ctrl.state.lock().unwrap_or_else(|e| e.into_inner());
                            st.last_transcript = Some(final_text.clone());
                        }
                        let paste_start = Instant::now();
                        let mut paste_text = final_text;
                        if s.append_trailing_space {
                            paste_text.push(' ');
                        }
                        let preserve = s.preserve_clipboard;
                        let paste_res = tokio::task::spawn_blocking(move || {
                            paste::paste_at_cursor(&paste_text, preserve)
                        })
                        .await
                        .unwrap_or_else(|e| Err(format!("Paste task failed: {e}")));

                        let paste_ms = telemetry::ms(paste_start.elapsed());
                        let total_ms = telemetry::ms(stop_requested_at.elapsed());
                        record.paste_ms = Some(paste_ms);
                        record.stop_to_paste_ms = Some(total_ms);

                        match paste_res {
                            Ok(()) => {
                                record.outcome = DictationOutcome::Success;
                                overlay::hide();
                            }
                            Err(err) => {
                                record.outcome = DictationOutcome::Failed;
                                record.error = Some(err.clone());
                                tracing::error!(error = %err, "Failed to paste transcript");
                                overlay::show_notice(
                                    "Paste failed (check Accessibility)".into(),
                                    overlay::NoticeKind::Error,
                                    s.overlay_position,
                                );
                            }
                        }
                    }
                }
                Err(err) => {
                    record.outcome = DictationOutcome::Failed;
                    record.error = Some(err.clone());
                    tracing::error!(
                        elapsed_ms = active.started_at.elapsed().as_millis() as u64,
                        error = %err,
                        "Transcription failed"
                    );
                    overlay::show_notice(
                        short_error_label(&err),
                        overlay::NoticeKind::Error,
                        s.overlay_position,
                    );
                }
            }

            ctrl.finish_polishing_state();
            record.log_and_persist();
            // Pre-warm next standby socket right after finishing so next dictation is instant.
            ctrl.warm_asr();
            ctrl.refresh_tray();
        });
    }

    fn finish_polishing_state(&self) {
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.polishing = false;
        }
        if let Some(hk) = self
            .hotkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            hk.set_cancel_enabled(false);
        }
    }

    fn abort_failed_recording(&self, run_seq: u64) {
        let active = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.recording.as_ref().is_some_and(|r| r.run_seq == run_seq) {
                st.toggled_on = false;
                st.press_started_at = None;
                st.recording.take()
            } else {
                None
            }
        };
        if let Some(active) = active {
            if let Some(sess) = active.asr.lock().unwrap_or_else(|e| e.into_inner()).take() {
                sess.cancel();
            }
            drop(active.capture);
            if let Some(hk) = self
                .hotkeys
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                hk.set_cancel_enabled(false);
            }
            self.refresh_tray();
        }
    }

    pub fn cancel_current(&self) {
        let (active, was_polishing) = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let was_polishing = st.polishing;
            st.polishing = false;
            st.toggled_on = false;
            st.press_started_at = None;
            (st.recording.take(), was_polishing)
        };
        if let Some(active) = active {
            if let Some(sess) = active.asr.lock().unwrap_or_else(|e| e.into_inner()).take() {
                sess.cancel();
            }
            drop(active.capture);
            tracing::info!(run_id = active.run_seq, "Cancelled active recording");
        } else if was_polishing {
            tracing::info!("Cancelled polishing wait");
        } else {
            return;
        }
        if let Some(hk) = self
            .hotkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            hk.set_cancel_enabled(false);
        }
        overlay::hide();
        self.refresh_tray();
    }

    pub fn handle_tray_action(self: &Arc<Self>, action: tray::TrayAction) {
        match action {
            tray::TrayAction::Cancel => self.cancel_current(),
            tray::TrayAction::CopyLastTranscript => {
                let text = self
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_transcript
                    .clone();
                if let Some(text) = text {
                    paste::copy_to_clipboard(&text);
                }
            }
            tray::TrayAction::ToggleSmartMode => {
                self.update_settings(|s| s.smart_mode = !s.smart_mode);
                self.warm_asr();
                self.refresh_tray();
            }
            tray::TrayAction::OpenCustomVocabularyDialog => {
                let current = self.settings_snapshot().custom_vocabulary;
                let ctrl = Arc::clone(self);
                tray::prompt_custom_vocabulary_on_main(current, move |words| {
                    let normalized = settings::normalize_custom_vocabulary(
                        &words,
                        crate::asr::gemini_live::GEMINI_MAX_CUSTOM_VOCABULARY,
                    );
                    let count = normalized.len();
                    let s = ctrl.update_settings(|s| s.custom_vocabulary = normalized);
                    ctrl.warm_asr();
                    ctrl.refresh_tray();
                    let notice = match count {
                        0 => "Cleared custom vocabulary".to_string(),
                        1 => "Saved 1 custom word".to_string(),
                        n => format!("Saved {n} custom words"),
                    };
                    overlay::show_notice(notice, overlay::NoticeKind::Warning, s.overlay_position);
                });
            }
            tray::TrayAction::SelectMicrophone(mic) => {
                self.update_settings(|s| s.microphone = mic);
                self.refresh_tray();
            }
            tray::TrayAction::SetShortcut(binding) => {
                self.apply_new_shortcut(binding);
            }
            tray::TrayAction::RecordShortcutInteractive => {
                let current = self.settings_snapshot().shortcut;
                if let Some(hk) = self
                    .hotkeys
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    hk.set_suspended(true);
                }
                let ctrl = Arc::clone(self);
                tray::prompt_record_shortcut_on_main(current, move |captured| {
                    if let Some(hk) = ctrl
                        .hotkeys
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                    {
                        hk.set_suspended(false);
                    }
                    if let Some(binding) = captured {
                        ctrl.apply_new_shortcut(binding);
                    }
                });
            }
            tray::TrayAction::SetActivation(mode) => {
                self.update_settings(|s| s.shortcut_activation = mode);
                self.refresh_tray();
            }
            tray::TrayAction::SetOverlayPosition(pos) => {
                self.update_settings(|s| s.overlay_position = pos);
                self.refresh_tray();
            }
            tray::TrayAction::ToggleSoundFeedback => {
                self.update_settings(|s| s.sound_feedback = !s.sound_feedback);
                self.refresh_tray();
            }
            tray::TrayAction::SetSoundTheme(theme) => {
                let s = self.update_settings(|s| s.sound_theme = theme);
                sound::play_chime(
                    s.sound_feedback,
                    s.sound_theme,
                    s.sound_volume,
                    ChimeKind::Start,
                );
                self.refresh_tray();
            }
            tray::TrayAction::ToggleLaunchAtLogin => {
                let s = self.update_settings(|s| s.launch_at_login = !s.launch_at_login);
                autostart::set_launch_at_login(s.launch_at_login);
                self.refresh_tray();
            }
            tray::TrayAction::ToggleTrailingSpace => {
                self.update_settings(|s| s.append_trailing_space = !s.append_trailing_space);
                self.refresh_tray();
            }
            tray::TrayAction::OpenApiKeyDialog => {
                let current = self.settings_snapshot().gemini_api_key;
                let ctrl = Arc::clone(self);
                tray::prompt_api_key_on_main(current, move |new_key| {
                    let s = ctrl.update_settings(|s| s.gemini_api_key = new_key);
                    ctrl.refresh_tray();
                    if s.effective_api_key().is_empty() {
                        ctrl.warm_asr();
                        return;
                    }
                    let provider = ctrl.build_provider(&s);
                    let pos = s.overlay_position;
                    let ctrl_test = Arc::clone(&ctrl);
                    ctrl.rt.spawn(async move {
                        match provider.test_connection().await {
                            Ok(rtt_ms) => {
                                tracing::info!(rtt_ms, "Gemini Live API key verified");
                                overlay::show_notice(
                                    format!("Gemini Live connected ({rtt_ms} ms)"),
                                    overlay::NoticeKind::Warning,
                                    pos,
                                );
                                ctrl_test.warm_asr();
                            }
                            Err(err) => {
                                tracing::warn!(error = %err, "Gemini Live connection test failed");
                                overlay::show_notice(
                                    short_error_label(&err),
                                    overlay::NoticeKind::Error,
                                    pos,
                                );
                            }
                        }
                        ctrl_test.refresh_tray();
                    });
                });
            }
            tray::TrayAction::OpenPermissionsDialog => {
                let ctrl = Arc::clone(self);
                tray::prompt_permissions_on_main(move || {
                    ctrl.try_init_hotkeys();
                    ctrl.refresh_tray();
                });
            }
            tray::TrayAction::OpenSettingsFile => {
                let path = settings::settings_path();
                if !path.exists() {
                    let _ = self.settings_snapshot().save();
                }
                let _ = Command::new("open").arg(path).spawn();
            }
            tray::TrayAction::OpenLogsFolder => {
                let dir = settings::logs_dir();
                let _ = std::fs::create_dir_all(&dir);
                let _ = Command::new("open").arg(dir).spawn();
            }
            tray::TrayAction::Quit => {
                self.cancel_current();
                crate::asr::gemini_live::clear_standby();
                DispatchQueue::main().exec_async(|| {
                    if let Some(mtm) = MainThreadMarker::new() {
                        NSApplication::sharedApplication(mtm).terminate(None);
                    }
                });
            }
        }
    }

    fn apply_new_shortcut(&self, binding: String) {
        if let Err(err) = hotkeys::validate_shortcut(&binding) {
            tracing::warn!(binding = %binding, error = %err, "Rejected invalid shortcut");
            return;
        }
        if let Some(hk) = self
            .hotkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && let Err(err) = hk.set_dictate_binding(&binding)
        {
            tracing::error!(binding = %binding, error = %err, "Failed to rebind shortcut");
            overlay::show_notice(
                "Shortcut registration failed".into(),
                overlay::NoticeKind::Error,
                self.settings_snapshot().overlay_position,
            );
            return;
        }
        self.update_settings(|s| s.shortcut = binding);
        self.refresh_tray();
    }
}

fn short_error_label(err: &str) -> String {
    let lower = err.to_lowercase();
    if lower.contains("api key")
        || lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthenticated")
    {
        "Invalid Gemini API Key".into()
    } else if lower.contains("timed out") {
        "Gemini Live timed out".into()
    } else if lower.contains("connect") || lower.contains("dns") || lower.contains("network") {
        "Unable to reach Gemini Live".into()
    } else {
        err.chars().take(44).collect()
    }
}
