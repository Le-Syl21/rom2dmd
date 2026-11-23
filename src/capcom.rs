//! Capcom main board (1995-96): Pinball Magic, Airborne, Breakshot, Flipper Football, Big Bang
//! Bar, Kingpin, Pool Player.
//!
//! There is no display board: the main MC68306 (68000 core at 16.67 MHz with on-chip chip
//! selects, two 8-bit ports, an interrupt controller and a DUART) draws into its own DRAM and the
//! custom U16 chip streams a RAM block to the display. From PinMAME `capcom.c` and the 68306
//! part of `m68kmame.c`:
//! - 0x00000000-0x0007ffff DRAM (the first 0x100 bytes are a copy of the ROM's vector table)
//! - 0x10000000-0x10ffffff ROM: pairs of 512 KiB EPROMs interleaved (U1L odd bytes, U1H even),
//!   pair n at n * 0x400000
//! - 0x30000000-0x3001ffff NVRAM
//! - 0x40000000 AUX I/O (lamps, driver board id), 0x40400000 EXT I/O (switch board, solenoids),
//!   0x40800000 cabinet switches, 0x40c00000 U16 (interrupt timers, DMD block address)
//! - 0xfffff7e1.. DUART (odd bytes), 0xffffffc0.. 68306 registers (chip selects, ports, ISR/ICR,
//!   system register)
//! - interrupts: IRQ1 = U16 timer, IRQ2 = zero cross (120 Hz, latched, acked through port B0),
//!   IRQ4 = U16 lines (two programmable timers, line 3 = DMD vblank), level 7 = DUART timer
//! - DMD: 256 dots of 2 bits per row (64 bytes), the 128x32 display skips the first 128 dots;
//!   the U16 makes four PWM frames of each picture: value 0..3 = 0, 25, 50, 100 %
use m68k::{AddressBus, CpuCore, CpuType};

pub const CLOCK_HZ: u64 = 16_670_000;
/// cycles of one taken `DBF` loop pass on the 68000 (the U16 periods are counted in passes)
const DBF_PASS: f64 = 10.0;
/// U16 IRQ4 lines 1/2: passes per unit of (0x1000 - register)
const IRQ4_PASSES_PER_UNIT: f64 = 88.675 / 14.0;

fn secs(c: u64) -> f64 {
    c as f64 / CLOCK_HZ as f64
}

#[derive(Default)]
struct Timer {
    /// next expiry (cycle), 0 = stopped
    next: u64,
    period: u64,
}

impl Timer {
    fn start(&mut self, now: u64, first: u64, period: u64) {
        self.next = if first == 0 { 0 } else { now + first };
        self.period = period;
    }
    fn due(&mut self, now: u64) -> bool {
        if self.next != 0 && now >= self.next {
            self.next = if self.period == 0 {
                0
            } else {
                self.next + self.period
            };
            true
        } else {
            false
        }
    }
}

pub struct Bus {
    pub rom: Vec<u8>,
    pub ram: Vec<u8>,
    pub nvram: Vec<u8>,
    pub now: u64,
    /// game has the switch board (all but Breakshot / Pool Player)
    switch_board: bool,
    pub sw: [u8; 12],
    // 68306 internal registers (word index = (address & 0x3f) >> 1)
    intreg: [u16; 0x20],
    duart: [u8; 0x20],
    mode_ptr: [usize; 2],
    tx_enable: [bool; 2],
    rx_enable: [bool; 2],
    tx_done: [u64; 2],
    pub sound_bytes: Vec<(u64, u8)>,
    duart_timer: Timer,
    duart_timer_out: bool,
    /// DUART timer interrupt waiting for the CPU (level 7, vector from the system register)
    tirq: bool,
    // ports
    parallel_a: u16,
    parallel_b: u16,
    pulse: bool,
    pulse_v: f64,
    pulse_t: f64,
    zero_cross: bool,
    zero_acq: bool,
    // U16
    pub dmd_addr: u32,
    u16a: [u16; 4],
    u16b: [u16; 4],
    irq4_mask: u16,
    irq_state: u16,
    irq1_adjust: u16,
    irq1_period: f64,
    irq4_period: [f64; 3],
    irq1_t: Timer,
    irq4_t: [Timer; 3],
    lamp_a: u16,
    lamp_b: u16,
    sw_col: u16,
    /// study: PC of the instruction being run, and per-PC counts of data reads from ROM at or
    /// above `watch_rom` and of writes to RAM at or above `watch_ram` (0 = off)
    pub pc: u32,
    pub watch_rom: u32,
    pub watch_ram: u32,
    pub rom_reads: std::collections::HashMap<u32, u64>,
    pub ram_writes: std::collections::HashMap<u32, u64>,
}

impl Bus {
    fn passes(p: f64) -> u64 {
        (p * DBF_PASS) as u64
    }

    fn porta_r(&self) -> u16 {
        let mut d = self.parallel_a & 0xcf;
        let now = secs(self.now);
        let rc = 11000.0 * 33.0e-9;
        let v = if self.pulse {
            self.pulse_v + (5.0 - self.pulse_v) * (1.0 - (-(now - self.pulse_t) / rc).exp())
        } else {
            self.pulse_v * (-(now - self.pulse_t) / rc).exp()
        };
        if v > (55.0 - 1.1 - 1.1) * 2000.0 / (56200.0 + 2000.0) {
            d |= 0x20;
        }
        if v > 2.5 {
            d |= 0x10;
        }
        d
    }

    fn portb_r(&self) -> u16 {
        (self.parallel_b & 0xef) | ((self.zero_cross as u16) << 4)
    }

    fn porta_w(&mut self, data: u16) {
        self.parallel_a = data;
    }

    fn portb_w(&mut self, data: u16) {
        let p = data & 2 != 0;
        if p != self.pulse {
            let now = secs(self.now);
            let rc = 11000.0 * 33.0e-9;
            self.pulse_v = if p {
                self.pulse_v * (-(now - self.pulse_t) / rc).exp()
            } else {
                self.pulse_v + (5.0 - self.pulse_v) * (1.0 - (-(now - self.pulse_t) / rc).exp())
            };
            self.pulse_t = now;
            self.pulse = p;
        }
        self.zero_acq = data & 1 != 0;
        if self.zero_acq {
            self.zero_cross = false;
        }
        self.parallel_b = data;
    }

    // ---- U16 ----
    fn u16_r(&self, off: u32) -> u16 {
        match off {
            0 => 0x00bc,
            1 => !(self.irq4_mask & self.irq_state),
            o if o < 4 => self.u16a[o as usize],
            o => self.u16b[(o & 3) as usize],
        }
    }

    fn u16_w(&mut self, off: u32, data: u16, mem_mask: u16) {
        match off {
            1 => {
                self.irq_state &= data;
                if data & 0x10 == 0 && self.irq1_adjust == data & 0x08 {
                    self.irq1_adjust = !data & 0x08;
                    let p = self.irq1_cycles();
                    self.irq1_t.start(self.now, p, p);
                }
            }
            2 => self.dmd_addr = (self.dmd_addr & 0xffff) | (data as u32) << 16,
            3 => self.dmd_addr = (self.dmd_addr & 0xffff_0000) | data as u32,
            0x200 => {
                const IRQ1_PASSES: [f64; 4] =
                    [22602.0 / 14.0, 11248.0 / 14.0, 5571.0 / 14.0, 2729.0 / 14.0];
                self.irq1_period = IRQ1_PASSES[((data >> 6) & 3) as usize];
                let p = self.irq1_cycles();
                self.irq1_t.start(self.now, p, p);
                for i in 0..3 {
                    let p = Self::passes(self.irq4_period[i]);
                    self.irq4_t[i].start(self.now, p, p);
                }
                self.irq4_mask = (data & 7) ^ 4;
                self.irq_state = 0;
            }
            0x201 => self.irq4_period[0] = (0x1000 - (data & 0xfff)) as f64 * IRQ4_PASSES_PER_UNIT,
            0x202 => self.irq4_period[1] = (0x1000 - (data & 0xfff)) as f64 * IRQ4_PASSES_PER_UNIT,
            _ => {}
        }
        if off < 4 {
            self.u16a[off as usize] = (self.u16a[off as usize] & mem_mask) | data;
        } else {
            self.u16b[(off & 3) as usize] = (self.u16b[(off & 3) as usize] & mem_mask) | data;
        }
    }

    fn irq1_cycles(&self) -> u64 {
        let p = if self.irq1_adjust != 0 {
            self.irq1_period * 120.0 / 165.0
        } else {
            self.irq1_period
        };
        Self::passes(p)
    }

    // ---- I/O boards (word offsets from 0x40000000) ----
    fn io_r(&self, off: u32) -> u16 {
        match off {
            0x08 => {
                if self.switch_board {
                    0x0300
                } else {
                    0x0700
                }
            }
            0x06 | 0x0a => 0xffff,
            0x0c => (!self.lamp_a & 0xff00) | (self.lamp_a & 0xff),
            0x0d => (!self.lamp_b & 0xff00) | (self.lamp_b & 0xff),
            0x20_0008..=0x20_000b if self.switch_board => {
                let c = (off - 0x20_0007) as usize;
                ((self.sw[c + 4] as u16) << 8 | self.sw[c] as u16) ^ 0xffff
            }
            0x40_0000 => {
                let d = if self.switch_board {
                    (self.sw[0] as u16) << 8 | self.sw[9] as u16
                } else {
                    let mut d = self.sw[9] as u16;
                    for b in 0..8 {
                        if self.sw_col == 0x80 >> b {
                            d |= (self.sw[1 + b] as u16) << 8;
                        }
                    }
                    d
                };
                d ^ 0xffff
            }
            _ => 0,
        }
    }

    fn io_w(&mut self, off: u32, data: u16) {
        match off {
            0x0c => {
                if self.switch_board {
                    self.lamp_a = (data & 0xff00) | (!data & 0xff);
                } else {
                    self.sw_col = data;
                }
            }
            0x0d => self.lamp_b = (data & 0xff00) | (!data & 0xff),
            _ => {}
        }
    }

    // ---- DUART ----
    fn set_dusr(&mut self, ch: usize, v: u8) {
        let i = 2 + ch * 0x10;
        self.duart[i] = v;
        let mut isr = self.duart[10] & !(1 << (ch * 4)) & !(1 << (ch * 4 + 1));
        isr |= ((v >> 2) & 1) << (ch * 4);
        let rx = if self.duart[ch * 0x10] & 0x40 != 0 {
            (v >> 1) & 1
        } else {
            v & 1
        };
        isr |= rx << (ch * 4 + 1);
        self.duart[10] = isr;
    }

    fn duart_timer_restart(&mut self, reset: bool) {
        self.duart_timer.next = 0;
        let clk = match self.duart[9] & 0x30 {
            0x10 | 0x30 => 3_686_400.0 / 16.0,
            _ => 3_686_400.0,
        };
        let preload = (self.duart[12] as u32) << 8 | self.duart[14] as u32;
        if preload < 2 {
            return;
        }
        let n = if self.duart[9] & 0x40 != 0 || reset {
            preload
        } else {
            0xffff
        };
        let c = (n as f64 / clk * CLOCK_HZ as f64) as u64;
        self.duart_timer.start(self.now, c.max(1), 0);
    }

    fn duart_timer_fire(&mut self) {
        self.duart_timer_out = !self.duart_timer_out;
        if !self.duart_timer_out {
            self.duart[10] |= 0x08;
            self.duart_check_int();
        }
        self.duart_timer_restart(false);
    }

    fn duart_check_int(&mut self) {
        if self.duart[10] & self.duart[11] != 0 && self.duart[10] & 0x08 != 0 {
            let irq = (self.duart[11] & 0x08) >> 3;
            let tirq = ((self.intreg[0x1d] & 0x8000) >> 14) as u8;
            if irq + tirq == 2 {
                self.tirq = true;
            }
        }
    }

    fn duart_r(&mut self, a: u32) -> u8 {
        let i = (a - 0xf7e1) as usize & 0x1f;
        let mut d = self.duart[i];
        match a {
            0xf7e1 | 0xf7f1 => {
                let ch = (a == 0xf7f1) as usize;
                d = self.duart[ch * 0x10 + self.mode_ptr[ch]];
                self.mode_ptr[ch] = 1;
            }
            0xf7e7 | 0xf7f7 => {
                let ch = (a == 0xf7f7) as usize;
                let s = self.duart[2 + ch * 0x10] & !1;
                self.set_dusr(ch, s);
            }
            0xf7fd => {
                if self.duart_timer_out {
                    self.duart[10] |= 0x08;
                    self.duart_check_int();
                }
                self.duart_timer_out = false;
                self.duart_timer_restart(true);
            }
            0xf7ff => {
                self.duart[10] &= !0x08;
                if self.duart[9] & 0x40 == 0 {
                    self.duart_timer_out = false;
                    self.duart_timer.next = 0;
                }
            }
            _ => {}
        }
        d
    }

    fn duart_w(&mut self, a: u32, v: u8) {
        match a {
            0xf7e1 | 0xf7f1 => {
                let ch = (a == 0xf7f1) as usize;
                self.duart[ch * 0x10 + self.mode_ptr[ch]] = v;
                self.mode_ptr[ch] = 1;
                return;
            }
            0xf7e3 => {
                self.duart[3] = v;
                return;
            }
            0xf7f3 => {
                self.duart[19] = v;
                return;
            }
            0xf7e5 | 0xf7f5 => {
                self.duart_cmd((a == 0xf7f5) as usize, v);
                return;
            }
            0xf7e7 | 0xf7f7 => {
                let ch = (a == 0xf7f7) as usize;
                if self.tx_enable[ch] {
                    self.duart[7 + ch * 0x10] = v;
                    let s = self.duart[2 + ch * 0x10] & !0x0c;
                    self.set_dusr(ch, s);
                    // the holding register empties at once (TxRDY), the byte is out ~100 cycles later
                    let s = self.duart[2 + ch * 0x10] | 0x04;
                    self.set_dusr(ch, s);
                    self.tx_done[ch] = self.now + 100;
                    if ch == 1 {
                        self.sound_bytes.push((self.now, v));
                    }
                }
                return;
            }
            0xf7e9 => {
                self.duart[9] = v;
                return;
            }
            0xf7eb => {
                self.duart[11] = v;
                self.duart_check_int();
                return;
            }
            _ => {}
        }
        self.duart[(a - 0xf7e1) as usize & 0x1f] = v;
    }

    fn duart_cmd(&mut self, ch: usize, v: u8) {
        let sr = 2 + ch * 0x10;
        match (v & 0x70) >> 4 {
            1 => self.mode_ptr[ch] = 0,
            2 => {
                self.rx_enable[ch] = false;
                let s = self.duart[sr] & !3;
                self.set_dusr(ch, s);
            }
            3 => {
                self.tx_enable[ch] = false;
                let s = self.duart[sr] & !0x0c;
                self.set_dusr(ch, s);
            }
            4 => {
                let s = self.duart[sr] & !0xf0;
                self.set_dusr(ch, s);
            }
            5 => self.duart[10] &= !0x44,
            _ => {}
        }
        match (v & 0x0c) >> 2 {
            1 => {
                self.tx_enable[ch] = true;
                let s = self.duart[sr] | 0x0c;
                self.set_dusr(ch, s);
            }
            2 => {
                self.tx_enable[ch] = false;
                let s = self.duart[sr] & !0x0c;
                self.set_dusr(ch, s);
            }
            _ => {}
        }
        match v & 3 {
            1 => self.rx_enable[ch] = true,
            2 => self.rx_enable[ch] = false,
            _ => {}
        }
    }

    // ---- 68306 registers ----
    fn intreg_w(&mut self, a: u32, data: u16, word: bool) {
        if word && a & 1 != 0 {
            return;
        }
        if a >= 0xffff_ffc0 {
            let reg = ((a & 0x3f) >> 1) as usize;
            let old = self.intreg[reg];
            let data = if word {
                data
            } else if a & 1 != 0 {
                (old & 0xff00) | (data & 0xff)
            } else {
                (old & 0xff) | (data << 8)
            };
            match reg {
                0x18 => {
                    let ch = (data ^ old) & self.intreg[0x19];
                    self.intreg[0x18] = data;
                    let o = data & self.intreg[0x19];
                    if ch & 0xff00 != 0 {
                        self.porta_w(o >> 8);
                    }
                    if ch & 0xff != 0 {
                        self.portb_w(o & 0xff);
                    }
                }
                0x19 => {
                    let ch = data & !old;
                    self.intreg[0x19] = data;
                    let o = data & self.intreg[0x18];
                    if ch & 0xff00 != 0 {
                        self.porta_w(o >> 8);
                    }
                    if ch & 0xff != 0 {
                        self.portb_w(o & 0xff);
                    }
                }
                0x1f => self.intreg[0x1f] = (old & 0x8000) | (data & 0x7fff),
                _ => self.intreg[reg] = data,
            }
        } else if a >= 0xffff_f7e0 {
            let v = if word { data >> 8 } else { data } as u8;
            let a = a & 0xffff;
            if a & 1 == 1 {
                self.duart_w(a, v);
            }
        }
    }

    fn intreg_r(&mut self, a: u32, word: bool) -> u16 {
        if word && a & 1 != 0 {
            return 0;
        }
        let mut d = 0u16;
        if a >= 0xffff_ffc0 {
            let reg = ((a & 0x3f) >> 1) as usize;
            d = match reg {
                0x1a => {
                    let pdir = self.intreg[0x19];
                    let pdata = self.intreg[0x18];
                    if word {
                        ((self.porta_r() << 8 | self.portb_r()) & !pdir) | (pdata & pdir)
                    } else if a & 1 != 0 {
                        (self.portb_r() & !pdir) | (pdata & (pdir | 0xff00))
                    } else {
                        ((self.porta_r() << 8) & !pdir) | (pdata & (pdir | 0xff))
                    }
                }
                0x1f => {
                    let v = self.intreg[0x1f];
                    self.intreg[0x1f] &= 0x7fff;
                    v
                }
                0x1e => 0,
                r => self.intreg[r],
            };
        } else if a >= 0xffff_f7e0 {
            let a16 = a & 0xffff;
            if a16 & 1 == 1 {
                d = self.duart_r(a16) as u16;
            } else {
                // word read at the even address: the DUART answers on the low byte
                d = self.duart_r(a16 + 1) as u16;
                return d;
            }
        }
        if word || a & 1 != 0 { d } else { d >> 8 }
    }

    fn rd16(&mut self, a: u32) -> u16 {
        match a {
            0x0000_0000..=0x0007_ffff => {
                let i = a as usize & !1;
                u16::from_be_bytes([self.ram[i], self.ram[i + 1]])
            }
            0x1000_0000..=0x10ff_ffff => {
                if self.watch_rom != 0 && a >= self.watch_rom {
                    *self.rom_reads.entry(self.pc).or_default() += 1;
                }
                let i = (a - 0x1000_0000) as usize & !1;
                if i + 1 < self.rom.len() {
                    u16::from_be_bytes([self.rom[i], self.rom[i + 1]])
                } else {
                    0xffff
                }
            }
            0x3000_0000..=0x3001_ffff => {
                let i = (a - 0x3000_0000) as usize & !1;
                u16::from_be_bytes([self.nvram[i], self.nvram[i + 1]])
            }
            0x4000_0000..=0x40bf_ffff => self.io_r((a - 0x4000_0000) >> 1),
            0x40c0_0000..=0x40c0_07ff => self.u16_r(((a - 0x40c0_0000) >> 1) & 0x203),
            _ => 0,
        }
    }

    fn wr16(&mut self, a: u32, v: u16, mem_mask: u16) {
        // mem_mask: bits KEPT (MAME convention); v already positioned
        match a {
            0x0000_0000..=0x0007_ffff => {
                let i = a as usize & !1;
                if self.watch_ram != 0 && a >= self.watch_ram {
                    *self.ram_writes.entry(self.pc).or_default() += 1;
                }
                if mem_mask & 0xff00 == 0 {
                    self.ram[i] = (v >> 8) as u8;
                }
                if mem_mask & 0xff == 0 {
                    self.ram[i + 1] = v as u8;
                }
            }
            0x3000_0000..=0x3001_ffff => {
                let i = (a - 0x3000_0000) as usize & !1;
                if mem_mask & 0xff00 == 0 {
                    self.nvram[i] = (v >> 8) as u8;
                }
                if mem_mask & 0xff == 0 {
                    self.nvram[i + 1] = v as u8;
                }
            }
            0x4000_0000..=0x40bf_ffff => self.io_w((a - 0x4000_0000) >> 1, v),
            0x40c0_0000..=0x40c0_07ff => self.u16_w(((a - 0x40c0_0000) >> 1) & 0x203, v, mem_mask),
            _ => {}
        }
    }
}

impl AddressBus for Bus {
    fn read_byte(&mut self, a: u32) -> u8 {
        if a >= 0xffff_f000 {
            return self.intreg_r(a, false) as u8;
        }
        let w = self.rd16(a & !1);
        if a & 1 == 0 { (w >> 8) as u8 } else { w as u8 }
    }
    fn read_word(&mut self, a: u32) -> u16 {
        if a >= 0xffff_f000 {
            return self.intreg_r(a, true);
        }
        self.rd16(a)
    }
    fn read_long(&mut self, a: u32) -> u32 {
        (self.read_word(a) as u32) << 16 | self.read_word(a.wrapping_add(2)) as u32
    }
    fn write_byte(&mut self, a: u32, v: u8) {
        if a >= 0xffff_f000 {
            return self.intreg_w(a, v as u16, false);
        }
        if a & 1 == 0 {
            self.wr16(a, (v as u16) << 8, 0x00ff);
        } else {
            self.wr16(a & !1, v as u16, 0xff00);
        }
    }
    fn write_word(&mut self, a: u32, v: u16) {
        if a >= 0xffff_f000 {
            return self.intreg_w(a, v, true);
        }
        self.wr16(a, v, 0);
    }
    fn write_long(&mut self, a: u32, v: u32) {
        self.write_word(a, (v >> 16) as u16);
        self.write_word(a.wrapping_add(2), v as u16);
    }
    fn interrupt_acknowledge(&mut self, level: u8) -> u32 {
        if level == 7 && self.tirq {
            self.tirq = false;
            self.intreg[0x1c] &= !0x8000;
            return (self.intreg[0x1f] & 0xff) as u32;
        }
        0xFFFF_FFFF // autovector
    }
}

pub struct Board {
    pub cpu: CpuCore,
    pub bus: Bus,
    pub cycles: u64,
    zc: Timer,
    raster: Timer,
    pub width: usize,
    pub height: usize,
    pub on_frame: Option<crate::scan::FrameSink>,
    /// study: (call site, target) counts of JSR/BSR when enabled
    pub calls: Option<std::collections::HashMap<(u32, u32), u64>>,
}

impl Board {
    /// `rom` = the CPU ROM space image (pair n of EPROMs at n * 0x400000), `width` = 128 or 256.
    pub fn new(rom: Vec<u8>, width: usize, switch_board: bool, inv_sw: &[u8]) -> Self {
        let mut ram = vec![0u8; 0x8_0000];
        ram[..0x100].copy_from_slice(&rom[..0x100]);
        let mut sw = [0u8; 12];
        for (s, &v) in sw.iter_mut().zip(inv_sw) {
            *s = v;
        }
        let mut intreg = [0u16; 0x20];
        intreg[0x1f] = 0x040f;
        intreg[0x1d] = 0x00ff;
        intreg[0] = 0x0001;
        intreg[1] = 0xff0e;
        let mut bus = Bus {
            rom,
            ram,
            nvram: vec![0; 0x2_0000],
            now: 0,
            switch_board,
            sw,
            intreg,
            duart: [0; 0x20],
            mode_ptr: [0; 2],
            tx_enable: [false; 2],
            rx_enable: [false; 2],
            tx_done: [0; 2],
            sound_bytes: Vec::new(),
            duart_timer: Timer::default(),
            duart_timer_out: false,
            tirq: false,
            parallel_a: 0,
            parallel_b: 0,
            pulse: false,
            pulse_v: 0.0,
            pulse_t: 0.0,
            zero_cross: false,
            zero_acq: false,
            dmd_addr: 0,
            u16a: [0x00bc, 0, 0, 0],
            u16b: [0; 4],
            irq4_mask: 0,
            irq_state: 0,
            irq1_adjust: 0,
            irq1_period: 22668.0 / 14.0,
            irq4_period: [
                0x800 as f64 * IRQ4_PASSES_PER_UNIT,
                0x800 as f64 * IRQ4_PASSES_PER_UNIT,
                if width > 128 { 0x1000 } else { 0x800 } as f64 * IRQ4_PASSES_PER_UNIT,
            ],
            irq1_t: Timer::default(),
            irq4_t: Default::default(),
            lamp_a: 0,
            lamp_b: 0,
            sw_col: 0,
            pc: 0,
            watch_rom: 0,
            watch_ram: 0,
            rom_reads: Default::default(),
            ram_writes: Default::default(),
        };
        let mut cpu = CpuCore::new();
        cpu.set_cpu_type(CpuType::M68000);
        cpu.address_mask = 0xffff_ffff;
        cpu.reset(&mut bus);
        let height = if width > 128 { 64 } else { 32 };
        let zc = CLOCK_HZ / 120;
        // the row clock is 16279.409 Hz; the U16 makes a group of 4 frames per picture
        let raster = (height as f64 / (16279.409 / 4.0) * CLOCK_HZ as f64) as u64;
        let mut b = Board {
            cpu,
            bus,
            cycles: 0,
            zc: Timer::default(),
            raster: Timer::default(),
            width,
            height,
            on_frame: None,
            calls: None,
        };
        b.zc.start(0, zc, zc);
        b.raster.start(0, raster, raster);
        b
    }

    /// The picture the U16 streams: one value 0..3 per dot.
    pub fn frame(&self) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let skip = if w == 128 { 32 } else { 0 };
        let mut out = vec![0u8; w * h];
        let base = self.bus.dmd_addr as usize;
        for y in 0..h {
            let row = base + y * 64 + skip;
            for x in 0..w {
                let wa = (row + (x / 8) * 2) & 0x7fffe;
                let lum = u16::from_be_bytes([self.bus.ram[wa], self.bus.ram[wa + 1]]);
                let k = x % 8;
                // first dot in the top two bits (PinMAME rasterizes it as bit 0, its frames are LSB first)
                out[y * w + x] = ((lum >> (14 - 2 * k)) & 3) as u8;
            }
        }
        out
    }

    fn irq_level(&self) -> u8 {
        if self.bus.tirq {
            return 7;
        }
        let b = &self.bus;
        let lines = b.intreg[0x1c] & b.intreg[0x1d] & 0x7f00;
        let l = lines >> 7;
        if l == 0 {
            0
        } else {
            15 - l.leading_zeros() as u8
        }
    }

    fn update_isr(&mut self) {
        let b = &mut self.bus;
        let irq1 = b.irq_state & 0x10 != 0;
        let irq4 = b.irq4_mask & b.irq_state != 0;
        let mut isr = b.intreg[0x1c] & !(0x100 | 0x200 | 0x800);
        if irq1 {
            isr |= 0x100;
        }
        if b.zero_cross {
            isr |= 0x200;
        }
        if irq4 {
            isr |= 0x800;
        }
        b.intreg[0x1c] = isr;
    }

    pub fn step(&mut self) -> u32 {
        self.bus.pc = self.cpu.pc;
        let call = if self.calls.is_some() {
            let op = self.bus.rd16(self.cpu.pc);
            (op & 0xffc0 == 0x4e80 || op & 0xff00 == 0x6100).then_some(self.cpu.pc)
        } else {
            None
        };
        self.update_isr();
        let lvl = self.irq_level();
        self.cpu.set_irq(lvl);
        let c = match self.cpu.step(&mut self.bus) {
            m68k::StepResult::Ok { cycles } => cycles.max(4) as u64,
            _ => 4,
        };
        self.cycles += c;
        if let (Some(from), Some(calls)) = (call, self.calls.as_mut()) {
            *calls.entry((from, self.cpu.pc)).or_default() += 1;
        }
        let now = self.cycles;
        self.bus.now = now;
        if self.zc.due(now) && !self.bus.zero_acq {
            self.bus.zero_cross = true;
        }
        if self.bus.irq1_t.due(now) {
            self.bus.irq_state |= 0x10;
        }
        for i in 0..3 {
            if self.bus.irq4_t[i].due(now) && self.bus.irq4_mask & (1 << i) != 0 {
                self.bus.irq_state |= 1 << i;
            }
        }
        if self.bus.duart_timer.due(now) {
            self.bus.duart_timer_fire();
        }
        for ch in 0..2 {
            if self.bus.tx_done[ch] != 0 && now >= self.bus.tx_done[ch] {
                self.bus.tx_done[ch] = 0;
                let s = self.bus.duart[2 + ch * 0x10] | 0x08;
                self.bus.set_dusr(ch, s);
            }
        }
        if self.raster.due(now) {
            let f = self.frame();
            if let Some(cb) = self.on_frame.as_mut() {
                cb(now, &f);
            }
        }
        c as u32
    }

    pub fn run(&mut self, cycles: u64) {
        let end = self.cycles + cycles;
        while self.cycles < end {
            self.step();
        }
    }
}

/// Per-game hardware facts from PinMAME `capgames.c`: (set prefix, display width, switch board,
/// inverted switches by row).
const GAMES: &[(&str, usize, bool, &[u8])] = &[
    (
        "pmv",
        128,
        true,
        &[0, 0x00, 0x3f, 0x04, 0x00, 0x00, 0x08, 0x00, 0x0e],
    ),
    (
        "abv",
        128,
        true,
        &[0, 0x00, 0x70, 0x40, 0x78, 0x00, 0x40, 0x80, 0x03],
    ),
    (
        "bsv",
        128,
        false,
        &[0, 0x00, 0x00, 0x38, 0x00, 0x30, 0x00, 0x01],
    ),
    (
        "bsb",
        128,
        false,
        &[0, 0x00, 0x00, 0x38, 0x00, 0x30, 0x00, 0x01],
    ),
    (
        "pp1",
        128,
        false,
        &[0, 0x00, 0x00, 0x38, 0x00, 0x30, 0x00, 0x01],
    ),
    ("ffv", 256, true, &[0, 0x0f, 0x0f, 0x8c, 0x80]),
    ("bbb", 128, true, &[0, 0x00, 0x01, 0x78, 0x00, 0x00, 0x01]),
    ("kpb", 128, true, &[0, 0x01, 0x00, 0x78, 0x88, 0x08, 0x10]),
];

/// The CPU ROM space from a directory of the set's files: `u<n>h*`/`u<n>l*` pairs (even / odd
/// bytes) or `u<n>hu<n>l*` files already interleaved, pair n at (n - 1) * 0x400000.
/// Flipper Football loads U4 before U3 (pair 4 at 0x800000, pair 3 at 0xc00000).
pub fn load_rom(dir: &str) -> Vec<u8> {
    let swap34 = std::path::Path::new(dir)
        .file_name()
        .is_some_and(|n| n.to_string_lossy().to_lowercase().starts_with("ffv"));
    let mut rom = vec![0xffu8; 0x100_0000];
    let mut top = 0;
    for n in 1..=4 {
        let mut hi = None;
        let mut lo = None;
        let mut both = None;
        for e in std::fs::read_dir(dir).unwrap() {
            let name = e.unwrap().file_name().to_string_lossy().to_lowercase();
            let p = format!("{dir}/{name}");
            if name.starts_with(&format!("u{n}hu{n}l")) {
                both = Some(p);
            } else if name.starts_with(&format!("u{n}h")) || name.starts_with(&format!("bsu{n}h")) {
                hi = Some(p);
            } else if name.starts_with(&format!("u{n}l")) || name.starts_with(&format!("bsu{n}l")) {
                lo = Some(p);
            }
        }
        let slot = match n {
            3 if swap34 => 4,
            4 if swap34 => 3,
            n => n,
        };
        let base = (slot - 1) * 0x40_0000;
        if let Some(p) = both {
            let d = std::fs::read(p).unwrap();
            rom[base..base + d.len()].copy_from_slice(&d);
            top = top.max(base + d.len());
        } else if let (Some(h), Some(l)) = (hi, lo) {
            let (h, l) = (std::fs::read(h).unwrap(), std::fs::read(l).unwrap());
            for (i, (a, b)) in h.iter().zip(l.iter()).enumerate() {
                rom[base + 2 * i] = *a;
                rom[base + 2 * i + 1] = *b;
            }
            top = top.max(base + 2 * h.len());
        }
    }
    rom.truncate(top);
    rom
}

pub fn board_for(dir: &str) -> Board {
    let set = std::path::Path::new(dir)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    let g = GAMES
        .iter()
        .find(|g| set.starts_with(g.0))
        .unwrap_or_else(|| panic!("unknown Capcom set {set}"));
    let mut b = Board::new(load_rom(dir), g.1, g.2, g.3);
    if let Some(d) = std::env::var("CAP_NVRAM")
        .ok()
        .and_then(|p| std::fs::read(p).ok())
    {
        b.bus.nvram[..d.len()].copy_from_slice(&d);
    }
    b
}

/// Pictures kept by a scan: (cycle stamp, levels), consecutive duplicates removed.
type Recorded = Vec<(u64, Vec<u8>)>;

fn write_frame(o: &mut impl std::io::Write, ms: u64, f: &[u8], w: usize) {
    writeln!(o, "0x{ms:08x}").unwrap();
    for row in f.chunks(w) {
        writeln!(
            o,
            "{}",
            row.iter().map(|v| (b'0' + v) as char).collect::<String>()
        )
        .unwrap();
    }
    writeln!(o).unwrap();
}

pub fn cli(args: &[String]) {
    match args[1].as_str() {
        // cap-boot <set dir> <secs> <out.txt>: boot and dump every new displayed picture
        "cap-boot" => {
            let mut b = board_for(&args[2]);
            let secs: f64 = args[3].parse().unwrap();
            let w = b.width;
            let o = std::rc::Rc::new(std::cell::RefCell::new(std::io::BufWriter::new(
                std::fs::File::create(&args[4]).unwrap(),
            )));
            let o2 = o.clone();
            let mut last = Vec::new();
            b.on_frame = Some(Box::new(move |t, f| {
                if f != last.as_slice() {
                    write_frame(&mut *o2.borrow_mut(), t * 1000 / CLOCK_HZ, f, w);
                    last = f.to_vec();
                }
            }));
            let t = std::time::Instant::now();
            for k in 0..((secs * 10.0) as u64) {
                b.run(CLOCK_HZ / 10);
                eprintln!(
                    "{k:4} pc {:08x} sr {:04x} dmd {:08x} isr {:04x} icr {:04x} snd {} ({:.1} s real)",
                    b.cpu.pc,
                    b.cpu.get_sr(),
                    b.bus.dmd_addr,
                    b.bus.intreg[0x1c],
                    b.bus.intreg[0x1d],
                    b.bus.sound_bytes.len(),
                    t.elapsed().as_secs_f64()
                );
            }
            if let Ok(p) = std::env::var("CAP_SAVE_NVRAM") {
                std::fs::write(p, &b.bus.nvram).unwrap();
            }
        }
        // cap-study <set dir> <from secs> <to secs> [rom watch hex] [ram watch hex]: boot, then
        // count calls, ROM data reads and RAM writes per PC over the window
        "cap-study" => {
            let mut b = board_for(&args[2]);
            let (t1, t2): (f64, f64) = (args[3].parse().unwrap(), args[4].parse().unwrap());
            let hex = |i: usize, d: u32| {
                args.get(i)
                    .map(|v| u32::from_str_radix(v, 16).unwrap())
                    .unwrap_or(d)
            };
            b.run((t1 * CLOCK_HZ as f64) as u64);
            b.bus.watch_rom = hex(5, 0x1040_0000);
            b.bus.watch_ram = hex(6, 0x7_0000);
            b.calls = Some(Default::default());
            b.run(((t2 - t1) * CLOCK_HZ as f64) as u64);
            let top = |m: &std::collections::HashMap<u32, u64>, what: &str| {
                let mut v: Vec<_> = m.iter().collect();
                v.sort_by(|a, b| b.1.cmp(a.1));
                for (pc, n) in v.iter().take(40) {
                    println!("{what} {pc:08x} {n}");
                }
            };
            top(&b.bus.rom_reads, "romread");
            top(&b.bus.ram_writes, "ramwrite");
            let mut v: Vec<_> = b.calls.as_ref().unwrap().iter().collect();
            v.sort();
            for ((f, t), n) in v {
                println!("call {f:08x} {t:08x} {n}");
            }
        }
        // cap-scan <set dir> <scenes.txt> <boot secs> <cap secs> <outdir> [first] [last]: one fresh
        // board per scene; at the first natural DM_ADD call after the boot, its stacked arguments
        // are replaced by the scene's, then every new picture is recorded until the display
        // manager no longer runs the scene's function (it ended), or the cap.
        // scenes.txt lines: `n arg1 arg2 ...` in call order (hex or decimal, `?` = keep the
        // natural call's value); env:
        //   CAP_DMADD   DM_ADD address
        //   CAP_LAYOUT  argument sizes in call order, `l`/`w`, `=hex` forces a value (the
        //               priority), e.g. `l,l,w,w=8,l` (Pinball Magic) or `l,l,l,w=f,w,l,w`
        //               (Kingpin); the first argument is the scene's function
        //   CAP_CUR     RAM address of the current entry's function, or
        //   CAP_SLOTS   `base,size,count,state offset`: the scene runs while a slot holds its
        //               function with a non-zero state
        "cap-scan" => {
            let list = std::fs::read_to_string(&args[3]).unwrap();
            let boot: f64 = args[4].parse().unwrap();
            let cap: f64 = args[5].parse().unwrap();
            let out = &args[6];
            std::fs::create_dir_all(out).unwrap();
            let first: u32 = args.get(7).map(|v| v.parse().unwrap()).unwrap_or(0);
            let last: u32 = args.get(8).map(|v| v.parse().unwrap()).unwrap_or(u32::MAX);
            let hexs = |v: &str| -> i64 {
                let neg = v.starts_with('-');
                let v = v.trim_start_matches('-');
                let x = if let Some(h) = v.strip_prefix("0x") {
                    i64::from_str_radix(h, 16).unwrap()
                } else {
                    v.parse().unwrap()
                };
                if neg { -x } else { x }
            };
            let envhex = |k: &str| {
                std::env::var(k)
                    .ok()
                    .map(|v| u32::from_str_radix(&v, 16).unwrap())
            };
            let dmadd = envhex("CAP_DMADD").expect("CAP_DMADD");
            let cur = envhex("CAP_CUR");
            let slots: Option<Vec<u32>> = std::env::var("CAP_SLOTS").ok().map(|v| {
                v.split(',')
                    .map(|x| u32::from_str_radix(x, 16).unwrap())
                    .collect()
            });
            let layout: Vec<(bool, Option<u32>)> = std::env::var("CAP_LAYOUT")
                .unwrap_or("l,l,w,w=8,l".into())
                .split(',')
                .map(|a| {
                    let (k, v) = a
                        .split_once('=')
                        .map(|(k, v)| (k, Some(v)))
                        .unwrap_or((a, None));
                    (k == "l", v.map(|v| u32::from_str_radix(v, 16).unwrap()))
                })
                .collect();
            for line in list.lines() {
                let f: Vec<&str> = line.split_whitespace().collect();
                if f.len() < 2 {
                    continue;
                }
                let n: u32 = f[0].parse().unwrap();
                if n < first || n > last {
                    continue;
                }
                let func = hexs(f[1]) as u32;
                let rest = f[2..].join(" ");
                let mut b = board_for(&args[2]);
                let w = b.width;
                let frames: std::rc::Rc<std::cell::RefCell<Recorded>> = Default::default();
                let fr = frames.clone();
                b.on_frame = Some(Box::new(move |t, f| {
                    let mut v = fr.borrow_mut();
                    if v.last().map(|l| l.1.as_slice() != f).unwrap_or(true) {
                        v.push((t, f.to_vec()));
                    }
                }));
                b.run((boot * CLOCK_HZ as f64) as u64);
                // the next natural DM_ADD
                let limit = b.cycles + 60 * CLOCK_HZ;
                while b.cpu.pc != dmadd && b.cycles < limit {
                    b.step();
                }
                let mut a = b.cpu.dar[15] + 4;
                for (k, (long, force)) in layout.iter().enumerate() {
                    let v = force
                        .map(|v| v as i64)
                        .or_else(|| f.get(k + 1).filter(|v| **v != "?").map(|v| hexs(v)));
                    if let Some(v) = v {
                        if *long {
                            b.bus.write_long(a, v as u32);
                        } else {
                            b.bus.write_word(a, v as u16);
                        }
                    }
                    a += if *long { 4 } else { 2 };
                }
                {
                    let mut v = frames.borrow_mut();
                    let keep = v.pop();
                    v.clear();
                    v.extend(keep);
                }
                let t0 = b.cycles;
                let (mut started, mut ended, mut kind) = (false, None, "cap");
                while b.cycles - t0 < (cap * CLOCK_HZ as f64) as u64 {
                    b.run(CLOCK_HZ / 1000);
                    let running = if let Some(cur) = cur {
                        b.bus.read_long(cur) == func
                    } else {
                        let sl = slots.as_ref().expect("CAP_CUR or CAP_SLOTS");
                        (0..sl[2]).any(|k| {
                            let a = sl[0] + k * sl[1];
                            b.bus.read_long(a) == func && b.bus.read_word(a + sl[3]) != 0
                        })
                    };
                    if running {
                        started = true;
                    } else if started && ended.is_none() {
                        ended = Some(b.cycles);
                        kind = "end";
                    }
                    if let Some(e) = ended {
                        // keep a little of what follows, then stop
                        if b.cycles - e > CLOCK_HZ / 5 {
                            break;
                        }
                    }
                    if !started && b.cycles - t0 > 2 * CLOCK_HZ {
                        kind = "nostart";
                        break;
                    }
                }
                let f = frames.borrow();
                let s = |t: Option<u64>| {
                    t.map(|t| format!("{:.2}", (t - t0) as f64 / CLOCK_HZ as f64))
                        .unwrap_or("-".into())
                };
                println!(
                    "{n:3} {kind:7} busy -..{} frames {} fn {func:08x} args {}",
                    s(ended),
                    f.len(),
                    rest
                );
                let mut o = std::io::BufWriter::new(
                    std::fs::File::create(format!("{out}/00_{n:03}.txt")).unwrap(),
                );
                for (t, fr) in f.iter() {
                    // cut what comes after the scene ended; a scene that never became current
                    // keeps only the picture it found
                    if ended.is_some_and(|e| *t > e) || (kind == "nostart" && *t > t0) {
                        break;
                    }
                    write_frame(&mut o, t.saturating_sub(t0) * 1000 / CLOCK_HZ, fr, w);
                }
            }
        }
        // cap-hook <set dir> <secs> <addr,...> [words]: boot and print each time the CPU reaches one
        // of the addresses, with the first stack words (the arguments of a call)
        "cap-hook" => {
            let mut b = board_for(&args[2]);
            let secs: f64 = args[3].parse().unwrap();
            let hooks: Vec<u32> = args[4]
                .split(',')
                .map(|v| u32::from_str_radix(v, 16).unwrap())
                .collect();
            let nw: u32 = args.get(5).map(|v| v.parse().unwrap()).unwrap_or(8);
            let end = (secs * CLOCK_HZ as f64) as u64;
            while b.cycles < end {
                if hooks.contains(&b.cpu.pc) {
                    let sp = b.cpu.dar[15];
                    let w: Vec<String> = (0..nw)
                        .map(|i| format!("{:04x}", b.bus.read_word(sp + 2 * i)))
                        .collect();
                    println!(
                        "{:.3} {:08x} sp {:08x} {}",
                        b.cycles as f64 / CLOCK_HZ as f64,
                        b.cpu.pc,
                        sp,
                        w.join(" ")
                    );
                }
                b.step();
            }
        }
        _ => eprintln!("unknown cap subcommand"),
    }
}
