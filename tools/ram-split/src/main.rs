//! One-shot tool: set the CH32V307's flash/RAM split (SRAM_CODE_MODE option
//! bits) to TARGET_MODE, keeping every other option byte as it was (read
//! protection stays off). Resets the chip once the new option bytes are
//! written; on the next boot it sees the target mode and idles.
//!
//! Flash it with `cargo run --release`, then flash your real firmware (built
//! with a matching memory.x) over it. Change TARGET_MODE to pick a split.
//!
//! USER option byte bits 7:6 (SRAM_CODE_MODE):
//!   00 = 192K flash + 128K RAM    01 = 224K + 96K
//!   10 = 256K flash +  64K RAM    11 = 288K + 32K (factory default)
//!
//! This memory.x stays at 32K RAM so the tool runs before and after the switch.

#![no_std]
#![no_main]

use core::ptr::{read_volatile, write_volatile};

use {ch32_hal as hal, panic_halt as _};

/// 0b00 = 192K flash + 128K RAM (what FreeNetRadio needs; other firmware also
/// runs fine on it, it just leaves RAM unused).
const TARGET_MODE: u8 = 0b00;

const FLASH_KEYR: usize = 0x4002_2004;
const FLASH_OBKEYR: usize = 0x4002_2008;
const FLASH_STATR: usize = 0x4002_200C;
const FLASH_CTLR: usize = 0x4002_2010;
const KEY1: u32 = 0x4567_0123;
const KEY2: u32 = 0xCDEF_89AB;
const STATR_BSY: u32 = 1 << 0;
const STATR_EOP: u32 = 1 << 5;
const CTLR_OBPG: u32 = 1 << 4;
const CTLR_OBER: u32 = 1 << 5;
const CTLR_STRT: u32 = 1 << 6;
const CTLR_LOCK: u32 = 1 << 7;

/// Option bytes: 8 halfwords (value in the low byte, complement in the high).
/// [RDPR, USER, DATA0, DATA1, WRP0, WRP1, WRP2, WRP3]
const OB_BASE: usize = 0x1FFF_F800;
/// RDPR value meaning "read protection off".
const RDPR_UNPROTECTED: u8 = 0xA5;

unsafe fn rd(addr: usize) -> u32 {
    read_volatile(addr as *const u32)
}
unsafe fn wr(addr: usize, v: u32) {
    write_volatile(addr as *mut u32, v)
}
unsafe fn wait_idle() {
    while rd(FLASH_STATR) & STATR_BSY != 0 {}
    wr(FLASH_STATR, STATR_EOP); // write-1-to-clear
}

#[hal::entry]
fn main() -> ! {
    let _p = hal::init(Default::default());

    unsafe {
        // Each option byte is stored with its complement in the high byte.
        // A pair that doesn't match (e.g. WCH's erased pattern 0xE339) is
        // invalid: treat it as its default, 0xFF, and rewrite it.
        let mut ob = [0u8; 8];
        let mut all_valid = true;
        for (i, b) in ob.iter_mut().enumerate() {
            let hw = read_volatile((OB_BASE + 2 * i) as *const u16);
            let (lo, hi) = (hw as u8, (hw >> 8) as u8);
            if lo ^ hi == 0xFF {
                *b = lo;
            } else {
                *b = 0xFF;
                all_valid = false;
            }
        }

        if ob[1] >> 6 != TARGET_MODE || !all_valid {
            ob[0] = RDPR_UNPROTECTED;
            ob[1] = (ob[1] & 0x3F) | (TARGET_MODE << 6);

            // Unlock flash, then the option bytes.
            if rd(FLASH_CTLR) & CTLR_LOCK != 0 {
                wr(FLASH_KEYR, KEY1);
                wr(FLASH_KEYR, KEY2);
            }
            wr(FLASH_OBKEYR, KEY1);
            wr(FLASH_OBKEYR, KEY2);

            // Erase the option byte area.
            wr(FLASH_CTLR, rd(FLASH_CTLR) | CTLR_OBER);
            wr(FLASH_CTLR, rd(FLASH_CTLR) | CTLR_STRT);
            wait_idle();
            wr(FLASH_CTLR, rd(FLASH_CTLR) & !CTLR_OBER);

            // Program all of them back. WCH flash erases to 0xE339, not 0xFF,
            // so every byte must be written (and RDPR in particular, or read
            // protection turns on). The hardware generates the complements.
            wr(FLASH_CTLR, rd(FLASH_CTLR) | CTLR_OBPG);
            for (i, &b) in ob.iter().enumerate() {
                write_volatile((OB_BASE + 2 * i) as *mut u16, b as u16);
                wait_idle();
            }
            wr(FLASH_CTLR, rd(FLASH_CTLR) & !CTLR_OBPG);
            wr(FLASH_CTLR, rd(FLASH_CTLR) | CTLR_LOCK);

            // New option bytes load on reset: PFIC system reset (KEY3 | SYSRESET).
            wr(0xE000_E048, 0xBEEF_0080);
        }
    }

    loop {
        core::hint::spin_loop();
    }
}
