//! The settings menu, opened with the remote's # button. Main page: the
//! visualiser (meters / spectrum / cycling), an Equaliser submenu, time zone
//! offset, daylight saving, what the status bar shows (clock / codec / both /
//! off), whether the dimmed idle screen shows the time, and the board's
//! address (read-only). The Equaliser page has a preset switcher and bass / mid / treble. Drawn
//! by display.rs; buttons are routed here by remote.rs while it's open: up /
//! down pick a row, left / right change it, OK opens a submenu or closes the
//! menu (from the Equaliser page, OK goes back), # closes it from anywhere.

use core::cell::Cell;

use critical_section::Mutex;
use embassy_time::{Duration, Instant};

use crate::{clock, display, eq, ElapsedSat};

/// The menu closes itself after this long without a button.
const IDLE_CLOSE: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Main,
    Equaliser,
    About,
}

pub const ROWS: u8 = 8;
pub const ROW_VISUAL: u8 = 0;
pub const ROW_EQ: u8 = 1;
pub const ROW_ZONE: u8 = 2;
pub const ROW_DST: u8 = 3;
pub const ROW_INFO: u8 = 4;
pub const ROW_SLEEP_CLOCK: u8 = 5;
pub const ROW_ADDRESS: u8 = 6;
pub const ROW_ABOUT: u8 = 7;

/// About page: items are the repo / licence header, then one per library;
/// `row` is the first item showing, and two items fit on screen.
pub fn about_items() -> u8 {
    (crate::about::count() + 1).min(255) as u8
}

/// Equaliser page rows: the preset switcher, then bass / mid / treble.
pub const EQ_ROWS: u8 = 4;
pub const EQ_PRESET: u8 = 0;
pub const EQ_BASS: u8 = 1;

#[derive(Clone, Copy)]
struct State {
    open: bool,
    page: Page,
    row: u8,
    last_input: Instant,
}

static STATE: Mutex<Cell<State>> =
    Mutex::new(Cell::new(State { open: false, page: Page::Main, row: 0, last_input: Instant::MIN }));

#[derive(Clone, Copy)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Ok,
}

fn update(f: impl FnOnce(&mut State)) {
    critical_section::with(|cs| {
        let c = STATE.borrow(cs);
        let mut s = c.get();
        f(&mut s);
        c.set(s);
    });
}

/// Whether the menu is showing. Closes it if it has sat idle too long.
pub fn is_open() -> bool {
    critical_section::with(|cs| {
        let c = STATE.borrow(cs);
        let mut s = c.get();
        if s.open && s.last_input.elapsed_sat() >= IDLE_CLOSE {
            s.open = false;
            c.set(s);
        }
        s.open
    })
}

/// The page showing and its selected row, if the menu is open.
pub fn view() -> Option<(Page, u8)> {
    is_open().then(|| critical_section::with(|cs| {
        let s = STATE.borrow(cs).get();
        (s.page, s.row)
    }))
}

/// Open the menu, or close it if it's open (the # button).
pub fn toggle() {
    let was_open = is_open();
    update(|s| {
        s.open = !was_open;
        s.page = Page::Main;
        s.row = 0;
        s.last_input = Instant::now();
    });
    display::wake();
}

fn enter_about() {
    update(|s| {
        s.page = Page::About;
        s.row = 0;
    });
}

fn enter_equaliser() {
    update(|s| {
        s.page = Page::Equaliser;
        s.row = 0;
    });
}

pub fn key(k: Key) {
    update(|s| s.last_input = Instant::now());
    let (page, row) = critical_section::with(|cs| {
        let s = STATE.borrow(cs).get();
        (s.page, s.row)
    });
    if page == Page::About {
        // Scroll: up / down by one library, left / right by a screenful.
        let last = about_items().saturating_sub(2);
        let to = |r: i32| r.clamp(0, last as i32) as u8;
        match k {
            Key::Up => update(|s| s.row = to(s.row as i32 - 1)),
            Key::Down => update(|s| s.row = to(s.row as i32 + 1)),
            Key::Left => update(|s| s.row = to(s.row as i32 - 2)),
            Key::Right => update(|s| s.row = to(s.row as i32 + 2)),
            Key::Ok => update(|s| {
                s.page = Page::Main;
                s.row = ROW_ABOUT;
            }),
        }
        return;
    }
    let rows = if page == Page::Main { ROWS } else { EQ_ROWS };
    let step = if matches!(k, Key::Right) { 1 } else { -1 };
    match (page, k) {
        (_, Key::Up) => update(|s| s.row = (s.row + rows - 1) % rows),
        (_, Key::Down) => update(|s| s.row = (s.row + 1) % rows),
        (Page::Main, Key::Ok) if row == ROW_EQ => enter_equaliser(),
        (Page::Main, Key::Ok | Key::Right) if row == ROW_ABOUT => enter_about(),
        (Page::Main, Key::Ok) => update(|s| s.open = false),
        (Page::About, _) => {}
        (Page::Equaliser, Key::Ok) => update(|s| {
            s.page = Page::Main;
            s.row = ROW_EQ;
        }),
        (Page::Main, Key::Right) if row == ROW_EQ => enter_equaliser(),
        (Page::Main, Key::Left | Key::Right) => match row {
            ROW_VISUAL => display::step_visualiser(step),
            ROW_ZONE => clock::set_offset_qh(clock::offset_qh() + step),
            ROW_DST => clock::set_dst_enabled(!clock::dst_enabled()),
            ROW_INFO => display::step_status_info(step),
            ROW_SLEEP_CLOCK => display::set_sleep_clock(!display::sleep_clock()),
            _ => {}
        },
        (Page::Equaliser, Key::Left | Key::Right) if row == EQ_PRESET => eq::step_preset(step),
        (Page::Equaliser, Key::Left | Key::Right) => {
            let band = (row - EQ_BASS) as usize;
            eq::adjust_gain(band, step);
        }
    }
}
