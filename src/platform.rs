//! Everything that differs between Android and the desktop dev build lives behind `Platform`.

use crossbeam_channel::Sender;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

#[derive(Clone, Debug, Default)]
pub struct NowPlayingInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub position_ms: u64,
    pub playing: bool,
    pub cover_path: Option<String>,
}

/// Commands coming from outside the app UI: the notification, lock screen, headset buttons,
/// audio-focus changes and Android lifecycle.
#[derive(Clone, Debug)]
pub enum NativeCmd {
    Play,
    Pause,
    Toggle,
    Next,
    Prev,
    SeekMs(u64),
    Stop,
    /// the app came back to the foreground -> look for new files
    Resume,
    /// the user just granted the audio permission
    PermissionGranted,
}

/// Sender used by the JNI exports (they have no other way to reach the app).
pub static NATIVE_TX: OnceLock<Sender<NativeCmd>> = OnceLock::new();

pub trait Platform: Send + Sync {
    fn has_audio_permission(&self) -> bool;
    fn request_audio_permission(&self);
    fn request_notification_permission(&self);
    fn music_roots(&self) -> Vec<PathBuf>;
    fn update_now_playing(&self, info: &NowPlayingInfo);
    fn stop_playback_service(&self);
    /// (top, bottom) system bar insets in dp
    fn insets_dp(&self) -> (f32, f32);
}

pub fn create() -> Arc<dyn Platform> {
    #[cfg(target_os = "android")]
    {
        Arc::new(crate::android::AndroidPlatform)
    }
    #[cfg(not(target_os = "android"))]
    {
        Arc::new(DesktopPlatform)
    }
}

#[cfg(not(target_os = "android"))]
pub struct DesktopPlatform;

#[cfg(not(target_os = "android"))]
impl Platform for DesktopPlatform {
    fn has_audio_permission(&self) -> bool {
        true
    }
    fn request_audio_permission(&self) {}
    fn request_notification_permission(&self) {}
    fn music_roots(&self) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(d) = dirs::audio_dir() {
            v.push(d);
        } else if let Some(h) = dirs::home_dir() {
            v.push(h.join("Music"));
        }
        v
    }
    fn update_now_playing(&self, info: &NowPlayingInfo) {
        log::info!("now playing: {} - {} (playing={})", info.artist, info.title, info.playing);
    }
    fn stop_playback_service(&self) {}
    fn insets_dp(&self) -> (f32, f32) {
        (0.0, 0.0)
    }
}
