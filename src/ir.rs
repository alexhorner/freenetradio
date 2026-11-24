//! NEC infrared remote receiver on PB10 (VS1838B-type 38 kHz receiver).
//!
//! The receiver's output idles high and goes low during each IR burst. NEC:
//!
//!   frame:  9 ms burst, 4.5 ms gap, 32 bits, 560 us burst
//!   bit:    560 us burst, then 560 us gap (0) or 1690 us gap (1), LSB first
//!   bytes:  address, ~address, command, ~command
//!   repeat: 9 ms burst, 2.25 ms gap, 560 us burst, every ~110 ms while held
//!
//! Timing is measured in the EXTI interrupt, not in a task: MP3 decoding
//! blocks the executor for ~12 ms at a time, which would wreck sub-millisecond
//! pulse measurements. The interrupt timestamps each edge and queues the
//! length of the period that just ended; `ir_task` decodes at leisure.
//!
//! This bypasses ch32-hal's `exti` feature (disabled in Cargo.toml), whose
//! handler only wakes a task.

use core::ptr::{read_volatile, write_volatile};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant};
use crate::ElapsedSat;
use hal::peripherals::PB10;
use hal::Peri;
use portable_atomic::{AtomicU32, Ordering};

use crate::hal;

/// A completed level period: how long the line stayed `low` (a burst) or high.
#[derive(Clone, Copy)]
struct Period {
    us: u32,
    low: bool,
}

/// Interrupt -> task. 128 periods is two full frames.
static PERIODS: Channel<CriticalSectionRawMutex, Period, 128> = Channel::new();
static LAST_EDGE_US: AtomicU32 = AtomicU32::new(0);

/// Diagnostics for /stats: last decoded (address << 8 | command), counts.
#[cfg(feature = "diag")]
pub static LAST_CODE: AtomicU32 = AtomicU32::new(u32::MAX);
#[cfg(feature = "diag")]
pub static FRAMES: AtomicU32 = AtomicU32::new(0);
pub static ERRORS: AtomicU32 = AtomicU32::new(0);

const GPIOB_CFGHR: usize = 0x4001_0C04;
const GPIOB_INDR: usize = 0x4001_0C08;
const GPIOB_OUTDR: usize = 0x4001_0C0C;
const AFIO_EXTICR3: usize = 0x4001_0010; // EXTI8..11 port selection
const EXTI_INTENR: usize = 0x4001_0400;
const EXTI_RTENR: usize = 0x4001_0408;
const EXTI_FTENR: usize = 0x4001_040C;
const EXTI_INTFR: usize = 0x4001_0414;
const RCC_APB2PCENR: usize = 0x4002_1018;
const LINE: u32 = 10;

unsafe fn rd(a: usize) -> u32 {
    read_volatile(a as *const u32)
}
unsafe fn wr(a: usize, v: u32) {
    write_volatile(a as *mut u32, v)
}

/// Configure PB10 as a pulled-up input and interrupt on both edges.
pub fn init(_pin: Peri<'static, PB10>) {
    unsafe {
        wr(RCC_APB2PCENR, rd(RCC_APB2PCENR) | 1 << 0 | 1 << 3); // AFIO, GPIOB
        // PB10: input with pull-up/down (0x8), pull-up selected via OUTDR.
        let shift = (LINE - 8) * 4;
        wr(GPIOB_CFGHR, rd(GPIOB_CFGHR) & !(0xF << shift) | 0x8 << shift);
        wr(GPIOB_OUTDR, rd(GPIOB_OUTDR) | 1 << LINE);
        // EXTI10 <- port B (1).
        let shift = (LINE - 8) * 4;
        wr(AFIO_EXTICR3, rd(AFIO_EXTICR3) & !(0xF << shift) | 1 << shift);
        wr(EXTI_RTENR, rd(EXTI_RTENR) | 1 << LINE);
        wr(EXTI_FTENR, rd(EXTI_FTENR) | 1 << LINE);
        wr(EXTI_INTFR, 1 << LINE);
        wr(EXTI_INTENR, rd(EXTI_INTENR) | 1 << LINE);
        qingke::pfic::enable_interrupt(hal::pac::Interrupt::EXTI15_10 as u8);
    }
}

#[hal::interrupt]
unsafe fn EXTI15_10() {
    if rd(EXTI_INTFR) & 1 << LINE == 0 {
        return;
    }
    wr(EXTI_INTFR, 1 << LINE);
    let now = Instant::now().as_micros() as u32;
    let then = LAST_EDGE_US.swap(now, Ordering::Relaxed);
    // The pin's level now is the opposite of the period that just ended.
    let high_now = rd(GPIOB_INDR) & 1 << LINE != 0;
    let _ = PERIODS.try_send(Period { us: now.wrapping_sub(then), low: high_now });
}

/// A decoded button press.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Press {
    pub address: u16,
    pub command: u8,
    /// True for NEC repeat codes (button held).
    pub repeat: bool,
}

fn near(us: u32, target: u32, tolerance: u32) -> bool {
    us.abs_diff(target) <= tolerance
}

/// Decodes NEC frames and calls `on_press` for each press and repeat.
#[embassy_executor::task]
pub async fn ir_task() -> ! {
    enum State {
        Idle,
        LeaderBurst,
        BitBurst { bits: u32, count: u8 },
        BitGap { bits: u32, count: u8 },
        RepeatEnd,
        FrameEnd { bits: u32 },
    }
    let mut state = State::Idle;
    let mut last: Option<(Press, Instant)> = None;

    loop {
        let p = PERIODS.receive().await;
        state = match state {
            // Waiting for a 9 ms leader burst (anything else is noise).
            State::Idle | State::RepeatEnd | State::FrameEnd { .. } if p.low && near(p.us, 9000, 1500) => State::LeaderBurst,
            State::LeaderBurst if !p.low && near(p.us, 4500, 700) => State::BitBurst { bits: 0, count: 0 },
            State::LeaderBurst if !p.low && near(p.us, 2250, 500) => State::RepeatEnd,
            State::BitBurst { bits, count } if p.low && near(p.us, 560, 300) => {
                if count == 32 {
                    // The trailing burst closes the frame.
                    State::FrameEnd { bits }
                } else {
                    State::BitGap { bits, count }
                }
            }
            State::BitGap { bits, count } if !p.low && near(p.us, 560, 300) => State::BitBurst { bits, count: count + 1 },
            State::BitGap { bits, count } if !p.low && near(p.us, 1690, 400) => {
                State::BitBurst { bits: bits | 1 << count, count: count + 1 }
            }
            State::RepeatEnd if p.low && near(p.us, 560, 300) => {
                // Repeat code: only valid shortly after a press.
                if let Some((press, at)) = last {
                    if at.elapsed_sat() < Duration::from_millis(250) {
                        let press = Press { repeat: true, ..press };
                        last = Some((press, Instant::now()));
                        crate::remote::on_press(press);
                    }
                }
                State::Idle
            }
            State::Idle => State::Idle,
            _ => {
                ERRORS.fetch_add(1, Ordering::Relaxed);
                State::Idle
            }
        };

        if let State::FrameEnd { bits } = state {
            let [a, a_inv, cmd, cmd_inv] = bits.to_le_bytes();
            if cmd ^ cmd_inv == 0xFF {
                // Standard NEC has ~address; extended NEC uses a 16-bit address.
                let address = if a ^ a_inv == 0xFF { a as u16 } else { u16::from_le_bytes([a, a_inv]) };
                let press = Press { address, command: cmd, repeat: false };
                #[cfg(feature = "diag")]
                FRAMES.fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "diag")]
                LAST_CODE.store((address as u32) << 8 | cmd as u32, Ordering::Relaxed);
                last = Some((press, Instant::now()));
                crate::remote::on_press(press);
            } else {
                ERRORS.fetch_add(1, Ordering::Relaxed);
            }
            state = State::Idle;
        }
    }
}
