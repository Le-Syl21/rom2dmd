//! Sega 192x64 DMD controller board (237-0139-00, display 520-5075-00): Maverick, Frankenstein,
//! Baywatch.
//!
//! A 68000 at 12 MHz prepares frames in RAM for a 6845. From PinMAME `dedmd.c`:
//! - 0x000000-0x0fffff ROM: two 512 KiB EPROMs interleaved (ROM 0 = odd bytes, ROM 3 = even)
//! - 0x800000-0x80ffff RAM (64 KiB)
//! - 0xc00010 CRTC address, 0xc00012 CRTC data (upper byte); any access acknowledges IRQ2
//! - 0xc00020 read: command latch (clears BUSY and IRQ1); write: status byte to the main CPU
//! - IRQ1 (autovector) = command ready, IRQ2 = new frame
//! - one refresh every 2509 x 64 CPU cycles (74.73 Hz): the plane at `src` shows 2/3 of the time,
//!   the one at `src | 0x800` 1/3, `src = (start address & 0x3c00) << 2`
use m68k::{AddressBus, CpuCore, CpuType};

pub const CLOCK_HZ: u64 = 12_000_000;
pub const CYCLES_PER_FRAME: u64 = 2509 * 64;
pub const W: usize = 192;
pub const H: usize = 64;

pub struct Bus {
    pub rom: Vec<u8>,
    pub ram: Vec<u8>,
    pub crtc_addr: u8,
    pub crtc: [u8; 32],
    pub cmd: u8,
    pub busy: bool,
    pub status: u8,
    pub irq1: bool,
    pub irq2: bool,
}

impl Bus {
    fn rd(&mut self, a: u32) -> u8 {
        let a = a & 0xff_ffff;
        match a {
            0x00_0000..=0x0f_ffff => self.rom.get(a as usize).copied().unwrap_or(0xff),
            0x80_0000..=0x80_ffff => self.ram[(a - 0x80_0000) as usize],
            0xc0_0010 | 0xc0_0011 => {
                self.irq2 = false;
                0
            }
            0xc0_0012 => {
                self.irq2 = false;
                self.crtc[(self.crtc_addr & 31) as usize]
            }
            0xc0_0013 => {
                self.irq2 = false;
                0
            }
            0xc0_0020 => 0,
            0xc0_0021 => {
                self.busy = false;
                self.irq1 = false;
                self.cmd
            }
            _ => 0xff,
        }
    }

    fn wr(&mut self, a: u32, v: u8) {
        let a = a & 0xff_ffff;
        match a {
            0x80_0000..=0x80_ffff => self.ram[(a - 0x80_0000) as usize] = v,
            0xc0_0010 => {
                self.irq2 = false;
                self.crtc_addr = v;
            }
            0xc0_0012 => {
                self.irq2 = false;
                self.crtc[(self.crtc_addr & 31) as usize] = v;
            }
            0xc0_0011 | 0xc0_0013 => self.irq2 = false,
            0xc0_0021 => self.status = v,
            _ => {}
        }
    }
}

impl AddressBus for Bus {
    fn read_byte(&mut self, address: u32) -> u8 {
        self.rd(address)
    }
    fn read_word(&mut self, address: u32) -> u16 {
        // the latch is read as a word: one access, upper byte first, then the command byte
        u16::from_be_bytes([self.rd(address), self.rd(address + 1)])
    }
    fn read_long(&mut self, address: u32) -> u32 {
        ((self.read_word(address) as u32) << 16) | self.read_word(address + 2) as u32
    }
    fn write_byte(&mut self, address: u32, value: u8) {
        self.wr(address, value);
    }
    fn write_word(&mut self, address: u32, value: u16) {
        let [h, l] = value.to_be_bytes();
        self.wr(address, h);
        self.wr(address + 1, l);
    }
    fn write_long(&mut self, address: u32, value: u32) {
        self.write_word(address, (value >> 16) as u16);
        self.write_word(address + 2, value as u16);
    }
    fn interrupt_acknowledge(&mut self, level: u8) -> u32 {
        match level {
            2 => self.irq2 = false,
            1 => self.irq1 = false,
            _ => {}
        }
        0xFFFF_FFFF // autovector
    }
}

pub struct Board {
    pub cpu: CpuCore,
    pub bus: Bus,
    cycles: u64,
    next_vblank: u64,
    pub msb_left: bool,
    pub on_frame: Option<crate::scan::FrameSink>,
}

impl Board {
    /// `rom0` holds the odd bytes, `rom3` the even ones.
    pub fn new(rom0: &[u8], rom3: &[u8]) -> Self {
        let mut rom = vec![0xffu8; 0x10_0000];
        for (i, (&o, &e)) in rom0.iter().zip(rom3.iter()).enumerate().take(0x8_0000) {
            rom[2 * i + 1] = o;
            rom[2 * i] = e;
        }
        let mut bus = Bus {
            rom,
            ram: vec![0; 0x1_0000],
            crtc_addr: 0,
            crtc: [0; 32],
            cmd: 0,
            busy: false,
            status: 0,
            irq1: false,
            irq2: false,
        };
        let mut cpu = CpuCore::new();
        cpu.set_cpu_type(CpuType::M68000);
        cpu.reset(&mut bus);
        Board {
            cpu,
            bus,
            cycles: 0,
            next_vblank: CYCLES_PER_FRAME,
            msb_left: std::env::var("LSB_LEFT").is_err(),
            on_frame: None,
        }
    }

    pub fn frame(&self) -> Vec<u8> {
        let base = ((self.bus.crtc[12] as usize) << 8) | self.bus.crtc[13] as usize;
        let src = (base & 0x3c00) << 2;
        let mut out = vec![0u8; W * H];
        for i in 0..W * H / 8 {
            let a = self.bus.ram[(src + i) & 0xffff];
            let b = self.bus.ram[((src | 0x800) + i) & 0xffff];
            for bit in 0..8 {
                let sh = if self.msb_left { 7 - bit } else { bit };
                out[i * 8 + bit] = 2 * ((a >> sh) & 1) + ((b >> sh) & 1);
            }
        }
        out
    }
}

impl crate::scan::Dmd for Board {
    const CLOCK_HZ: u64 = CLOCK_HZ;
    const WIDTH: usize = W;

    fn run(&mut self, cycles: u64) {
        let end = self.cycles + cycles;
        while self.cycles < end {
            let level = if self.bus.irq2 {
                2
            } else if self.bus.irq1 {
                1
            } else {
                0
            };
            self.cpu.set_irq(level);
            let c = match self.cpu.step(&mut self.bus) {
                m68k::StepResult::Ok { cycles } => cycles.max(4) as u64,
                _ => 4,
            };
            self.cycles += c;
            if self.cycles >= self.next_vblank {
                self.next_vblank += CYCLES_PER_FRAME;
                let f = self.frame();
                let t = self.cycles;
                if let Some(cb) = self.on_frame.as_mut() {
                    cb(t, &f);
                }
                self.bus.irq2 = true;
            }
        }
    }

    fn send(&mut self, byte: u8) {
        self.bus.cmd = byte;
        self.bus.busy = true;
        self.bus.irq1 = true;
    }

    fn busy(&self) -> bool {
        self.bus.busy
    }

    fn cycles(&self) -> u64 {
        self.cycles
    }

    fn set_on_frame(&mut self, f: crate::scan::FrameSink) {
        self.on_frame = Some(f);
    }
}
