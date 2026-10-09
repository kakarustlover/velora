//! Embedded cover art -> 256x256 JPEG thumbnails cached on disk. The scanner only records
//! *whether* a file has art (`cover_key`); the pixels are extracted lazily in the background.

use crate::db::Db;
use lofty::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const THUMB: u32 = 256;

/// Stable 64-bit FNV-1a hash of the file path, as 16 hex digits.
pub fn key_for(path: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in path.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub fn cache_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.jpg"))
}

/// Extracts the first embedded picture of `track_path` into the cache. Returns true when a thumbnail exists afterwards.
pub fn ensure(dir: &Path, track_path: &str, key: &str) -> bool {
    let out = cache_path(dir, key);
    if out.exists() {
        return true;
    }
    let Ok(tagged) = lofty::read_from_path(track_path) else { return false };
    let pic = tagged.tags().iter().find_map(|t| t.pictures().first().cloned());
    let Some(pic) = pic else { return false };
    let Ok(img) = image::load_from_memory(pic.data()) else { return false };
    let thumb = img.resize_to_fill(THUMB, THUMB, image::imageops::FilterType::Triangle);
    let tmp = out.with_extension("tmp.jpg");
    if thumb.to_rgb8().save(&tmp).is_err() {
        return false;
    }
    std::fs::rename(&tmp, &out).is_ok()
}

/// Generates every missing thumbnail (called by the scanner thread after a scan). `progress` fires every 40 covers.
pub fn generate_missing(db_path: &Path, dir: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut()) {
    let _ = std::fs::create_dir_all(dir);
    let Ok(db) = Db::open(db_path) else { return };
    let Ok(tracks) = db.all_tracks() else { return };
    let mut made = 0usize;
    for t in tracks.iter().filter(|t| !t.cover_key.is_empty()) {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if cache_path(dir, &t.cover_key).exists() {
            continue;
        }
        if ensure(dir, &t.path, &t.cover_key) {
            made += 1;
            if made % 40 == 0 {
                progress();
            }
        }
    }
    if made % 40 != 0 {
        progress();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_and_16_hex_digits() {
        let a = key_for("/storage/emulated/0/Music/a.mp3");
        assert_eq!(a, key_for("/storage/emulated/0/Music/a.mp3"));
        assert_eq!(a.len(), 16);
        assert_ne!(a, key_for("/storage/emulated/0/Music/b.mp3"));
    }
}
