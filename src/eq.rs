//! Real-time equalizer: 10 peaking/shelf bands + bass shelf + treble shelf + preamp.
//! Filters are RBJ "Audio EQ Cookbook" biquads computed in f64 for stability at 31 Hz.
//! The parameters live in atomics so the UI thread can change them while audio runs;
//! the audio thread notices the `version` counter and recomputes coefficients.

use rodio::Source;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const BANDS: [f64; 10] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
const BASS_HZ: f64 = 110.0;
const TREBLE_HZ: f64 = 7500.0;
const Q: f64 = 1.1;
const N_FILTERS: usize = 12; // bass + 10 bands + treble

pub struct EqParams {
    pub on: AtomicBool,
    pub gains: [AtomicI32; 10],
    pub bass: AtomicI32,
    pub treble: AtomicI32,
    pub pre: AtomicI32,
    pub version: AtomicU32,
}

impl EqParams {
    pub fn new() -> Self {
        Self {
            on: AtomicBool::new(true),
            gains: [(); 10].map(|_| AtomicI32::new(0)),
            bass: AtomicI32::new(0),
            treble: AtomicI32::new(0),
            pre: AtomicI32::new(0),
            version: AtomicU32::new(1),
        }
    }
    pub fn bump(&self) {
        self.version.fetch_add(1, Ordering::Release);
    }
    pub fn set_all(&self, on: bool, gains: &[i32; 10], bass: i32, treble: i32, pre: i32) {
        self.on.store(on, Ordering::Relaxed);
        for (a, g) in self.gains.iter().zip(gains.iter()) {
            a.store(*g, Ordering::Relaxed);
        }
        self.bass.store(bass, Ordering::Relaxed);
        self.treble.store(treble, Ordering::Relaxed);
        self.pre.store(pre, Ordering::Relaxed);
        self.bump();
    }
}

impl Default for EqParams {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Coef {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

#[derive(Clone, Copy, Default)]
struct State {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

fn norm(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Coef {
    Coef { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

pub fn peaking(fs: f64, f0: f64, gain_db: f64, q: f64) -> Coef {
    let a = 10f64.powf(gain_db / 40.0);
    let w = 2.0 * std::f64::consts::PI * f0 / fs;
    let (s, c) = w.sin_cos();
    let alpha = s / (2.0 * q);
    norm(1.0 + alpha * a, -2.0 * c, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * c, 1.0 - alpha / a)
}

pub fn low_shelf(fs: f64, f0: f64, gain_db: f64) -> Coef {
    let a = 10f64.powf(gain_db / 40.0);
    let w = 2.0 * std::f64::consts::PI * f0 / fs;
    let (s, c) = w.sin_cos();
    let alpha = s / 2.0 * 2f64.sqrt(); // shelf slope S = 1
    let sa = 2.0 * a.sqrt() * alpha;
    norm(
        a * ((a + 1.0) - (a - 1.0) * c + sa),
        2.0 * a * ((a - 1.0) - (a + 1.0) * c),
        a * ((a + 1.0) - (a - 1.0) * c - sa),
        (a + 1.0) + (a - 1.0) * c + sa,
        -2.0 * ((a - 1.0) + (a + 1.0) * c),
        (a + 1.0) + (a - 1.0) * c - sa,
    )
}

pub fn high_shelf(fs: f64, f0: f64, gain_db: f64) -> Coef {
    let a = 10f64.powf(gain_db / 40.0);
    let w = 2.0 * std::f64::consts::PI * f0 / fs;
    let (s, c) = w.sin_cos();
    let alpha = s / 2.0 * 2f64.sqrt();
    let sa = 2.0 * a.sqrt() * alpha;
    norm(
        a * ((a + 1.0) + (a - 1.0) * c + sa),
        -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
        a * ((a + 1.0) + (a - 1.0) * c - sa),
        (a + 1.0) - (a - 1.0) * c + sa,
        2.0 * ((a - 1.0) - (a + 1.0) * c),
        (a + 1.0) - (a - 1.0) * c - sa,
    )
}

/// Builds the 12 filter coefficient sets for a sample rate (bass, 10 bands, treble).
pub fn build_chain(fs: f64, gains: &[i32; 10], bass: i32, treble: i32) -> [Coef; N_FILTERS] {
    let mut c = [Coef::default(); N_FILTERS];
    c[0] = low_shelf(fs, BASS_HZ, bass as f64);
    for i in 0..10 {
        let f0 = BANDS[i].min(fs * 0.45);
        c[1 + i] = if i == 0 {
            low_shelf(fs, f0, gains[i] as f64)
        } else if i == 9 {
            high_shelf(fs, f0, gains[i] as f64)
        } else {
            peaking(fs, f0, gains[i] as f64, Q)
        };
    }
    c[11] = high_shelf(fs, TREBLE_HZ.min(fs * 0.45), treble as f64);
    c
}

pub struct EqSource<S: Source<Item = f32>> {
    inner: S,
    params: Arc<EqParams>,
    ver: u32,
    coefs: [Coef; N_FILTERS],
    states: Vec<[State; N_FILTERS]>,
    channels: usize,
    idx: usize,
    fs: f64,
    pre: f64,
    active: bool,
}

impl<S: Source<Item = f32>> EqSource<S> {
    pub fn new(inner: S, params: Arc<EqParams>) -> Self {
        let channels = inner.channels().max(1) as usize;
        let fs = inner.sample_rate().max(8000) as f64;
        let mut s = Self {
            inner,
            params,
            ver: 0,
            coefs: [Coef::default(); N_FILTERS],
            states: vec![[State::default(); N_FILTERS]; channels],
            channels,
            idx: 0,
            fs,
            pre: 1.0,
            active: false,
        };
        s.recompute();
        s
    }

    fn recompute(&mut self) {
        self.ver = self.params.version.load(Ordering::Acquire);
        let mut g = [0i32; 10];
        for (i, a) in self.params.gains.iter().enumerate() {
            g[i] = a.load(Ordering::Relaxed);
        }
        let bass = self.params.bass.load(Ordering::Relaxed);
        let treble = self.params.treble.load(Ordering::Relaxed);
        let pre = self.params.pre.load(Ordering::Relaxed);
        let on = self.params.on.load(Ordering::Relaxed);
        self.coefs = build_chain(self.fs, &g, bass, treble);
        self.pre = 10f64.powf(pre as f64 / 20.0);
        // bypass completely (zero CPU) when the equalizer is off or flat
        self.active = on && (g.iter().any(|v| *v != 0) || bass != 0 || treble != 0 || pre != 0);
    }
}

impl<S: Source<Item = f32>> Iterator for EqSource<S> {
    type Item = f32;
    #[inline]
    fn next(&mut self) -> Option<f32> {
        let x = self.inner.next()?;
        if self.params.version.load(Ordering::Acquire) != self.ver {
            self.recompute();
        }
        let ch = self.idx;
        self.idx += 1;
        if self.idx >= self.channels {
            self.idx = 0;
        }
        if !self.active {
            return Some(x);
        }
        let st = &mut self.states[ch];
        let mut v = x as f64;
        for k in 0..N_FILTERS {
            let c = &self.coefs[k];
            let s = &mut st[k];
            let y = c.b0 * v + c.b1 * s.x1 + c.b2 * s.x2 - c.a1 * s.y1 - c.a2 * s.y2;
            s.x2 = s.x1;
            s.x1 = v;
            s.y2 = s.y1;
            s.y1 = y;
            v = y;
        }
        Some((v * self.pre).clamp(-1.0, 1.0) as f32)
    }
}

impl<S: Source<Item = f32>> Source for EqSource<S> {
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        let r = self.inner.try_seek(pos);
        for s in self.states.iter_mut() {
            *s = [State::default(); N_FILTERS];
        }
        self.idx = 0;
        r
    }
}

// ---------------------------------------------------------------------------------
// Presets (same numbers as the HTML design) and the response curve for the UI
// ---------------------------------------------------------------------------------
pub const PRESETS: [[i32; 10]; 8] = [
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],        // Flat
    [7, 6, 5, 3, 1, 0, 0, 0, 0, 0],        // Bass
    [0, 0, 0, 0, 0, 1, 3, 5, 6, 7],        // Treble
    [-2, -2, -1, 1, 3, 4, 4, 3, 1, 0],     // Vocal
    [5, 4, 3, 1, -1, -1, 1, 3, 4, 5],      // Rock
    [-1, 1, 3, 4, 3, 0, -1, -1, 1, 2],     // Pop
    [3, 2, 1, 2, -2, -2, 0, 1, 2, 3],      // Jazz
    [5, 4, 1, 0, -2, 2, 1, 1, 4, 5],       // Electronic
];
pub const PRESET_NAMES: [&str; 9] = ["Flat", "Bass", "Treble", "Vocal", "Rock", "Pop", "Jazz", "Electronic", "Custom"];

/// SVG path (viewBox 320x64) of the band gains, smoothed with Catmull-Rom -> cubic Bezier.
pub fn curve_path(g: &[i32; 10]) -> String {
    let (w, h) = (320.0f64, 64.0f64);
    let pts: Vec<(f64, f64)> =
        g.iter().enumerate().map(|(i, v)| ((i as f64 + 0.5) / 10.0 * w, h / 2.0 - *v as f64 * (h / 2.0 - 6.0) / 12.0)).collect();
    let mut d = format!("M{:.1} {:.1}", pts[0].0, pts[0].1);
    for i in 0..9 {
        let p0 = if i == 0 { pts[0] } else { pts[i - 1] };
        let (p1, p2) = (pts[i], pts[i + 1]);
        let p3 = if i + 2 < 10 { pts[i + 2] } else { p2 };
        let c1 = (p1.0 + (p2.0 - p0.0) / 6.0, p1.1 + (p2.1 - p0.1) / 6.0);
        let c2 = (p2.0 - (p3.0 - p1.0) / 6.0, p2.1 - (p3.1 - p1.1) / 6.0);
        d.push_str(&format!("C{:.1} {:.1} {:.1} {:.1} {:.1} {:.1}", c1.0, c1.1, c2.0, c2.1, p2.0, p2.1));
    }
    d.push_str(&format!("L{:.1} {:.1}L{:.1} {:.1}Z", pts[9].0, h, pts[0].0, h));
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gain_db_at(c: &Coef, fs: f64, f: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * f / fs;
        let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let nr = c.b0 + c.b1 * c1 + c.b2 * c2;
        let ni = -(c.b1 * s1 + c.b2 * s2);
        let dr = 1.0 + c.a1 * c1 + c.a2 * c2;
        let di = -(c.a1 * s1 + c.a2 * s2);
        10.0 * ((nr * nr + ni * ni) / (dr * dr + di * di)).log10()
    }

    #[test]
    fn peaking_hits_requested_gain_at_center() {
        let c = peaking(44100.0, 1000.0, 6.0, 1.1);
        assert!((gain_db_at(&c, 44100.0, 1000.0) - 6.0).abs() < 0.05);
        assert!(gain_db_at(&c, 44100.0, 50.0).abs() < 0.5);
    }

    #[test]
    fn shelves_reach_gain_far_from_corner() {
        let l = low_shelf(48000.0, 110.0, 9.0);
        assert!((gain_db_at(&l, 48000.0, 20.0) - 9.0).abs() < 0.5);
        assert!(gain_db_at(&l, 48000.0, 8000.0).abs() < 0.5);
        let h = high_shelf(48000.0, 7500.0, -6.0);
        assert!((gain_db_at(&h, 48000.0, 20000.0) + 6.0).abs() < 0.7);
    }

    #[test]
    fn curve_path_is_closed_and_has_nine_segments() {
        let p = curve_path(&PRESETS[1]);
        assert!(p.starts_with('M') && p.ends_with('Z'));
        assert_eq!(p.matches('C').count(), 9);
    }
}
