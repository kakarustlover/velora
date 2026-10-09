//! Persistent library. Scanning once is enough: every file is stored with its size and
//! modification time, so later launches only compare (path, size, mtime) and parse tags
//! for files that are new or changed.

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct TrackRow {
    pub id: i64,
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: i64,
    pub cover_key: String, // "" = file has no embedded cover
}

#[derive(Clone, Debug)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub track_ids: Vec<i64>,
}

pub struct Db {
    conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tracks(
    id          INTEGER PRIMARY KEY,
    path        TEXT NOT NULL UNIQUE,
    size        INTEGER NOT NULL,
    mtime       INTEGER NOT NULL,
    title       TEXT NOT NULL,
    artist      TEXT NOT NULL,
    album       TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    cover_key   TEXT NOT NULL DEFAULT '',
    added_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_tracks_title ON tracks(title COLLATE NOCASE);
CREATE TABLE IF NOT EXISTS playlists(
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE TABLE IF NOT EXISTS playlist_tracks(
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    track_id    INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    pos         INTEGER NOT NULL,
    PRIMARY KEY(playlist_id, track_id)
);
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db { conn })
    }

    // ---------------------------------------------------------------- tracks
    pub fn all_tracks(&self) -> rusqlite::Result<Vec<TrackRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,path,size,mtime,title,artist,album,duration_ms,cover_key FROM tracks ORDER BY title COLLATE NOCASE, id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(TrackRow {
                id: r.get(0)?,
                path: r.get(1)?,
                size: r.get(2)?,
                mtime: r.get(3)?,
                title: r.get(4)?,
                artist: r.get(5)?,
                album: r.get(6)?,
                duration_ms: r.get(7)?,
                cover_key: r.get(8)?,
            })
        })?;
        rows.collect()
    }

    /// path -> (size, mtime): what the scanner compares against.
    pub fn known_files(&self) -> rusqlite::Result<HashMap<String, (i64, i64)>> {
        let mut st = self.conn.prepare("SELECT path,size,mtime FROM tracks")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))))?;
        rows.collect()
    }

    pub fn upsert_batch(&mut self, rows: &[TrackRow]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO tracks(path,size,mtime,title,artist,album,duration_ms,cover_key) VALUES(?,?,?,?,?,?,?,?)
                 ON CONFLICT(path) DO UPDATE SET size=excluded.size, mtime=excluded.mtime, title=excluded.title,
                   artist=excluded.artist, album=excluded.album, duration_ms=excluded.duration_ms, cover_key=excluded.cover_key",
            )?;
            for r in rows {
                st.execute(params![r.path, r.size, r.mtime, r.title, r.artist, r.album, r.duration_ms, r.cover_key])?;
            }
        }
        tx.commit()
    }

    pub fn delete_paths(&mut self, paths: &[String]) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut st = tx.prepare("DELETE FROM tracks WHERE path=?")?;
            for p in paths {
                n += st.execute(params![p])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn track_count(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
    }

    // ------------------------------------------------------------- playlists
    pub fn playlists(&self) -> rusqlite::Result<Vec<Playlist>> {
        let mut out: Vec<Playlist> = Vec::new();
        {
            let mut st = self.conn.prepare("SELECT id,name FROM playlists ORDER BY created_at, id")?;
            let rows = st.query_map([], |r| Ok(Playlist { id: r.get(0)?, name: r.get(1)?, track_ids: vec![] }))?;
            for r in rows {
                out.push(r?);
            }
        }
        let mut st = self.conn.prepare("SELECT track_id FROM playlist_tracks WHERE playlist_id=? ORDER BY pos")?;
        for p in out.iter_mut() {
            let ids = st.query_map(params![p.id], |r| r.get::<_, i64>(0))?;
            p.track_ids = ids.collect::<Result<Vec<_>, _>>()?;
        }
        Ok(out)
    }

    /// Creates (id = None) or updates a playlist and replaces its tracks. Returns the id.
    pub fn save_playlist(&mut self, id: Option<i64>, name: &str, track_ids: &[i64]) -> rusqlite::Result<i64> {
        let tx = self.conn.transaction()?;
        let pid = match id {
            Some(i) => {
                tx.execute("UPDATE playlists SET name=? WHERE id=?", params![name, i])?;
                tx.execute("DELETE FROM playlist_tracks WHERE playlist_id=?", params![i])?;
                i
            }
            None => {
                tx.execute("INSERT INTO playlists(name) VALUES(?)", params![name])?;
                tx.last_insert_rowid()
            }
        };
        {
            let mut st = tx.prepare("INSERT OR IGNORE INTO playlist_tracks(playlist_id,track_id,pos) VALUES(?,?,?)")?;
            for (pos, t) in track_ids.iter().enumerate() {
                st.execute(params![pid, t, pos as i64])?;
            }
        }
        tx.commit()?;
        Ok(pid)
    }

    pub fn delete_playlist(&mut self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM playlists WHERE id=?", params![id])?;
        Ok(())
    }

    // -------------------------------------------------------------- settings
    pub fn get(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM settings WHERE key=?", params![key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set(&self, key: &str, value: &str) {
        let _ = self.conn.execute(
            "INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, title: &str) -> TrackRow {
        TrackRow {
            path: path.into(),
            size: 10,
            mtime: 5,
            title: title.into(),
            artist: "A".into(),
            album: "B".into(),
            duration_ms: 1000,
            ..Default::default()
        }
    }

    #[test]
    fn upsert_is_idempotent_and_updates_changed_files() {
        let dir = std::env::temp_dir().join(format!("velora_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut db = Db::open(&dir.join("t.db")).unwrap();
        db.upsert_batch(&[row("/m/a.mp3", "Zed"), row("/m/b.mp3", "alpha")]).unwrap();
        db.upsert_batch(&[row("/m/a.mp3", "Zed 2")]).unwrap();
        assert_eq!(db.track_count().unwrap(), 2);
        let all = db.all_tracks().unwrap();
        assert_eq!(all[0].title, "alpha"); // case-insensitive order
        assert_eq!(all[1].title, "Zed 2");
        assert_eq!(db.known_files().unwrap().len(), 2);
        assert_eq!(db.delete_paths(&["/m/a.mp3".to_string()]).unwrap(), 1);
    }

    #[test]
    fn playlists_round_trip_and_cascade() {
        let dir = std::env::temp_dir().join(format!("velora_test_pl_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut db = Db::open(&dir.join("t.db")).unwrap();
        db.upsert_batch(&[row("/m/a.mp3", "a"), row("/m/b.mp3", "b")]).unwrap();
        let ids: Vec<i64> = db.all_tracks().unwrap().iter().map(|t| t.id).collect();
        let pid = db.save_playlist(None, "Mix", &ids).unwrap();
        assert_eq!(db.playlists().unwrap()[0].track_ids, ids);
        db.delete_paths(&["/m/a.mp3".to_string()]).unwrap();
        assert_eq!(db.playlists().unwrap()[0].track_ids.len(), 1); // ON DELETE CASCADE
        db.delete_playlist(pid).unwrap();
        assert!(db.playlists().unwrap().is_empty());
    }
}
