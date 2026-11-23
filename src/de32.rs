//! Data East / Sega-Stern Whitestar 128x32 DMD controller board (520-5055-xx).
//!
//! The board has its own 6809 at 2 MHz running the display ROM. The main CPU only
//! sends command bytes through a latch (IRQ on the DMD CPU), so the board can be
//! emulated alone and driven with any command sequence.
//!
//! Memory map (from PinMAME `dedmd.c`):
//! - 0x0000-0x1fff static RAM
//! - 0x2000-0x2fff banked RAM (8 x 4 KiB), bank = bits 5..7 of the bank latch
//! - 0x3000-0x3fff I/O: +0 CRTC address, +1 CRTC data, +2/+3 read = command latch
//!   (clears IRQ / busy), +2 write = bank latch (ROM bank = bits 0..4)
//! - 0x4000-0x7fff read: banked ROM (32 x 16 KiB); write: status nibble to the main CPU
//! - 0x8000-0xffff the last 32 KiB of the display ROM
//!
//! Video: every vblank the 6845 start address selects two 512-byte planes in RAM:
//! the plane at `src` is shown 2/3 of the time, the one at `src | 0x200` 1/3,
//! hence 4 exact levels (2*a + b). FIRQ is pulsed at each vblank.

use mc6809_core::{Cpu, Memory};

/// CPU cycles per vblank: measured 78.07 Hz for 3 PWM slots on board 520-5055-00.
pub const CYCLES_PER_FRAME_DE: u64 = 25620;
/// Same for the Whitestar boards (520-5055-01..03): 77.77 Hz.
pub const CYCLES_PER_FRAME_WS: u64 = 25720;
pub const CLOCK_HZ: u64 = 2_000_000;

pub struct Bus {
    pub rom: Vec<u8>,
    pub sram: [u8; 0x2000],
    pub ram: Vec<u8>, // 8 x 4 KiB
    pub ram_bank: usize,
    pub rom_bank: usize,
    pub crtc_addr: u8,
    pub crtc: [u8; 32],
    pub cmd: u8,
    pub busy: bool,
    pub status: u8,
    pub status_writes: Vec<(u64, u8)>,
    pub latch_reads: u64,
    pub now: u64,
    pub pc: u16,
    pub watch: Option<(u16, u16)>,
    pub watch_log: Vec<(u16, u16)>,
}

impl Bus {
    fn rom_byte(&self, off: usize) -> u8 {
        self.rom[off % self.rom.len()]
    }
}

/// Callback receiving each new 128x32 picture.
pub type FrameSink32 = Box<dyn FnMut(u64, &[u8; 4096])>;

impl Memory for Bus {
    fn read(&mut self, addr: u16) -> u8 {
        let a = addr as usize;
        if let Some((lo, hi)) = self.watch
            && addr >= lo
            && addr <= hi
            && self.watch_log.len() < 4096
            && !self.watch_log.contains(&(self.pc, addr))
        {
            self.watch_log.push((self.pc, addr));
        }
        match a {
            0x0000..=0x1fff => self.sram[a],
            0x2000..=0x2fff => self.ram[self.ram_bank * 0x1000 + (a - 0x2000)],
            0x3000..=0x3fff => match a & 3 {
                1 => self.crtc[(self.crtc_addr & 31) as usize],
                2 | 3 => {
                    self.busy = false;
                    self.latch_reads += 1;
                    self.cmd
                }
                _ => 0,
            },
            0x4000..=0x7fff => self.rom_byte(self.rom_bank * 0x4000 + (a - 0x4000)),
            _ => {
                let base = self.rom.len() - 0x8000;
                self.rom_byte(base + (a - 0x8000))
            }
        }
    }

    fn write(&mut self, addr: u16, val: u8) {
        let a = addr as usize;
        match a {
            0x0000..=0x1fff => self.sram[a] = val,
            0x2000..=0x2fff => self.ram[self.ram_bank * 0x1000 + (a - 0x2000)] = val,
            0x3000..=0x3fff => match a & 3 {
                0 => self.crtc_addr = val,
                1 => self.crtc[(self.crtc_addr & 31) as usize] = val,
                2 => {
                    self.ram_bank = (val >> 5) as usize;
                    self.rom_bank = (val & 0x1f) as usize;
                }
                _ => {}
            },
            0x4000..=0x7fff => {
                self.status = val & 0x0f;
                self.status_writes.push((self.now, self.status));
            }
            _ => {}
        }
    }
}

pub struct Board {
    pub cpu: Cpu,
    pub bus: Bus,
    pub frame_cycles: u64,
    next_vblank: u64,
    irq_pending: bool,
    /// One 4096-byte frame (levels 0..3) per vblank, with its cycle stamp.
    pub on_frame: Option<FrameSink32>,
    pub msb_left: bool,
    /// Program addresses to log: each hit records (cycle, registers before the instruction).
    pub hooks: Vec<u16>,
    /// Each hit records (cycle, registers, RAM word at `hook_ram`: the current task on JP).
    pub hook_log: Vec<(u64, mc6809_core::Registers, u16)>,
    pub hook_ram: usize,
}

impl Board {
    pub fn new(rom: Vec<u8>, frame_cycles: u64) -> Self {
        let mut bus = Bus {
            rom,
            sram: [0; 0x2000],
            ram: vec![0; 0x8000],
            ram_bank: 0,
            rom_bank: 0,
            crtc_addr: 0,
            crtc: [0; 32],
            cmd: 0,
            busy: false,
            status: 0,
            status_writes: Vec::new(),
            latch_reads: 0,
            now: 0,
            pc: 0,
            watch: None,
            watch_log: Vec::new(),
        };
        let mut cpu = Cpu::new();
        cpu.reset(&mut bus);
        Board {
            cpu,
            bus,
            frame_cycles,
            next_vblank: frame_cycles,
            irq_pending: false,
            on_frame: None,
            msb_left: true,
            hooks: Vec::new(),
            hook_log: Vec::new(),
            hook_ram: 9,
        }
    }

    /// Main CPU side: put a byte in the latch and raise IRQ (strobe of ctrl bit 0).
    pub fn send(&mut self, byte: u8) {
        self.bus.cmd = byte;
        self.bus.busy = true;
        self.irq_pending = true;
        self.cpu.set_irq(true);
    }

    pub fn start_address(&self) -> usize {
        ((self.bus.crtc[12] as usize) << 8) | self.bus.crtc[13] as usize
    }

    pub fn frame(&self) -> [u8; 4096] {
        let base = self.start_address();
        let src = (base << 2) & 0x7c00;
        let ram = &self.bus.ram[self.bus.ram_bank * 0x1000..];
        let mut out = [0u8; 4096];
        for i in 0..512 {
            let a = ram.get((src + i) % ram.len().max(1)).copied().unwrap_or(0);
            let b = ram
                .get(((src | 0x200) + i) % ram.len().max(1))
                .copied()
                .unwrap_or(0);
            for bit in 0..8 {
                let sh = if self.msb_left { 7 - bit } else { bit };
                out[i * 8 + bit] = 2 * ((a >> sh) & 1) + ((b >> sh) & 1);
            }
        }
        out
    }

    /// Run `cycles` CPU cycles, pulsing FIRQ and emitting a frame at each vblank.
    pub fn run(&mut self, cycles: u64) {
        let end = self.cpu.cycles() + cycles;
        while self.cpu.cycles() < end {
            let before_masked = self.cpu.registers().cc.irq_inhibit();
            let irq_vec = u16::from_be_bytes([self.bus.read(0xfff8), self.bus.read(0xfff9)]);
            self.bus.now = self.cpu.cycles();
            self.bus.pc = self.cpu.registers().pc;
            if !self.hooks.is_empty() && self.hooks.contains(&self.bus.pc) {
                let w = u16::from_be_bytes([
                    self.bus.sram[self.hook_ram],
                    self.bus.sram[self.hook_ram + 1],
                ]);
                self.hook_log.push((self.bus.now, *self.cpu.registers(), w));
            }
            self.cpu.step(&mut self.bus);
            if self.cpu.illegal() {
                self.cpu.clear_illegal();
            }
            // HOLD_LINE semantics: the request is dropped once the CPU takes it,
            // and also when the latch is read.
            if self.irq_pending
                && ((!before_masked && self.cpu.registers().pc == irq_vec) || !self.bus.busy)
            {
                self.irq_pending = false;
                self.cpu.set_irq(false);
            }
            if self.cpu.cycles() >= self.next_vblank {
                self.next_vblank += self.frame_cycles;
                let f = self.frame();
                let t = self.cpu.cycles();
                if let Some(cb) = self.on_frame.as_mut() {
                    cb(t, &f);
                }
                let cursor = ((self.bus.crtc[14] as u16) << 8) | self.bus.crtc[15] as u16;
                if cursor != 0 && !self.cpu.registers().cc.firq_inhibit() {
                    self.cpu.set_firq(true);
                    self.cpu.step(&mut self.bus);
                    self.cpu.set_firq(false);
                }
            }
        }
    }
}
