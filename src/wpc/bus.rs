//! WPC CPU board memory map and ASIC (Williams A-12742), from WPC-EMU `cpu-board.js`,
//! `asic.js`, `mapper/*.js` and PinMAME `wpc.c`.
//!
//! - 0x0000-0x2fff RAM (0x3c00-0x3faf too), write-protected by the RAM lock registers
//! - 0x3000-0x3bff DMD windows, 0x3fb0-0x3fbf DMD control (see `dmd.rs`)
//! - 0x3fc0-0x3fdb external I/O, 0x3fdc-0x3fdf sound board interface
//! - 0x3fe0-0x3fff ASIC (plus 0x3fd4, the Fliptronic port): solenoids 0x3fe0-0x3fe3, lamp
//!   row/column 0x3fe4/0x3fe5, GI triacs 0x3fe6 and the other write-only outputs just keep
//!   their last value in `Bus::ram`
//! - 0x4000-0x7fff banked ROM (16 KiB pages), 0x8000-0xffff the last 32 KiB of the ROM

use std::sync::Arc;

use mc6809_core::Memory;

use super::dmd::Dmd;
use super::pic::Pic;
use super::sound::Sound;

pub const FLIPTRONIC_PORT_A: u16 = 0x3fd4;
pub const SW_JUMPER: u16 = 0x3fe7;
pub const SW_CABINET: u16 = 0x3fe8;
pub const SW_ROW: u16 = 0x3fe9;
pub const SW_COL: u16 = 0x3fea;
pub const FLIPPER_COIL95: u16 = 0x3fee;
pub const FLIPPER_SW95: u16 = 0x3fef;
pub const LEDS: u16 = 0x3ff2;
pub const SHIFT_ADDR_H: u16 = 0x3ff4;
pub const SHIFT_ADDR_L: u16 = 0x3ff5;
pub const SHIFT_BIT: u16 = 0x3ff6;
pub const SHIFT_BIT2: u16 = 0x3ff7;
pub const FIRQ_SRC: u16 = 0x3ff8;
pub const CLK_HOURS: u16 = 0x3ffa;
pub const CLK_MINS: u16 = 0x3ffb;
pub const ROM_BANK: u16 = 0x3ffc;
pub const RAM_LOCK: u16 = 0x3ffd;
pub const RAM_LOCKSIZE: u16 = 0x3ffe;
pub const ZEROCROSS: u16 = 0x3fff;

pub const DMD_PAGE3200: u16 = 0x3fb8;
pub const DMD_PAGE3000: u16 = 0x3fb9;
pub const DMD_PAGE3600: u16 = 0x3fba;
pub const DMD_PAGE3400: u16 = 0x3fbb;
pub const DMD_HIGH_PAGE: u16 = 0x3fbc;
pub const DMD_FIRQ_LINE: u16 = 0x3fbd;
pub const DMD_LOW_PAGE: u16 = 0x3fbe;
pub const DMD_ACTIVE_PAGE: u16 = 0x3fbf;

pub const SOUND_DATA: u16 = 0x3fdc;
pub const SOUND_CONTROL: u16 = 0x3fdd;

/// Value of RAM_LOCK that unlocks the protected RAM.
const RAM_UNLOCK: u8 = 0xb4;
/// Cycles between two zero crossings (120 Hz at 2 MHz).
pub const ZEROCROSS_CYCLES: u32 = 16667;
/// IRQ period: 976 Hz.
pub const IRQ_CYCLES: u32 = 2049;
/// Watchdog: one IRQ period plus a grace period.
pub const WATCHDOG_CYCLES: i64 = IRQ_CYCLES as i64 * 2 + 64;
/// Switch "always closed" (24), closed on every WPC game.
const SW_ALWAYS_CLOSED: u8 = 24;
/// NVRAM bytes the clock read fills (year, month, day, weekday, hour, valid, checksum).
const NVRAM_CLOCK: usize = 0x1800;

/// Real-time clock seen by the game: a fixed instant, optionally advancing with the
/// emulated time. Kept frozen by default so that runs are reproducible.
#[derive(Clone, Copy)]
pub struct Clock {
    /// Seconds since 1970-01-01 00:00 UTC.
    pub base: i64,
    pub running: bool,
}

impl Default for Clock {
    fn default() -> Self {
        // 2024-06-01 12:00:00 UTC
        Clock {
            base: 1_717_243_200,
            running: false,
        }
    }
}

/// Calendar fields of a UTC instant: (year, month 1-12, day 1-31, weekday 0-6 from Sunday,
/// hour, minute).
pub fn civil(t: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let weekday = (days + 4).rem_euclid(7) as u32;
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (
        y,
        m,
        d,
        weekday,
        (secs / 3600) as u32,
        ((secs / 60) % 60) as u32,
    )
}

#[derive(Clone)]
pub struct Bus {
    pub rom: Arc<Vec<u8>>,
    /// 16 KiB: RAM, plus the last value written to every hardware register (as on WPC-EMU,
    /// several registers read back from here).
    pub ram: Vec<u8>,
    pub page_mask: u8,
    pub rom_bank: u8,
    /// Address bits that select the protected RAM, once set through RAM_LOCKSIZE.
    pub protect_mask: Option<u16>,
    /// Switch matrix: index 0 = cabinet (coin door) inputs, 1..=8 = matrix columns,
    /// 9 = Fliptronic switches; bit n = row n+1.
    pub switches: [u8; 10],
    pub active_column: u8,
    pub dip: u8,
    pub zero_cross: bool,
    pub ticks_zero_cross: u32,
    pub watchdog_ticks: i64,
    pub watchdog_expired: u32,
    pub firq_source_dmd: bool,
    pub irq_count_gi: u32,
    pub periodic_irq_enabled: bool,
    pub blank: bool,
    pub led_toggles: u32,
    old_led: u8,
    pub fliptronic_out: u8,
    pub pic: Option<Pic>,
    pub dmd: Dmd,
    pub sound: Sound,
    pub extio: [u8; 32],
    /// Read overrides (address, value), applied before the memory map.
    pub patches: Vec<(u16, u8)>,
    /// Banks past the game part of the ROM read 0 (WPC-EMU) instead of the system part.
    pub zero_top_banks: bool,
    /// Writes to 0x8000-0xffff land in (a private copy of) the ROM, as on WPC-EMU.
    pub rom_writes: bool,
    pub clock: Clock,
    /// CPU cycles since reset, kept current by the board (drives a running clock).
    pub now: u64,
    pub protected_write_attempts: u64,
}

impl Bus {
    pub fn new(rom: Arc<Vec<u8>>, page_mask: u8, pic: Option<Pic>, pre_dcs: bool) -> Self {
        let mut b = Bus {
            rom,
            ram: vec![0; 0x4000],
            page_mask,
            rom_bank: 0,
            protect_mask: None,
            switches: [0; 10],
            active_column: 0,
            dip: 0,
            zero_cross: false,
            ticks_zero_cross: 0,
            watchdog_ticks: WATCHDOG_CYCLES,
            watchdog_expired: 0,
            firq_source_dmd: false,
            irq_count_gi: 0,
            periodic_irq_enabled: true,
            blank: true,
            led_toggles: 0,
            old_led: 0,
            fliptronic_out: 0,
            pic,
            dmd: Dmd::default(),
            sound: Sound::new(pre_dcs),
            extio: [0; 32],
            patches: Vec::new(),
            zero_top_banks: false,
            rom_writes: false,
            clock: Clock::default(),
            now: 0,
            protected_write_attempts: 0,
        };
        b.set_switch(SW_ALWAYS_CLOSED, true);
        b
    }

    /// ASIC reset (the CPU reset line): RAM, DMD and switches keep their contents.
    pub fn reset(&mut self) {
        self.periodic_irq_enabled = true;
        self.rom_bank = 0;
        self.led_toggles = 0;
        self.old_led = 0;
        self.firq_source_dmd = false;
        self.irq_count_gi = 0;
        self.zero_cross = false;
        self.ticks_zero_cross = 0;
        self.protect_mask = None;
        if let Some(p) = self.pic.as_mut() {
            p.reset();
        }
        self.blank = true;
        self.watchdog_ticks = WATCHDOG_CYCLES;
        self.watchdog_expired = 0;
        self.sound.reset();
    }

    /// Switch `n` in WPC numbering (column * 10 + row, 11..=88; 91..=98 address column 9).
    pub fn set_switch(&mut self, n: u8, closed: bool) {
        if !(11..=95).contains(&n) {
            return;
        }
        let (col, row) = (((n - 1) / 10) as usize, (n - 1) % 10);
        if row < 8 {
            if closed {
                self.switches[col] |= 1 << row;
            } else {
                self.switches[col] &= !(1 << row);
            }
        }
        self.switches[2] |= 0x08;
    }

    pub fn switch(&self, n: u8) -> bool {
        let (col, row) = (((n.max(1) - 1) / 10) as usize, (n.max(1) - 1) % 10);
        col < 10 && row < 8 && self.switches[col] & (1 << row) != 0
    }

    /// Advance the ASIC timers by `ticks` cycles.
    pub fn tick(&mut self, ticks: u32) {
        self.ticks_zero_cross += ticks;
        if self.ticks_zero_cross >= ZEROCROSS_CYCLES {
            self.ticks_zero_cross -= ZEROCROSS_CYCLES;
            self.zero_cross = true;
        }
        self.watchdog_ticks -= ticks as i64;
        if self.watchdog_ticks < 0 {
            self.watchdog_ticks = WATCHDOG_CYCLES;
            self.watchdog_expired += 1;
        }
    }

    fn time(&self) -> i64 {
        if self.clock.running {
            self.clock.base + (self.now / 2_000_000) as i64
        } else {
            self.clock.base
        }
    }

    fn rom_banked(&self, off: usize) -> u8 {
        let a = self.rom_bank as usize * 0x4000 + off;
        if self.zero_top_banks && a >= self.rom.len() - 0x8000 {
            return 0;
        }
        self.rom[a % self.rom.len()]
    }

    fn rom_fixed(&self, off: usize) -> u8 {
        self.rom[self.rom.len() - 0x8000 + off]
    }

    /// Side-effect free read, for code fetch analysis.
    pub fn peek(&self, addr: u16) -> u8 {
        let a = addr as usize;
        match addr {
            0x0000..=0x2fff | 0x3c00..=0x3faf => self.ram[a],
            0x3000..=0x3bff => self.dmd.read(addr),
            0x3fb0..=0x3fff => self.ram[a],
            0x4000..=0x7fff => self.rom_banked(a - 0x4000),
            _ => self.rom_fixed(a - 0x8000),
        }
    }

    fn ram_writable(&self, addr: u16) -> bool {
        self.ram[RAM_LOCK as usize] == RAM_UNLOCK || self.protect_mask.is_none_or(|m| addr & m != m)
    }

    fn hw_read(&mut self, addr: u16) -> u8 {
        let a = addr as usize;
        match addr {
            0x3000..=0x3bff => self.dmd.read(addr),
            // WPC-EMU reads back the FIRQ line register; the other DMD and printer
            // registers read 0
            DMD_FIRQ_LINE => self.ram[a],
            0x3fb0..=0x3fbf => 0,
            FLIPTRONIC_PORT_A => !self.switches[9],
            0x3fc6 => 0xff, // no ticket dispenser
            0x3fc0..=0x3fdb => self.extio[a - 0x3fc0],
            SOUND_DATA => self.sound.read_data(),
            SOUND_CONTROL => self.sound.read_control(),
            0x3fde | 0x3fdf => 0,
            _ => self.asic_read(addr),
        }
    }

    fn asic_read(&mut self, addr: u16) -> u8 {
        let a = addr as usize;
        let r = &self.ram;
        match addr {
            FLIPPER_COIL95 | FLIPPER_SW95 => !self.switches[9],
            SW_CABINET => self.switches[0],
            ROM_BANK => r[a] & self.page_mask,
            SW_ROW => match self.pic.as_mut() {
                Some(p) => {
                    let sw = self.switches;
                    p.read(|c| sw.get(c).copied().unwrap_or(0))
                }
                None => self.switches[self.active_column as usize],
            },
            SHIFT_ADDR_H => {
                let lo = r[SHIFT_ADDR_L as usize] as u16 + (r[SHIFT_BIT as usize] >> 3) as u16;
                (r[SHIFT_ADDR_H as usize] as u16 + (lo >> 8)) as u8
            }
            SHIFT_ADDR_L => r[SHIFT_ADDR_L as usize].wrapping_add(r[SHIFT_BIT as usize] >> 3),
            SHIFT_BIT | SHIFT_BIT2 => 1 << (r[a] & 7),
            CLK_HOURS => {
                let (y, mo, d, wd, h, _) = civil(self.time());
                let f = [
                    (y >> 8) as u8,
                    y as u8,
                    mo as u8,
                    d as u8,
                    wd as u8 + 1,
                    0,
                    1,
                ];
                self.ram[NVRAM_CLOCK..NVRAM_CLOCK + 7].copy_from_slice(&f);
                let sum: u32 = f.iter().map(|&v| v as u32).sum();
                let chk = 0xffff - sum;
                self.ram[NVRAM_CLOCK + 7] = (chk >> 8) as u8;
                self.ram[NVRAM_CLOCK + 8] = chk as u8;
                h as u8
            }
            CLK_MINS => civil(self.time()).5 as u8,
            SW_JUMPER => self.dip,
            ZEROCROSS => {
                if self.zero_cross {
                    self.irq_count_gi = 0;
                }
                let v = ((self.zero_cross as u8) << 7) | (r[a] & 0x7f);
                self.zero_cross = false;
                v
            }
            FIRQ_SRC => {
                if self.firq_source_dmd {
                    0
                } else {
                    0x80
                }
            }
            _ => r[a],
        }
    }

    fn hw_write(&mut self, addr: u16, val: u8) {
        let a = addr as usize;
        self.ram[a] = val;
        match addr {
            0x3000..=0x3bff => self.dmd.write(addr, val),
            DMD_FIRQ_LINE => self.dmd.request_firq = true,
            DMD_ACTIVE_PAGE => self.dmd.next_active = Some(val & 0x0f),
            DMD_LOW_PAGE => self.dmd.select(0, val),
            DMD_HIGH_PAGE => self.dmd.select(1, val),
            DMD_PAGE3000 => self.dmd.select(2, val),
            DMD_PAGE3200 => self.dmd.select(3, val),
            DMD_PAGE3400 => self.dmd.select(4, val),
            DMD_PAGE3600 => self.dmd.select(5, val),
            0x3fb0..=0x3fbf => {}
            FLIPTRONIC_PORT_A | FLIPPER_SW95 => self.fliptronic_out = !val,
            FLIPPER_COIL95 => self.fliptronic_out = val,
            0x3fc0..=0x3fdb => self.extio[a - 0x3fc0] = val,
            SOUND_DATA => self.sound.write_data(val),
            SOUND_CONTROL => self.sound.write_control(val),
            0x3fde | 0x3fdf => {}
            RAM_LOCKSIZE => {
                if self.ram[RAM_LOCK as usize] == RAM_UNLOCK {
                    self.protect_mask = Some(protect_mask(val));
                }
            }
            SW_COL => match self.pic.as_mut() {
                Some(p) => p.write(val),
                None => {
                    self.active_column = match val {
                        0x01 | 0x02 | 0x04 | 0x08 | 0x10 | 0x20 | 0x40 | 0x80 => {
                            val.trailing_zeros() as u8 + 1
                        }
                        _ => 0,
                    }
                }
            },
            FIRQ_SRC => self.firq_source_dmd = false,
            LEDS => {
                if val != self.old_led {
                    self.led_toggles += 1;
                    self.old_led = val;
                }
            }
            ROM_BANK => self.rom_bank = val & self.page_mask,
            ZEROCROSS => {
                if val & 0x04 != 0 {
                    self.watchdog_ticks = WATCHDOG_CYCLES;
                }
                if val & 0x02 != 0 {
                    self.blank = false;
                }
                if val & 0x80 != 0 {
                    self.irq_count_gi += 1;
                }
                self.periodic_irq_enabled = val & 0x10 != 0;
            }
            // solenoids, lamps, GI, ROM lock, clock, shifter, ext boards: the value kept in
            // `ram` is all there is
            _ => {}
        }
    }
}

/// Protected RAM address mask for a RAM_LOCKSIZE value (PinMAME).
pub fn protect_mask(val: u8) -> u16 {
    const SWAP_NIBBLE: [u16; 16] = [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15];
    (SWAP_NIBBLE[(val & 0x0f) as usize] + (val & 0xf0) as u16 + 0x10) << 8
}

impl Memory for Bus {
    fn read(&mut self, addr: u16) -> u8 {
        if !self.patches.is_empty()
            && let Some(&(_, v)) = self.patches.iter().find(|p| p.0 == addr)
        {
            return v;
        }
        let a = addr as usize;
        match addr {
            0x0000..=0x2fff | 0x3c00..=0x3faf => self.ram[a],
            0x3000..=0x3fff => self.hw_read(addr),
            0x4000..=0x7fff => self.rom_banked(a - 0x4000),
            _ => self.rom_fixed(a - 0x8000),
        }
    }

    fn write(&mut self, addr: u16, val: u8) {
        match addr {
            0x0000..=0x2fff | 0x3c00..=0x3faf => {
                if self.ram_writable(addr) {
                    self.ram[addr as usize] = val;
                } else {
                    self.protected_write_attempts += 1;
                }
            }
            0x3000..=0x3fff => self.hw_write(addr, val),
            0x8000..=0xffff if self.rom_writes => {
                let off = self.rom.len() - 0x8000 + (addr as usize - 0x8000);
                Arc::make_mut(&mut self.rom)[off] = val;
            }
            // ROM: writes go nowhere
            _ => {}
        }
    }
}
