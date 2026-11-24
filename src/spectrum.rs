//! Spectrum analyser data. The (patched) MP3 decoder sums the energy of its
//! frequency lines into `BANDS` bands per frame (see vendor/nanomp3-core);
//! `publish` turns that into bar heights on a dB scale and hands them to the
//! display, which draws them (display.rs).

use core::cell::Cell;

use critical_section::Mutex;
use embassy_time::{Duration, Instant};

use crate::ElapsedSat;

pub const BANDS: usize = nanomp3_core::SPECTRUM_BANDS;

/// The bar scale: a band's mean power (the decoder's units) in dB maps
/// DB_FLOOR..DB_FLOOR + DB_RANGE onto 0..=255.
const DB_FLOOR: f32 = SPECTRUM_FLOOR_DB;
const DB_RANGE: f32 = SPECTRUM_RANGE_DB;
pub const SPECTRUM_FLOOR_DB: f32 = -62.0;
pub const SPECTRUM_RANGE_DB: f32 = 40.0;

/// Bar heights (0..=255) and when they were published.
static BARS: Mutex<Cell<([u8; BANDS], Option<Instant>)>> = Mutex::new(Cell::new(([0; BANDS], None)));
/// Newer data than this is "live"; older means the decoder has stopped.
const FRESH: Duration = Duration::from_millis(120);

/// Debug: the raw dB of a few bands for the last frame, for /stats.
#[cfg(feature = "diag")]
pub static DEBUG_DB: Mutex<Cell<[i16; 5]>> = Mutex::new(Cell::new([0; 5]));

/// log2 of a positive, normal f32, accurate to about 0.01 (cubic fit of the
/// mantissa; no libm on this target).
fn log2(x: f32) -> f32 {
    let bits = x.to_bits();
    let e = ((bits >> 23) & 0xFF) as i32 - 127;
    let m = f32::from_bits((bits & 0x007F_FFFF) | 0x3F80_0000); // 1.0 ..< 2.0
    let frac = -1.741_793_9 + (2.821_202_6 + (-1.469_956_8 + (0.447_179_55 - 0.056_570_851 * m) * m) * m) * m;
    e as f32 + frac
}

/// Convert the decoder's band sums (over `n` granule-channels) to bars.
pub fn publish(energy: &[f32; BANDS], n: u32) {
    if n == 0 {
        return;
    }
    let mut bars = [0u8; BANDS];
    #[cfg(feature = "diag")]
    let mut dbg = [0i16; 5];
    for (_i, (&e, bar)) in energy.iter().zip(bars.iter_mut()).enumerate() {
        let p = e / n as f32;
        if p > 1e-8 {
            let db = 3.0103 * log2(p);
            *bar = ((db - DB_FLOOR) * 255.0 / DB_RANGE).clamp(0.0, 255.0) as u8;
            #[cfg(feature = "diag")]
            if let Some(k) = [0usize, 10, 20, 30, 39].iter().position(|&b| b == _i) {
                dbg[k] = db as i16;
            }
        }
    }
    critical_section::with(|cs| {
        BARS.borrow(cs).set((bars, Some(Instant::now())));
        #[cfg(feature = "diag")]
        DEBUG_DB.borrow(cs).set(dbg);
    });
}

/// The latest bars, or None if there's no live data (not decoding).
pub fn read() -> Option<[u8; BANDS]> {
    let (bars, at) = critical_section::with(|cs| BARS.borrow(cs).get());
    at.filter(|t| t.elapsed_sat() < FRESH).map(|_| bars)
}
