//! Library scanner.
//!  * walks the music roots, silently ignoring every extension that is not a common audio format
//!  * compares (path, size, mtime) with what is already stored -> unchanged files are never re-parsed
//!  * parses tags of new/changed files in parallel (bounded thread pool) and writes them in batches
//!  * removes rows of files that no longer exist
//! The first scan of a big library is the only expensive one; later runs are a cheap stat() walk.

use crate::covers;
use crate::db::{Db, TrackRow};
use lofty::prelude::*;
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;
use walkdir::{DirEntry, WalkDir};

/// Common formats that rodio/symphonia can decode. Anything else is ignored without a word.
pub const EXTS: &[&str] = &["mp3", "m4a", "aac", "flac", "wav", "ogg", "oga"];

const BATCH: usize = 120;

#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub added: usize,
    pub removed: usize,
    pub seen: usize,
}

fn is_audio(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => {
            let e = e.to_ascii_lowercase();
            EXTS.iter().any(|x| *x == e)
        }
        None => false,
    }
}

/// Directories we never enter: hidden ones, Android/ (app data), and folders carrying a .nomedia marker.
fn skip_dir(e: &DirEntry) -> bool {
    if e.depth() == 0 || !e.file_type().is_dir() {
        return false;
    }
    let name = e.file_name().to_string_lossy();
    if name.starts_with('.') {
        return true;
    }
    if e.depth() == 1 && name.eq_ignore_ascii_case("android") {
        return true;
    }
    e.path().join(".nomedia").exists()
}

fn mtime_secs(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Reads tags; returns None for anything lofty cannot understand (corrupt / not really audio).
fn parse(path: &Path, size: i64, mtime: i64) -> Option<TrackRow> {
    let tagged = lofty::read_from_path(path).ok()?;
    let duration_ms = tagged.properties().duration().as_millis() as i64;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let clean = |s: Option<std::borrow::Cow<'_, str>>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Unknown".into());
    let (title, artist, album, has_pic) = match tag {
        Some(t) => (clean(t.title()), clean(t.artist()), clean(t.album()), !t.pictures().is_empty()),
        None => (None, None, None, false),
    };
    let path_s = path.to_string_lossy().to_string();
    Some(TrackRow {
        id: 0,
        cover_key: if has_pic { covers::key_for(&path_s) } else { String::new() },
        path: path_s,
        size,
        mtime,
        title: title.unwrap_or(stem),
        artist: artist.unwrap_or_else(|| "Unknown artist".into()),
        album: album.unwrap_or_else(|| "Unknown album".into()),
        duration_ms,
    })
}

/// Incremental scan. `on_batch` is called after every committed batch with the number of new rows.
pub fn scan(
    db_path: &Path,
    roots: &[PathBuf],
    cancel: &AtomicBool,
    on_batch: &mut dyn FnMut(usize),
) -> Result<ScanStats, String> {
    let mut db = Db::open(db_path).map_err(|e| e.to_string())?;
    let known = db.known_files().map_err(|e| e.to_string())?;
    let mut stats = ScanStats::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut work: Vec<(PathBuf, i64, i64)> = Vec::new();
    let mut walked_any_root = false;

    for root in roots {
        if !root.is_dir() {
            continue;
        }
        walked_any_root = true;
        let walker = WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| !skip_dir(e));
        for entry in walker.filter_map(|e| e.ok()) {
            if cancel.load(Ordering::Relaxed) {
                return Ok(stats);
            }
            if !entry.file_type().is_file() || !is_audio(entry.path()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let (size, mtime) = (meta.len() as i64, mtime_secs(&meta));
            let key = entry.path().to_string_lossy().to_string();
            stats.seen += 1;
            match known.get(&key) {
                Some((s, m)) if *s == size && *m == mtime => {}
                _ => work.push((entry.into_path(), size, mtime)),
            }
            seen.insert(key);
        }
    }

    // Parse new / changed files with a small pool so the UI thread keeps its cores.
    let pool = rayon::ThreadPoolBuilder::new().num_threads(3).build().map_err(|e| e.to_string())?;
    for chunk in work.chunks(BATCH) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let rows: Vec<TrackRow> = pool.install(|| chunk.par_iter().filter_map(|(p, s, m)| parse(p, *s, *m)).collect());
        if rows.is_empty() {
            continue;
        }
        db.upsert_batch(&rows).map_err(|e| e.to_string())?;
        stats.added += rows.len();
        on_batch(rows.len());
    }

    // Deleted files: only rows whose file truly does not exist any more.
    if walked_any_root && !cancel.load(Ordering::Relaxed) {
        let gone: Vec<String> = known.keys().filter(|p| !seen.contains(*p) && !Path::new(p).exists()).cloned().collect();
        if !gone.is_empty() {
            stats.removed = db.delete_paths(&gone).map_err(|e| e.to_string())?;
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_common_audio_extensions_are_accepted() {
        assert!(is_audio(Path::new("/m/a.MP3")));
        assert!(is_audio(Path::new("/m/a.flac")));
        assert!(is_audio(Path::new("/m/a.m4a")));
        assert!(!is_audio(Path::new("/m/a.txt")));
        assert!(!is_audio(Path::new("/m/a.mp4")));
        assert!(!is_audio(Path::new("/m/noext")));
    }

    #[test]
    fn scan_ignores_unknown_files_and_hidden_dirs() {
        let root = std::env::temp_dir().join(format!("velora_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::create_dir_all(root.join("nomedia")).unwrap();
        std::fs::write(root.join("nomedia/.nomedia"), b"").unwrap();
        std::fs::write(root.join("readme.txt"), b"hello").unwrap();
        std::fs::write(root.join(".hidden/x.mp3"), b"not really audio").unwrap();
        std::fs::write(root.join("nomedia/y.mp3"), b"not really audio").unwrap();
        std::fs::write(root.join("broken.mp3"), b"not really audio").unwrap(); // lofty can't read it -> silently skipped
        let db = root.join("lib.db");
        let cancel = AtomicBool::new(false);
        let st = scan(&db, &[root.clone()], &cancel, &mut |_| {}).unwrap();
        assert_eq!(st.added, 0);
        assert_eq!(st.seen, 1); // only broken.mp3 is even considered
    }
}
