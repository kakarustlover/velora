//! Velora - glass music player. UI: Slint, audio: rodio/symphonia, tags: lofty, storage: SQLite.

slint::include_modules!();

pub mod app;
pub mod covers;
pub mod db;
pub mod eq;
pub mod platform;
pub mod player;
pub mod scanner;

#[cfg(target_os = "android")]
mod android;

/// Android entry point (called by android-activity on its own thread).
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: slint::android::AndroidApp) {
    use std::path::PathBuf;
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("velora"));
    std::panic::set_hook(Box::new(|info| {
        log::error!("PANIC: {info}");
    }));
    slint::android::init(app.clone()).expect("slint android init");
    let dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/data/dev.velora.player/files"));
    if let Err(e) = app::run(dir) {
        log::error!("Velora exited with an error: {e}");
    }
}
