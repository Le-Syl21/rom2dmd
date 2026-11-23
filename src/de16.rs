//! Data East 128x16 DMD controller board (520-5042-00): Checkpoint, Turtles, Batman, Star Trek, Hook.
//!
//! A Z80 at 4 MHz runs the display ROM; there is no video RAM: the program shifts the dots of
//! one half row (64 px) out through I/O ports and latches them, row by row.
//!
//! From PinMAME `dedmd.c`:
//! - memory: 0x0000-0x3fff the last 16 KiB of the ROM, 0x4000-0x7fff banked ROM (3-bit bank),
//!   0x8000-0x9fff RAM
//! - NMI from a /2048 divider of the main CPU clock (4 MHz / 2048), first one after /512
//! - INT = BUSY: raised when a command is latched, cleared when the program reads it (IDAT)
//! - ports (low address byte):
//!   `0----0--` write: COCLK, shifts in 2 dots taken at bits `(port & 3) * 2`
//!   `0----1--` write: CLATCH, the shifted 64 dots become the latched half row
//!   `1----0--` read: IDAT, the command byte
//!   `1-xxx1--` write, data bit 0: 001 bank0, 011 bank1, 101 bank2 (all inverted), 111 blanking,
//!   x=1 then: 001 status, 011 row data, 101 row clock, 111 test
//! - a half row latched twice in a row is a 2-frame PWM: the first latch shows 1/3 of the
//!   time, the second 2/3, hence 4 levels like the 128x32 board.

use std::cell::Cell;
use zilog_z80::bus::Bus as Z80Bus;
use zilog_z80::cpu::CPU;

pub const CLOCK_HZ: u64 = 4_000_000;
const NMI_PERIOD: u64 = 2048;

pub struct Bus {
    pub rom: Vec<u8>,
    pub ram: [u8; 0x2000],
    pub bank: usize,
    pub cmd: u8,
    pub busy: Cell<bool>,
    pub status: u8,
    pub test: bool,
    blnk: bool,
    rowdata: u8,
    rowclk: u8,
    row_latch: u32,
    last_row_latch: u32,
    dot_shift: u64,
    dot_latch: u64,
    /// Two PWM frames of 16 rows x 128 dots.
    framedata: [[[u8; 128]; 16]; 2],
    /// Set at the end of a refresh (blanking with no row selected).
    pub vblank: bool,
    in_vb: bool,
    pub latch_reads: Cell<u64>,
    int_raise: bool,
}

impl Bus {
    fn rom_byte(&self, off: usize) -> u8 {
        self.rom[off % self.rom.len()]
    }

    fn store_half_row(&mut self, frame: usize, row: usize, side: usize) {
        for k in 0..64 {
            self.framedata[frame][row][side * 64 + k] = ((self.dot_latch >> (63 - k)) & 1) as u8;
        }
    }

    fn updrow(&mut self) {
        if self.blnk && self.row_latch != 0 {
            let n = self.row_latch.trailing_zeros() as usize;
            let (row, side) = ((n >> 1) & 15, n & 1);
            if self.last_row_latch != self.row_latch {
                self.store_half_row(0, row, side);
                self.store_half_row(1, row, side);
            } else {
                self.store_half_row(0, row, side);
            }
            self.last_row_latch = self.row_latch;
        }
        let vb = self.blnk && self.row_latch == 0;
        if vb && !self.in_vb {
            self.vblank = true;
        }
        self.in_vb = vb;
    }
}

impl Z80Bus for Bus {
    fn read_byte(&self, address: u16) -> u8 {
        let a = address as usize;
        match a {
            0x0000..=0x3fff => self.rom_byte(self.rom.len() - 0x4000 + a),
            0x4000..=0x7fff => self.rom_byte(self.bank * 0x4000 + (a - 0x4000)),
            0x8000..=0x9fff => self.ram[a - 0x8000],
            _ => 0xff,
        }
    }

    fn write_byte(&mut self, address: u16, data: u8) {
        let a = address as usize;
        if (0x8000..=0x9fff).contains(&a) {
            self.ram[a - 0x8000] = data;
        }
    }

    fn read_io(&self, port: u16) -> u8 {
        let off = port & 0xff;
        if off & 0x84 == 0x80 {
            self.busy.set(false);
            self.latch_reads.set(self.latch_reads.get() + 1);
            return self.cmd;
        }
        0xff
    }

    fn write_io(&mut self, port: u16, data: u8) {
        let off = (port & 0xff) as u8;
        match off & 0x84 {
            0x00 => {
                let d = (data >> ((off & 3) * 2)) & 3;
                self.dot_shift = (self.dot_shift << 2) | d as u64;
            }
            0x04 => {
                self.dot_latch = self.dot_shift;
                self.updrow();
            }
            0x84 => {
                let d = data & 1;
                match off & 0xdc {
                    0x84 => self.bank = (self.bank & !1) | (d == 0) as usize,
                    0x8c => self.bank = (self.bank & !2) | (((d == 0) as usize) << 1),
                    0x94 => self.bank = (self.bank & !4) | (((d == 0) as usize) << 2),
                    0x9c => {
                        self.blnk = d != 0;
                        self.updrow();
                    }
                    0xc4 => self.status = d,
                    0xcc => self.rowdata = d,
                    0xd4 => {
                        if self.rowclk & !d != 0 {
                            self.row_latch = (self.row_latch << 1) | self.rowdata as u32;
                        }
                        self.rowclk = d;
                        self.updrow();
                    }
                    0xdc => {
                        self.test = d != 0;
                        if !self.test {
                            self.busy.set(true);
                            self.int_raise = true;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

pub struct Board {
    pub cpu: CPU,
    pub bus: Bus,
    cycles: u64,
    next_nmi: u64,
    int_on: bool,
    /// One frame of 128x16 levels (0..3) per refresh, with its cycle stamp.
    pub on_frame: Option<crate::scan::FrameSink>,
}

impl Board {
    pub fn new(rom: Vec<u8>) -> Self {
        let bus = Bus {
            rom,
            ram: [0; 0x2000],
            bank: 7,
            cmd: 0,
            busy: Cell::new(false),
            status: 0,
            test: true,
            blnk: false,
            rowdata: 0,
            rowclk: 0,
            row_latch: 0,
            last_row_latch: 0,
            dot_shift: 0,
            dot_latch: 0,
            framedata: [[[0; 128]; 16]; 2],
            vblank: false,
            in_vb: false,
            latch_reads: Cell::new(0),
            int_raise: false,
        };
        Board {
            cpu: CPU::new(),
            bus,
            cycles: 0,
            next_nmi: 512,
            int_on: false,
            on_frame: None,
        }
    }

    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    pub fn busy(&self) -> bool {
        self.bus.busy.get()
    }

    /// Main CPU side: latch a command byte; BUSY rises and holds INT.
    pub fn send(&mut self, byte: u8) {
        self.bus.cmd = byte;
        self.bus.busy.set(true);
        self.bus.int_raise = true;
    }

    /// Displayed levels: the first PWM frame shows 1/3 of the time, the second 2/3.
    pub fn frame(&self) -> Vec<u8> {
        let mut out = vec![0u8; 128 * 16];
        for r in 0..16 {
            for c in 0..128 {
                out[r * 128 + c] = 2 * self.bus.framedata[1][r][c] + self.bus.framedata[0][r][c];
            }
        }
        out
    }

    pub fn run(&mut self, cycles: u64) {
        let end = self.cycles + cycles;
        while self.cycles < end {
            if self.bus.int_raise {
                self.bus.int_raise = false;
                self.int_on = true;
                self.cpu.int_request(0xff);
            }
            self.cycles += self.cpu.execute(&mut self.bus) as u64;
            // INT follows BUSY (HOLD_LINE also drops it when the CPU takes it)
            if self.int_on && !self.bus.busy.get() {
                self.int_on = false;
                self.cpu.int_cancel();
            }
            if self.int_on && !self.cpu.has_pending_int() {
                self.int_on = false;
            }
            if self.cycles >= self.next_nmi {
                self.next_nmi += NMI_PERIOD;
                self.cpu.nmi_request();
            }
            if self.bus.vblank {
                self.bus.vblank = false;
                let f = self.frame();
                let t = self.cycles;
                if let Some(cb) = self.on_frame.as_mut() {
                    cb(t, &f);
                }
            }
        }
    }
}

impl crate::scan::Dmd for Board {
    const CLOCK_HZ: u64 = CLOCK_HZ;
    const WIDTH: usize = 128;
    fn run(&mut self, cycles: u64) {
        Board::run(self, cycles)
    }
    fn send(&mut self, byte: u8) {
        Board::send(self, byte)
    }
    fn busy(&self) -> bool {
        Board::busy(self)
    }
    fn cycles(&self) -> u64 {
        Board::cycles(self)
    }
    fn set_on_frame(&mut self, f: crate::scan::FrameSink) {
        self.on_frame = Some(f);
    }
}
