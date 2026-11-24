//! The radio's OLED screen:
//!
//!   Station name (address when stopped)    128 kbps 44.1 kHz / state
//!   ---------------------------------------------------
//!   Station description (scrolls if long)
//!   Now playing title (scrolls if long)
//!   ============  left level
//!   =========     right level
//!
//! Redrawn at 50 fps on a fixed grid while a stream is active, and a few
//! times a second otherwise. Scrolling moves exactly 1 px per drawn frame
//! (not by elapsed time): MP3 decoding takes ~12 ms per frame and can delay
//! a frame, and a 2 px catch-up jump looks worse than a brief slowdown.

use core::cell::RefCell;
use core::fmt::Write as _;
use portable_atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use critical_section::Mutex;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_5X7, FONT_6X10, FONT_6X13_BOLD};
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::Gray4;
use embedded_graphics::prelude::*;
use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embedded_graphics::primitives::{Arc, Circle, Line, PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::text::{Alignment, Baseline, Text, TextStyleBuilder};

use crate::radio::{self, Phase};
#[cfg(feature = "diag")]
use crate::ElapsedSat;
use crate::ssd1322::{Ssd1322, WIDTH};
use crate::text::Text as Buf;

const FRAME_ACTIVE: Duration = Duration::from_millis(20);
const FRAME_IDLE: Duration = Duration::from_millis(200);
const SCROLL_GAP: i32 = 80;

struct Net {
    ip: Option<[u8; 4]>,
    link_up: bool,
}

static NET: Mutex<RefCell<Net>> = Mutex::new(RefCell::new(Net { ip: None, link_up: false }));

/// On-screen message box (volume, preset, info...), shown until `until`.
struct Osd {
    lines: [Buf<64>; 3],
    count: usize,
    big: bool,
    until: Option<Instant>,
}

static OSD: Mutex<RefCell<Osd>> = Mutex::new(RefCell::new(Osd {
    lines: [Buf::new(), Buf::new(), Buf::new()],
    count: 0,
    big: false,
    until: None,
}));
/// Wakes an idle display immediately when a message is posted.
static WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// After this long stopped with no activity, the screen fades down to just
/// the logo at minimum brightness (saves the panel, less glare in a dark
/// room). Any activity brings it straight back.
const DIM_AFTER: Duration = Duration::from_secs(10);
/// How long dimming takes: the logo slides to the centre while the panel
/// fades, both eased in and out.
const DIM_TIME: Duration = Duration::from_millis(1500);
/// How far to dim when idle, 0.0 (not at all) ..= 1.0 (the dimmest the
/// panel settings go; see Ssd1322::set_dim).
/// 0.6 chosen by eye (0.7 and above was too dark on this panel).
const DIM_AMOUNT: f32 = 0.6;
/// The status bar and hint fade out over this fraction of DIM_TIME.
const UI_FADE_PART: f32 = 0.4;
/// Boot animation: the logo fades in alone in the centre, holds, then slides
/// down to its usual place while the rest of the screen fades in over the
/// last UI_FADE_PART of the slide (sleeping, in reverse).
const BOOT_FADE: Duration = Duration::from_millis(700);
const BOOT_HOLD: Duration = Duration::from_millis(200);
const BOOT_SLIDE: Duration = Duration::from_millis(1000);
/// Where the logo sits normally (below the hint text) and when dimmed.
const LOGO_AREA_AWAKE: (i32, i32) = (24, 63);
const LOGO_AREA_DIM: (i32, i32) = (0, 63);

/// What the status bar's right-hand side shows while playing (settings
/// menu). Stored as 0 = both so that records saved before this option
/// existed (a zero byte) keep the old look. Status text such as
/// CONNECTING or BUFFERING is always shown.
static STATUS_INFO: AtomicU8 = AtomicU8::new(INFO_BOTH);
pub const INFO_BOTH: u8 = 0;
pub const INFO_CLOCK: u8 = 1;
pub const INFO_CODEC: u8 = 2;
pub const INFO_OFF: u8 = 3;
/// The order the menu steps through them in.
const INFO_ORDER: [u8; 4] = [INFO_CLOCK, INFO_CODEC, INFO_BOTH, INFO_OFF];

pub fn status_info() -> u8 {
    STATUS_INFO.load(Ordering::Relaxed)
}
pub fn set_status_info(v: u8) {
    STATUS_INFO.store(if v <= INFO_OFF { v } else { INFO_BOTH }, Ordering::Relaxed);
}
/// Step to the next (+1) or previous (-1) choice, wrapping round.
pub fn step_status_info(step: i32) {
    let i = INFO_ORDER.iter().position(|&v| v == status_info()).unwrap_or(0) as i32;
    set_status_info(INFO_ORDER[(i + step).rem_euclid(INFO_ORDER.len() as i32) as usize]);
}
fn status_info_name(v: u8) -> &'static str {
    match v {
        INFO_CLOCK => "Clock",
        INFO_CODEC => "Codec",
        INFO_OFF => "Off",
        _ => "Both",
    }
}

/// Frame capture for screen recordings (diag build, `GET /screen/stream`):
/// while set, the display task hands each finished frame to the web task and
/// waits for it to be copied out, so every drawn frame is captured whole.
/// Animations that run on the clock use the frame count instead, so they
/// play at their true speed however slow the capture is.
#[cfg(feature = "diag")]
pub static CAPTURE: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "diag")]
pub static SNAP_READY: Signal<CriticalSectionRawMutex, ()> = Signal::new();
#[cfg(feature = "diag")]
pub static SNAP_DONE: Signal<CriticalSectionRawMutex, ()> = Signal::new();
#[cfg(feature = "diag")]
static CAPTURE_FRAMES: AtomicU32 = AtomicU32::new(0);

/// Milliseconds for clock-driven animation (the menu pointer's bob).
fn anim_ms() -> u64 {
    #[cfg(feature = "diag")]
    if CAPTURE.load(Ordering::Relaxed) {
        return CAPTURE_FRAMES.load(Ordering::Relaxed) as u64 * FRAME_ACTIVE.as_millis();
    }
    Instant::now().as_millis()
}

/// What fills the bottom of the screen while playing (settings menu): the
/// level meters, the spectrum analyser, or each in turn.
static VISUALISER: AtomicU8 = AtomicU8::new(VIS_METERS);
pub const VIS_METERS: u8 = 0;
pub const VIS_SPECTRUM: u8 = 1;
pub const VIS_CYCLE: u8 = 2;
/// In cycle mode the two alternate this often.
const VIS_CYCLE_PERIOD_MS: u64 = 5000;

pub fn visualiser() -> u8 {
    VISUALISER.load(Ordering::Relaxed)
}
pub fn set_visualiser(v: u8) {
    VISUALISER.store(if v <= VIS_CYCLE { v } else { VIS_METERS }, Ordering::Relaxed);
}
pub fn step_visualiser(step: i32) {
    set_visualiser((visualiser() as i32 + step).rem_euclid(3) as u8);
}
fn visualiser_name(v: u8) -> &'static str {
    match v {
        VIS_SPECTRUM => "Spectrum",
        VIS_CYCLE => "Cycle",
        _ => "Meters",
    }
}
/// Whether the spectrum (rather than the meters) is showing at `now`.
fn spectrum_showing(now: Instant) -> bool {
    match visualiser() {
        VIS_SPECTRUM => true,
        VIS_CYCLE => now.as_millis() / VIS_CYCLE_PERIOD_MS % 2 == 1,
        _ => false,
    }
}

/// Whether the dimmed, logo-only screen also shows the time (settings menu).
static SLEEP_CLOCK: AtomicBool = AtomicBool::new(true);
pub fn sleep_clock() -> bool {
    SLEEP_CLOCK.load(Ordering::Relaxed)
}
pub fn set_sleep_clock(on: bool) {
    SLEEP_CLOCK.store(on, Ordering::Relaxed);
}

/// When something last happened (button, message...), in ms since boot.
static LAST_ACTIVITY_MS: AtomicU32 = AtomicU32::new(0);

/// Note user activity: wakes a dimmed screen and restarts the dim timer.
pub fn wake() {
    LAST_ACTIVITY_MS.store(Instant::now().as_millis() as u32, Ordering::Relaxed);
    WAKE.signal(());
}

/// Show a short message (large text) for 1.5 s.
pub fn show_osd(text: &str) {
    LAST_ACTIVITY_MS.store(Instant::now().as_millis() as u32, Ordering::Relaxed);
    critical_section::with(|cs| {
        let mut o = OSD.borrow_ref_mut(cs);
        o.lines[0] = Buf::from_str(text);
        o.count = 1;
        o.big = text.len() <= 22;
        o.until = Some(Instant::now() + Duration::from_millis(1500));
    });
    WAKE.signal(());
}

/// Whether a message box is currently showing.
fn osd_showing() -> bool {
    crate::menu::is_open() || critical_section::with(|cs| OSD.borrow_ref(cs).until.is_some_and(|t| Instant::now() < t))
}

/// A filled black box with a 1 px grey border and cut corners, drawn straight
/// into the framebuffer. (embedded-graphics' RoundedRectangle tests every
/// pixel against the shape: ~10 ms for a box this size, enough to starve the
/// audio while one is on screen.)
fn draw_box(oled: &mut Ssd1322, x: usize, y: usize, w: usize, h: usize) {
    for yy in y..y + h {
        for xx in x..x + w {
            oled.set_pixel(xx, yy, 0);
        }
    }
    const BORDER: u8 = 10;
    for xx in x + 2..x + w - 2 {
        oled.set_pixel(xx, y, BORDER);
        oled.set_pixel(xx, y + h - 1, BORDER);
    }
    for yy in y + 2..y + h - 2 {
        oled.set_pixel(x, yy, BORDER);
        oled.set_pixel(x + w - 1, yy, BORDER);
    }
    // The corners' diagonals.
    for (dx, dy) in [(1, 1)] {
        oled.set_pixel(x + dx, y + dy, BORDER);
        oled.set_pixel(x + w - 1 - dx, y + dy, BORDER);
        oled.set_pixel(x + dx, y + h - 1 - dy, BORDER);
        oled.set_pixel(x + w - 1 - dx, y + h - 1 - dy, BORDER);
    }
}

/// The menu's pointer: a filled right-pointing triangle (4 px wide, 7 tall,
/// level with the row's capitals) with its left edge at x + `offset` px.
/// The offset can be fractional: the edge pixels get partial brightness
/// (coverage), so it moves smoothly on the 16-level panel.
fn draw_pointer(oled: &mut Ssd1322, x: i32, y: i32, offset: f32) {
    const WIDTHS: [usize; 7] = [1, 2, 3, 4, 3, 2, 1];
    let whole = offset as i32;
    let frac = offset - whole as f32;
    let level = |coverage: f32| (15.0 * coverage + 0.5) as u8;
    for (i, &w) in WIDTHS.iter().enumerate() {
        let yy = (y + 1 + i as i32) as usize;
        let x0 = x + whole;
        for k in 0..=w {
            // Coverage of pixel k: the span runs from `frac` to `w + frac`.
            let cov = if k == 0 { 1.0 - frac } else if k == w { frac } else { 1.0 };
            if cov > 0.0 {
                oled.set_pixel((x0 + k as i32) as usize, yy, level(cov));
            }
        }
    }
}

/// The settings menu, if it's open: a page filling everything below the
/// status bar. The main list scrolls to keep the cursor in view, with a
/// scrollbar down the right edge; the Equaliser page is a heading over the
/// preset switcher and the three bands.
fn draw_menu(oled: &mut Ssd1322) -> bool {
    use crate::menu::{Page, EQ_PRESET, EQ_ROWS, ROWS, ROW_ABOUT, ROW_ADDRESS, ROW_DST, ROW_EQ, ROW_INFO, ROW_SLEEP_CLOCK, ROW_VISUAL, ROW_ZONE};
    const VISIBLE: u8 = 5;
    const TOP: i32 = 13;
    const PITCH: i32 = 10;
    let Some((page, selected)) = crate::menu::view() else {
        return false;
    };
    if page == Page::About {
        return draw_about(oled, selected);
    }
    let left = TextStyleBuilder::new().baseline(Baseline::Top).build();
    let right = TextStyleBuilder::new().baseline(Baseline::Top).alignment(Alignment::Right).build();
    let (x_cursor, x_label, x_value) = (1, 10, WIDTH as i32 - 8);
    // The pointer bobs 0..3 px right and back, eased, every BOB_MS.
    const BOB_MS: u64 = 600;
    let phase = (anim_ms() % BOB_MS) as f32 / BOB_MS as f32;
    let tri = if phase < 0.5 { phase * 2.0 } else { 2.0 - phase * 2.0 };
    let bob = 3.0 * tri * tri * (3.0 - 2.0 * tri);

    let (rows, first, y0) = match page {
        Page::Main => (ROWS, selected.saturating_sub(VISIBLE - 1).min(ROWS - VISIBLE), TOP),
        Page::About => unreachable!(),
        Page::Equaliser => {
            let dim = MonoTextStyle::new(&FONT_6X10, Gray4::new(6));
            let _ = Text::with_text_style("EQUALISER", Point::new(x_label, TOP), dim, left).draw(oled);
            (EQ_ROWS, 0, TOP + PITCH)
        }
    };
    let shown = rows.min(VISIBLE);
    for row in first..first + shown {
        let y = y0 + (row - first) as i32 * PITCH;
        let on = row == selected;
        let text = MonoTextStyle::new(&FONT_6X10, if on { Gray4::WHITE } else { Gray4::new(9) });
        let mut value = Buf::<24>::new();
        // (label, adjustable with left/right, opens a submenu)
        let (label, adjustable, submenu) = match (page, row) {
            (Page::Main, ROW_VISUAL) => {
                value.push_str(visualiser_name(visualiser()));
                ("Visualiser", true, false)
            }
            (Page::Main, ROW_EQ) => {
                value.push_str(crate::eq::preset_name());
                ("Equaliser", false, true)
            }
            (Page::Main, ROW_ZONE) => {
                let mins = crate::clock::offset_qh() * 15;
                let _ = write!(value, "UTC{}{:02}:{:02}", if mins < 0 { '-' } else { '+' }, mins.abs() / 60, mins.abs() % 60);
                ("Time zone", true, false)
            }
            (Page::Main, ROW_DST) => {
                value.push_str(if crate::clock::dst_enabled() { "On" } else { "Off" });
                ("Daylight saving", true, false)
            }
            (Page::Main, ROW_INFO) => {
                value.push_str(status_info_name(status_info()));
                ("Status bar", true, false)
            }
            (Page::Main, ROW_SLEEP_CLOCK) => {
                value.push_str(if sleep_clock() { "On" } else { "Off" });
                ("Clock when sleeping", true, false)
            }
            (Page::Main, ROW_ADDRESS) => {
                match critical_section::with(|cs| NET.borrow_ref(cs).ip) {
                    Some(ip) => {
                        let _ = write!(value, "{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
                    }
                    None => value.push_str("none"),
                }
                ("Address", false, false)
            }
            (Page::Main, ROW_ABOUT) => {
                value.push_str("Licenses");
                ("About", false, true)
            }
            (Page::Equaliser, EQ_PRESET) => {
                value.push_str(crate::eq::preset_name());
                ("Preset", true, false)
            }
            (Page::Equaliser, row) => {
                let band = row - EQ_PRESET - 1;
                let db = crate::eq::gain(band as usize);
                if db == 0 {
                    value.push_str("0 dB");
                } else {
                    let _ = write!(value, "{}{} dB", if db < 0 { '-' } else { '+' }, db.abs());
                }
                (["Bass", "Mid", "Treble"][band as usize], true, false)
            }
            _ => ("", false, false),
        };
        if on {
            draw_pointer(oled, x_cursor, y, bob);
        }
        let _ = Text::with_text_style(label, Point::new(x_label, y), text, left).draw(oled);
        // Adjustable values get arrows to show left/right changes them; a
        // submenu row shows an arrow towards the submenu.
        let mut v = Buf::<32>::new();
        if on && adjustable {
            let _ = write!(v, "< {} >", value.as_str());
        } else if on && submenu {
            let _ = write!(v, "{} >", value.as_str());
        } else {
            v = Buf::from_str(value.as_str());
        }
        let _ = Text::with_text_style(v.as_str(), Point::new(x_value, y), text, right).draw(oled);
    }

    if rows > VISIBLE {
        // Scrollbar: a dim track with a brighter thumb, 2 px wide at the edge.
        let track_h = VISIBLE as usize * PITCH as usize;
        let thumb_h = track_h * VISIBLE as usize / rows as usize;
        let thumb_y = (track_h - thumb_h) * first as usize / (rows - VISIBLE) as usize;
        for yy in 0..track_h {
            let level = if (thumb_y..thumb_y + thumb_h).contains(&yy) { 9 } else { 2 };
            for xx in WIDTH - 2..WIDTH {
                oled.set_pixel(xx, TOP as usize + yy, level);
            }
        }
    }
    true
}

/// The About page: item 0 is the repository and licence, then one item per
/// open source library (name and licence, with the author under it). Two
/// items show at a time, starting at `first`.
fn draw_about(oled: &mut Ssd1322, first: u8) -> bool {
    use crate::about;
    const TOP: i32 = 14;
    const PITCH: i32 = 12;
    let left = TextStyleBuilder::new().baseline(Baseline::Top).build();
    let right = TextStyleBuilder::new().baseline(Baseline::Top).alignment(Alignment::Right).build();
    let bright = MonoTextStyle::new(&FONT_6X10, Gray4::WHITE);
    let dim = MonoTextStyle::new(&FONT_6X10, Gray4::new(7));
    let total = crate::menu::about_items();
    for slot in 0..2u8 {
        let item = first + slot;
        if item >= total {
            break;
        }
        let y = TOP + slot as i32 * 2 * PITCH;
        if item == 0 {
            let _ = Text::with_text_style(about::REPO, Point::new(0, y), bright, left).draw(oled);
            let mut l = Buf::<48>::new();
            let _ = write!(l, "License: {} - libraries below", about::LICENSE);
            let _ = Text::with_text_style(l.as_str(), Point::new(0, y + PITCH), dim, left).draw(oled);
        } else {
            let (name, author, lic) = about::entry(item as usize - 1);
            let mut by = Buf::<48>::new();
            let _ = write!(by, "by {author}");
            let _ = Text::with_text_style(name, Point::new(4, y), bright, left).draw(oled);
            let _ = Text::with_text_style(lic, Point::new(WIDTH as i32 - 8, y), bright, right).draw(oled);
            let _ = Text::with_text_style(by.as_str(), Point::new(4, y + PITCH), dim, left).draw(oled);
        }
    }
    // Scrollbar down the right edge, as on the main list.
    let track_h = 4 * PITCH as usize;
    let span = total.saturating_sub(2).max(1) as usize;
    let thumb_h = (track_h * 2 / total as usize).max(4);
    let thumb_y = (track_h - thumb_h) * first as usize / span;
    for yy in 0..track_h {
        let level = if (thumb_y..thumb_y + thumb_h).contains(&yy) { 9 } else { 2 };
        for xx in WIDTH - 2..WIDTH {
            oled.set_pixel(xx, TOP as usize + yy, level);
        }
    }
    true
}

/// Draw the message box over the middle of the screen, if one is showing.
fn draw_osd(oled: &mut Ssd1322) -> bool {
    let o = critical_section::with(|cs| {
        let mut o = OSD.borrow_ref_mut(cs);
        if o.until.is_some_and(|t| Instant::now() >= t) {
            o.until = None;
        }
        o.until.map(|_| (o.lines.clone(), o.count, o.big))
    });
    let Some((lines, count, big)) = o else {
        return false;
    };
    let rect = Rectangle::new(Point::new(6, 13), Size::new(WIDTH as u32 - 12, 38));
    draw_box(oled, rect.top_left.x as usize, rect.top_left.y as usize, rect.size.width as usize, rect.size.height as usize);
    let centered = TextStyleBuilder::new().baseline(Baseline::Middle).alignment(Alignment::Center).build();
    if big {
        let style = MonoTextStyle::new(&FONT_10X20, Gray4::WHITE);
        let _ = Text::with_text_style(lines[0].as_str(), rect.center(), style, centered).draw(oled);
    } else {
        let style = MonoTextStyle::new(&FONT_6X10, Gray4::WHITE);
        let top = rect.center().y - (count as i32 * 11) / 2 + 5;
        for (i, line) in lines[..count].iter().enumerate() {
            let _ = Text::with_text_style(line.as_str(), Point::new(WIDTH as i32 / 2, top + i as i32 * 11), style, centered)
                .draw(oled);
        }
    }
    true
}

pub fn set_ip(ip: Option<[u8; 4]>) {
    critical_section::with(|cs| NET.borrow_ref_mut(cs).ip = ip);
}

pub fn set_link(up: bool) {
    critical_section::with(|cs| NET.borrow_ref_mut(cs).link_up = up);
}

/// Diagnostics: frames drawn, and where the task last was
/// (1 = drawing, 2 = flushing to the panel, 3 = waiting for the next frame).
#[cfg(feature = "diag")]
pub static FRAMES: AtomicU32 = AtomicU32::new(0);
#[cfg(feature = "diag")]
pub static STAGE: AtomicU32 = AtomicU32::new(0);
/// Panel transfers that didn't finish within FLUSH_TIMEOUT (should stay 0).
#[cfg(feature = "diag")]
pub static FLUSH_TIMEOUTS: AtomicU32 = AtomicU32::new(0);
/// Longest time spent drawing the menu / message box in one frame (us).
#[cfg(feature = "diag")]
pub static OVERLAY_MAX_US: AtomicU32 = AtomicU32::new(0);
/// A frame takes ~7 ms to send; anything near this means the SPI DMA
/// transfer is stuck. Dropping the future aborts it, and the next frame
/// starts a fresh transfer, so a glitch costs a frame instead of the display.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(100);

#[cfg(feature = "diag")]
static LAST_FRAME_MS: AtomicU32 = AtomicU32::new(0);
#[cfg(feature = "diag")]
static MAX_FRAME_GAP_MS: AtomicU32 = AtomicU32::new(0);
#[cfg(feature = "diag")]
static MIN_FRAME_GAP_MS: AtomicU32 = AtomicU32::new(u32::MAX);

#[cfg(feature = "diag")]
fn record_frame() {
    let now = Instant::now().as_millis() as u32;
    let last = LAST_FRAME_MS.swap(now, Ordering::Relaxed);
    if last != 0 {
        MAX_FRAME_GAP_MS.fetch_max(now.wrapping_sub(last), Ordering::Relaxed);
        MIN_FRAME_GAP_MS.fetch_min(now.wrapping_sub(last), Ordering::Relaxed);
    }
}

/// (shortest, longest) gap between display frames since the last call, ms.
#[cfg(feature = "diag")]
pub fn take_frame_gaps_ms() -> (u32, u32) {
    let min = MIN_FRAME_GAP_MS.swap(u32::MAX, Ordering::Relaxed);
    (if min == u32::MAX { 0 } else { min }, MAX_FRAME_GAP_MS.swap(0, Ordering::Relaxed))
}

#[embassy_executor::task]
pub async fn display_task(mut oled: Ssd1322) -> ! {
    #[cfg(feature = "diag")]
    STAGE.store(10, Ordering::Relaxed);
    oled.init().await;
    let boot_start = Instant::now();
    let mut next_frame = Instant::now();
    let mut title_line = Marquee::<128>::new();
    // Hash of the playing URL, to notice when it changes (a copy of the URL
    // would cost 256 bytes of RAM).
    let mut preset_url_hash = 0u32;
    let mut preset_name: Option<Buf<{ crate::presets::NAME_MAX }>> = None;
    let mut seen_presets_gen = u32::MAX;
    // Dimming: when the current stopped spell began, and the panel's
    // brightness now (stepped towards its target, one step per FADE_STEP).
    let mut stopped_since: Option<Instant> = None;
    // When dimming began (None = awake), and the brightness last sent.
    let mut dim_start: Option<Instant> = None;
    let mut dim = 0.0f32;
    // Frames drawn since dimming began (diag screen recordings run the dim
    // on the frame count, as the capture is slower than the display).
    #[cfg(feature = "diag")]
    let mut dim_frames = 0u64;
    let mut station_line = Marquee::<128>::new();
    // Station name in the status bar, scrolling when it doesn't fit beside
    // the format / state text.
    let mut name_line = Marquee::<128>::new();
    // Level meters fall back smoothly instead of jumping to zero.
    let (mut meter_l, mut meter_r) = (0u8, 0u8);
    // Spectrum bars, falling back smoothly like the meters.
    let mut bars = [0u8; crate::spectrum::BANDS];

    loop {
        #[cfg(feature = "diag")]
        STAGE.store(1, Ordering::Relaxed);
        let s = radio::status();
        let (ip, link_up) = critical_section::with(|cs| {
            let n = NET.borrow_ref(cs);
            (n.ip, n.link_up)
        });
        let active = matches!(s.phase, Phase::Buffering | Phase::Playing);

        // Dim when stopped for DIM_AFTER with no activity (decided before
        // drawing, so the normal logo can be left out while it slides).
        let now = Instant::now();
        let idle_since = Instant::from_millis(LAST_ACTIVITY_MS.load(Ordering::Relaxed) as u64);
        stopped_since = match s.phase {
            Phase::Stopped => Some(stopped_since.unwrap_or(now).max(idle_since)),
            _ => None,
        };
        let asleep = !osd_showing() && stopped_since.is_some_and(|t| now.saturating_duration_since(t) >= DIM_AFTER);
        let boot_ms = now.saturating_duration_since(boot_start).as_millis() as f32;
        let booting = boot_ms < (BOOT_FADE + BOOT_HOLD + BOOT_SLIDE).as_millis() as f32;

        // The big line: the title, or a status message when there isn't one.
        let mut big = Buf::<128>::new();
        match s.phase {
            Phase::Error => big.push_str("Can't play"),
            Phase::Offline => big.push_str("No network"),
            Phase::Stopped if s.message.is_empty() => big.push_str("Stopped"),
            Phase::Stopped => big.push_str("Stopped"),
            Phase::Connecting => big.push_str("Connecting..."),
            _ if !s.title.is_empty() => big.push_str(s.title.as_str()),
            _ => big.push_str(if s.station.is_empty() { "Internet radio" } else { s.station.as_str() }),
        }
        title_line.set(big.as_str());

        // Fall back gently (8/255 per frame) so the tip's fade is visible.
        meter_l = s.level_l.max(meter_l.saturating_sub(8));
        meter_r = s.level_r.max(meter_r.saturating_sub(8));
        if !active {
            meter_l = 0;
            meter_r = 0;
        }

        // Spectrum bars: jump up to the latest data, fall 10/255 per frame.
        let fresh = if active { crate::spectrum::read() } else { None };
        for (i, bar) in bars.iter_mut().enumerate() {
            *bar = fresh.map_or(0, |f| f[i]).max(bar.saturating_sub(10));
        }

        let _ = oled.clear(Gray4::BLACK);
        let small = MonoTextStyle::new(&FONT_6X10, Gray4::new(8));
        let top = TextStyleBuilder::new().baseline(Baseline::Top).build();
        let top_right = TextStyleBuilder::new().baseline(Baseline::Top).alignment(Alignment::Right).build();

        // The stream's icy-name is usually "Name: description".
        let (name, description) = split_station(s.station.as_str());
        // Which preset is playing: looked up only when the URL changes
        // (scanning 99 flash slots every frame would waste time).
        let presets_gen = crate::presets::GENERATION.load(Ordering::Relaxed);
        let url_hash = s.url.as_bytes().iter().fold(0x811C_9DC5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193));
        if url_hash != preset_url_hash || presets_gen != seen_presets_gen {
            preset_url_hash = url_hash;
            seen_presets_gen = presets_gen;
            preset_name = crate::presets::find(s.url.as_str()).map(|i| crate::presets::get(i).name);
        }
        let name = match &preset_name {
            Some(n) if !n.is_empty() => n.as_str(),
            _ => name,
        };

        // Status bar, far right: the time, once NTP has set it. The other
        // right-hand text and the station name make room (5 chars + a gap).
        let info = status_info();
        let (show_clock, show_codec) = (matches!(info, INFO_BOTH | INFO_CLOCK), matches!(info, INFO_BOTH | INFO_CODEC));
        let clock = crate::clock::hour_minute().filter(|_| show_clock);
        let clock_px = if clock.is_some() { (CLOCK_W + 2 * 6) as i32 } else { 0 };

        // Status bar, right: the format while playing, otherwise the state.
        let mut right = Buf::<24>::new();
        match s.phase {
            Phase::Playing if s.sample_rate > 0 => {
                if show_codec {
                    let _ = write!(right, "{} kbps {}.{} kHz", s.bitrate, s.sample_rate / 1000, s.sample_rate % 1000 / 100);
                }
            }
            Phase::Buffering => {
                let _ = write!(right, "BUFFERING {}%", s.buffer_pct);
            }
            p => {
                for c in p.as_str().bytes() {
                    let _ = right.write_char(c.to_ascii_uppercase() as char);
                }
            }
        }
        // Status bar, left: the address when nothing is playing (no station
        // chosen, stopped, or failed; the remote's # button also shows it),
        // otherwise the station name, scrolling if it's too long to fit
        // beside the right-hand text (2 characters' gap).
        let show_address = s.url.is_empty() || matches!(s.phase, Phase::Stopped | Phase::Error);
        let mut left = Buf::<48>::new();
        let mut name_scrolling = false;
        match (link_up, ip) {
            (false, _) => left.push_str("No network link"),
            (true, None) => left.push_str("Getting address..."),
            (true, Some(ip)) if show_address => {
                let _ = write!(left, "http://{}.{}.{}.{}/", ip[0], ip[1], ip[2], ip[3]);
            }
            (true, Some(_)) => {
                let name_style = MonoTextStyle::new(&FONT_6X10, Gray4::new(13));
                let room = WIDTH as i32 - (right.as_bytes().len() as i32 + 2) * 6 - clock_px;
                name_line.set(name);
                name_scrolling = name_line.draw_in(&mut oled, name_style, 0, room, false);
            }
        }
        if !left.is_empty() {
            let _ = Text::with_text_style(left.as_str(), Point::new(0, 0), small, top).draw(&mut oled);
        }
        let _ = Text::with_text_style(right.as_str(), Point::new(WIDTH as i32 - 1 - clock_px, 0), small, top_right).draw(&mut oled);
        if let Some((h, m)) = clock {
            draw_clock(&mut oled, h, m, WIDTH as i32 - CLOCK_W as i32, 0, 8);
        }
        let _ = Line::new(Point::new(0, 11), Point::new(WIDTH as i32 - 1, 11))
            .into_styled(PrimitiveStyle::with_stroke(Gray4::new(3), 1))
            .draw(&mut oled);

        // The settings menu takes over everything below the status bar:
        // nothing else is drawn (or animated) while it's open.
        let menu_open = crate::menu::is_open();
        let (mut station_scrolling, mut title_scrolling) = (false, false);
        if !menu_open {
            // Station description (scrolling if long), or the error / hint text.
            match s.phase {
                Phase::Error | Phase::Stopped | Phase::Offline if !s.message.is_empty() => station_line.set(s.message.as_str()),
                Phase::Stopped => station_line.set("Select a station on the remote or web page"),
                _ => station_line.set(description),
            }
            let station_style = MonoTextStyle::new(&FONT_6X10, Gray4::new(11));
            station_scrolling = station_line.draw(&mut oled, station_style, 14, false);

            // Title: centred if it fits, otherwise scrolling. When stopped the
            // status bar already says so: show the logo instead.
            title_scrolling = if s.phase == Phase::Stopped {
                if !asleep && !booting {
                    draw_logo(&mut oled, LOGO_AREA_AWAKE.0, LOGO_AREA_AWAKE.1); // below the hint text
                }
                false
            } else {
                let title_style = MonoTextStyle::new(&FONT_10X20, Gray4::WHITE);
                title_line.draw(&mut oled, title_style, 25, true)
            };
        }
        let scrolling = station_scrolling || title_scrolling || name_scrolling;

        // Level meters (only while a stream is active): left above right,
        // filling the bottom of the screen. When muted they're replaced by a
        // flashing (500 ms on/off) badge.
        if menu_open {
            #[cfg(feature = "diag")]
            let t = Instant::now();
            draw_menu(&mut oled);
            #[cfg(feature = "diag")]
            OVERLAY_MAX_US.fetch_max(t.elapsed_sat().as_micros() as u32, Ordering::Relaxed);
        } else if active {
            if s.muted {
                if Instant::now().as_millis() / 500 % 2 == 0 {
                    draw_muted(&mut oled);
                }
            } else if spectrum_showing(now) {
                draw_spectrum(&mut oled, &bars);
            } else {
                draw_meter(&mut oled, 46, meter_l, "L");
                draw_meter(&mut oled, 56, meter_r, "R");
            }
        }

        let osd = !menu_open && draw_osd(&mut oled);

        // Dimming: the UI around the logo fades out, the logo slides to the
        // centre, and the whole panel dims (gray-scale table).
        let mut fading = false;
        let smoothstep = |t: f32| t * t * (3.0 - 2.0 * t);
        if booting {
            fading = true; // 50 fps for the animation
            let (fade, hold) = (BOOT_FADE.as_millis() as f32, BOOT_HOLD.as_millis() as f32);
            if boot_ms < fade + hold {
                // Just the logo, centred, fading in from black.
                let e = smoothstep((boot_ms / fade).min(1.0));
                let _ = oled.clear(Gray4::BLACK);
                draw_logo(&mut oled, LOGO_AREA_DIM.0, LOGO_AREA_DIM.1);
                oled.scale_levels((e * 16.0 + 0.5) as u8);
            } else {
                let t = ((boot_ms - fade - hold) / BOOT_SLIDE.as_millis() as f32).min(1.0);
                let e = smoothstep(t);
                // The UI fades in over the end of the slide...
                let ui = ((t - (1.0 - UI_FADE_PART)) / UI_FADE_PART).clamp(0.0, 1.0);
                if s.phase != Phase::Stopped && ui == 0.0 {
                    // A restored station is already playing: the logo would
                    // land on the meters, so it fades out where it is
                    // instead, then the UI fades in.
                    let out = 1.0 - smoothstep(t / (1.0 - UI_FADE_PART));
                    let _ = oled.clear(Gray4::BLACK);
                    draw_logo(&mut oled, LOGO_AREA_DIM.0, LOGO_AREA_DIM.1);
                    oled.scale_levels((out * 16.0 + 0.5) as u8);
                } else {
                    oled.scale_levels((ui * 16.0 + 0.5) as u8);
                }
                // ...while the logo slides down to its usual place.
                if s.phase == Phase::Stopped {
                    let lerp = |a: i32, b: i32| a + ((b - a) as f32 * e + 0.5) as i32;
                    draw_logo(
                        &mut oled,
                        lerp(LOGO_AREA_DIM.0, LOGO_AREA_AWAKE.0),
                        lerp(LOGO_AREA_DIM.1, LOGO_AREA_AWAKE.1),
                    );
                }
            }
        }
        let wanted = if asleep {
            // Progress 0..=1 through the dim, eased (smoothstep).
            let start = *dim_start.get_or_insert(now);
            #[allow(unused_mut)]
            let mut ms = now.saturating_duration_since(start).as_millis();
            #[cfg(feature = "diag")]
            if CAPTURE.load(Ordering::Relaxed) {
                ms = dim_frames * FRAME_ACTIVE.as_millis();
                dim_frames += 1;
            }
            let t = (ms as f32 / DIM_TIME.as_millis() as f32).min(1.0);
            let e = smoothstep(t);
            fading = t < 1.0;
            // The rest of the screen fades out over the first part.
            let ui = 1.0 - (t / UI_FADE_PART).min(1.0);
            oled.scale_levels((ui * 16.0 + 0.5) as u8);
            // The logo slides from its usual place to the centre.
            let lerp = |a: i32, b: i32| a + ((b - a) as f32 * e + 0.5) as i32;
            draw_logo(
                &mut oled,
                lerp(LOGO_AREA_AWAKE.0, LOGO_AREA_DIM.0),
                lerp(LOGO_AREA_AWAKE.1, LOGO_AREA_DIM.1),
            );
            // The time, under the logo (the logo block ends at row 46 once
            // it has settled). It fades in over the last quarter of the
            // slide: earlier, the logo's reflection is still passing through
            // the clock's rows.
            if sleep_clock() {
                if let Some((h, m)) = crate::clock::hour_minute() {
                    let c = smoothstep(((t - 0.75) / 0.25).clamp(0.0, 1.0));
                    draw_clock(&mut oled, h, m, (WIDTH as i32 - CLOCK_W as i32) / 2, 52, (7.0 * c + 0.5) as u8);
                }
            }
            DIM_AMOUNT * e
        } else {
            dim_start = None; // waking is instant (no slide back)
            #[cfg(feature = "diag")]
            {
                dim_frames = 0;
            }
            0.0
        };
        if wanted != dim {
            dim = wanted;
            oled.set_dim(dim);
        }

        #[cfg(feature = "diag")]
        if CAPTURE.load(Ordering::Relaxed) {
            SNAP_READY.signal(());
            let _ = with_timeout(Duration::from_millis(500), SNAP_DONE.wait()).await;
            CAPTURE_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(feature = "diag")]
        STAGE.store(2, Ordering::Relaxed);
        let _flushed = with_timeout(FLUSH_TIMEOUT, oled.flush()).await;
        #[cfg(feature = "diag")]
        if _flushed.is_err() {
            FLUSH_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(feature = "diag")]
        record_frame();
        #[cfg(feature = "diag")]
        FRAMES.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "diag")]
        STAGE.store(3, Ordering::Relaxed);

        // Muted counts as animating so the badge flashes on an even grid.
        // The menu's pointer bobs, so an open menu redraws on the fast grid.
        let animating = menu_open || active || scrolling || (s.muted && active);
        next_frame += if animating || osd || fading { FRAME_ACTIVE } else { FRAME_IDLE };
        if next_frame < Instant::now() {
            next_frame = Instant::now();
        }
        if animating {
            // Fixed frame grid (an early extra frame makes scrolling judder);
            // a new message appears on the next frame, within 20 ms.
            Timer::at(next_frame).await;
            WAKE.reset();
        } else {
            // Idle: show a new message straight away.
            select(Timer::at(next_frame), WAKE.wait()).await;
            next_frame = Instant::now();
        }
    }
}

/// Width of "HH:MM" in FONT_6X10 (5 cells of 6 px).
const CLOCK_W: u32 = 30;

/// Draw "HH:MM" with its top-left at (x, y), in FONT_6X10 at gray `level`.
/// The font's own colon sits a row low against its digits (digits span rows
/// 1..=7, the colon rows 2..=8), so the colon is drawn here instead, as two
/// 1x2 dots centred on the digits. The digits fill 5 of their 6 columns, so
/// the gap between "HH" and "MM" is columns 11..=17: its centre, column 14,
/// is only exactly centred by an odd-width mark (a 2 px dot is always half a
/// pixel off). 1 px also matches the digits' stroke weight.
fn draw_clock(oled: &mut Ssd1322, h: u8, m: u8, x: i32, y: i32, level: u8) {
    if level == 0 {
        return;
    }
    let style = MonoTextStyle::new(&FONT_6X10, Gray4::new(level));
    let top = TextStyleBuilder::new().baseline(Baseline::Top).build();
    let (mut hh, mut mm) = (Buf::<4>::new(), Buf::<4>::new());
    let _ = write!(hh, "{h:02}");
    let _ = write!(mm, "{m:02}");
    let _ = Text::with_text_style(hh.as_str(), Point::new(x, y), style, top).draw(oled);
    let _ = Text::with_text_style(mm.as_str(), Point::new(x + 18, y), style, top).draw(oled);
    for dy in [2, 5] {
        for oy in [dy, dy + 1] {
            oled.set_pixel((x + 14) as usize, (y + oy) as usize, level);
        }
    }
}

/// The FreeNetRadio logo, shown while stopped: a broadcast symbol (a dot
/// between radio-wave arcs that dim outwards) beside the word "NetRadio" (with "FREE" above it) in
/// a top-to-bottom gradient, with a faint fading reflection underneath.
/// Centred vertically between rows `area_top` and `area_bottom`.
fn draw_logo(oled: &mut Ssd1322, area_top: i32, area_bottom: i32) {
    const TEXT: &str = "NetRadio";
    const TEXT_W: i32 = 8 * 10; // FONT_10X20
    const ICON_W: i32 = 34;
    const GAP: i32 = 10;
    // Vertical layout. In FONT_10X20's 20 px cell these letters occupy rows
    // 3..=15; the icon reaches 4 px above and below them. The reflection
    // starts REFLECT_GAP px below the letters. The whole block is centred
    // in the given area.
    const LETTERS_OFFSET: i32 = 3;
    const LETTERS_H: i32 = 13;
    // "FREE" in small letter-spaced capitals sits above the wordmark, left
    // aligned with it (to the right of the icon). The block reaches
    // FREE_ABOVE px above the letters to hold it (the icon's own overhang of
    // 4 px is less).
    const FREE_ABOVE: i32 = 7;
    const REFLECT_GAP: i32 = 3;
    const REFLECT_ROWS: i32 = 10;
    const BLOCK_H: i32 = FREE_ABOVE + LETTERS_H + REFLECT_GAP + REFLECT_ROWS;
    let block_top = area_top + (area_bottom - area_top + 1 - BLOCK_H) / 2;
    let letters_top = block_top + FREE_ABOVE;
    let text_top = letters_top - LETTERS_OFFSET; // top of the text's cell
    let left = (WIDTH as i32 - (ICON_W + GAP + TEXT_W)) / 2;
    let text_x = left + ICON_W + GAP;

    // Broadcast icon: centre dot plus three arcs each side, bright to dim.
    let centre = Point::new(left + ICON_W / 2, letters_top + LETTERS_H / 2);
    let _ = Circle::with_center(centre, 7).into_styled(PrimitiveStyle::with_fill(Gray4::WHITE)).draw(oled);
    for (i, level) in [(0u32, 13u8), (1, 9), (2, 5)] {
        let diameter = 15 + i * 9;
        let style = PrimitiveStyle::with_stroke(Gray4::new(level), 2);
        let _ = Arc::with_center(centre, diameter, (-40.0f32).deg(), 80.0f32.deg()).into_styled(style).draw(oled);
        let _ = Arc::with_center(centre, diameter, 140.0f32.deg(), 80.0f32.deg()).into_styled(style).draw(oled);
    }

    // Wordmark, then recolour its pixels with a vertical gradient (15 -> 6).
    let top = TextStyleBuilder::new().baseline(Baseline::Top).build();
    let _ = Text::with_text_style(TEXT, Point::new(text_x, text_top), MonoTextStyle::new(&FONT_10X20, Gray4::WHITE), top).draw(oled);
    for dy in 0..20 {
        let level = 15 - (dy * 9 / 19) as u8;
        let y = (text_top + dy) as usize;
        for x in text_x as usize..(text_x + TEXT_W) as usize {
            if oled.pixel(x, y) != 0 {
                oled.set_pixel(x, y, level);
            }
        }
    }

    // Reflection: the letters mirrored below them (bottom row first),
    // a few pixels down, fading out.
    let letters_bottom = letters_top + LETTERS_H - 1;
    for r in 0..REFLECT_ROWS {
        let src_y = (letters_bottom - r) as usize;
        let dst_y = (letters_bottom + 1 + REFLECT_GAP + r) as usize;
        if dst_y >= crate::ssd1322::HEIGHT {
            break;
        }
        let level = (4 - r * 4 / REFLECT_ROWS) as u8; // 4 -> 1
        for x in text_x as usize..(text_x + TEXT_W) as usize {
            if oled.pixel(x, src_y) != 0 {
                oled.set_pixel(x, dst_y, level.max(1));
            }
        }
    }

    // FREE: FONT_5X7's capitals fill cell rows 1..=6, so the cell starts 8 px
    // above the letters to leave a 1 px gap (the N's left stem is 1 px in).
    let free_font = MonoFont { character_spacing: 1, ..FONT_5X7 };
    let _ = Text::with_text_style("FREE", Point::new(text_x + 1, letters_top - 8), MonoTextStyle::new(&free_font, Gray4::new(11)), top)
        .draw(oled);
}

/// The 6x13 bold font with 3 px between letters (its glyphs otherwise touch).
const BADGE_FONT: MonoFont<'static> = MonoFont { character_spacing: 3, ..FONT_6X13_BOLD };

/// "MUTED" in dark bold text on a bright rounded box, centred at the bottom.
fn draw_muted(oled: &mut Ssd1322) {
    let rect = Rectangle::new(Point::new(WIDTH as i32 / 2 - 50, 47), Size::new(100, 16));
    let _ = RoundedRectangle::with_equal_corners(rect, Size::new(4, 4))
        .into_styled(PrimitiveStyle::with_fill(Gray4::WHITE))
        .draw(oled);
    let centered = TextStyleBuilder::new().baseline(Baseline::Middle).alignment(Alignment::Center).build();
    let _ = Text::with_text_style("MUTED", rect.center() + Point::new(1, 1), MonoTextStyle::new(&BADGE_FONT, Gray4::BLACK), centered)
        .draw(oled);
}

/// An 8 px tall level bar at row `y`: a faint full-width track with a solid
/// white bar filled to `level` (0..=255, a dB scale, one pixel per step).
/// Its last TIP pixels fade out towards the end, so the bar's edge is soft
/// and it grows and shrinks smoothly rather than in segments.
/// The spectrum analyser in the bottom 18 rows: one 5 px bar per band with a
/// 1 px gap, brighter towards the top.
fn draw_spectrum(oled: &mut Ssd1322, bars: &[u8; crate::spectrum::BANDS]) {
    const TOP: usize = 46;
    const ROWS: usize = crate::ssd1322::HEIGHT - TOP;
    const X0: usize = (crate::ssd1322::WIDTH - crate::spectrum::BANDS * 6) / 2;
    for (i, &v) in bars.iter().enumerate() {
        let h = (v as usize * ROWS).div_ceil(255);
        for k in 0..h {
            let level = (4 + k * 11 / (ROWS - 1)) as u8;
            let y = crate::ssd1322::HEIGHT - 1 - k;
            for x in 0..5 {
                oled.set_pixel(X0 + i * 6 + x, y, level);
            }
        }
    }
}

fn draw_meter(oled: &mut Ssd1322, y: i32, level: u8, label: &str) {
    const HEIGHT: u32 = 8;
    let _ = Rectangle::new(Point::new(0, y), Size::new(WIDTH as u32, HEIGHT))
        .into_styled(PrimitiveStyle::with_fill(Gray4::new(1)))
        .draw(oled);
    draw_bar(oled, y, level);
    // A little channel label at the left, in the inverse of whatever's
    // under it (black on the white bar, bright on the dim track).
    let top = TextStyleBuilder::new().baseline(Baseline::Top).build();
    // FONT_5X7 capitals are 6 rows tall: +1 centres them in the 8 px bar.
    let _ = Text::with_text_style(label, Point::new(2, y + 1), MonoTextStyle::new(&FONT_5X7, Gray4::WHITE), top)
        .draw(&mut Invert(oled));
}

fn draw_bar(oled: &mut Ssd1322, y: i32, level: u8) {
    const HEIGHT: u32 = 8;
    const TIP: i32 = 16;
    // Filled width in pixels: 0..=256.
    let width = (level as i32 * WIDTH as i32 + 127) / 255;
    if width == 0 {
        return;
    }
    // Solid body, then the fading tip (brightest at the body, dimmest at the
    // very end). A short bar is all tip, scaled to its length.
    let tip = TIP.min(width);
    let body = width - tip;
    if body > 0 {
        let _ = Rectangle::new(Point::new(0, y), Size::new(body as u32, HEIGHT))
            .into_styled(PrimitiveStyle::with_fill(Gray4::WHITE))
            .draw(oled);
    }
    for k in 0..tip {
        // k = 0 is next to the body, k = tip - 1 the last lit pixel.
        let gray = (15 * (tip - k) + tip / 2) / (tip + 1);
        let gray = gray.max(2) as u8; // stay visibly above the track
        let _ = Rectangle::new(Point::new(body + k, y), Size::new(1, HEIGHT))
            .into_styled(PrimitiveStyle::with_fill(Gray4::new(gray)))
            .draw(oled);
    }
}

/// Split an icy-name like "Groove Salad: a nicely chilled plate of ambient
/// beats [SomaFM]" into name and description. Without a ": " the whole
/// thing is the name and the description repeats it (so the status bar's
/// cut-off name can still be read in full).
fn split_station(icy_name: &str) -> (&str, &str) {
    match icy_name.split_once(": ") {
        Some((name, description)) if !name.is_empty() => (name.trim(), description.trim()),
        _ => (icy_name, icy_name),
    }
}

/// Draw target that inverts the pixels drawn onto (15 - existing level),
/// ignoring the colour: text drawn through it is always readable.
struct Invert<'a>(&'a mut Ssd1322);

impl OriginDimensions for Invert<'_> {
    fn size(&self) -> Size {
        self.0.size()
    }
}

impl DrawTarget for Invert<'_> {
    type Color = Gray4;
    type Error = core::convert::Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Gray4>>>(&mut self, pixels: I) -> Result<(), Self::Error> {
        for Pixel(p, _) in pixels {
            if (0..WIDTH as i32).contains(&p.x) && (0..crate::ssd1322::HEIGHT as i32).contains(&p.y) {
                let (x, y) = (p.x as usize, p.y as usize);
                let level = 15 - self.0.pixel(x, y);
                self.0.set_pixel(x, y, level);
            }
        }
        Ok(())
    }
}

/// A line of monospaced text that scrolls right-to-left in a loop when it's
/// wider than the screen. It moves exactly 1 px per drawn frame (not by
/// elapsed time): a frame delayed by MP3 decoding then means a brief
/// slowdown rather than a visible 2 px jump.
struct Marquee<const N: usize> {
    text: Buf<N>,
    px: i32,
}

impl<const N: usize> Marquee<N> {
    const fn new() -> Self {
        Self { text: Buf::new(), px: 0 }
    }

    /// Change the text; scrolling restarts only if it actually changed.
    fn set(&mut self, text: &str) {
        if text.as_bytes() != self.text.as_bytes() {
            self.text = Buf::from_str(text);
            self.px = 0;
        }
    }

    /// Draw at row `y` (top of the text), across the full width. Returns
    /// true if it's scrolling.
    fn draw(&mut self, oled: &mut Ssd1322, style: MonoTextStyle<'_, Gray4>, y: i32, center: bool) -> bool {
        self.draw_in(oled, style, y, WIDTH as i32, center)
    }

    /// Draw at row `y` within the leftmost `width` pixels (text that fits
    /// is left-aligned or centred in them; longer text scrolls inside them,
    /// clipped at the edge). Returns true if it's scrolling.
    fn draw_in(&mut self, oled: &mut Ssd1322, style: MonoTextStyle<'_, Gray4>, y: i32, width: i32, center: bool) -> bool {
        let text = self.text.as_str();
        let char_w = style.font.character_size.width as i32 + style.font.character_spacing as i32;
        let len = text.len() as i32;
        let top = TextStyleBuilder::new().baseline(Baseline::Top).build();
        if len * char_w <= width {
            if center {
                let centered = TextStyleBuilder::new().baseline(Baseline::Top).alignment(Alignment::Center).build();
                let _ = Text::with_text_style(text, Point::new(width / 2, y), style, centered).draw(oled);
            } else {
                let _ = Text::with_text_style(text, Point::new(0, y), style, top).draw(oled);
            }
            return false;
        }
        // A narrow area (the status bar) gets a proportionally smaller gap.
        let period = len * char_w + SCROLL_GAP.min(width / 3);
        let offset = self.px % period;
        self.px = (offset + 1) % period;
        let area = Rectangle::new(Point::new(0, y), Size::new(width as u32, style.font.character_size.height));
        let mut clip = oled.clipped(&area);
        // The copy scrolling off, and the next one coming in behind it. Only
        // the characters in the area are drawn; the clip trims the ones
        // half across its edges.
        for copy in 0..2 {
            let x = copy * period - offset;
            if x >= width || x + len * char_w <= 0 {
                continue;
            }
            let first = if x < 0 { -x / char_w } else { 0 };
            let last = ((width - x + char_w - 1) / char_w).min(len);
            let _ = Text::with_text_style(&text[first as usize..last as usize], Point::new(x + first * char_w, y), style, top)
                .draw(&mut clip);
        }
        true
    }
}
