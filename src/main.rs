//! Desktop development build: `cargo run` opens the same UI in a window (library = ~/Music).

#[cfg(not(target_os = "android"))]
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let dir = dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("velora");
    if let Err(e) = velora::app::run(dir) {
        eprintln!("Velora error: {e}");
    }
}

// The Android build uses the library (`android_main` in lib.rs); this binary is unused there.
#[cfg(target_os = "android")]
fn main() {}
