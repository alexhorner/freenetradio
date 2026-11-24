//! Bass / mid / treble equaliser: three biquad filters on the decoded PCM,
//! before the volume. Bass is a low shelf at 200 Hz, mid a broad peak at
//! 1 kHz, treble a high shelf at 4 kHz (RBJ audio-EQ-cookbook designs), each
//! +-MAX_DB in 1 dB steps. The settings menu sets the gains (menu.rs); the
//! audio task owns an `Eq` and calls `update` / `process` (radio.rs).
//!
//! Boosting would clip loud music, so the input is turned down by the
//! largest boost (the volume control makes it up). With every gain at 0 the
//! filters are bypassed entirely: no CPU, bit-exact output.
//!
//! There's no libm on this target, so the few functions the filter design
//! needs (exp2, sin, cos, sqrt) are small approximations, good to ~1e-5.

use portable_atomic::{AtomicI8, AtomicU8, Ordering};

pub const MAX_DB: i32 = 10;

static GAINS: [AtomicI8; 3] = [AtomicI8::new(0), AtomicI8::new(0), AtomicI8::new(0)];

/// A band's gain in dB.
pub fn gain(band: usize) -> i32 {
    GAINS[band].load(Ordering::Relaxed) as i32
}
pub fn set_gain(band: usize, db: i32) {
    GAINS[band].store(db.clamp(-MAX_DB, MAX_DB) as i8, Ordering::Relaxed);
}
pub fn gains() -> [i8; 3] {
    [GAINS[0].load(Ordering::Relaxed), GAINS[1].load(Ordering::Relaxed), GAINS[2].load(Ordering::Relaxed)]
}

/// Built-in presets: name and the bass / mid / treble gains in dB.
pub const PRESETS: [(&str, [i8; 3]); 6] = [
    ("Flat", [0, 0, 0]),
    ("Bass Boost", [6, 0, 0]),
    ("Treble Boost", [0, 0, 6]),
    ("Vocal", [-2, 3, 2]),
    ("Rock", [5, -2, 4]),
    ("Warm", [3, 0, -3]),
];

/// The preset switcher also has a seventh, "Custom", entry after the
/// built-ins: it holds whatever gains the user last set by hand (flat until
/// they set some), so it can be switched away from and back to.
const CUSTOM: u8 = PRESETS.len() as u8;

/// Which preset the switcher is on, and the custom gains. Tracked explicitly
/// rather than inferred from the gains, because Custom can hold the same
/// gains as a built-in (it starts out flat).
static SELECTED: AtomicU8 = AtomicU8::new(0);
static CUSTOM_GAINS: [AtomicI8; 3] = [AtomicI8::new(0), AtomicI8::new(0), AtomicI8::new(0)];

pub fn selected() -> u8 {
    SELECTED.load(Ordering::Relaxed)
}
pub fn custom() -> [i8; 3] {
    [CUSTOM_GAINS[0].load(Ordering::Relaxed), CUSTOM_GAINS[1].load(Ordering::Relaxed), CUSTOM_GAINS[2].load(Ordering::Relaxed)]
}

/// The switcher's current entry: a built-in's name, or "Custom".
pub fn preset_name() -> &'static str {
    PRESETS.get(selected() as usize).map_or("Custom", |p| p.0)
}

/// Switch to the next (+1) or previous (-1) entry and apply its gains.
pub fn step_preset(step: i32) {
    let n = PRESETS.len() as i32 + 1;
    let sel = (selected() as i32 + step).rem_euclid(n) as u8;
    let gains = PRESETS.get(sel as usize).map_or_else(custom, |p| p.1);
    for (band, &db) in gains.iter().enumerate() {
        set_gain(band, db as i32);
    }
    SELECTED.store(sel, Ordering::Relaxed);
}

/// Change one band by hand: the gains become the Custom preset, which is
/// then selected.
pub fn adjust_gain(band: usize, step: i32) {
    set_gain(band, gain(band) + step);
    let g = gains();
    for (c, &db) in CUSTOM_GAINS.iter().zip(g.iter()) {
        c.store(db, Ordering::Relaxed);
    }
    SELECTED.store(CUSTOM, Ordering::Relaxed);
}

/// Load saved state: the gains in effect, the switcher entry, the custom gains.
pub fn restore(gains: [i8; 3], selected: u8, custom: [i8; 3]) {
    for (band, &db) in gains.iter().enumerate() {
        set_gain(band, db as i32);
    }
    for (c, &db) in CUSTOM_GAINS.iter().zip(custom.iter()) {
        c.store(db.clamp(-MAX_DB as i8, MAX_DB as i8), Ordering::Relaxed);
    }
    SELECTED.store(selected.min(CUSTOM), Ordering::Relaxed);
}

// ---- Math ---------------------------------------------------------------

/// 2^x for |x| < ~100.
fn exp2(x: f32) -> f32 {
    let mut n = x as i32; // truncates towards zero
    if (n as f32) > x {
        n -= 1; // floor
    }
    let f = x - n as f32; // 0.0 ..< 1.0
    let p = 1.0 + f * (0.693_147_2 + f * (0.240_226_5 + f * (0.055_504_1 + f * (0.009_618_1 + f * 0.001_333_4))));
    p * f32::from_bits(((n + 127) as u32) << 23)
}

fn sqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut y = f32::from_bits((x.to_bits() >> 1) + 0x1FBD_1DF5);
    for _ in 0..3 {
        y = 0.5 * (y + x / y);
    }
    y
}

const PI: f32 = core::f32::consts::PI;

/// (sin, cos) of x in 0..=PI.
fn sin_cos(x: f32) -> (f32, f32) {
    // Reduce to 0..=PI/2: sin(x) = sin(PI - x), cos(x) = -cos(PI - x).
    let (t, flip) = if x > PI / 2.0 { (PI - x, true) } else { (x, false) };
    let t2 = t * t;
    let sin = t * (1.0 - t2 / 6.0 * (1.0 - t2 / 20.0 * (1.0 - t2 / 42.0 * (1.0 - t2 / 72.0))));
    let cos = 1.0 - t2 / 2.0 * (1.0 - t2 / 12.0 * (1.0 - t2 / 30.0 * (1.0 - t2 / 56.0 * (1.0 - t2 / 90.0))));
    (sin, if flip { -cos } else { cos })
}

// ---- Filter design ------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    LowShelf,
    Peak,
    HighShelf,
}

/// Normalised biquad coefficients [b0, b1, b2, a1, a2] (RBJ cookbook).
fn design(kind: Kind, freq: f32, rate: f32, db: i32) -> [f32; 5] {
    let a = exp2(db as f32 * 0.083_048_2); // 10^(dB / 40)
    let (sin, cos) = sin_cos(2.0 * PI * freq / rate);
    let (b0, b1, b2, a0, a1, a2);
    match kind {
        Kind::Peak => {
            let alpha = sin / (2.0 * 0.7); // Q = 0.7: broad
            b0 = 1.0 + alpha * a;
            b1 = -2.0 * cos;
            b2 = 1.0 - alpha * a;
            a0 = 1.0 + alpha / a;
            a1 = -2.0 * cos;
            a2 = 1.0 - alpha / a;
        }
        Kind::LowShelf | Kind::HighShelf => {
            // Shelf slope 1: alpha = sin / sqrt(2).
            let two_sqrt_a_alpha = 2.0 * sqrt(a) * sin * 0.707_106_8;
            let (ap, am) = (a + 1.0, a - 1.0);
            if matches!(kind, Kind::LowShelf) {
                b0 = a * (ap - am * cos + two_sqrt_a_alpha);
                b1 = 2.0 * a * (am - ap * cos);
                b2 = a * (ap - am * cos - two_sqrt_a_alpha);
                a0 = ap + am * cos + two_sqrt_a_alpha;
                a1 = -2.0 * (am + ap * cos);
                a2 = ap + am * cos - two_sqrt_a_alpha;
            } else {
                b0 = a * (ap + am * cos + two_sqrt_a_alpha);
                b1 = -2.0 * a * (am + ap * cos);
                b2 = a * (ap + am * cos - two_sqrt_a_alpha);
                a0 = ap - am * cos + two_sqrt_a_alpha;
                a1 = 2.0 * (am - ap * cos);
                a2 = ap - am * cos - two_sqrt_a_alpha;
            }
        }
    }
    let inv = 1.0 / a0;
    [b0 * inv, b1 * inv, b2 * inv, a1 * inv, a2 * inv]
}

const BANDS: [(Kind, f32); 3] = [(Kind::LowShelf, 200.0), (Kind::Peak, 1000.0), (Kind::HighShelf, 4000.0)];

// ---- The equaliser ------------------------------------------------------

pub struct Eq {
    coef: [[f32; 5]; 3],
    /// Transposed direct form II state: [channel][band][z1, z2].
    state: [[[f32; 2]; 3]; 2],
    /// Input scale (turns the signal down by the largest boost).
    pre: f32,
    /// The gains and sample rate the coefficients were made for.
    gains: [i8; 3],
    rate: u32,
    active: bool,
}

impl Eq {
    pub const fn new() -> Self {
        Self { coef: [[0.0; 5]; 3], state: [[[0.0; 2]; 3]; 2], pre: 1.0, gains: [0; 3], rate: 0, active: false }
    }

    /// Pick up changed gains or sample rate (cheap when nothing changed).
    pub fn update(&mut self, rate: u32) {
        let gains = gains();
        if gains == self.gains && rate == self.rate {
            return;
        }
        self.gains = gains;
        self.rate = rate;
        self.active = gains != [0; 3] && rate >= 8000;
        if !self.active {
            self.state = [[[0.0; 2]; 3]; 2]; // no stale tail when re-enabled
            return;
        }
        for (i, &(kind, freq)) in BANDS.iter().enumerate() {
            self.coef[i] = design(kind, freq, rate as f32, gains[i] as i32);
        }
        let boost = gains.iter().copied().max().unwrap_or(0).max(0) as f32;
        self.pre = exp2(-boost * 0.166_096_4); // 10^(-boost / 20)
    }

    /// Filter one stereo sample.
    #[inline]
    pub fn process(&mut self, l: i16, r: i16) -> (i16, i16) {
        if !self.active {
            return (l, r);
        }
        (self.channel(0, l), self.channel(1, r))
    }

    #[inline]
    fn channel(&mut self, ch: usize, x: i16) -> i16 {
        let mut v = x as f32 * self.pre;
        for band in 0..3 {
            let c = &self.coef[band];
            let z = &mut self.state[ch][band];
            let y = c[0] * v + z[0];
            z[0] = c[1] * v - c[3] * y + z[1];
            z[1] = c[2] * v - c[4] * y;
            v = y;
        }
        (v as i32).clamp(-32768, 32767) as i16
    }
}
