//! Minimal SSD1322 driver (256x64, 4-bit grayscale) over 4-wire SPI,
//! with an in-RAM framebuffer that `embedded-graphics` draws into.

use core::convert::Infallible;

use embedded_graphics::pixelcolor::{Gray4, GrayColor};
use embedded_graphics::prelude::*;
use embassy_time::Timer;
use hal::gpio::Output;
use hal::mode::Async;
use hal::peripherals::SPI1;
use hal::spi::Spi;

use crate::hal;

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 64;
/// Two 4-bit pixels per byte.
pub const BUF_LEN: usize = WIDTH * HEIGHT / 2;

/// The SSD1322 has 480 columns of RAM; 256-pixel-wide panels are wired to the
/// middle of it. One column address = 4 pixels, so 0x1C..=0x5B is 256 pixels.
const COL_START: u8 = 0x1C;
const COL_END: u8 = COL_START + (WIDTH / 4) as u8 - 1;

/// Re-map setting (command 0xA0): horizontal address increment, nibble re-map,
/// COM scan reversed; second byte enables dual COM line mode. This is the
/// usual setting for 256x64 modules. If the image appears mirrored or
/// upside-down, toggle bit 1 (column re-map) and/or bit 4 (COM scan direction).
const REMAP: [u8; 2] = [0x14, 0x11];

pub struct Ssd1322 {
    spi: Spi<'static, SPI1, Async>,
    cs: Output<'static>,
    dc: Output<'static>,
    rst: Output<'static>,
    buf: &'static mut [u8; BUF_LEN],
}

impl Ssd1322 {
    pub fn new(
        spi: Spi<'static, SPI1, Async>,
        cs: Output<'static>,
        dc: Output<'static>,
        rst: Output<'static>,
        buf: &'static mut [u8; BUF_LEN],
    ) -> Self {
        Self { spi, cs, dc, rst, buf }
    }

    fn command(&mut self, cmd: u8, args: &[u8]) {
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spi.blocking_write(&[cmd]);
        if !args.is_empty() {
            self.dc.set_high();
            let _ = self.spi.blocking_write(args);
        }
        self.cs.set_high();
    }

    /// Hardware reset and the standard init sequence for 256x64 modules.
    ///
    /// Uses embassy's `Timer`, not `hal::delay::Delay`: `Delay` reprograms
    /// SysTick (embassy's time base) and stops it, freezing all timers.
    pub async fn init(&mut self) {
        self.cs.set_high();
        self.rst.set_low();
        Timer::after_millis(1).await;
        self.rst.set_high();
        Timer::after_millis(10).await;

        self.command(0xFD, &[0x12]); // unlock command interface
        self.command(0xAE, &[]); // display off
        self.command(0xCA, &[0x3F]); // multiplex ratio: 64 rows
        self.command(0xA2, &[0x00]); // display offset
        self.command(0xA1, &[0x00]); // display start line
        self.command(0xA0, &REMAP); // re-map and dual COM mode
        self.command(0xB5, &[0x00]); // GPIO pins disabled
        self.command(0xAB, &[0x01]); // internal VDD regulator
        self.command(0xD1, &[0xA2, 0x20]); // display enhancement B: normal (only documented setting)
        self.apply_tuning(&Tuning::DEFAULT); // clock, currents, timings, voltages, gamma
        self.command(0xA6, &[]); // normal display mode
        self.command(0xA9, &[]); // exit partial display

        self.clear(Gray4::BLACK);
        self.flush().await;
        self.command(0xAF, &[]); // display on
    }

    /// Send all the adjustable analog/timing settings to the panel.
    pub fn apply_tuning(&mut self, t: &Tuning) {
        let t = t.clamped();
        self.command(0xB3, &[t.osc_freq << 4 | t.clock_div]); // clock divider / oscillator
        self.command(0xB4, &[if t.external_vsl { 0xA0 } else { 0xA2 }, if t.enhanced_low_gs { 0xFD } else { 0xB5 }]);
        self.command(0xC1, &[t.contrast_current]);
        self.command(0xC7, &[t.master_contrast]);
        self.command(0xB1, &[t.phase2 << 4 | t.phase1]);
        self.command(0xBB, &[t.precharge_voltage]);
        self.command(0xB6, &[t.second_precharge]);
        self.command(0xBE, &[t.vcomh]);
        if t.gamma_x10 == 10 {
            self.command(0xB9, &[]); // built-in linear grayscale table
        } else {
            self.command(0xB8, &gamma_table(t.gamma_x10)); // custom table...
            self.command(0x00, &[]); // ...and enable it
        }
    }

    /// Dimming, 0.0 (normal) ..= 1.0 (dimmest). The panel's brightness has a
    /// floor set by the pre-charge stage (every pixel is pre-charged towards
    /// 0.6 x VCC before it's driven), so lowering only the drive current or
    /// master contrast barely dims it. This fades all four together, from
    /// the init values to their minimums: master contrast (0xC7), segment
    /// current (0xC1), pre-charge voltage (0xBB) and second pre-charge
    /// period (0xB6). The gray-scale table is untouched, so gradients keep
    /// their shape.
    pub fn set_dim(&mut self, amount: f32) {
        let d = amount.clamp(0.0, 1.0);
        let lerp = |from: u8, to: u8| (from as f32 + (to as f32 - from as f32) * d + 0.5) as u8;
        let t = Tuning::DEFAULT;
        self.command(0xC7, &[lerp(t.master_contrast, 0)]);
        self.command(0xC1, &[lerp(t.contrast_current, 40)]);
        self.command(0xBB, &[lerp(t.precharge_voltage, 0)]);
        self.command(0xB6, &[lerp(t.second_precharge, 1)]);
    }

    /// Multiply every pixel's gray level by `num`/16 (fades what's drawn).
    pub fn scale_levels(&mut self, num: u8) {
        let num = num.min(16) as u16;
        for b in self.buf.iter_mut() {
            let hi = ((*b >> 4) as u16 * num / 16) as u8;
            let lo = ((*b & 0x0F) as u16 * num / 16) as u8;
            *b = hi << 4 | lo;
        }
    }

    pub fn clear(&mut self, color: Gray4) {
        let l = color.luma();
        self.buf.fill(l << 4 | l);
    }

    /// Set one pixel; 0..=15 gray level. No bounds checks beyond masking.
    #[inline(always)]
    pub fn set_pixel(&mut self, x: usize, y: usize, level: u8) {
        let i = y * (WIDTH / 2) + x / 2;
        let b = &mut self.buf[i];
        // Left pixel of each pair is the high nibble.
        *b = if x & 1 == 0 { (*b & 0x0F) | (level << 4) } else { (*b & 0xF0) | (level & 0x0F) };
    }

    /// Read one pixel's gray level (0..=15).
    #[inline(always)]
    pub fn pixel(&self, x: usize, y: usize) -> u8 {
        let b = self.buf[y * (WIDTH / 2) + x / 2];
        if x & 1 == 0 { b >> 4 } else { b & 0x0F }
    }

    /// Send the whole framebuffer to the panel (8 KB; ~7 ms at 9 MHz SPI).
    /// The frame goes out by DMA, so other tasks keep running meanwhile.
    pub async fn flush(&mut self) {
        self.command(0x15, &[COL_START, COL_END]); // column window
        self.command(0x75, &[0, (HEIGHT - 1) as u8]); // row window
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spi.blocking_write(&[0x5Cu8]); // write RAM
        self.dc.set_high();
        let _ = self.spi.write(&self.buf[..]).await;
        self.cs.set_high();
    }
}

/// The SSD1322's adjustable analog and timing settings.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// 0xC1: segment drive current, 0..=255.
    pub contrast_current: u8,
    /// 0xC7: master scale on all currents, 0..=15.
    pub master_contrast: u8,
    /// 0xB1 low nibble: phase 1 (reset) length, 2..=15 -> 2n+1 clocks.
    pub phase1: u8,
    /// 0xB1 high nibble: phase 2 (first pre-charge) length, 3..=15 clocks.
    pub phase2: u8,
    /// 0xBB: pre-charge voltage, 0..=31 -> 0.20..0.60 x VCC.
    pub precharge_voltage: u8,
    /// 0xB6: second pre-charge period, 0..=15 clocks.
    pub second_precharge: u8,
    /// 0xBE: COM deselect voltage, 0..=7 -> 0.72..0.86 x VCC.
    pub vcomh: u8,
    /// 0xB4 A: external VSL (most modules) vs internal.
    pub external_vsl: bool,
    /// 0xB4 B: "enhanced low gray-scale display quality".
    pub enhanced_low_gs: bool,
    /// 0xB3 low nibble: clock divide ratio, 0..=10 -> /1../1024.
    pub clock_div: u8,
    /// 0xB3 high nibble: oscillator frequency, 0..=15.
    pub osc_freq: u8,
    /// Gray-scale gamma x10, 5..=30. 10 = the built-in linear table (0xB9);
    /// anything else uploads a custom table (0xB8).
    pub gamma_x10: u8,
}

impl Tuning {
    /// The settings this driver has always used (typical for 256x64 modules).
    pub const DEFAULT: Tuning = Tuning {
        contrast_current: 0x9F,
        master_contrast: 0x0F,
        phase1: 2,
        phase2: 14,
        precharge_voltage: 0x1F,
        second_precharge: 0x08,
        vcomh: 0x07,
        external_vsl: true,
        enhanced_low_gs: true,
        clock_div: 1,
        osc_freq: 9,
        gamma_x10: 10,
    };

    /// Clamp every field to the range the SSD1322 datasheet allows.
    pub fn clamped(self) -> Self {
        Self {
            contrast_current: self.contrast_current,
            master_contrast: self.master_contrast.min(15),
            phase1: self.phase1.clamp(2, 15),
            phase2: self.phase2.clamp(3, 15),
            precharge_voltage: self.precharge_voltage.min(31),
            second_precharge: self.second_precharge.min(15),
            vcomh: self.vcomh.min(7),
            external_vsl: self.external_vsl,
            enhanced_low_gs: self.enhanced_low_gs,
            clock_div: self.clock_div.min(10),
            osc_freq: self.osc_freq.min(15),
            gamma_x10: self.gamma_x10.clamp(5, 30),
        }
    }
}

/// 15-entry gray-scale table (GS1..GS15 pulse widths, 0..=180, strictly
/// increasing) for the given gamma: GSn = 180 * (n/15)^gamma.
fn gamma_table(gamma_x10: u8) -> [u8; 15] {
    let gamma = gamma_x10 as f32 / 10.0;
    let mut table = [0u8; 15];
    let mut prev = 0u8;
    for (i, entry) in table.iter_mut().enumerate() {
        let x = (i + 1) as f32 / 15.0;
        let v = (180.0 * exp(gamma * ln(x)) + 0.5) as u8;
        // Keep it strictly increasing, leaving room for the remaining steps.
        let max = 180 - (14 - i) as u8;
        let v = v.max(prev + 1).min(max);
        *entry = v;
        prev = v;
    }
    table
}

/// Natural log for x > 0 (no libm in no_std): split off the binary exponent,
/// then ln(m) = 2*atanh((m-1)/(m+1)) by series. Accurate to ~1e-6.
fn ln(x: f32) -> f32 {
    let bits = x.to_bits();
    let e = ((bits >> 23) & 0xFF) as i32 - 127;
    let m = f32::from_bits((bits & 0x007F_FFFF) | 0x3F80_0000); // 1 <= m < 2
    let z = (m - 1.0) / (m + 1.0);
    let z2 = z * z;
    let series = z * (1.0 + z2 * (1.0 / 3.0 + z2 * (1.0 / 5.0 + z2 * (1.0 / 7.0 + z2 / 9.0))));
    e as f32 * core::f32::consts::LN_2 + 2.0 * series
}

/// e^y for y <= 0 (enough for x^gamma with x <= 1): 2^(int) * e^(frac) by series.
fn exp(y: f32) -> f32 {
    let k = (y / core::f32::consts::LN_2) as i32 - 1; // floor for y <= 0
    let r = y - k as f32 * core::f32::consts::LN_2; // 0 <= r < ln 2
    let mut term = 1.0f32;
    let mut sum = 1.0f32;
    for n in 1..10 {
        term *= r / n as f32;
        sum += term;
    }
    if k < -126 {
        0.0
    } else {
        sum * f32::from_bits(((k + 127) as u32) << 23)
    }
}

impl OriginDimensions for Ssd1322 {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl DrawTarget for Ssd1322 {
    type Color = Gray4;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(p, c) in pixels {
            if (0..WIDTH as i32).contains(&p.x) && (0..HEIGHT as i32).contains(&p.y) {
                self.set_pixel(p.x as usize, p.y as usize, c.luma());
            }
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        Ssd1322::clear(self, color);
        Ok(())
    }
}
