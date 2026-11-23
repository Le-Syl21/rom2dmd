//! Alvin G. DMD boards PCA020A (128x32): Mystery Castle, Pistol Poker; and PCA020 (env
//! ALVG_PCA020=1): Al's Garage Band Goes On A World Tour.
//!
//! An 8031 at 12 MHz. From PinMAME `alvgdmd.c`:
//! - code: 0x0000-0x7fff the program ROM (U4), 0x8000-0xffff a 32 KiB window of the data ROM
//!   (U5..), bank = bits 0..4 written to 0xa000
//! - external data (MOVX): 0x0000-0x7fff RAM; 0x8000 read = command latch (clears INT0), 0x9000
//!   write = byte back to the main CPU, 0xa000 bank, 0xb000 ROWSTART (PCA020A: `(v & 0x1f) |
//!   0x20`), 0xc000 COLSTART
//! - P1 = video page (bits 0..3), P3.0 = PLANS ENABLE, P3.5 = display disabled, P3.4 (T0) reads 0
//!   on PCA020A
//! - INT0 held low by the main CPU's strobe until the latch is read; INT1 pulsed once per four
//!   rasterized frames (74.6 Hz)
//! - picture: four 512-byte planes from the RAM at `vid_page << 11 + (colstart >> 3)`, rows at
//!   `(rowstart + 0x20|0x40|0x60|0x00) & 0x7f`, 16 bytes each; PinMAME sums the four 1-bit planes
//!   (0..4, its ALVG2 filter shows them as 0/3/7/11/15). With PLANS ENABLE off the same plane is
//!   shown four times.
//! - PCA020 differences: ROWSTART keeps bits 5-6 (the plane bits GPLAN0..1 are free), the row
//!   counter starts one row later (`rowstart + 1`), T0 reads 1, and INT1 comes every
//!   4 x 32 x 4 x 319.25 clocks (measured on real hardware).
use i8051::{
    Cpu, CpuContext, CpuView, Interrupt, MemoryMapper, PortMapper, ReadOnlyMemoryMapper, Register,
};

pub const CLOCK_HZ: u64 = 12_000_000;
/// INT1 period in clocks: 4 frames x 32 rows x 4 x 314 VCLOCK, VCLOCK = clock / 4
const INT1_CLOCKS: u64 = 4 * 32 * 4 * 314 * 4 / 4;
/// PCA020: 319.25 VCLOCK per row
const INT1_CLOCKS_PCA020: u64 = 4 * 32 * 4 * 31925 / 100;

const SFR_P1: u8 = 0x90;
const SFR_P3: u8 = 0xb0;

pub struct Hw {
    code: Vec<u8>,
    data: Vec<u8>,
    bank: usize,
    pub ram: Vec<u8>,
    sfr: [u8; 128],
    rowstart: u8,
    colstart: u8,
    pub cmd: u8,
    int0: bool,
    int1: bool,
    latch_read: bool,
    /// PCA020 board (Al's Garage Band) rather than PCA020A
    pca020: bool,
}

impl ReadOnlyMemoryMapper for Hw {
    fn len(&self) -> u32 {
        0x10000
    }
    fn read<C: CpuView>(&self, _cpu: &C, addr: u32) -> u8 {
        let a = addr as usize & 0xffff;
        if a < 0x8000 {
            self.code[a % self.code.len()]
        } else {
            self.data[(self.bank + a - 0x8000) % self.data.len()]
        }
    }
}

pub enum XWrite {
    Ram(usize, u8),
    Ctl(u16, u8),
}

impl MemoryMapper for Hw {
    type WriteValue = XWrite;
    fn len(&self) -> u32 {
        0x10000
    }
    fn read<C: CpuView>(&self, _cpu: &C, addr: u32) -> u8 {
        let a = addr as usize & 0xffff;
        if a < 0x8000 {
            self.ram[a]
        } else if a & 0xf000 == 0x8000 {
            // the latch is cleared in `step` (reads cannot mutate here)
            self.cmd
        } else {
            0
        }
    }
    fn prepare_write<C: CpuView>(&self, _cpu: &C, addr: u32, value: u8) -> XWrite {
        let a = addr as usize & 0xffff;
        if a < 0x8000 {
            XWrite::Ram(a, value)
        } else {
            XWrite::Ctl(a as u16 & 0xf000, value)
        }
    }
    fn write(&mut self, w: XWrite) {
        match w {
            XWrite::Ram(a, v) => self.ram[a] = v,
            XWrite::Ctl(0xa000, v) => self.bank = ((v & 0x1f) as usize) * 0x8000,
            XWrite::Ctl(0xb000, v) => {
                self.rowstart = if self.pca020 { v & 0x7f } else { (v & 0x1f) | 0x20 }
            }
            XWrite::Ctl(0xc000, v) => self.colstart = v,
            XWrite::Ctl(..) => {}
        }
    }
}

impl PortMapper for Hw {
    type WriteValue = (u8, u8);
    fn interest<C: CpuView>(&self, _cpu: &C, _addr: u8) -> bool {
        true
    }
    fn read<C: CpuView>(&self, _cpu: &C, addr: u8) -> u8 {
        let v = self.sfr[(addr & 0x7f) as usize];
        if addr == SFR_P3 {
            // INT0/INT1 pins low while requested, T0 (P3.4) wired to 0 on PCA020A, 1 on PCA020
            let mut p = v & !0x1c;
            if self.pca020 {
                p |= 0x10;
            }
            if !self.int0 {
                p |= 0x04;
            }
            p |= 0x08; // INT1 is raised directly (see `step`)
            p
        } else {
            v
        }
    }
    fn read_latch<C: CpuView>(&self, _cpu: &C, addr: u8) -> u8 {
        self.sfr[(addr & 0x7f) as usize]
    }
    fn prepare_write<C: CpuView>(&self, _cpu: &C, addr: u8, value: u8) -> (u8, u8) {
        (addr, value)
    }
    fn write(&mut self, (addr, value): (u8, u8)) {
        self.sfr[(addr & 0x7f) as usize] = value;
    }
}

impl CpuContext for Hw {
    type Ports = Hw;
    type Xdata = Hw;
    type Code = Hw;
    fn ports(&self) -> &Hw {
        self
    }
    fn xdata(&self) -> &Hw {
        self
    }
    fn code(&self) -> &Hw {
        self
    }
    fn ports_mut(&mut self) -> &mut Hw {
        self
    }
    fn xdata_mut(&mut self) -> &mut Hw {
        self
    }
    fn code_mut(&mut self) -> &mut Hw {
        self
    }
}

fn env_hex(k: &str, d: u16) -> u16 {
    std::env::var(k)
        .ok()
        .and_then(|v| u16::from_str_radix(&v, 16).ok())
        .unwrap_or(d)
}

/// Machine cycles (12 clocks each) of an 8051 opcode.
fn machine_cycles(op: u8) -> u64 {
    match op {
        0xa4 | 0x84 => 4,
        // AJMP / ACALL
        o if o & 0x0f == 0x01 => 2,
        0x02
        | 0x12
        | 0x22
        | 0x32
        | 0x10
        | 0x20
        | 0x30
        | 0x40
        | 0x50
        | 0x60
        | 0x70
        | 0x80
        | 0x73
        | 0x83
        | 0x93
        | 0x82
        | 0x72
        | 0xb0
        | 0xa0
        | 0x90
        | 0x92
        | 0xa3
        | 0xe0
        | 0xe2
        | 0xe3
        | 0xf0
        | 0xf2
        | 0xf3
        | 0xb4..=0xbf
        | 0xd5
        | 0xd8..=0xdf
        | 0xc0
        | 0xd0
        | 0x85
        | 0x86
        | 0x87
        | 0x88..=0x8f
        | 0xa6
        | 0xa7
        | 0xa8..=0xaf
        | 0x43
        | 0x53
        | 0x63
        | 0x75 => 2,
        _ => 1,
    }
}

pub struct Board {
    cpu: Cpu,
    pub hw: Hw,
    clocks: u64,
    next_int1: u64,
    int1_clocks: u64,
    /// the program's "next command byte" routine and the main loop's return address after
    /// calling it (Mystery Castle: 0x0ae9, 0x006a; env ALVG_GETBYTE / ALVG_IDLE_RET)
    get_byte: u16,
    idle_ret: u16,
    pub on_frame: Option<crate::scan::FrameSink>,
}

impl Board {
    /// `code` = the 64 KiB program ROM (its first half is used), `data` = the data ROM(s).
    pub fn new(code: Vec<u8>, data: Vec<u8>) -> Self {
        // the data ROM region is 1 MiB, a 256 KiB ROM repeated (PinMAME ROM_RELOAD)
        let mut d = Vec::with_capacity(0x100000);
        while d.len() < 0x100000 {
            d.extend_from_slice(&data);
        }
        d.truncate(0x100000);
        let mut sfr = [0u8; 128];
        for p in [0x80u8, 0x90, 0xa0, 0xb0] {
            sfr[(p & 0x7f) as usize] = 0xff;
        }
        sfr[(0x81 & 0x7f) as usize] = 0x07; // SP
        let pca020 = std::env::var("ALVG_PCA020").is_ok_and(|v| v == "1");
        let int1_clocks = if pca020 { INT1_CLOCKS_PCA020 } else { INT1_CLOCKS };
        Board {
            cpu: Cpu::new(),
            hw: Hw {
                code,
                data: d,
                bank: 0,
                ram: vec![0; 0x8000],
                sfr,
                rowstart: 0x20,
                colstart: 0,
                cmd: 0,
                int0: false,
                int1: false,
                latch_read: false,
                pca020,
            },
            clocks: 0,
            next_int1: int1_clocks,
            int1_clocks,
            get_byte: env_hex("ALVG_GETBYTE", 0x0ae9),
            idle_ret: env_hex("ALVG_IDLE_RET", 0x006a),
            on_frame: None,
        }
    }

    fn picture(&self) -> Vec<u8> {
        let mut out = vec![0u8; 128 * 32];
        let p3 = self.hw.sfr[(SFR_P3 & 0x7f) as usize];
        if p3 & 0x20 != 0 {
            return out; // display disabled
        }
        let page = (self.hw.sfr[(SFR_P1 & 0x7f) as usize] & 0x0f) as usize;
        let base = (page << 11) + ((self.hw.colstart as usize >> 3) & 0x0f);
        // the PCA020 row counter starts one row after ROWSTART
        let rs = self.hw.rowstart as usize + self.hw.pca020 as usize;
        // PLANS ENABLE: four planes; otherwise the plane at `rowstart & 0x1f` four times
        let (planes, mask): ([usize; 4], usize) = if p3 & 0x01 != 0 {
            ([0x20, 0x40, 0x60, 0x00], 0x7f)
        } else {
            ([0, 0, 0, 0], 0x1f)
        };
        for off in planes {
            let start = base + (((rs + off) & mask) << 4);
            for (i, o) in out.iter_mut().enumerate() {
                let byte = self.hw.ram[(start + i / 8) & 0x7fff];
                // first dot in bit 7
                if byte >> (7 - i % 8) & 1 != 0 {
                    *o += 1;
                }
            }
        }
        out
    }

    fn step(&mut self) {
        let op = {
            let pc = self.cpu.pc_ext(&self.hw);
            (&self.cpu, &self.hw).read_code(pc)
        };
        // MOVX A,@DPTR (0xe0) or @Ri (0xe2/0xe3) on the latch clears INT0
        if self.hw.int0 && matches!(op, 0xe0 | 0xe2 | 0xe3) {
            let addr = if op == 0xe0 {
                self.cpu.dptr()
            } else {
                let r = self.cpu.r(op & 1) as u16;
                r | (self.hw.sfr[(0xa0 & 0x7f) as usize] as u16) << 8
            };
            if addr & 0xf000 == 0x8000 {
                self.hw.latch_read = true;
            }
        }
        // INT1 is edge-triggered (IT1 set by the program): the request waits until the CPU can
        // take it, so it is raised directly rather than through the P3.3 pin level
        if self.hw.int1 && self.cpu.interrupt(Interrupt::External1) {
            self.hw.int1 = false;
        }
        self.cpu.step(&mut self.hw);
        if self.hw.latch_read {
            self.hw.latch_read = false;
            self.hw.int0 = false;
        }
        self.clocks += machine_cycles(op) * 12;
        if self.clocks >= self.next_int1 {
            self.next_int1 += self.int1_clocks;
            self.hw.int1 = true;
            let pic = self.picture();
            let t = self.clocks;
            if let Some(f) = self.on_frame.as_mut() {
                f(t, &pic);
            }
        }
    }
}

impl Board {
    /// Disassembly of `n` instructions of the code space from `start` (bank `bank`), for study.
    pub fn disassemble(&mut self, start: u32, n: usize, bank: usize) -> Vec<String> {
        self.hw.bank = bank * 0x8000;
        let mut pc = start;
        let mut out = Vec::new();
        for _ in 0..n {
            let ins = self.cpu.decode(&self.hw, pc);
            out.push(format!("{pc:04x}: {ins}"));
            pc += ins.len() as u32;
        }
        out
    }
}

impl Board {
    /// State summary for study: pc, IE, queue pointers, P1, P3.
    pub fn state(&self) -> String {
        let r = &self.cpu.internal_ram;
        format!(
            "t={:.3}s pc={:04x} ie={:02x} sp={:02x} q={:02x}/{:02x} p1={:02x} p3={:02x} int0={} int1={} bank={:x}",
            self.clocks as f64 / CLOCK_HZ as f64,
            self.cpu.pc,
            self.cpu.register(Register::IE),
            self.cpu.register(Register::SP),
            r[0x37],
            r[0x38],
            self.hw.sfr[0x10],
            self.hw.sfr[0x30],
            self.hw.int0,
            self.hw.int1,
            self.hw.bank / 0x8000
        )
    }
}

impl crate::scan::Dmd for Board {
    const CLOCK_HZ: u64 = CLOCK_HZ;
    const WIDTH: usize = 128;
    fn run(&mut self, cycles: u64) {
        let end = self.clocks + cycles;
        while self.clocks < end {
            self.step();
        }
    }
    fn send(&mut self, byte: u8) {
        self.hw.cmd = byte;
        self.hw.int0 = true;
    }
    fn busy(&self) -> bool {
        self.hw.int0
    }
    fn cycles(&self) -> u64 {
        self.clocks
    }
    fn set_on_frame(&mut self, f: crate::scan::FrameSink) {
        self.on_frame = Some(f);
    }
    fn wants_arg(&self) -> bool {
        // in the "next byte" routine with an empty queue, called from a command handler rather
        // than from the main loop
        let pc = self.cpu.pc;
        if self.hw.int0 || pc < self.get_byte || pc > self.get_byte + 8 {
            return false;
        }
        let sp = self.cpu.register(Register::SP) as usize;
        let ram = &self.cpu.internal_ram;
        let ret = (ram[sp & 0xff] as u16) << 8 | ram[(sp + 255) & 0xff] as u16;
        ret != self.idle_ret
    }
}
