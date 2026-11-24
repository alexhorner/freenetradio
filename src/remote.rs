//! What the IR remote's buttons do.
//!
//!   up / down      volume +/- 1 (repeats after holding for 0.4 s); 0 mutes
//!   *              mute / unmute (unmuting at volume 0 goes to 2)
//!   left / right   previous / next preset station
//!   OK             play / stop
//!   digits         preset number 1..99: type one or two digits; a single
//!                  digit plays after a 1.2 s pause (a lone 0 means 10)
//!   #              settings menu (time zone, daylight saving, address);
//!                  while open: up/down pick a row, left/right change it
//!                  (hold to repeat), OK or # close
//!
//! Codes are for the common 17-key NEC remote sold with VS1838B receiver
//! kits (address 0x00). Any other code is shown on the display so a
//! different remote can be mapped.

use core::cell::Cell;
use core::fmt::Write as _;

use critical_section::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Instant};
use crate::ElapsedSat;

use crate::display;
use crate::menu::{self, Key};
use crate::ir::Press;
use crate::presets::{self, SLOTS};
use crate::radio::{self, Phase};
use crate::text::Text;

const VOLUME_STEP: u8 = 1;
/// NEC remotes start sending repeat codes ~110 ms into a press, so a normal
/// tap would count twice. Ignore repeats until the button has been held
/// this long.
const HOLD_DELAY: Duration = Duration::from_millis(400);

/// How long to wait for a second digit.
const DIGIT_TIMEOUT: Duration = Duration::from_millis(1200);

/// Digits typed so far: (value, count). Committed by `digit_task`.
static PENDING: Mutex<Cell<Option<(usize, u8)>>> = Mutex::new(Cell::new(None));
static DIGIT: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Plays the typed preset number once no further digit arrives in time.
#[embassy_executor::task]
pub async fn digit_task() -> ! {
    loop {
        DIGIT.wait().await;
        // Each new digit restarts the wait.
        while with_timeout(DIGIT_TIMEOUT, DIGIT.wait()).await.is_ok() {}
        commit_digits();
    }
}

/// Play the pending preset number, if any.
fn commit_digits() {
    if let Some((value, count)) = critical_section::with(|cs| PENDING.borrow(cs).take()) {
        // A lone 0 is preset 10, as on a one-digit remote.
        let n = if count == 1 && value == 0 { 10 } else { value };
        if (1..=SLOTS).contains(&n) {
            play_preset(n - 1);
        } else {
            display::show_osd("No preset 0");
        }
    }
}

fn digit(d: u8) {
    let (value, count) = critical_section::with(|cs| {
        let p = PENDING.borrow(cs);
        let (v, c) = p.get().unwrap_or((0, 0));
        let next = (v * 10 + d as usize, c + 1);
        p.set(Some(next));
        next
    });
    if count >= 2 {
        commit_digits(); // two digits is the most a preset number has
        DIGIT.reset();
    } else {
        let mut t: Text<16> = Text::new();
        let _ = write!(t, "Preset {value}-");
        display::show_osd(t.as_str());
        DIGIT.signal(());
    }
}

/// When the current press started (its first, non-repeat frame).
static PRESS_START: Mutex<Cell<Option<Instant>>> = Mutex::new(Cell::new(None));

#[derive(Clone, Copy)]
enum Button {
    Digit(u8),
    Star,
    Hash,
    Up,
    Down,
    Left,
    Right,
    Ok,
}

fn button(command: u8) -> Option<Button> {
    Some(match command {
        0x45 => Button::Digit(1),
        0x46 => Button::Digit(2),
        0x47 => Button::Digit(3),
        0x44 => Button::Digit(4),
        0x40 => Button::Digit(5),
        0x43 => Button::Digit(6),
        0x07 => Button::Digit(7),
        0x15 => Button::Digit(8),
        0x09 => Button::Digit(9),
        0x19 => Button::Digit(0),
        0x16 => Button::Star,
        0x0D => Button::Hash,
        0x18 => Button::Up,
        0x52 => Button::Down,
        0x08 => Button::Left,
        0x5A => Button::Right,
        0x1C => Button::Ok,
        _ => return None,
    })
}

/// IR command code for a button name used by the web page's virtual remote.
pub fn command_for(name: &[u8]) -> Option<u8> {
    Some(match name {
        b"1" => 0x45,
        b"2" => 0x46,
        b"3" => 0x47,
        b"4" => 0x44,
        b"5" => 0x40,
        b"6" => 0x43,
        b"7" => 0x07,
        b"8" => 0x15,
        b"9" => 0x09,
        b"0" => 0x19,
        b"star" => 0x16,
        b"hash" => 0x0D,
        b"up" => 0x18,
        b"down" => 0x52,
        b"left" => 0x08,
        b"right" => 0x5A,
        b"ok" => 0x1C,
        _ => return None,
    })
}

pub fn on_press(press: Press) {
    display::wake(); // any button wakes a dimmed screen
    let Some(b) = button(press.command) else {
        if !press.repeat {
            let mut t: Text<32> = Text::new();
            let _ = write!(t, "IR code 0x{:02X}", press.command);
            display::show_osd(t.as_str());
        }
        return;
    };
    // Only volume repeats while a button is held, and only after HOLD_DELAY.
    let menu_open = menu::is_open();
    if press.repeat {
        let held = critical_section::with(|cs| PRESS_START.borrow(cs).get()).map(|t| t.elapsed_sat());
        let repeats = if menu_open { matches!(b, Button::Left | Button::Right) } else { matches!(b, Button::Up | Button::Down) };
        if !repeats || held.is_none_or(|h| h < HOLD_DELAY) {
            return;
        }
    } else {
        critical_section::with(|cs| PRESS_START.borrow(cs).set(Some(Instant::now())));
        // Another button while a number is half-typed: finish the number first.
        if !matches!(b, Button::Digit(_)) {
            commit_digits();
        }
    }
    if menu_open {
        match b {
            Button::Up => menu::key(Key::Up),
            Button::Down => menu::key(Key::Down),
            Button::Left => menu::key(Key::Left),
            Button::Right => menu::key(Key::Right),
            Button::Ok => menu::key(Key::Ok),
            Button::Hash => menu::toggle(),
            Button::Star | Button::Digit(_) => {}
        }
        return;
    }
    match b {
        Button::Up | Button::Down => {
            let v = radio::status().volume;
            let v = if matches!(b, Button::Up) { (v + VOLUME_STEP).min(100) } else { v.saturating_sub(VOLUME_STEP) };
            radio::set_volume(v); // 0 mutes
            if v == 0 {
                display::show_osd("Muted");
            } else {
                let mut t: Text<16> = Text::new();
                let _ = write!(t, "Volume {v}");
                display::show_osd(t.as_str());
            }
        }
        Button::Star => {
            if radio::toggle_mute() {
                display::show_osd("Muted");
            } else {
                let mut t: Text<16> = Text::new();
                let _ = write!(t, "Volume {}", radio::status().volume);
                display::show_osd(t.as_str());
            }
        }
        Button::Ok => {
            let s = radio::status();
            if matches!(s.phase, Phase::Connecting | Phase::Buffering | Phase::Playing | Phase::Offline) {
                radio::stop();
                display::show_osd("Stopped");
            } else if !s.url.is_empty() {
                radio::play(s.url.as_str());
                display::show_osd("Play");
            } else {
                play_preset(0);
            }
        }
        Button::Digit(d) => digit(d),
        Button::Left | Button::Right => {
            // Step to the next/previous non-empty preset.
            let forward = matches!(b, Button::Right);
            let start = presets::find(radio::status().url.as_str());
            let next = (1..=SLOTS)
                .map(|k| match start {
                    None if forward => k - 1,
                    None => SLOTS - k,
                    Some(i) if forward => (i + k) % SLOTS,
                    Some(i) => (i + SLOTS * 2 - k) % SLOTS,
                })
                .find(|&i| !presets::get(i).is_empty());
            match next {
                Some(i) => play_preset(i),
                None => display::show_osd("No presets saved"),
            }
        }
        Button::Hash => menu::toggle(),
    }
}

/// Play preset `index` (0-based) and show it on the display.
pub fn play_preset(index: usize) {
    let p = presets::get(index);
    let mut t: Text<48> = Text::new();
    if p.is_empty() {
        let _ = write!(t, "Preset {} is empty", index + 1);
    } else {
        radio::play(p.url.as_str());
        let _ = write!(t, "{} {}", index + 1, p.name.as_str());
    }
    display::show_osd(t.as_str());
}
