//! Native macOS feedback chimes via `NSSound` (zero external audio playback crates).

use crate::settings::SoundTheme;
use dispatch2::DispatchQueue;
use objc2::AnyThread;
use objc2_app_kit::NSSound;
use objc2_foundation::{NSBundle, NSString};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChimeKind {
    Start,
    Stop,
}

fn resolve_sound_path(theme: SoundTheme, kind: ChimeKind) -> Option<PathBuf> {
    let filename = match kind {
        ChimeKind::Start => theme.start_filename(),
        ChimeKind::Stop => theme.stop_filename(),
    };

    // 1. Installed app bundle: LIVI.app/Contents/Resources/<filename>
    if let Some(res_path) = NSBundle::mainBundle().resourcePath() {
        let candidate = PathBuf::from(res_path.to_string()).join(filename);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    // 2. Local development fallback: ./resources/<filename>
    let dev_candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join(filename);
    if dev_candidate.is_file() {
        return Some(dev_candidate);
    }

    None
}

/// Plays a start or stop chime asynchronously on the macOS main queue via `NSSound`.
pub fn play_chime(enabled: bool, theme: SoundTheme, volume: f32, kind: ChimeKind) {
    if !enabled {
        return;
    }
    let Some(path) = resolve_sound_path(theme, kind) else {
        tracing::debug!("Chime WAV not found for {:?}/{:?}", theme, kind);
        return;
    };
    let path_str = path.to_string_lossy().into_owned();
    let clamped_vol = volume.clamp(0.0, 1.0);

    DispatchQueue::main().exec_async(move || {
        let ns_path = NSString::from_str(&path_str);
        let sound = NSSound::initWithContentsOfFile_byReference(NSSound::alloc(), &ns_path, true);
        if let Some(sound) = sound {
            sound.setVolume(clamped_vol);
            let _ = sound.play();
        }
    });
}
