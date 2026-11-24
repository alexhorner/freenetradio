//! I2S audio output on SPI2 (I2S2) with a circular DMA ring buffer.
//!
//! ch32-hal has no I2S driver, so SPI2's I2S mode and DMA1 channel 5
//! (SPI2_TX) are set up directly through their registers. The layout matches
//! the STM32F10x-family peripherals the CH32V307 copies.
//!
//! Pins: PB12 = WS (LRCK), PB13 = CK (BCK), PB15 = SD (DIN). No master clock
//! is output: the PCM5102A generates its own from BCK when its SCK pin is
//! tied to GND.
//!
//! Format: Philips I2S, 16-bit samples in 32-bit channel slots, so
//! BCK = 64 x fs (the PCM5102A's PLL mode is happiest with 64 fs).
//!
//! The DMA loops over `RING` forever. The audio task writes decoded samples
//! ahead of the DMA's read position and zeroes what has been played, so if
//! the writer stalls the output falls silent instead of repeating old audio.

use core::ptr::{addr_of_mut, read_volatile, write_volatile};

use hal::peripherals::{PB12, PB13, PB15, SPI2};
use hal::Peri;

use crate::hal;

/// Ring size in stereo frames (4096 = 93 ms at 44.1 kHz).
pub const RING_FRAMES: usize = 4096;
const RING_LEN: usize = RING_FRAMES * 2; // interleaved L/R i16 samples

/// I2S clock = SYSCLK (RCC CFGR2 I2S2SRC = 0).
const I2S_CLOCK_HZ: u32 = 144_000_000;

/// Times the I2S transmitter ran out of data (UDR flag): the DMA missed a
/// sample slot, which is an audible click.
#[cfg(feature = "diag")]
pub static I2S_UNDERRUNS: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);
/// Times the DMA reported a transfer error.
#[cfg(feature = "diag")]
pub static DMA_ERRORS: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);

static mut RING: [i16; RING_LEN] = [0; RING_LEN];

// SPI2 / I2S2
const SPI2_BASE: usize = 0x4000_3800;
const SPI2_CTLR2: usize = SPI2_BASE + 0x04;
const SPI2_STATR: usize = SPI2_BASE + 0x08;
const SPI2_DATAR: usize = SPI2_BASE + 0x0C;
const SPI2_I2SCFGR: usize = SPI2_BASE + 0x1C;
const SPI2_I2SPR: usize = SPI2_BASE + 0x20;
const CTLR2_TXDMAEN: u32 = 1 << 1;
const STATR_TXE: u32 = 1 << 1;
const STATR_BSY: u32 = 1 << 7;
const I2SCFGR_CHLEN_32: u32 = 1 << 0;
const I2SCFGR_MASTER_TX: u32 = 0b10 << 8;
const I2SCFGR_I2SE: u32 = 1 << 10;
const I2SCFGR_I2SMOD: u32 = 1 << 11;
const I2SPR_ODD: u32 = 1 << 8;

// DMA1 channel 5 (SPI2_TX)
const DMA1_BASE: usize = 0x4002_0000;
const DMA1_CH5_CFGR: usize = DMA1_BASE + 0x58;
const DMA1_CH5_CNTR: usize = DMA1_BASE + 0x5C;
const DMA1_CH5_PADDR: usize = DMA1_BASE + 0x60;
const DMA1_CH5_MADDR: usize = DMA1_BASE + 0x64;
const DMA_EN: u32 = 1 << 0;
const DMA_DIR_FROM_MEM: u32 = 1 << 4;
const DMA_CIRC: u32 = 1 << 5;
const DMA_MINC: u32 = 1 << 7;
const DMA_PSIZE_16: u32 = 0b01 << 8;
const DMA_MSIZE_16: u32 = 0b01 << 10;
const DMA_PL_HIGH: u32 = 0b10 << 12;

// RCC and GPIOB
const RCC_AHBPCENR: usize = 0x4002_1014;
const RCC_APB2PCENR: usize = 0x4002_1018;
const RCC_APB1PCENR: usize = 0x4002_101C;
const GPIOB_CFGHR: usize = 0x4001_0C04;

unsafe fn rd(addr: usize) -> u32 {
    read_volatile(addr as *const u32)
}
unsafe fn wr(addr: usize, v: u32) {
    write_volatile(addr as *mut u32, v)
}
unsafe fn modify(addr: usize, f: impl FnOnce(u32) -> u32) {
    wr(addr, f(rd(addr)))
}

fn ring() -> *mut i16 {
    addr_of_mut!(RING) as *mut i16
}

pub struct I2sOut {
    /// Next sample index (even = start of a frame) the writer will fill.
    write: usize,
    /// Read position seen at the last `consume_played`.
    played: usize,
    /// Running totals (frames) of what's been queued and what the DMA has
    /// played. Unlike ring positions these can't wrap, so they show when the
    /// DMA has overtaken the writer (an underrun) however late we are.
    written_total: u64,
    played_total: u64,
    sample_rate: u32,
}

impl I2sOut {
    /// Configure the pins, SPI2 in I2S master-transmit mode and the DMA ring,
    /// then start clocking out (silence) at `sample_rate`.
    pub fn new(
        _spi: Peri<'static, SPI2>,
        _ws: Peri<'static, PB12>,
        _ck: Peri<'static, PB13>,
        _sd: Peri<'static, PB15>,
        sample_rate: u32,
    ) -> Self {
        unsafe {
            modify(RCC_AHBPCENR, |v| v | 1); // DMA1
            modify(RCC_APB2PCENR, |v| v | 1 << 3); // GPIOB
            modify(RCC_APB1PCENR, |v| v | 1 << 14); // SPI2

            // PB12, PB13, PB15: alternate-function push-pull, 50 MHz (0xB).
            modify(GPIOB_CFGHR, |v| {
                let mut v = v;
                for pin in [12, 13, 15] {
                    let shift = (pin - 8) * 4;
                    v = (v & !(0xF << shift)) | (0xB << shift);
                }
                v
            });

            // DMA: memory -> SPI2 DATAR, 16-bit, circular over the whole ring.
            wr(DMA1_CH5_CFGR, 0);
            wr(DMA1_CH5_PADDR, SPI2_DATAR as u32);
            wr(DMA1_CH5_MADDR, ring() as u32);
            wr(DMA1_CH5_CNTR, RING_LEN as u32);
            wr(
                DMA1_CH5_CFGR,
                DMA_DIR_FROM_MEM | DMA_CIRC | DMA_MINC | DMA_PSIZE_16 | DMA_MSIZE_16 | DMA_PL_HIGH,
            );

            wr(SPI2_I2SCFGR, 0);
            wr(SPI2_I2SPR, prescaler(sample_rate));
            wr(SPI2_I2SCFGR, I2SCFGR_I2SMOD | I2SCFGR_MASTER_TX | I2SCFGR_CHLEN_32); // Philips, 16-bit data
            modify(SPI2_CTLR2, |v| v | CTLR2_TXDMAEN);
            modify(DMA1_CH5_CFGR, |v| v | DMA_EN);
            modify(SPI2_I2SCFGR, |v| v | I2SCFGR_I2SE);
        }
        Self { write: 0, played: 0, written_total: 0, played_total: 0, sample_rate }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The exact rate the I2S clock divider gives for `sample_rate`.
    pub fn actual_rate(sample_rate: u32) -> u32 {
        let pr = prescaler(sample_rate);
        let div = 2 * (pr & 0xFF) + (pr >> 8 & 1);
        I2S_CLOCK_HZ / (64 * div)
    }

    /// Change the sample rate. Stops the I2S clock and DMA, reprograms the
    /// divider, and restarts both from the beginning of a silent ring (so
    /// the left/right order can't end up swapped).
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        unsafe {
            // Stop DMA requests first: while DMA keeps feeding it, the I2S
            // never goes idle (BSY stays set). Then let the last frame drain.
            // The waits are bounded so a wedged peripheral can't hang us.
            modify(SPI2_CTLR2, |v| v & !CTLR2_TXDMAEN);
            wait_until(|| rd(SPI2_STATR) & STATR_TXE != 0);
            wait_until(|| rd(SPI2_STATR) & STATR_BSY == 0);
            modify(SPI2_I2SCFGR, |v| v & !I2SCFGR_I2SE);
            modify(DMA1_CH5_CFGR, |v| v & !DMA_EN);

            for i in 0..RING_LEN {
                write_volatile(ring().add(i), 0);
            }
            wr(DMA1_CH5_MADDR, ring() as u32);
            wr(DMA1_CH5_CNTR, RING_LEN as u32);
            wr(SPI2_I2SPR, prescaler(sample_rate));

            modify(DMA1_CH5_CFGR, |v| v | DMA_EN);
            modify(SPI2_CTLR2, |v| v | CTLR2_TXDMAEN);
            modify(SPI2_I2SCFGR, |v| v | I2SCFGR_I2SE);
        }
        self.played = 0;
        self.write = 256;
        self.written_total = self.played_total + 128;
    }

    /// Sample index the DMA will read next (even = start of a frame,
    /// rounded down).
    fn read_pos(&self) -> usize {
        let remaining = unsafe { rd(DMA1_CH5_CNTR) } as usize;
        (RING_LEN - remaining.min(RING_LEN)) % RING_LEN & !1
    }

    /// Account for (and zero) everything the DMA has played since the last
    /// call, so a stalled writer produces silence rather than looping stale
    /// audio. Must be called at least once per ring period (~93 ms).
    pub fn consume_played(&mut self) {
        // UDR (STATR bit 3) is cleared by reading STATR; DMA1 ISR bit 19 is
        // channel 5's transfer error (cleared via IFCR bit 19).
        #[cfg(feature = "diag")]
        unsafe {
            if rd(SPI2_STATR) & (1 << 3) != 0 {
                I2S_UNDERRUNS.fetch_add(1, portable_atomic::Ordering::Relaxed);
            }
            if rd(DMA1_BASE) & (1 << 19) != 0 {
                DMA_ERRORS.fetch_add(1, portable_atomic::Ordering::Relaxed);
                wr(DMA1_BASE + 4, 1 << 19);
            }
        }
        let pos = self.read_pos();
        let mut i = self.played;
        while i != pos {
            unsafe { write_volatile(ring().add(i), 0) };
            i = (i + 1) % RING_LEN;
        }
        self.played_total += (((pos + RING_LEN - self.played) % RING_LEN) / 2) as u64;
        self.played = pos;
    }

    /// Frames queued but not yet played (0 if the DMA has overtaken us).
    pub fn queued_frames(&self) -> usize {
        self.written_total.saturating_sub(self.played_total) as usize
    }

    /// Frames that can be written without overwriting unplayed audio.
    pub fn free_frames(&self) -> usize {
        (RING_FRAMES - 1).saturating_sub(self.queued_frames())
    }

    /// If the DMA has caught up with or overtaken the writer (an underrun),
    /// restart writing a little ahead of it. Returns true if that happened.
    pub fn resync_if_underrun(&mut self, lead_frames: usize) -> bool {
        if self.written_total <= self.played_total {
            self.write = (self.read_pos() + lead_frames * 2) % RING_LEN;
            self.written_total = self.played_total + lead_frames as u64;
            true
        } else {
            false
        }
    }

    /// Append one stereo frame.
    #[inline(always)]
    pub fn push(&mut self, left: i16, right: i16) {
        unsafe {
            write_volatile(ring().add(self.write), left);
            write_volatile(ring().add(self.write + 1), right);
        }
        self.write = (self.write + 2) % RING_LEN;
        self.written_total += 1;
    }

    /// Fade the audio already queued for the DAC down to silence over at most
    /// `frames` frames, and silence everything after that, so stopping
    /// doesn't cut the waveform off mid-cycle (an audible click). The DMA
    /// keeps playing meanwhile; we start a little ahead of it.
    pub fn fade_out(&mut self, frames: usize) {
        const MARGIN: usize = 16; // frames left untouched just ahead of the DMA
        let queued = self.queued_frames();
        if queued <= MARGIN {
            return;
        }
        let start = (self.read_pos() + 2 * MARGIN) % RING_LEN;
        let count = queued - MARGIN;
        let n = count.min(frames).max(1);
        for k in 0..count {
            let i = (start + 2 * k) % RING_LEN;
            let gain = if k < n { ((n - k) * 32768 / n) as i32 } else { 0 };
            unsafe {
                let l = ring().add(i);
                let r = ring().add(i + 1);
                write_volatile(l, ((read_volatile(l) as i32 * gain) >> 15) as i16);
                write_volatile(r, ((read_volatile(r) as i32 * gain) >> 15) as i16);
            }
        }
        self.write = (start + 2 * n) % RING_LEN;
        self.written_total = self.played_total + (MARGIN + n) as u64;
    }

    /// Silence the whole ring and restart writing just ahead of the DMA.
    pub fn flush_silence(&mut self) {
        for i in 0..RING_LEN {
            unsafe { write_volatile(ring().add(i), 0) };
        }
        let pos = self.read_pos();
        self.played_total += (((pos + RING_LEN - self.played) % RING_LEN) / 2) as u64;
        self.played = pos;
        self.write = (pos + 256) % RING_LEN;
        self.written_total = self.played_total + 128;
    }
}

/// Spin until `done` (at most ~1 ms at 144 MHz).
fn wait_until(done: impl Fn() -> bool) {
    for _ in 0..50_000 {
        if done() {
            return;
        }
    }
}

/// I2SPR value for `sample_rate`: fs = I2SCLK / (64 * (2*I2SDIV + ODD)).
fn prescaler(sample_rate: u32) -> u32 {
    let div = ((I2S_CLOCK_HZ + 32 * sample_rate) / (64 * sample_rate)).clamp(4, 511);
    (div / 2) | if div & 1 != 0 { I2SPR_ODD } else { 0 }
}
