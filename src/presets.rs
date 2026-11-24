//! Station presets (slots 1..=99), saved in flash so they survive power-off.
//!
//! They live in the top 32 KB of the chip's 480 KB flash, above the firmware
//! (fast code in the first 192 KB, cold code in the slow region after it).
//! The flash is erased and written in 256-byte pages, so each slot is one
//! page and saving one slot never touches the others. Each slot page holds a
//! magic number, checksum, name (<= 32) and URL (<= 212). Layout, relative
//! to BASE = 0x0807_7000 (the original 10-slot area, kept so presets saved
//! before slots 11..99 existed still load):
//!
//!   BASE + 0..=9 pages            slots 1..=10
//!   BASE + 10 pages               marker: present = "use the saved slots"
//!   BASE - 1..=89 pages           slots 11..=99 (growing downwards)
//!
//! Without the marker (a new board, or after "reset to defaults") the
//! built-in list fills slots 1..=10 and the rest are empty. The first save
//! writes slots 1..=10, then the marker, so a power cut mid-save leaves the
//! defaults rather than a half-written set. A slot with a bad checksum (or
//! never written: erased flash) reads as empty.
//!
//! Slots are read straight from memory-mapped flash: no RAM copy.

use core::cell::RefCell;

use critical_section::Mutex;
use embassy_time::Timer;
use hal::flash::{Blocking, Flash};

use crate::hal;
use crate::text::Text;

pub const SLOTS: usize = 99;
/// Slots with built-in defaults (and the original flash layout).
const BASE_SLOTS: usize = 10;
pub const NAME_MAX: usize = 32;
pub const URL_MAX: usize = 212;

pub const PAGE: usize = 256;
/// Offset from the start of flash of slot 1's page (the original area in
/// the last 4 KB). memory.x keeps 0x0807_0000.. free for presets.
const BASE_OFFSET: u32 = (hal::flash::FLASH_SIZE - 4096) as u32;
/// Pseudo slot index for the marker page (BASE + 10 pages).
const MARKER_PAGE: usize = usize::MAX;
// Slot 99 must stay above settings.rs's ring (0x0807_0000..0x0807_1000).
const _: () = assert!(BASE_OFFSET as usize - (SLOTS - BASE_SLOTS) * PAGE >= 0x7_1000);

/// Flash offset of a slot's page (0-based index), or the marker page.
fn page_offset(index: usize) -> u32 {
    if index == MARKER_PAGE {
        BASE_OFFSET + (BASE_SLOTS * PAGE) as u32
    } else if index < BASE_SLOTS {
        BASE_OFFSET + (index * PAGE) as u32
    } else {
        BASE_OFFSET - ((index - BASE_SLOTS + 1) * PAGE) as u32
    }
}

const SLOT_MAGIC: [u8; 4] = *b"NRS1";
const MARKER_MAGIC: [u8; 4] = *b"NRPM";

// Slot page layout.
const OFF_CHECKSUM: usize = 4;
const OFF_NAME_LEN: usize = 8;
const OFF_URL_LEN: usize = 9;
const OFF_NAME: usize = 12;
const OFF_URL: usize = OFF_NAME + NAME_MAX;
const _: () = assert!(OFF_URL + URL_MAX == PAGE);

/// Built-in stations (all checked to serve 128 kbit/s MP3 over plain http).
const DEFAULTS: [(&str, &str); BASE_SLOTS] = [
    ("Groove Salad", "http://ice1.somafm.com/groovesalad-128-mp3"),
    ("Drone Zone", "http://ice1.somafm.com/dronezone-128-mp3"),
    ("DEF CON Radio", "http://ice1.somafm.com/defcon-128-mp3"),
    ("Secret Agent", "http://ice1.somafm.com/secretagent-128-mp3"),
    ("Lush", "http://ice1.somafm.com/lush-128-mp3"),
    ("Indie Pop Rocks", "http://ice1.somafm.com/indiepop-128-mp3"),
    ("Space Station", "http://ice1.somafm.com/spacestation-128-mp3"),
    ("Deep Space One", "http://ice1.somafm.com/deepspaceone-128-mp3"),
    ("Beat Blender", "http://ice1.somafm.com/beatblender-128-mp3"),
    ("Underground 80s", "http://ice1.somafm.com/u80s-128-mp3"),
];

#[derive(Clone, Copy)]
pub struct Preset {
    pub name: Text<NAME_MAX>,
    pub url: Text<URL_MAX>,
}

impl Preset {
    pub fn is_empty(&self) -> bool {
        self.url.is_empty()
    }
}

static FLASH: Mutex<RefCell<Option<Flash<'static, Blocking>>>> = Mutex::new(RefCell::new(None));

/// Bumped whenever presets change, so cached lookups (the display) refresh.
pub static GENERATION: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);

pub fn init(flash: Flash<'static, Blocking>) {
    critical_section::with(|cs| FLASH.borrow_ref_mut(cs).replace(flash));
}

fn page(index: usize) -> &'static [u8] {
    page_at(page_offset(index))
}

/// The 256-byte flash page at `offset` from the start of flash (mapped at
/// 0x0800_0000).
pub fn page_at(offset: u32) -> &'static [u8] {
    unsafe { core::slice::from_raw_parts((0x0800_0000 + offset as usize) as *const u8, PAGE) }
}

fn checksum(bytes: &[u8]) -> u32 {
    // FNV-1a
    bytes.iter().fold(0x811C_9DC5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

fn saved_in_use() -> bool {
    page(MARKER_PAGE)[..4] == MARKER_MAGIC
}

/// Preset `index` (0-based).
pub fn get(index: usize) -> Preset {
    let mut p = Preset { name: Text::new(), url: Text::new() };
    if index >= SLOTS {
        return p;
    }
    if !saved_in_use() {
        if let Some((name, url)) = DEFAULTS.get(index) {
            p.name.push_str(name);
            p.url.push_str(url);
        }
        return p;
    }
    let pg = page(index);
    let stored = u32::from_le_bytes(pg[OFF_CHECKSUM..OFF_CHECKSUM + 4].try_into().unwrap());
    if pg[..4] != SLOT_MAGIC || stored != checksum(&pg[OFF_NAME_LEN..]) {
        return p; // corrupt or never written: empty
    }
    let name_len = (pg[OFF_NAME_LEN] as usize).min(NAME_MAX);
    let url_len = (pg[OFF_URL_LEN] as usize).min(URL_MAX);
    p.name.push_sanitized(&pg[OFF_NAME..OFF_NAME + name_len]);
    p.url.push_sanitized(&pg[OFF_URL..OFF_URL + url_len]);
    p
}

/// The slot whose URL is `url`, if any.
pub fn find(url: &str) -> Option<usize> {
    if url.is_empty() {
        return None;
    }
    (0..SLOTS).find(|&i| get(i).url.as_str() == url)
}

fn encode(name: &str, url: &str) -> [u8; PAGE] {
    let mut pg = [0u8; PAGE];
    let name = &name.as_bytes()[..name.len().min(NAME_MAX)];
    let url = &url.as_bytes()[..url.len().min(URL_MAX)];
    pg[..4].copy_from_slice(&SLOT_MAGIC);
    pg[OFF_NAME_LEN] = name.len() as u8;
    pg[OFF_URL_LEN] = url.len() as u8;
    pg[OFF_NAME..OFF_NAME + name.len()].copy_from_slice(name);
    pg[OFF_URL..OFF_URL + url.len()].copy_from_slice(url);
    let sum = checksum(&pg[OFF_NAME_LEN..]);
    pg[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&sum.to_le_bytes());
    pg
}

/// Erase and rewrite preset page `index`.
fn write_page(index: usize, data: &[u8; PAGE]) -> Result<(), &'static str> {
    write_page_at(page_offset(index), data)
}

/// Erase and rewrite the page at `offset` from the start of flash (also
/// used by settings.rs). Takes ~25 ms; nothing else runs from flash
/// meanwhile (code and constants run from the zero-wait region).
// Never inlined: this must stay in zero-wait flash (freenetradio.x's .slow
// rules don't match presets), not get folded into a caller living in the
// slow flash it writes.
#[inline(never)]
pub fn write_page_at(offset: u32, data: &[u8; PAGE]) -> Result<(), &'static str> {
    let mut flash = critical_section::with(|cs| FLASH.borrow_ref_mut(cs).take()).ok_or("Flash busy")?;
    let result = flash
        .blocking_erase(offset, offset + PAGE as u32)
        .and_then(|_| flash.blocking_write(offset, data))
        .map_err(|_| "Flash write failed");
    critical_section::with(|cs| FLASH.borrow_ref_mut(cs).replace(flash));
    GENERATION.fetch_add(1, portable_atomic::Ordering::Relaxed);
    result?;
    if page_at(offset) != &data[..] {
        return Err("Flash verify failed");
    }
    Ok(())
}

/// Save preset `index` (0-based). An empty URL clears the slot.
pub async fn save(index: usize, name: &str, url: &str) -> Result<(), &'static str> {
    if index >= SLOTS {
        return Err("No such preset");
    }
    if !saved_in_use() {
        // First save: copy the defaults into slots 1..=10, then the marker
        // (higher slots were wiped by reset(), or never written).
        if index >= BASE_SLOTS {
            write_page(index, &encode(name, url))?;
        }
        for i in 0..BASE_SLOTS {
            let (n, u) = if i == index { (name, url) } else { DEFAULTS[i] };
            write_page(i, &encode(n, u))?;
            // Each page blocks for ~25 ms. Pause between them so the audio
            // task can top its ~93 ms buffer back up (decoding runs ~2x
            // real time); a mere yield isn't enough over 11 pages.
            Timer::after_millis(50).await;
        }
        let mut marker = [0u8; PAGE];
        marker[..4].copy_from_slice(&MARKER_MAGIC);
        write_page(MARKER_PAGE, &marker)
    } else {
        write_page(index, &encode(name, url))
    }
}

/// Go back to the built-in list: erase the marker, then wipe any saved
/// slots 11..=99 so they don't reappear after the next save.
pub async fn reset() -> Result<(), &'static str> {
    if saved_in_use() {
        write_page(MARKER_PAGE, &[0u8; PAGE])?;
    }
    for i in BASE_SLOTS..SLOTS {
        if page(i)[..4] == SLOT_MAGIC {
            write_page(i, &[0u8; PAGE])?;
            Timer::after_millis(50).await; // as in save(): keep audio fed
        }
    }
    Ok(())
}
