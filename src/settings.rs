//! Remembers the user's settings across power cycles: the last station, volume
//! and mute, whether it was playing, the clock, display and equaliser
//! choices. The radio comes back as it was left.
//!
//! Two kinds of record, in the 4 KB at 0x0807_0000..0x0807_1000 (the bottom
//! of the 32 KB kept for presets, below slot 99; see presets.rs):
//!
//!   - the settings (a small fixed record) rotate through pages 0..=11;
//!   - the station URL (up to 240 characters) rotates through pages 12..=15.
//!
//! They're separate so a volume or EQ change doesn't rewrite the URL, and
//! changing station doesn't rewrite the settings.
//!
//! Flash wear: a flash page survives on the order of 10,000 erases, so
//! saving on every button press would wear it out quickly while someone
//! flicks through stations or holds volume-up. Instead:
//!
//!   - nothing is saved until the thing has stayed the same for SETTLE
//!     (10 s), and only if it differs from what's already saved, so a burst
//!     of changes costs one write once it's over;
//!   - each kind rotates through its pages, every record carrying a sequence
//!     number; the newest valid one wins at boot. The old record stays valid
//!     until the new one is written, so a power cut mid-write loses nothing.
//!
//! (The chip's flash can only be programmed a whole 256-byte page at a time
//! after erasing it, so one page per save is as small as a record gets.)
//!
//! Settings page (magic "NRC1"):
//!
//!   0 magic   4 sequence (u32 LE)   8 checksum (FNV-1a of 12..)
//!   12 volume  13 muted  14 playing
//!   15 UTC offset (i8, quarter hours)  16 DST enabled  17 status bar info
//!   18 bit 0: hide the clock on the dimmed screen; bits 1-2: visualiser
//!   19 bass  20 mid  21 treble (i8, dB)  22 EQ preset switcher entry
//!   23 custom bass  24 custom mid  25 custom treble (i8, dB)
//!
//! URL page (magic "NRU1"):
//!
//!   0 magic   4 sequence   8 checksum (FNV-1a of the URL bytes)
//!   12 URL length   16.. URL

use embassy_time::{Duration, Instant, Timer};

use crate::clock;
use crate::display;
use crate::eq;
use crate::presets::{self, PAGE};
use crate::radio::{self, Phase};
use crate::text::Text;
use crate::ElapsedSat;

const RING_START: u32 = 0x7_0000;
const SETTINGS_PAGES: u32 = 12;
const URL_PAGES: u32 = 4;
const URL_FIRST_PAGE: u32 = SETTINGS_PAGES;
const SETTINGS_MAGIC: [u8; 4] = *b"NRC1";
const URL_MAGIC: [u8; 4] = *b"NRU1";
const OFF_URL: usize = 16;
pub const URL_MAX: usize = PAGE - OFF_URL;
/// How long something must stay unchanged before it's saved.
const SETTLE: Duration = Duration::from_secs(10);

/// Saves since power-on, failed saves, and the last failure (diagnostics).
pub static SAVES: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);
pub static FAILURES: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);
pub static LAST_ERROR: critical_section::Mutex<core::cell::Cell<&'static str>> = critical_section::Mutex::new(core::cell::Cell::new(""));

#[derive(Clone, Copy, PartialEq, Eq)]
struct Settings {
    volume: u8,
    muted: bool,
    playing: bool,
    /// Time zone offset in quarter hours, and whether DST is enabled.
    utc_qh: i8,
    dst: bool,
    /// Status bar contents (display::INFO_*).
    info: u8,
    /// Show the time on the dimmed idle screen.
    sleep_clock: bool,
    /// Meters / spectrum / cycle (display::VIS_*).
    visualiser: u8,
    /// Equaliser gains in dB: bass, mid, treble; the preset switcher's entry;
    /// and the Custom preset's gains.
    eq: [i8; 3],
    eq_selected: u8,
    eq_custom: [i8; 3],
}

impl Settings {
    /// What should be remembered right now (`status` is radio::status()).
    fn current(status: &radio::Status) -> Self {
        Settings {
            volume: status.volume,
            muted: status.muted,
            // Stopped means the user stopped it. Anything else (including an
            // error) means they last asked for it to play.
            playing: !status.url.is_empty() && status.phase != Phase::Stopped,
            utc_qh: clock::offset_qh() as i8,
            dst: clock::dst_enabled(),
            info: display::status_info(),
            sleep_clock: display::sleep_clock(),
            visualiser: display::visualiser(),
            eq: eq::gains(),
            eq_selected: eq::selected(),
            eq_custom: eq::custom(),
        }
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    // FNV-1a
    bytes.iter().fold(0x811C_9DC5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

fn page_offset(page: u32) -> u32 {
    RING_START + page * PAGE as u32
}

// ---- Settings records ---------------------------------------------------

/// The settings record in settings slot `slot`, if it's valid: (sequence, settings).
fn read_settings(slot: u32) -> Option<(u32, Settings)> {
    let pg = presets::page_at(page_offset(slot));
    if pg[..4] != SETTINGS_MAGIC || u32::from_le_bytes(pg[8..12].try_into().unwrap()) != checksum(&pg[12..]) {
        return None; // never written (erased flash), or corrupt
    }
    let max = eq::MAX_DB as i8;
    let gain = |i: usize| (pg[i] as i8).clamp(-max, max);
    let settings = Settings {
        volume: pg[12].min(100),
        muted: pg[13] != 0,
        playing: pg[14] != 0,
        utc_qh: (pg[15] as i8).clamp(-48, 56),
        dst: pg[16] != 0,
        info: pg[17],
        sleep_clock: pg[18] & 1 == 0,
        visualiser: (pg[18] >> 1) & 3,
        eq: [gain(19), gain(20), gain(21)],
        eq_selected: pg[22],
        eq_custom: [gain(23), gain(24), gain(25)],
    };
    Some((u32::from_le_bytes(pg[4..8].try_into().unwrap()), settings))
}

/// The page for `s`, sequence number left 0. Its checksum covers the
/// settings but not the sequence, so it doubles as a fingerprint: equal
/// checksums mean equal settings (near enough, for deciding to save).
fn encode_settings(s: &Settings) -> [u8; PAGE] {
    let mut pg = [0u8; PAGE];
    pg[..4].copy_from_slice(&SETTINGS_MAGIC);
    pg[12] = s.volume;
    pg[13] = s.muted as u8;
    pg[14] = s.playing as u8;
    pg[15] = s.utc_qh as u8;
    pg[16] = s.dst as u8;
    pg[17] = s.info;
    pg[18] = !s.sleep_clock as u8 | (s.visualiser << 1);
    for (i, &db) in s.eq.iter().enumerate() {
        pg[19 + i] = db as u8;
    }
    pg[22] = s.eq_selected;
    for (i, &db) in s.eq_custom.iter().enumerate() {
        pg[23 + i] = db as u8;
    }
    let sum = checksum(&pg[12..]);
    pg[8..12].copy_from_slice(&sum.to_le_bytes());
    pg
}

// ---- URL records --------------------------------------------------------

/// The URL record in URL slot `slot`, if it's valid: (sequence, URL).
fn read_url(slot: u32) -> Option<(u32, Text<URL_MAX>)> {
    let pg = presets::page_at(page_offset(URL_FIRST_PAGE + slot));
    let len = (pg[12] as usize).min(URL_MAX);
    if pg[..4] != URL_MAGIC || u32::from_le_bytes(pg[8..12].try_into().unwrap()) != checksum(&pg[OFF_URL..OFF_URL + len]) {
        return None;
    }
    let mut url = Text::new();
    url.push_sanitized(&pg[OFF_URL..OFF_URL + len]);
    Some((u32::from_le_bytes(pg[4..8].try_into().unwrap()), url))
}

/// The page for `url` (sequence left 0).
fn encode_url(url: &[u8]) -> [u8; PAGE] {
    let mut pg = [0u8; PAGE];
    pg[..4].copy_from_slice(&URL_MAGIC);
    pg[8..12].copy_from_slice(&checksum(url).to_le_bytes());
    pg[12] = url.len() as u8;
    pg[OFF_URL..OFF_URL + url.len()].copy_from_slice(url);
    pg
}

// ---- Startup ------------------------------------------------------------

/// Restore the saved settings and the station into the player (call before
/// the stream task starts; it starts playing then if it was playing before).
pub fn restore() {
    let settings = (0..SETTINGS_PAGES).filter_map(read_settings).max_by_key(|&(seq, _)| seq).map(|(_, s)| s);
    let url = (0..URL_PAGES).filter_map(read_url).max_by_key(|&(seq, _)| seq).map(|(_, u)| u);
    if let Some(s) = &settings {
        clock::set_offset_qh(s.utc_qh as i32);
        clock::set_dst_enabled(s.dst);
        display::set_status_info(s.info);
        display::set_sleep_clock(s.sleep_clock);
        display::set_visualiser(s.visualiser);
        eq::restore(s.eq, s.eq_selected, s.eq_custom);
    }
    if settings.is_some() || url.is_some() {
        let url = url.unwrap_or(Text::new());
        let (volume, muted, playing) = match &settings {
            Some(s) => (s.volume, s.muted, s.playing),
            None => (radio::status().volume, false, false),
        };
        radio::restore(url.as_str(), volume, muted, playing);
    }
}

// ---- Saving -------------------------------------------------------------

/// One ring of pages being saved to: where it is, what's saved, and what's
/// waiting to settle.
struct Ring {
    first_page: u32,
    pages: u32,
    /// The page (within the ring) last written, and its sequence number.
    slot: u32,
    seq: u32,
    /// Fingerprint of what's saved (None: nothing valid yet).
    saved: Option<u32>,
    /// Fingerprint seen last check, and since when it has been unchanged.
    pending: u32,
    changed_at: Instant,
}

impl Ring {
    /// Find the newest valid record (`fingerprint_of` gives a page's
    /// fingerprint if it's valid) and start tracking `now`.
    fn new(first_page: u32, pages: u32, valid: impl Fn(u32) -> Option<(u32, u32)>, now: u32) -> Self {
        let (slot, seq, saved) = (0..pages)
            .filter_map(|slot| valid(slot).map(|(seq, fp)| (slot, seq, fp)))
            .max_by_key(|&(_, seq, _)| seq)
            .map_or((pages - 1, 0, None), |(slot, seq, fp)| (slot, seq, Some(fp)));
        Ring { first_page, pages, slot, seq, saved, pending: now, changed_at: Instant::now() }
    }

    /// Call about once a second with the current fingerprint; `page` builds
    /// the record to save (with sequence number zero) when it's time.
    fn tick(&mut self, now: u32, page: impl FnOnce() -> [u8; PAGE]) {
        if now != self.pending {
            self.pending = now;
            self.changed_at = Instant::now();
        } else if self.saved != Some(now) && self.changed_at.elapsed_sat() >= SETTLE {
            let mut pg = page();
            pg[4..8].copy_from_slice(&self.seq.wrapping_add(1).to_le_bytes());
            // Next page round the ring.
            let next = (self.slot + 1) % self.pages;
            match presets::write_page_at(page_offset(self.first_page + next), &pg) {
                Ok(()) => {
                    self.slot = next;
                    self.seq = self.seq.wrapping_add(1);
                    self.saved = Some(now);
                    SAVES.fetch_add(1, portable_atomic::Ordering::Relaxed);
                }
                // Flash busy (a preset save) or a failed write: try again
                // later rather than spinning on it.
                Err(e) => {
                    FAILURES.fetch_add(1, portable_atomic::Ordering::Relaxed);
                    critical_section::with(|cs| LAST_ERROR.borrow(cs).set(e));
                    self.changed_at = Instant::now();
                }
            }
        }
    }
}

fn settings_fingerprint(s: &Settings) -> u32 {
    let pg = encode_settings(s);
    u32::from_le_bytes(pg[8..12].try_into().unwrap())
}

/// The URL to remember: the playing one, unless it's too long to store (a
/// cut one wouldn't work).
fn url_to_save(status: &radio::Status) -> &[u8] {
    let url = status.url.as_bytes();
    if url.len() <= URL_MAX { url } else { &[] }
}

/// Saves the settings and the URL once each has settled (see the module
/// docs). Only fingerprints are kept between checks, not copies of them
/// (RAM is tight).
#[embassy_executor::task]
pub async fn save_task() -> ! {
    let (mut cfg, mut url) = {
        let status = radio::status();
        let cfg = Ring::new(
            0,
            SETTINGS_PAGES,
            |slot| {
                let pg = presets::page_at(page_offset(slot));
                read_settings(slot).map(|(seq, _)| (seq, u32::from_le_bytes(pg[8..12].try_into().unwrap())))
            },
            settings_fingerprint(&Settings::current(&status)),
        );
        let url = Ring::new(
            URL_FIRST_PAGE,
            URL_PAGES,
            |slot| {
                let pg = presets::page_at(page_offset(URL_FIRST_PAGE + slot));
                read_url(slot).map(|(seq, _)| (seq, u32::from_le_bytes(pg[8..12].try_into().unwrap())))
            },
            checksum(url_to_save(&status)),
        );
        (cfg, url)
    };
    loop {
        Timer::after_secs(1).await;
        let status = radio::status();
        let s = Settings::current(&status);
        cfg.tick(settings_fingerprint(&s), || encode_settings(&s));
        let u = url_to_save(&status);
        // An empty URL isn't worth saving over a real one (a failed play
        // request can clear it); only a non-empty change is remembered.
        if !u.is_empty() {
            url.tick(checksum(u), || encode_url(u));
        }
    }
}
