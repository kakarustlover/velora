//! Playback engine. One dedicated thread owns the audio output (rodio's OutputStream is !Send).
//! The UI and the Android notification talk to it only through `Cmd`s; it answers with `Event`s.
//! Files that cannot be opened/decoded are skipped silently, as requested.

use crate::eq::{EqParams, EqSource};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct QTrack {
    pub id: i64,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub cover_key: String,
}

#[derive(Debug)]
pub enum Cmd {
    /// Replace the queue and start playing `start`.
    SetQueue { tracks: Vec<QTrack>, start: usize },
    TogglePlay,
    Play,
    Pause,
    Next,
    Prev,
    /// Absolute position in milliseconds.
    SeekMs(u64),
    SetShuffle(bool),
    /// 0 = off, 1 = all, 2 = one
    SetRepeat(u8),
    Stop,
}

#[derive(Debug, Clone)]
pub enum Event {
    TrackChanged { track: QTrack, index: usize },
    PlayState { playing: bool },
    Position { ms: u64, dur_ms: u64 },
    QueueEnded,
}

#[derive(Clone)]
pub struct PlayerHandle {
    tx: Sender<Cmd>,
}

impl PlayerHandle {
    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }
}

pub fn spawn(params: Arc<EqParams>, events: Sender<Event>) -> PlayerHandle {
    let (tx, rx) = crossbeam_channel::unbounded::<Cmd>();
    std::thread::Builder::new()
        .name("velora-player".into())
        .spawn(move || Engine::new(rx, events, params).run())
        .expect("spawn player thread");
    PlayerHandle { tx }
}

struct Engine {
    rx: Receiver<Cmd>,
    ev: Sender<Event>,
    params: Arc<EqParams>,
    out: Option<(OutputStream, OutputStreamHandle)>,
    sink: Option<Sink>,
    queue: Vec<QTrack>,
    idx: usize,
    playing: bool,
    shuffle: bool,
    repeat: u8,
    rng: u64,
    last_tick: Instant,
}

impl Engine {
    fn new(rx: Receiver<Cmd>, ev: Sender<Event>, params: Arc<EqParams>) -> Self {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(88172645463325252);
        Engine {
            rx,
            ev,
            params,
            out: None,
            sink: None,
            queue: vec![],
            idx: 0,
            playing: false,
            shuffle: false,
            repeat: 0,
            rng: seed | 1,
            last_tick: Instant::now(),
        }
    }

    fn rand(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn run(mut self) {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(120)) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.tick();
        }
    }

    fn tick(&mut self) {
        let Some(sink) = &self.sink else { return };
        if !self.playing {
            return;
        }
        if sink.empty() {
            self.on_track_end();
            return;
        }
        if self.last_tick.elapsed() >= Duration::from_millis(250) {
            self.last_tick = Instant::now();
            let dur = self.queue.get(self.idx).map(|t| t.duration_ms).unwrap_or(0);
            let _ = self.ev.send(Event::Position { ms: sink.get_pos().as_millis() as u64, dur_ms: dur });
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::SetQueue { tracks, start } => {
                self.queue = tracks;
                self.idx = start.min(self.queue.len().saturating_sub(1));
                if !self.queue.is_empty() {
                    self.start_current(0, true, 1);
                }
            }
            Cmd::TogglePlay => {
                if self.playing {
                    self.set_playing(false)
                } else {
                    self.set_playing(true)
                }
            }
            Cmd::Play => self.set_playing(true),
            Cmd::Pause => self.set_playing(false),
            Cmd::Next => self.advance(false, 1),
            Cmd::Prev => self.previous(),
            Cmd::SeekMs(ms) => {
                if let Some(s) = &self.sink {
                    let _ = s.try_seek(Duration::from_millis(ms));
                    let dur = self.queue.get(self.idx).map(|t| t.duration_ms).unwrap_or(0);
                    let _ = self.ev.send(Event::Position { ms, dur_ms: dur });
                }
            }
            Cmd::SetShuffle(v) => self.shuffle = v,
            Cmd::SetRepeat(v) => self.repeat = v.min(2),
            Cmd::Stop => {
                self.stop_sink();
                self.playing = false;
                let _ = self.ev.send(Event::PlayState { playing: false });
            }
        }
    }

    fn set_playing(&mut self, on: bool) {
        match &self.sink {
            Some(s) => {
                if on {
                    s.play()
                } else {
                    s.pause()
                }
                self.playing = on;
                let _ = self.ev.send(Event::PlayState { playing: on });
            }
            None => {
                // nothing loaded yet: (re)start the current queue entry if there is one
                if on && !self.queue.is_empty() {
                    self.start_current(0, true, 1);
                }
            }
        }
    }

    fn stop_sink(&mut self) {
        if let Some(s) = self.sink.take() {
            s.stop();
        }
    }

    /// Opens queue[idx] and starts it. If the file cannot be decoded we silently move on.
    /// `dir` is the direction used to look for the next playable entry (1 or -1).
    fn start_current(&mut self, seek_ms: u64, autoplay: bool, dir: i32) {
        let n = self.queue.len();
        if n == 0 {
            return;
        }
        for _ in 0..n {
            if self.open(self.idx, seek_ms, autoplay) {
                return;
            }
            // unplayable -> skip silently
            self.idx = ((self.idx as i64 + dir as i64).rem_euclid(n as i64)) as usize;
        }
        // nothing in the queue is playable
        self.stop_sink();
        self.playing = false;
        let _ = self.ev.send(Event::PlayState { playing: false });
        let _ = self.ev.send(Event::QueueEnded);
    }

    fn open(&mut self, idx: usize, seek_ms: u64, autoplay: bool) -> bool {
        let Some(t) = self.queue.get(idx).cloned() else { return false };
        self.stop_sink();
        if self.out.is_none() {
            self.out = OutputStream::try_default().ok();
        }
        let Some((_, handle)) = self.out.as_ref() else { return false };
        let Ok(file) = File::open(&t.path) else { return false };
        let Ok(dec) = Decoder::new(BufReader::new(file)) else { return false };
        let Ok(sink) = Sink::try_new(handle) else { return false };
        let src = EqSource::new(dec.convert_samples::<f32>(), self.params.clone());
        if !autoplay {
            sink.pause();
        }
        sink.append(src);
        if seek_ms > 0 {
            let _ = sink.try_seek(Duration::from_millis(seek_ms));
        }
        self.sink = Some(sink);
        self.playing = autoplay;
        self.last_tick = Instant::now();
        let _ = self.ev.send(Event::TrackChanged { track: t, index: idx });
        let _ = self.ev.send(Event::PlayState { playing: autoplay });
        true
    }

    fn pick_next(&mut self, auto: bool) -> Option<usize> {
        let n = self.queue.len();
        if n == 0 {
            return None;
        }
        if auto && self.repeat == 2 {
            return Some(self.idx);
        }
        if self.shuffle && n > 1 {
            loop {
                let c = (self.rand() % n as u64) as usize;
                if c != self.idx {
                    return Some(c);
                }
            }
        }
        if self.idx + 1 < n {
            Some(self.idx + 1)
        } else if self.repeat == 1 || !auto {
            Some(0)
        } else {
            None
        }
    }

    fn advance(&mut self, auto: bool, dir: i32) {
        match self.pick_next(auto) {
            Some(i) => {
                self.idx = i;
                self.start_current(0, true, dir);
            }
            None => {
                self.stop_sink();
                self.playing = false;
                let _ = self.ev.send(Event::PlayState { playing: false });
                let _ = self.ev.send(Event::QueueEnded);
            }
        }
    }

    fn on_track_end(&mut self) {
        self.advance(true, 1);
    }

    fn previous(&mut self) {
        // like every other player: >3 s into a track restarts it, otherwise go back
        if let Some(s) = &self.sink {
            if s.get_pos() > Duration::from_secs(3) {
                let _ = s.try_seek(Duration::from_millis(0));
                return;
            }
        }
        let n = self.queue.len();
        if n == 0 {
            return;
        }
        self.idx = (self.idx + n - 1) % n;
        self.start_current(0, true, -1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        let (_tx, rx) = crossbeam_channel::unbounded();
        let (etx, _erx) = crossbeam_channel::unbounded();
        let mut e = Engine::new(rx, etx, Arc::new(EqParams::new()));
        e.queue = (0..4).map(|i| QTrack { id: i, ..Default::default() }).collect();
        e
    }

    #[test]
    fn sequential_end_of_queue_rules() {
        let mut e = engine();
        e.idx = 3;
        e.repeat = 0;
        assert_eq!(e.pick_next(true), None); // stops at the end
        e.repeat = 1;
        assert_eq!(e.pick_next(true), Some(0)); // repeat-all wraps
        e.repeat = 2;
        assert_eq!(e.pick_next(true), Some(3)); // repeat-one
        e.repeat = 0;
        assert_eq!(e.pick_next(false), Some(0)); // manual "next" wraps
    }

    #[test]
    fn shuffle_never_repeats_current() {
        let mut e = engine();
        e.shuffle = true;
        e.idx = 2;
        for _ in 0..200 {
            assert_ne!(e.pick_next(true), Some(2));
        }
    }
}
