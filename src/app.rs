//! Application controller: connects the Slint UI (AppState) with the library, scanner, player,
//! equalizer, playlists, persisted settings and the Android media notification.
//!
//! Threads
//!   * UI thread (Slint event loop) - cheap callbacks only
//!   * velora-scan  - permission wait, first scan, periodic incremental scans, cover thumbnails
//!   * velora-player- audio (see player.rs)
//!   * velora-events- turns player events / notification commands into UI + notification updates

use crate::covers;
use crate::db::{Db, Playlist, TrackRow};
use crate::eq::{self, EqParams};
use crate::platform::{self, NativeCmd, NowPlayingInfo, Platform, NATIVE_TX};
use crate::player::{self, Cmd, Event, PlayerHandle, QTrack};
use crate::scanner;
use crate::{AppState, ListRow, MainWindow, PickRow, PlCard};
use crossbeam_channel::{Receiver, Sender};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ROW_H: f32 = 58.0;
const WINDOW_ROWS: usize = 16;
const CARD_STEP: f32 = 216.0; // playlist card height 204 + gap 12 (must match app.slint)
const DETAIL_HEADER: f32 = 118.0;
const SCAN_EVERY: Duration = Duration::from_secs(45);

// ------------------------------------------------------------------------------------------
// Shared state (guarded by one mutex; callbacks only hold it briefly)
// ------------------------------------------------------------------------------------------
#[derive(Clone)]
struct Row {
    kind: i32,
    id: i64,
    title: String,
    sub: String,
    right: String,
    cover: String,
    track_idx: Option<usize>, // index into `tracks` for songs
}

struct Shared {
    tracks: Vec<TrackRow>,
    playlists: Vec<Playlist>,
    view: Vec<Row>,
    queue_ids: Vec<i64>, // ids of the queue the player currently uses (for highlighting / playlist play)
    tab: i32,
    query: String,
    pl_view: Option<i64>,
    scroll_t: f32,
    // playlist editor draft
    draft_id: Option<i64>,
    draft_name: String,
    draft_sel: Vec<i64>,
    del_armed: bool,
    // equalizer
    eq_on: bool,
    eq_gains: [i32; 10],
    eq_bass: i32,
    eq_treble: i32,
    eq_pre: i32,
    eq_preset: i32,
    // transport
    shuffle: bool,
    repeat: i32,
    liked: std::collections::HashSet<i64>,
    now: Option<QTrack>,
    now_pos_ms: u64,
    playing: bool,
    intro_gen: u64,
    notif_perm_asked: bool,
}

impl Shared {
    fn new() -> Self {
        Shared {
            tracks: vec![],
            playlists: vec![],
            view: vec![],
            queue_ids: vec![],
            tab: 0,
            query: String::new(),
            pl_view: None,
            scroll_t: 0.0,
            draft_id: None,
            draft_name: String::new(),
            draft_sel: vec![],
            del_armed: false,
            eq_on: true,
            eq_gains: [0; 10],
            eq_bass: 0,
            eq_treble: 0,
            eq_pre: 0,
            eq_preset: 0,
            shuffle: false,
            repeat: 0,
            liked: Default::default(),
            now: None,
            now_pos_ms: 0,
            playing: false,
            intro_gen: 0,
            notif_perm_asked: false,
        }
    }
}

type SharedRef = Arc<Mutex<Shared>>;

fn fmt_time(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn hue_of(id: i64) -> i32 {
    ((id.rem_euclid(1000) * 47 + 200) % 360) as i32
}

fn qtrack(t: &TrackRow) -> QTrack {
    QTrack {
        id: t.id,
        path: t.path.clone(),
        title: t.title.clone(),
        artist: t.artist.clone(),
        album: t.album.clone(),
        duration_ms: t.duration_ms.max(0) as u64,
        cover_key: t.cover_key.clone(),
    }
}

fn n_songs(n: usize) -> String {
    format!("{} {}", n, if n == 1 { "song" } else { "songs" })
}

// ------------------------------------------------------------------------------------------
// Building the list the UI shows (songs / albums / singers / playlist detail)
// ------------------------------------------------------------------------------------------
fn rebuild_view(s: &mut Shared) -> String {
    let q = s.query.trim().to_lowercase();
    let matches = |t: &TrackRow| q.is_empty() || t.title.to_lowercase().contains(&q) || t.artist.to_lowercase().contains(&q) || t.album.to_lowercase().contains(&q);
    s.view.clear();
    let mut footer = String::new();
    match (s.tab, s.pl_view) {
        (3, Some(pid)) => {
            if let Some(p) = s.playlists.iter().find(|p| p.id == pid) {
                let by_id: HashMap<i64, usize> = s.tracks.iter().enumerate().map(|(i, t)| (t.id, i)).collect();
                for tid in &p.track_ids {
                    if let Some(&i) = by_id.get(tid) {
                        let t = &s.tracks[i];
                        s.view.push(Row { kind: 0, id: t.id, title: t.title.clone(), sub: format!("{} - {}", t.artist, t.album), right: fmt_time(t.duration_ms.max(0) as u64), cover: t.cover_key.clone(), track_idx: Some(i) });
                    }
                }
            }
        }
        (0, _) => {
            let mut total_ms = 0i64;
            for (i, t) in s.tracks.iter().enumerate() {
                if matches(t) {
                    total_ms += t.duration_ms;
                    s.view.push(Row { kind: 0, id: t.id, title: t.title.clone(), sub: format!("{} - {}", t.artist, t.album), right: fmt_time(t.duration_ms.max(0) as u64), cover: t.cover_key.clone(), track_idx: Some(i) });
                }
            }
            if !s.view.is_empty() {
                footer = format!("{} · {} min", n_songs(s.view.len()), ((total_ms as f64 / 60000.0).round() as i64).max(1));
            }
        }
        (1, _) | (2, _) => {
            let by_album = s.tab == 1;
            let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
            let mut pos: HashMap<String, usize> = HashMap::new();
            for (i, t) in s.tracks.iter().enumerate() {
                let key = if by_album { t.album.clone() } else { t.artist.clone() };
                if !q.is_empty() && !key.to_lowercase().contains(&q) {
                    continue;
                }
                match pos.get(&key) {
                    Some(&g) => groups[g].1.push(i),
                    None => {
                        pos.insert(key.clone(), groups.len());
                        groups.push((key, vec![i]));
                    }
                }
            }
            groups.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
            for (gi, (name, idxs)) in groups.iter().enumerate() {
                let cover = idxs.iter().map(|i| s.tracks[*i].cover_key.clone()).find(|k| !k.is_empty()).unwrap_or_default();
                s.view.push(Row { kind: if by_album { 1 } else { 2 }, id: gi as i64, title: name.clone(), sub: n_songs(idxs.len()), right: String::new(), cover, track_idx: None });
            }
            let n = s.view.len();
            footer = if by_album { format!("{} {}", n, if n == 1 { "album" } else { "albums" }) } else { format!("{} {}", n, if n == 1 { "singer" } else { "singers" }) };
        }
        _ => {}
    }
    footer
}

fn content_height(s: &Shared) -> f32 {
    match (s.tab, s.pl_view) {
        (3, Some(_)) => DETAIL_HEADER + s.view.len() as f32 * ROW_H + 8.0,
        (3, None) => ((s.playlists.len() + 1 + 1) / 2) as f32 * CARD_STEP + 40.0,
        _ => s.view.len() as f32 * ROW_H + 48.0,
    }
}

// ------------------------------------------------------------------------------------------
// Pushing state into the UI (always on the UI thread)
// ------------------------------------------------------------------------------------------
fn row_model(s: &Shared, first: usize) -> Vec<ListRow> {
    s.view
        .iter()
        .enumerate()
        .skip(first)
        .take(WINDOW_ROWS)
        .map(|(i, r)| ListRow {
            kind: r.kind,
            idx: i as i32,
            id: r.id as i32,
            title: SharedString::from(r.title.as_str()),
            sub: SharedString::from(r.sub.as_str()),
            right: SharedString::from(r.right.as_str()),
            cover: SharedString::from(r.cover.as_str()),
            hue: hue_of(r.id),
        })
        .collect()
}

fn playlist_cards(s: &Shared) -> Vec<PlCard> {
    let by_id: HashMap<i64, &TrackRow> = s.tracks.iter().map(|t| (t.id, t)).collect();
    s.playlists
        .iter()
        .map(|p| {
            let get = |k: usize| -> (SharedString, i32) {
                match p.track_ids.get(k).and_then(|id| by_id.get(id)) {
                    Some(t) => (SharedString::from(t.cover_key.as_str()), hue_of(t.id)),
                    None => (SharedString::default(), 200),
                }
            };
            let (k1, h1) = get(0);
            let (k2, h2) = get(1);
            let (k3, h3) = get(2);
            let (k4, h4) = get(3);
            PlCard { id: p.id as i32, name: SharedString::from(p.name.as_str()), sub: SharedString::from(n_songs(p.track_ids.len()).as_str()), k1, k2, k3, k4, h1, h2, h3, h4, n: p.track_ids.len() as i32 }
        })
        .collect()
}

/// Recomputes the view and pushes rows + metadata into the UI. Must run on the UI thread.
/// The lock is released BEFORE any Slint setter runs: setters may synchronously trigger
/// callbacks (e.g. `changed viewport-y` -> `scrolled`) that need the same mutex.
fn refresh_ui(ui: &MainWindow, sh: &SharedRef) {
    let (rows, first, total, offset, content, footer, cards, pl_view, pl_info) = {
        let mut s = sh.lock().unwrap();
        let footer = rebuild_view(&mut s);
        let content = content_height(&s);
        let offset = if s.tab == 3 && s.pl_view.is_some() { DETAIL_HEADER } else { 0.0 };
        let first = (((s.scroll_t - 6.0 - offset) / ROW_H).floor().max(0.0) as usize).min(s.view.len().saturating_sub(1));
        let footer = match (s.tab, s.pl_view) {
            (3, None) => format!("{} {}", s.playlists.len(), if s.playlists.len() == 1 { "playlist" } else { "playlists" }),
            _ => footer,
        };
        let pl_info = s.pl_view.and_then(|pid| s.playlists.iter().find(|p| p.id == pid).map(|p| (p.name.clone(), n_songs(p.track_ids.len()))));
        (row_model(&s, first), first, s.view.len(), offset, content, footer, playlist_cards(&s), s.pl_view, pl_info)
    };
    let st = ui.global::<AppState>();
    st.set_rows(ModelRc::new(VecModel::from(rows)));
    st.set_first_row(first as i32);
    st.set_total_rows(total as i32);
    st.set_rows_offset(offset);
    st.set_list_content_h(content);
    st.set_footer(SharedString::from(footer.as_str()));
    st.set_playlists(ModelRc::new(VecModel::from(cards)));
    st.set_pl_view(pl_view.map(|v| v as i32).unwrap_or(-1));
    if let Some((name, sub)) = pl_info {
        st.set_pl_title(SharedString::from(name.as_str()));
        st.set_pl_sub(SharedString::from(sub.as_str()));
    }
}

fn scroll_window(ui: &MainWindow, sh: &SharedRef, t: f32) {
    let st = ui.global::<AppState>();
    let current_first = st.get_first_row();
    let update = {
        let mut s = sh.lock().unwrap();
        s.scroll_t = t;
        let offset = if s.tab == 3 && s.pl_view.is_some() { DETAIL_HEADER } else { 0.0 };
        let first = (((t - 6.0 - offset) / ROW_H).floor().max(0.0) as usize).min(s.view.len().saturating_sub(1));
        if first as i32 != current_first {
            Some((row_model(&s, first), first))
        } else {
            None
        }
    };
    if let Some((rows, first)) = update {
        st.set_rows(ModelRc::new(VecModel::from(rows)));
        st.set_first_row(first as i32);
    }
}

/// Orbit ring around the play button: on for exactly 5 seconds, and only if no newer start replaced it.
fn start_intro(weak: slint::Weak<MainWindow>, shared: SharedRef, id: u64) {
    if let Some(ui) = weak.upgrade() {
        ui.global::<AppState>().set_intro(true);
    }
    slint::Timer::single_shot(Duration::from_secs(5), move || {
        if shared.lock().unwrap().intro_gen == id {
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppState>().set_intro(false);
            }
        }
    });
}

fn push_eq(ui: &MainWindow, s: &Shared) {
    let st = ui.global::<AppState>();
    st.set_eq_on(s.eq_on);
    st.set_eq_bands(ModelRc::new(VecModel::from(s.eq_gains.to_vec())));
    st.set_eq_bass(s.eq_bass);
    st.set_eq_treble(s.eq_treble);
    st.set_eq_pre(s.eq_pre);
    st.set_eq_preset(s.eq_preset);
    st.set_eq_curve(SharedString::from(eq::curve_path(&s.eq_gains).as_str()));
    st.set_eq_summary(SharedString::from(format!("{} · {}", eq::PRESET_NAMES[s.eq_preset.clamp(0, 8) as usize], if s.eq_on { "On" } else { "Off" }).as_str()));
}

fn push_editor(ui: &MainWindow, s: &Shared) {
    let st = ui.global::<AppState>();
    let picks: Vec<PickRow> = s
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| PickRow { idx: i as i32, title: SharedString::from(t.title.as_str()), sub: SharedString::from(t.artist.as_str()), cover: SharedString::from(t.cover_key.as_str()), hue: hue_of(t.id), sel: s.draft_sel.contains(&t.id) })
        .collect();
    st.set_pick_rows(ModelRc::new(VecModel::from(picks)));
    st.set_pl_count(SharedString::from(format!("{} selected", s.draft_sel.len()).as_str()));
    st.set_pl_can_save(!s.draft_name.trim().is_empty());
    st.set_pl_del_armed(s.del_armed);
}

// ------------------------------------------------------------------------------------------
// Settings persistence
// ------------------------------------------------------------------------------------------
fn load_settings(db: &Db, s: &mut Shared, ui: &MainWindow, params: &EqParams) {
    let st = ui.global::<AppState>();
    let geti = |k: &str, d: i32| db.get(k).and_then(|v| v.parse::<i32>().ok()).unwrap_or(d);
    st.set_pal(geti("pal", 0).clamp(0, 9));
    st.set_dark(geti("dark", 0) == 1);
    st.set_wall(geti("wall", 0).clamp(0, 4));
    st.set_perf(geti("perf", 1).clamp(0, 2));
    st.set_cover_style(geti("cover", 0).clamp(0, 5));
    st.set_eq_look(geti("eqlook", 0).clamp(0, 5));
    s.shuffle = geti("shuffle", 0) == 1;
    s.repeat = geti("repeat", 0).clamp(0, 2);
    st.set_shuffle(s.shuffle);
    st.set_repeat(s.repeat);
    if let Some(j) = db.get("eq") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&j) {
            s.eq_on = v["on"].as_bool().unwrap_or(true);
            if let Some(a) = v["g"].as_array() {
                for (i, x) in a.iter().take(10).enumerate() {
                    s.eq_gains[i] = x.as_i64().unwrap_or(0).clamp(-12, 12) as i32;
                }
            }
            s.eq_bass = v["bass"].as_i64().unwrap_or(0).clamp(-12, 12) as i32;
            s.eq_treble = v["treble"].as_i64().unwrap_or(0).clamp(-12, 12) as i32;
            s.eq_pre = v["pre"].as_i64().unwrap_or(0).clamp(-12, 12) as i32;
            s.eq_preset = v["preset"].as_i64().unwrap_or(0).clamp(0, 8) as i32;
        }
    }
    params.set_all(s.eq_on, &s.eq_gains, s.eq_bass, s.eq_treble, s.eq_pre);
    push_eq(ui, s);
}

fn save_eq(db_path: &Path, s: &Shared) {
    if let Ok(db) = Db::open(db_path) {
        let v = serde_json::json!({"on": s.eq_on, "g": s.eq_gains, "bass": s.eq_bass, "treble": s.eq_treble, "pre": s.eq_pre, "preset": s.eq_preset});
        db.set("eq", &v.to_string());
    }
}

fn save_kv(db_path: &Path, k: &str, v: i32) {
    if let Ok(db) = Db::open(db_path) {
        db.set(k, &v.to_string());
    }
}

// ------------------------------------------------------------------------------------------
// Entry point shared by Android and desktop
// ------------------------------------------------------------------------------------------
pub fn run(data_dir: PathBuf) -> Result<(), slint::PlatformError> {
    let _ = std::fs::create_dir_all(&data_dir);
    let db_path = data_dir.join("velora.db");
    let cover_dir = data_dir.join("covers");
    let _ = std::fs::create_dir_all(&cover_dir);
    log::info!("Velora starting, data dir = {}", data_dir.display());

    let platform: Arc<dyn Platform> = platform::create();
    let ui = MainWindow::new()?;
    let weak = ui.as_weak();
    let shared: SharedRef = Arc::new(Mutex::new(Shared::new()));
    let params = Arc::new(EqParams::new());

    // ---- 1. restore everything that was saved, show the stored library immediately -------------
    {
        let db = Db::open(&db_path).expect("open database");
        let mut s = shared.lock().unwrap();
        s.tracks = db.all_tracks().unwrap_or_default();
        s.playlists = db.playlists().unwrap_or_default();
        load_settings(&db, &mut s, &ui, &params);
        log::info!("library restored from database: {} tracks, {} playlists (no scan needed)", s.tracks.len(), s.playlists.len());
    }
    refresh_ui(&ui, &shared);
    let (top, bottom) = platform.insets_dp();
    ui.global::<AppState>().set_safe_t(top);
    ui.global::<AppState>().set_safe_b(bottom);

    // ---- 2. cover images: lazy, cached, never blocks scrolling for long -------------------------
    {
        let cache: Rc<RefCell<HashMap<String, slint::Image>>> = Rc::new(RefCell::new(HashMap::new()));
        let dir = cover_dir.clone();
        ui.global::<AppState>().on_cover_image(move |key| {
            let key = key.to_string();
            if key.is_empty() {
                return slint::Image::default();
            }
            if let Some(img) = cache.borrow().get(&key) {
                return img.clone();
            }
            match slint::Image::load_from_path(&covers::cache_path(&dir, &key)) {
                Ok(img) => {
                    let mut c = cache.borrow_mut();
                    if c.len() > 400 {
                        c.clear();
                    }
                    c.insert(key, img.clone());
                    img
                }
                Err(_) => slint::Image::default(),
            }
        });
    }

    // ---- 3. player + equalizer -----------------------------------------------------------------------
    let (ev_tx, ev_rx) = crossbeam_channel::unbounded::<Event>();
    let player = player::spawn(params.clone(), ev_tx);
    {
        let s = shared.lock().unwrap();
        player.send(Cmd::SetShuffle(s.shuffle));
        player.send(Cmd::SetRepeat(s.repeat as u8));
    }

    // ---- 4. native commands (notification buttons, headset, lifecycle) -------------------------
    let (nat_tx, nat_rx) = crossbeam_channel::unbounded::<NativeCmd>();
    let _ = NATIVE_TX.set(nat_tx.clone());

    // ---- 5. scanner thread: permission first, then scan, then keep looking for new files ------------
    let scan_trigger: (Sender<()>, Receiver<()>) = crossbeam_channel::unbounded();
    spawn_scanner(platform.clone(), db_path.clone(), cover_dir.clone(), shared.clone(), weak.clone(), scan_trigger.1.clone());

    // ---- 6. event thread: player events + native commands -> UI & notification -----------------------
    spawn_event_thread(platform.clone(), shared.clone(), weak.clone(), player.clone(), ev_rx, nat_rx, scan_trigger.0.clone(), db_path.clone());

    // ---- 7. UI callbacks ---------------------------------------------------------------------------------
    wire_callbacks(&ui, shared.clone(), player.clone(), params.clone(), db_path.clone(), platform.clone());

    // ---- 8. animation clock, splash ------------------------------------------------------------------------
    let clock = slint::Timer::default();
    {
        let w = weak.clone();
        let mut phase = 0.0f32;
        clock.start(slint::TimerMode::Repeated, Duration::from_millis(33), move || {
            let Some(ui) = w.upgrade() else { return };
            let st = ui.global::<AppState>();
            phase = (phase + 3.3) % 36000.0; // 100 deg/s, wraps on a multiple of every pattern period
            st.set_phase(phase);
            if st.get_playing() && st.get_cover_style() == 2 {
                st.set_disc_angle((st.get_disc_angle() + 0.66) % 360.0); // 20 deg/s
            }
        });
    }
    let splash = slint::Timer::default();
    {
        let w = weak.clone();
        splash.start(slint::TimerMode::SingleShot, Duration::from_millis(1800), move || {
            if let Some(ui) = w.upgrade() {
                ui.global::<AppState>().set_splash(false);
            }
        });
    }

    ui.run()
}

// ------------------------------------------------------------------------------------------
// Scanner thread
// ------------------------------------------------------------------------------------------
fn spawn_scanner(platform: Arc<dyn Platform>, db_path: PathBuf, cover_dir: PathBuf, shared: SharedRef, weak: slint::Weak<MainWindow>, trigger: Receiver<()>) {
    std::thread::Builder::new()
        .name("velora-scan".into())
        .spawn(move || {
            // The only thing we ask the user for at start: access to music.
            if !platform.has_audio_permission() {
                log::info!("asking for audio permission");
                platform.request_audio_permission();
                while !platform.has_audio_permission() {
                    std::thread::sleep(Duration::from_millis(400));
                }
                log::info!("audio permission granted -> scanning now");
            }
            let cancel = AtomicBool::new(false);
            let mut first = true;
            loop {
                let roots = platform.music_roots();
                let t0 = std::time::Instant::now();
                let mut last_push = std::time::Instant::now();
                let mut batch_added = 0usize;
                let res = scanner::scan(&db_path, &roots, &cancel, &mut |n| {
                    batch_added += n;
                    if last_push.elapsed() > Duration::from_millis(700) {
                        last_push = std::time::Instant::now();
                        reload_library(&db_path, &shared, &weak);
                    }
                });
                match res {
                    Ok(st) => {
                        log::info!("scan finished in {:?}: seen {}, new {}, removed {}", t0.elapsed(), st.seen, st.added, st.removed);
                        if st.added > 0 || st.removed > 0 || first {
                            reload_library(&db_path, &shared, &weak);
                        }
                        if st.added > 0 {
                            covers::generate_missing(&db_path, &cover_dir, &cancel, &mut || reload_library(&db_path, &shared, &weak));
                        } else if first {
                            covers::generate_missing(&db_path, &cover_dir, &cancel, &mut || {});
                        }
                    }
                    Err(e) => log::warn!("scan error: {e}"),
                }
                first = false;
                // sleep until the next periodic scan or until the app is resumed
                let _ = trigger.recv_timeout(SCAN_EVERY);
                while trigger.try_recv().is_ok() {}
            }
        })
        .expect("spawn scanner");
}

/// Reads the library from the database and refreshes the UI (called from worker threads).
fn reload_library(db_path: &Path, shared: &SharedRef, weak: &slint::Weak<MainWindow>) {
    let Ok(db) = Db::open(db_path) else { return };
    let tracks = db.all_tracks().unwrap_or_default();
    let playlists = db.playlists().unwrap_or_default();
    let sh = shared.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        {
            let mut s = sh.lock().unwrap();
            s.tracks = tracks;
            s.playlists = playlists;
        }
        refresh_ui(&ui, &sh);
    });
}

// ------------------------------------------------------------------------------------------
// Event thread
// ------------------------------------------------------------------------------------------
#[allow(clippy::too_many_arguments)]
fn spawn_event_thread(
    platform: Arc<dyn Platform>,
    shared: SharedRef,
    weak: slint::Weak<MainWindow>,
    player: PlayerHandle,
    ev_rx: Receiver<Event>,
    nat_rx: Receiver<NativeCmd>,
    scan_now: Sender<()>,
    db_path: PathBuf,
) {
    std::thread::Builder::new()
        .name("velora-events".into())
        .spawn(move || {
            let cover_dir = db_path.parent().map(|p| p.join("covers")).unwrap_or_default();
            loop {
                crossbeam_channel::select! {
                    recv(ev_rx) -> ev => {
                        let Ok(ev) = ev else { break };
                        on_player_event(ev, &platform, &shared, &weak, &cover_dir);
                    }
                    recv(nat_rx) -> c => {
                        let Ok(c) = c else { break };
                        match c {
                            NativeCmd::Play => player.send(Cmd::Play),
                            NativeCmd::Pause => player.send(Cmd::Pause),
                            NativeCmd::Toggle => player.send(Cmd::TogglePlay),
                            NativeCmd::Next => player.send(Cmd::Next),
                            NativeCmd::Prev => player.send(Cmd::Prev),
                            NativeCmd::SeekMs(ms) => player.send(Cmd::SeekMs(ms)),
                            NativeCmd::Stop => { player.send(Cmd::Pause); }
                            NativeCmd::Resume | NativeCmd::PermissionGranted => { let _ = scan_now.send(()); }
                        }
                    }
                }
            }
        })
        .expect("spawn event thread");
}

fn on_player_event(ev: Event, platform: &Arc<dyn Platform>, shared: &SharedRef, weak: &slint::Weak<MainWindow>, cover_dir: &Path) {
    match ev {
        Event::TrackChanged { track, index: _ } => {
            let (liked, intro_id) = {
                let mut s = shared.lock().unwrap();
                s.now = Some(track.clone());
                s.now_pos_ms = 0;
                s.intro_gen += 1;
                (s.liked.contains(&track.id), s.intro_gen)
            };
            let sh_timer = shared.clone();
            // the first time music plays, ask (once) for the notification permission (Android 13+)
            let ask = {
                let mut s = shared.lock().unwrap();
                let a = !s.notif_perm_asked;
                s.notif_perm_asked = true;
                a
            };
            if ask {
                platform.request_notification_permission();
            }
            let cover_path = if track.cover_key.is_empty() {
                None
            } else {
                let _ = covers::ensure(cover_dir, &track.path, &track.cover_key);
                let p = covers::cache_path(cover_dir, &track.cover_key);
                p.exists().then(|| p.to_string_lossy().to_string())
            };
            platform.update_now_playing(&NowPlayingInfo {
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                duration_ms: track.duration_ms,
                position_ms: 0,
                playing: true,
                cover_path,
            });
            let weak2 = weak.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let st = ui.global::<AppState>();
                st.set_has_track(true);
                st.set_np_title(SharedString::from(track.title.as_str()));
                st.set_np_artist(SharedString::from(track.artist.as_str()));
                st.set_np_cover(SharedString::from(track.cover_key.as_str()));
                st.set_np_hue(hue_of(track.id));
                st.set_np_id(track.id as i32);
                st.set_liked(liked);
                st.set_t_cur(SharedString::from("0:00"));
                st.set_t_dur(SharedString::from(fmt_time(track.duration_ms).as_str()));
                st.set_progress(0.0);
                // orbit ring for the first 5 seconds only
                start_intro(weak2.clone(), sh_timer.clone(), intro_id);
            });
        }
        Event::PlayState { playing } => {
            let (np, pos) = {
                let mut s = shared.lock().unwrap();
                s.playing = playing;
                (s.now.clone(), s.now_pos_ms)
            };
            if let Some(t) = np {
                platform.update_now_playing(&NowPlayingInfo {
                    title: t.title,
                    artist: t.artist,
                    album: t.album,
                    duration_ms: t.duration_ms,
                    position_ms: pos,
                    playing,
                    cover_path: if t.cover_key.is_empty() { None } else { Some(covers::cache_path(cover_dir, &t.cover_key).to_string_lossy().to_string()) },
                });
            }
            let sh_timer = shared.clone();
            let weak2 = weak.clone();
            let intro_id = {
                let mut s = shared.lock().unwrap();
                s.intro_gen += 1;
                s.intro_gen
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let st = ui.global::<AppState>();
                st.set_playing(playing);
                if playing {
                    start_intro(weak2, sh_timer, intro_id);
                } else {
                    st.set_intro(false);
                }
            });
        }
        Event::Position { ms, dur_ms } => {
            shared.lock().unwrap().now_pos_ms = ms;
            let frac = if dur_ms > 0 { (ms as f32 / dur_ms as f32).clamp(0.0, 1.0) } else { 0.0 };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let st = ui.global::<AppState>();
                st.set_progress(frac);
                st.set_t_cur(SharedString::from(fmt_time(ms).as_str()));
            });
        }
        Event::QueueEnded => {
            platform.stop_playback_service();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.global::<AppState>().set_playing(false);
            });
        }
    }
}

// ------------------------------------------------------------------------------------------
// UI callbacks (UI thread)
// ------------------------------------------------------------------------------------------
fn wire_callbacks(ui: &MainWindow, shared: SharedRef, player: PlayerHandle, params: Arc<EqParams>, db_path: PathBuf, _platform: Arc<dyn Platform>) {
    let st = ui.global::<AppState>();
    let weak = ui.as_weak();

    // ---- library -------------------------------------------------------------------------------------------
    {
        let (w, sh) = (weak.clone(), shared.clone());
        st.on_scrolled(move |t| {
            if let Some(ui) = w.upgrade() {
                scroll_window(&ui, &sh, t);
            }
        });
    }
    {
        let (w, sh) = (weak.clone(), shared.clone());
        st.on_set_tab(move |t| {
            if let Some(ui) = w.upgrade() {
                {
                    let mut s = sh.lock().unwrap();
                    s.tab = t;
                    s.pl_view = None;
                    s.scroll_t = 0.0;
                }
                ui.global::<AppState>().set_tab(t);
                refresh_ui(&ui, &sh);
            }
        });
    }
    {
        let (w, sh) = (weak.clone(), shared.clone());
        st.on_query_edited(move |q| {
            if let Some(ui) = w.upgrade() {
                sh.lock().unwrap().query = q.to_string();
                refresh_ui(&ui, &sh);
            }
        });
    }
    {
        let (w, sh, p) = (weak.clone(), shared.clone(), player.clone());
        st.on_row_tapped(move |idx| {
            let Some(ui) = w.upgrade() else { return };
            let mut s = sh.lock().unwrap();
            let Some(row) = s.view.get(idx.max(0) as usize).cloned() else { return };
            match row.kind {
                0 => {
                    // play: the queue is exactly what the user sees (search result / playlist / all songs)
                    let (tracks, start) = {
                        let mut tracks: Vec<QTrack> = Vec::new();
                        let mut start = 0usize;
                        for (i, r) in s.view.iter().enumerate() {
                            if r.kind != 0 {
                                continue;
                            }
                            if i == idx.max(0) as usize {
                                start = tracks.len();
                            }
                            if let Some(ti) = r.track_idx {
                                tracks.push(qtrack(&s.tracks[ti]));
                            }
                        }
                        (tracks, start)
                    };
                    s.queue_ids = tracks.iter().map(|t| t.id).collect();
                    p.send(Cmd::SetQueue { tracks, start });
                }
                1 | 2 => {
                    // like the HTML: tapping an album / singer filters the song list by that name
                    s.tab = 0;
                    s.query = row.title.clone();
                    s.scroll_t = 0.0;
                    drop(s);
                    let st = ui.global::<AppState>();
                    st.set_tab(0);
                    st.set_query(SharedString::from(row.title.as_str()));
                    refresh_ui(&ui, &sh);
                }
                _ => {}
            }
        });
    }

    // ---- transport ---------------------------------------------------------------------------------------------
    {
        let p = player.clone();
        st.on_toggle_play(move || p.send(Cmd::TogglePlay));
    }
    {
        let p = player.clone();
        st.on_next(move || p.send(Cmd::Next));
    }
    {
        let p = player.clone();
        st.on_prev(move || p.send(Cmd::Prev));
    }
    {
        let (sh, p) = (shared.clone(), player.clone());
        st.on_seek(move |f| {
            let dur = sh.lock().unwrap().now.as_ref().map(|t| t.duration_ms).unwrap_or(0);
            p.send(Cmd::SeekMs((dur as f32 * f.clamp(0.0, 1.0)) as u64));
        });
    }
    {
        let (w, sh, p, dbp) = (weak.clone(), shared.clone(), player.clone(), db_path.clone());
        st.on_toggle_shuffle(move || {
            let v = {
                let mut s = sh.lock().unwrap();
                s.shuffle = !s.shuffle;
                s.shuffle
            };
            p.send(Cmd::SetShuffle(v));
            save_kv(&dbp, "shuffle", v as i32);
            if let Some(ui) = w.upgrade() {
                ui.global::<AppState>().set_shuffle(v);
            }
        });
    }
    {
        let (w, sh, p, dbp) = (weak.clone(), shared.clone(), player.clone(), db_path.clone());
        st.on_cycle_repeat(move || {
            let v = {
                let mut s = sh.lock().unwrap();
                s.repeat = (s.repeat + 1) % 3;
                s.repeat
            };
            p.send(Cmd::SetRepeat(v as u8));
            save_kv(&dbp, "repeat", v);
            if let Some(ui) = w.upgrade() {
                ui.global::<AppState>().set_repeat(v);
            }
        });
    }
    {
        let (w, sh) = (weak.clone(), shared.clone());
        st.on_toggle_like(move || {
            let mut s = sh.lock().unwrap();
            let Some(id) = s.now.as_ref().map(|t| t.id) else { return };
            let liked = if s.liked.remove(&id) { false } else { s.liked.insert(id) };
            if let Some(ui) = w.upgrade() {
                ui.global::<AppState>().set_liked(liked);
            }
        });
    }

    // ---- appearance (persisted) -------------------------------------------------------------------------------------
    macro_rules! persist_int {
        ($handler:ident, $setter:ident, $key:literal) => {{
            let (w, dbp) = (weak.clone(), db_path.clone());
            st.$handler(move |v| {
                save_kv(&dbp, $key, v as i32);
                if let Some(ui) = w.upgrade() {
                    ui.global::<AppState>().$setter(v);
                }
            });
        }};
    }
    persist_int!(on_set_pal, set_pal, "pal");
    persist_int!(on_set_wall, set_wall, "wall");
    persist_int!(on_set_perf, set_perf, "perf");
    persist_int!(on_set_cover_style, set_cover_style, "cover");
    persist_int!(on_set_eq_look, set_eq_look, "eqlook");
    {
        let (w, dbp) = (weak.clone(), db_path.clone());
        st.on_set_dark(move |v| {
            save_kv(&dbp, "dark", v as i32);
            if let Some(ui) = w.upgrade() {
                ui.global::<AppState>().set_dark(v);
            }
        });
    }

    // ---- equalizer ------------------------------------------------------------------------------------------------------
    {
        let (w, sh, pa, dbp) = (weak.clone(), shared.clone(), params.clone(), db_path.clone());
        st.on_eq_band(move |i, v| {
            let mut s = sh.lock().unwrap();
            if (0..10).contains(&i) {
                s.eq_gains[i as usize] = v.clamp(-12, 12);
                s.eq_preset = 8;
            }
            pa.set_all(s.eq_on, &s.eq_gains, s.eq_bass, s.eq_treble, s.eq_pre);
            save_eq(&dbp, &s);
            if let Some(ui) = w.upgrade() {
                push_eq(&ui, &s);
            }
        });
    }
    {
        let (w, sh, pa, dbp) = (weak.clone(), shared.clone(), params.clone(), db_path.clone());
        st.on_eq_knob(move |k, v| {
            let mut s = sh.lock().unwrap();
            let v = v.clamp(-12, 12);
            match k {
                0 => s.eq_bass = v,
                1 => s.eq_treble = v,
                _ => s.eq_pre = v,
            }
            pa.set_all(s.eq_on, &s.eq_gains, s.eq_bass, s.eq_treble, s.eq_pre);
            save_eq(&dbp, &s);
            if let Some(ui) = w.upgrade() {
                push_eq(&ui, &s);
            }
        });
    }
    {
        let (w, sh, pa, dbp) = (weak.clone(), shared.clone(), params.clone(), db_path.clone());
        st.on_eq_pick_preset(move |i| {
            let mut s = sh.lock().unwrap();
            if (0..8).contains(&i) {
                s.eq_preset = i;
                s.eq_gains = eq::PRESETS[i as usize];
            }
            pa.set_all(s.eq_on, &s.eq_gains, s.eq_bass, s.eq_treble, s.eq_pre);
            save_eq(&dbp, &s);
            if let Some(ui) = w.upgrade() {
                push_eq(&ui, &s);
            }
        });
    }
    {
        let (w, sh, pa, dbp) = (weak.clone(), shared.clone(), params.clone(), db_path.clone());
        st.on_eq_toggle(move || {
            let mut s = sh.lock().unwrap();
            s.eq_on = !s.eq_on;
            pa.set_all(s.eq_on, &s.eq_gains, s.eq_bass, s.eq_treble, s.eq_pre);
            save_eq(&dbp, &s);
            if let Some(ui) = w.upgrade() {
                push_eq(&ui, &s);
            }
        });
    }
    {
        let (w, sh, pa, dbp) = (weak.clone(), shared.clone(), params.clone(), db_path.clone());
        st.on_eq_reset(move || {
            let mut s = sh.lock().unwrap();
            s.eq_gains = [0; 10];
            s.eq_bass = 0;
            s.eq_treble = 0;
            s.eq_pre = 0;
            s.eq_preset = 0;
            pa.set_all(s.eq_on, &s.eq_gains, 0, 0, 0);
            save_eq(&dbp, &s);
            if let Some(ui) = w.upgrade() {
                push_eq(&ui, &s);
            }
        });
    }

    // ---- playlists -----------------------------------------------------------------------------------------------------------
    {
        let (w, sh) = (weak.clone(), shared.clone());
        st.on_pl_open(move |id| {
            if let Some(ui) = w.upgrade() {
                {
                    let mut s = sh.lock().unwrap();
                    s.pl_view = if id < 0 { None } else { Some(id as i64) };
                    s.scroll_t = 0.0;
                }
                refresh_ui(&ui, &sh);
            }
        });
    }
    {
        let (w, sh) = (weak.clone(), shared.clone());
        st.on_pl_new(move || {
            if let Some(ui) = w.upgrade() {
                {
                    let mut s = sh.lock().unwrap();
                    s.draft_id = None;
                    s.draft_name.clear();
                    s.draft_sel.clear();
                    s.del_armed = false;
                    push_editor(&ui, &s);
                }
                let st = ui.global::<AppState>();
                st.set_pl_editing(false);
                st.set_pl_name(SharedString::default());
                st.set_show_pl_editor(true);
            }
        });
    }
    {
        let (w, sh) = (weak.clone(), shared.clone());
        st.on_pl_edit(move || {
            if let Some(ui) = w.upgrade() {
                {
                    let mut s = sh.lock().unwrap();
                    let Some(pid) = s.pl_view else { return };
                    let Some(p) = s.playlists.iter().find(|p| p.id == pid).cloned() else { return };
                    s.draft_id = Some(p.id);
                    s.draft_name = p.name.clone();
                    s.draft_sel = p.track_ids.clone();
                    s.del_armed = false;
                    push_editor(&ui, &s);
                    ui.global::<AppState>().set_pl_name(SharedString::from(p.name.as_str()));
                }
                let st = ui.global::<AppState>();
                st.set_pl_editing(true);
                st.set_show_pl_editor(true);
            }
        });
    }
    {
        let (w, sh) = (weak.clone(), shared.clone());
        st.on_pl_toggle_pick(move |idx| {
            if let Some(ui) = w.upgrade() {
                let mut s = sh.lock().unwrap();
                let Some(id) = s.tracks.get(idx.max(0) as usize).map(|t| t.id) else { return };
                if let Some(pos) = s.draft_sel.iter().position(|x| *x == id) {
                    s.draft_sel.remove(pos);
                } else {
                    s.draft_sel.push(id);
                }
                push_editor(&ui, &s);
            }
        });
    }
    {
        let (w, sh) = (weak.clone(), shared.clone());
        st.on_pl_name_edited(move |name| {
            if let Some(ui) = w.upgrade() {
                let mut s = sh.lock().unwrap();
                s.draft_name = name.to_string();
                ui.global::<AppState>().set_pl_can_save(!s.draft_name.trim().is_empty());
            }
        });
    }
    {
        let (w, sh, dbp) = (weak.clone(), shared.clone(), db_path.clone());
        st.on_pl_save(move || {
            let Some(ui) = w.upgrade() else { return };
            let (id, name, sel) = {
                let s = sh.lock().unwrap();
                (s.draft_id, s.draft_name.trim().to_string(), s.draft_sel.clone())
            };
            if name.is_empty() {
                return;
            }
            if let Ok(mut db) = Db::open(&dbp) {
                match db.save_playlist(id, &name, &sel) {
                    Ok(pid) => {
                        let mut s = sh.lock().unwrap();
                        s.playlists = db.playlists().unwrap_or_default();
                        s.tab = 3;
                        if id.is_none() {
                            s.pl_view = None;
                        } else {
                            s.pl_view = Some(pid);
                        }
                    }
                    Err(e) => log::warn!("save playlist: {e}"),
                }
            }
            let st = ui.global::<AppState>();
            st.set_show_pl_editor(false);
            st.set_tab(3);
            refresh_ui(&ui, &sh);
        });
    }
    {
        let (w, sh, dbp) = (weak.clone(), shared.clone(), db_path.clone());
        st.on_pl_delete(move || {
            let Some(ui) = w.upgrade() else { return };
            let armed = sh.lock().unwrap().del_armed;
            if !armed {
                let mut s = sh.lock().unwrap();
                s.del_armed = true;
                ui.global::<AppState>().set_pl_del_armed(true);
                return;
            }
            let id = sh.lock().unwrap().draft_id;
            if let (Some(id), Ok(mut db)) = (id, Db::open(&dbp)) {
                let _ = db.delete_playlist(id);
                let mut s = sh.lock().unwrap();
                s.playlists = db.playlists().unwrap_or_default();
                s.pl_view = None;
                s.del_armed = false;
            }
            ui.global::<AppState>().set_show_pl_editor(false);
            refresh_ui(&ui, &sh);
        });
    }
    {
        let (w, sh) = (weak.clone(), shared.clone());
        st.on_pl_close_editor(move || {
            if let Some(ui) = w.upgrade() {
                sh.lock().unwrap().del_armed = false;
                let st = ui.global::<AppState>();
                st.set_pl_del_armed(false);
                st.set_show_pl_editor(false);
            }
        });
    }
    {
        let (w, sh, p) = (weak.clone(), shared.clone(), player.clone());
        st.on_pl_play(move |shuffle| {
            let mut s = sh.lock().unwrap();
            let tracks: Vec<QTrack> = s.view.iter().filter_map(|r| r.track_idx.map(|i| qtrack(&s.tracks[i]))).collect();
            if tracks.is_empty() {
                return;
            }
            s.queue_ids = tracks.iter().map(|t| t.id).collect();
            let start = if shuffle { (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as usize).unwrap_or(0)) % tracks.len() } else { 0 };
            if shuffle {
                s.shuffle = true;
                p.send(Cmd::SetShuffle(true));
                if let Some(ui) = w.upgrade() {
                    ui.global::<AppState>().set_shuffle(true);
                }
            }
            p.send(Cmd::SetQueue { tracks, start });
        });
    }
}
