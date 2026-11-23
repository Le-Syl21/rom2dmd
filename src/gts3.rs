//! Gottlieb System 3 DMD controller board (128x32): Street Fighter II, Stargate, Mario...
//!
//! A 65C02 at 3.58 MHz / 2 runs `dsprom.bin`. From PinMAME `gts3.c`:
//! - 0x0000-0x1fff RAM; the shown frame is 512 bytes at 0x1000 + (6845 start address >> 2), 1 bit
//!   per dot, first dot in bit 7
//! - 0x2800/0x2801 6845 address/data, 0x3000 read = command latch, 0x3800 write = output port
//!   (bits 0..4 ROM bank: A14..A18 of the window at 0x4000; bit 5 = status 1, toggled when a byte
//!   was taken; bit 6 status 2)
//! - 0x8000-0xffff the last 32 KiB of the ROM
//! - IRQ (held until taken) when the main CPU strobes a byte, NMI at each vblank
//! - shades come from frames alternating at each vblank: PinMAME sums the last N 1-bit frames
//!   (N = 6, 8 or 12 depending on the game) and maps the count to 4 or 5 levels.
use mos6502::cpu::CPU;
use mos6502::instruction::Cmos6502;
use mos6502::memory::Bus as CpuBus;

pub const CLOCK_HZ: u64 = 1_789_772;

pub struct Bus {
    pub rom: Vec<u8>,
    pub ram: [u8; 0x2000],
    pub bank: usize,
    pub crtc_addr: u8,
    pub crtc: [u8; 32],
    pub cmd: u8,
    pub irq: bool,
    pub nmi: bool,
    pub status1: u8,
}

impl CpuBus for Bus {
    fn get_byte(&mut self, address: u16) -> u8 {
        let a = address as usize;
        match a {
            0x0000..=0x1fff => self.ram[a],
            0x2801 => self.crtc[(self.crtc_addr & 31) as usize],
            0x3000 => self.cmd,
            0x4000..=0x7fff => self.rom[(self.bank + a - 0x4000) % self.rom.len()],
            0x8000..=0xffff => self.rom[self.rom.len() - 0x8000 + a - 0x8000],
            _ => 0xff,
        }
    }
    fn set_byte(&mut self, address: u16, value: u8) {
        let a = address as usize;
        match a {
            0x0000..=0x1fff => self.ram[a] = value,
            0x2800 => self.crtc_addr = value,
            0x2801 => self.crtc[(self.crtc_addr & 31) as usize] = value,
            0x3800 => {
                self.bank = ((value & 0x1f) as usize) << 14;
                self.status1 = (value >> 5) & 1;
            }
            _ => {}
        }
    }
    fn nmi_pending(&mut self) -> bool {
        std::mem::take(&mut self.nmi)
    }
    fn irq_pending(&mut self) -> bool {
        self.irq
    }
    fn irq_vector(&mut self) -> u16 {
        // HOLD_LINE: the request ends once the CPU takes it
        self.irq = false;
        0xfffe
    }
}

pub struct Board {
    pub cpu: CPU<Bus, Cmos6502>,
    next_vblank: u64,
    raw: Vec<[u8; 512]>,
    /// frames summed, and the count -> level table (PinMAME's GTS3 combiners)
    n_frames: usize,
    levels: Vec<u8>,
    sent_status: Option<u8>,
    pub on_frame: Option<crate::scan::FrameSink>,
}

impl Board {
    /// `combiner`: "a" (6 frames, default), "b" (8: Stargate, Big Hurt, Waterworld, Andretti, Barb
    /// Wire, Brooks, Strikes N' Spares) or "5" (12 frames, 5 levels: Mario, Cue Ball).
    pub fn new(rom: Vec<u8>, combiner: &str) -> Self {
        let bus = Bus {
            rom,
            ram: [0; 0x2000],
            bank: 0,
            crtc_addr: 0,
            crtc: [0; 32],
            cmd: 0,
            irq: false,
            nmi: false,
            status1: 0,
        };
        let mut cpu = CPU::new(bus, Cmos6502);
        cpu.reset();
        let (n_frames, levels) = match combiner {
            "b" => (8, vec![0, 1, 2, 2, 2, 2, 2, 2, 3]),
            "5" => (12, vec![0, 1, 1, 2, 2, 2, 3, 3, 3, 3, 3, 3, 4]),
            _ => (6, vec![0, 1, 2, 2, 2, 2, 3]),
        };
        Board {
            cpu,
            next_vblank: 30_000,
            raw: Vec::new(),
            n_frames,
            levels,
            sent_status: None,
            on_frame: None,
        }
    }

    fn frame_cycles(&self) -> u64 {
        let c = &self.cpu.memory.crtc;
        let v = (c[0] as u64 + 1) * ((c[4] as u64 + 1) * (c[9] as u64 + 1) + c[5] as u64);
        if v < 1000 { 4760 } else { v } // ~376 Hz until the 6845 is programmed
    }

    fn combined(&self) -> Vec<u8> {
        let mut sum = vec![0u8; 4096];
        for f in self.raw.iter().rev().take(self.n_frames) {
            for (i, &b) in f.iter().enumerate() {
                for bit in 0..8 {
                    sum[i * 8 + bit] += (b >> (7 - bit)) & 1;
                }
            }
        }
        // PinMAME's "4C_A" combiner switches to a second table when a dot counts 4
        if self.n_frames == 6 && sum.contains(&4) {
            let alt = [0u8, 1, 1, 2, 2, 2, 3];
            return sum.iter().map(|&s| alt[s as usize]).collect();
        }
        sum.iter().map(|&s| self.levels[s as usize]).collect()
    }
}

impl crate::scan::Dmd for Board {
    const CLOCK_HZ: u64 = CLOCK_HZ;
    const WIDTH: usize = 128;

    fn run(&mut self, cycles: u64) {
        let end = self.cpu.cycles + cycles;
        while self.cpu.cycles < end {
            self.cpu.single_step();
            if self.cpu.cycles >= self.next_vblank {
                self.next_vblank += self.frame_cycles();
                let m = &self.cpu.memory;
                let base = 0x1000 + ((((m.crtc[12] as usize) << 8) | m.crtc[13] as usize) >> 2);
                let mut f = [0u8; 512];
                for (i, b) in f.iter_mut().enumerate() {
                    *b = m.ram[(base + i) & 0x1fff];
                }
                self.raw.push(f);
                if self.raw.len() > 16 {
                    self.raw.remove(0);
                }
                let out = self.combined();
                let t = self.cpu.cycles;
                if let Some(cb) = self.on_frame.as_mut() {
                    cb(t, &out);
                }
                self.cpu.memory.nmi = true;
            }
        }
    }

    fn send(&mut self, byte: u8) {
        if std::env::var("GTS3_DEBUG").is_ok() {
            let m = &self.cpu.memory;
            eprintln!(
                "send {byte:02x}: pc {:04x} cyc {} crtc {:02x?} bank {:x} st {} ws {:?}",
                self.cpu.registers.program_counter,
                self.cpu.cycles,
                &m.crtc[..16],
                m.bank,
                m.status1,
                self.cpu.wait_state()
            );
        }
        self.cpu.memory.cmd = byte;
        self.cpu.memory.irq = true;
        self.sent_status = Some(self.cpu.memory.status1);
    }

    /// The board toggles status 1 once it took the byte.
    fn busy(&self) -> bool {
        self.sent_status
            .is_some_and(|s| s == self.cpu.memory.status1)
    }

    fn cycles(&self) -> u64 {
        self.cpu.cycles
    }

    fn set_on_frame(&mut self, f: crate::scan::FrameSink) {
        self.on_frame = Some(f);
    }
}
