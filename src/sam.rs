//! Stern S.A.M. main board (2006-2014): an Atmel AT91R40008 (ARM7TDMI, 40 MHz) that runs the whole
//! game and draws the 128x32 DMD into its own RAM. Memory map, IO and DMD mixer from PinMAME
//! `src/wpc/sam.c`, the on-chip peripherals (AIC, timers, USART, PIO, remap) from
//! `src/cpu/at91/at91.c`. The CPU core is the `arm32_cpu` crate (PinMAME's ARM core has no Thumb,
//! so the games are ARM code only).
//!
//! Only what the display needs is modelled: lamps, coils and sound writes are dropped.

use arm32_cpu::{Cpu, Exception, Memory, Mode, reg};
use std::rc::Rc;

pub const CLOCK_HZ: u64 = 40_000_000;
/// FIQ (sound/IO tick) measured on a real machine.
const FIQ_HZ: f64 = 4008.0;
/// Display scan rate (PinMAME `SAM_DMDFREQ`).
const DMD_HZ: f64 = 62.67;
/// Zero-cross toggle rate (PinMAME `SAM_ZC_FREQ`).
const ZC_HZ: f64 = 120.0;

#[derive(Clone, Default)]
struct Timer {
    chan_mode: u32,
    status: u32,
    irq_mask: u32,
    counter: u32,
    rega: u32,
    regb: u32,
    regc: u32,
    div: u32,
    /// next RC compare (cycle), 0 = stopped
    next: u64,
    period: u64,
}

#[derive(Clone, Default)]
struct Usart {
    mr: u32,
    imr: u32,
    csr: u32,
    thr: u32,
    pending_thr: bool,
    brgr: u32,
    rpr: u32,
    rcr: u32,
    tpr: u32,
    tcr: u32,
    rx: std::collections::VecDeque<u8>,
    /// cycles per byte, 0 = stopped
    period: f64,
    next: f64,
}

const US_TXEMPTY: u32 = 1 << 9;
const US_ENDTX: u32 = 1 << 4;
const US_ENDRX: u32 = 1 << 3;
const US_TXRDY: u32 = 1 << 1;
const US_RXRDY: u32 = 1;

#[derive(Clone)]
pub struct Bus {
    flash: Rc<Vec<u8>>,
    pub page0: Vec<u8>,
    reset_ram: Vec<u8>,
    pub ram: Vec<u8>,
    pub nvram: Vec<u8>,
    usb: Vec<u8>,
    sam2: bool,
    bank: u32,
    pub video_page: [u32; 2],
    sw_stb: usize,
    zc: bool,
    /// switch columns as PinMAME's coreGlobals.swMatrix: 0 = dedicated D17-D24, 1..8 = matrix
    /// (Stern switch n = column (n-1)/8+1, bit (n-1)%8), 9 = coins, 11 = flippers
    pub sw: [u8; 12],
    pub dip: u8,
    pub coindoor_closed: bool,
    block_timers: bool,
    pub cycles: u64,
    // AT91 peripherals
    remap: bool,
    aic_smr: [u32; 32],
    aic_svr: [u32; 32],
    aic_mask: u32,
    aic_pending: u32,
    aic_status: u32,
    aic_stack: Vec<u32>,
    aic_spurious: u32,
    prio: [u8; 31],
    pub pending_irq: bool,
    tc: [Timer; 3],
    tc_block_mode: u32,
    us: [Usart; 2],
    pio_enabled: u32,
    pio_output: u32,
    pio_out_data: u32,
    pio_data: u32,
    pio_irq_mask: u32,
    pio_irq_status: u32,
    power: u32,
    /// set when the page registers are written (picture flips)
    pub flipped: bool,
    /// bytes the game sent on USART 0 (node boards) and 1 (console)
    pub tx: [Vec<u8>; 2],
}

impl Bus {
    fn new(flash: Rc<Vec<u8>>) -> Bus {
        let mut page0 = vec![0u8; 0x100000];
        let n = flash.len().min(0x100000);
        page0[..n].copy_from_slice(&flash[..n]);
        let sam2 = flash.len() > 0x2000000;
        let mut b = Bus {
            flash,
            page0,
            reset_ram: vec![0; 0x100000],
            ram: vec![0; 0x100000],
            nvram: vec![0xff; 0x20000],
            usb: vec![0; 0x100],
            sam2,
            bank: 0,
            video_page: [0; 2],
            sw_stb: 0,
            zc: false,
            sw: [0; 12],
            dip: 0x80,
            coindoor_closed: true,
            block_timers: false,
            cycles: 0,
            remap: false,
            aic_smr: [0; 32],
            aic_svr: [0; 32],
            aic_mask: 0,
            aic_pending: 0,
            aic_status: 0,
            aic_stack: Vec::new(),
            aic_spurious: 0,
            prio: [0; 31],
            pending_irq: false,
            tc: Default::default(),
            tc_block_mode: 0,
            us: Default::default(),
            pio_enabled: 0x1FF_FFFF,
            pio_output: 0,
            pio_out_data: 0,
            pio_data: 0,
            pio_irq_mask: 0,
            pio_irq_status: 0,
            power: 0x17c,
            flipped: false,
            tx: [Vec::new(), Vec::new()],
        };
        for u in b.us.iter_mut() {
            u.csr = US_ENDTX | US_ENDRX | US_TXRDY | US_TXEMPTY;
        }
        b.build_prio();
        b
    }

    fn build_prio(&mut self) {
        let mut c = 0;
        for p in (0..8).rev() {
            for j in 1..32 {
                if self.aic_smr[j] & 7 == p {
                    self.prio[c] = j as u8;
                    c += 1;
                }
            }
        }
    }

    fn assert_if_greater(&mut self) {
        let act = self.aic_pending & self.aic_mask;
        if self.aic_status > 0 {
            for &p in self.prio.iter() {
                if p as u32 == self.aic_status {
                    break;
                }
                if act & (1 << p) != 0 {
                    self.pending_irq = true;
                    break;
                }
            }
        } else if act != 0 {
            self.pending_irq = true;
        }
    }

    fn fire_irq(&mut self, n: u32) {
        self.aic_pending |= 1 << n;
        self.assert_if_greater();
    }

    fn flash_byte(&self, a: u32) -> u8 {
        let off = if a & 0x800000 != 0 {
            ((self.bank as usize) << 23) | (a as usize & 0x7FFFFF)
        } else {
            a as usize & 0x7FFFFF
        };
        self.flash.get(off).copied().unwrap_or(0)
    }

    // ---- SAM IO (sam.c) ----

    /// samswitch_r for an access starting at `a`
    fn switch_r(&self, a: u32) -> u32 {
        match a {
            0x1100000 => {
                let mut d = 0u32;
                if self.sw_stb < 4 {
                    d = ((self.sw[2 + self.sw_stb * 2] as u32) << 8)
                        | self.sw[1 + self.sw_stb * 2] as u32;
                }
                d ^ 0xffff
            }
            0x1100002 => {
                let cds = self.sw[9] as u32;
                let f = self.sw[11];
                let rev = |n: u8| -> u8 {
                    ((n & 1) << 3) | ((n & 2) << 1) | ((n & 4) >> 1) | ((n & 8) >> 3)
                };
                let fls = (rev(f & 0x0f) | (rev(f >> 4) << 4)) as u32;
                let mut d = (fls << 8) | cds;
                d |= (d & ((1 << 8) | (1 << 10) | (1 << 12) | (1 << 14))) << 1;
                d ^ 0xffff
            }
            0x1100004 => (((self.dip as u32) << 8) | self.sw[0] as u32) ^ 0xffff,
            0x1100005 => (self.dip ^ 0xff) as u32,
            0x1180000 => self.bank | 0x10,
            _ => 0,
        }
    }

    fn io_r(&self, a: u32) -> u32 {
        if a == 0x02400025 {
            (7 << 3) | ((self.zc as u32) << 2) | if self.coindoor_closed { 3 } else { 0 }
        } else {
            0
        }
    }

    /// samdmdram_w: `lanes` = bytes written in the 32-bit word, `v` positioned in the word
    fn dmdram_w(&mut self, word: u32, lanes: u32, v: u32) {
        match word {
            0x1100008 => {
                self.sw_stb = if v != 0 && v & (v - 1) == 0 {
                    v.trailing_zeros() as usize
                } else {
                    0
                };
            }
            0x1100020 => {
                if lanes & 0b1100 != 0 {
                    self.video_page[1] = (v >> 16) & 0xffff;
                } else {
                    self.video_page[0] = v;
                }
                self.flipped = true;
            }
            _ => {}
        }
    }

    fn bank_w(&mut self, a: u32, v: u32) {
        if a == 0x02580000 {
            self.bank = v & if self.sam2 { 0x0f } else { 0x03 };
        }
    }

    // ---- AT91 on-chip peripherals (at91.c) ----

    fn internal_w(&mut self, a: u32, d: u32) {
        match (a & 0xFF000) >> 12 {
            0x00 => {
                if a == 0xffe00020 && d & 1 != 0 {
                    self.page0.copy_from_slice(&self.reset_ram);
                    self.remap = true;
                }
            }
            0xcc => self.usart_w(0, a, d),
            0xd0 => self.usart_w(1, a, d),
            0xe0 => {
                let o = a & 0xff;
                if o < 0xc0 {
                    let n = (o / 0x40) as usize;
                    let cyc = self.cycles;
                    let t = &mut self.tc[n];
                    match o - 0x40 * n as u32 {
                        0x00 => {
                            t.status &= !0x600FF;
                            if d & 1 != 0 {
                                t.status |= 0x10000;
                            }
                            if d & 2 != 0 {
                                t.status &= !0x10000;
                                t.next = 0;
                            }
                            if d & 4 != 0 {
                                t.counter = 0;
                                if t.status & 0x10000 != 0
                                    && t.chan_mode & 0x4000 != 0
                                    && t.irq_mask & 0x10 != 0
                                {
                                    t.period = (t.div as u64 * t.regc as u64).max(1);
                                    t.next = cyc + t.period;
                                }
                            }
                        }
                        0x04 => {
                            t.chan_mode = d;
                            t.div = [2, 8, 32, 128, 1024, 0, 0, 0][(d & 7) as usize];
                        }
                        0x10 => t.counter = d,
                        0x14 => t.rega = d,
                        0x18 => t.regb = d,
                        0x1c => t.regc = d,
                        0x24 => t.irq_mask |= d,
                        0x28 => t.irq_mask &= !d,
                        _ => {}
                    }
                } else if o == 0xc4 {
                    self.tc_block_mode = d;
                }
            }
            0xf0 => match a & 0xff {
                0x00 => self.pio_enabled |= d,
                0x04 => self.pio_enabled &= !(d & !0x18F0000),
                0x10 => self.pio_output |= d,
                0x14 => self.pio_output &= !d,
                0x30 => {
                    let d = d & self.pio_output & self.pio_enabled;
                    self.pio_out_data |= d;
                    if d != 0 {
                        self.pio_pins(self.pio_data | d);
                    }
                }
                0x34 => {
                    let d = d & self.pio_output & self.pio_enabled;
                    self.pio_out_data &= !d;
                    if d != 0 {
                        self.pio_pins(self.pio_data & !d);
                    }
                }
                0x40 => self.pio_irq_mask |= d,
                0x44 => self.pio_irq_mask &= !d,
                _ => {}
            },
            0xf4 => match a {
                0xffff4004 => self.power |= d,
                0xffff4008 => self.power &= !d,
                _ => {}
            },
            0xff => {
                let o = a & 0x1ff;
                match o {
                    0x000..=0x07c => {
                        self.aic_smr[(o / 4) as usize] = d;
                        self.build_prio();
                    }
                    0x080..=0x0fc => self.aic_svr[((o - 0x80) / 4) as usize] = d,
                    0x120 => {
                        self.aic_mask |= d;
                        self.assert_if_greater();
                    }
                    0x124 => self.aic_mask &= !d,
                    0x128 => self.aic_pending &= !d,
                    0x12c => self.aic_pending |= d,
                    0x130 => {
                        if let Some(s) = self.aic_stack.pop() {
                            self.aic_status = s;
                            self.assert_if_greater();
                        }
                    }
                    0x134 => self.aic_spurious = d,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn internal_r(&mut self, a: u32) -> u32 {
        match (a & 0xFF000) >> 12 {
            0xcc => self.usart_r(0, a),
            0xd0 => self.usart_r(1, a),
            0xe0 => {
                let o = a & 0xff;
                if o < 0xc0 {
                    let n = (o / 0x40) as usize;
                    let t = &mut self.tc[n];
                    match o - 0x40 * n as u32 {
                        0x04 => t.chan_mode,
                        0x10 => t.counter,
                        0x14 => t.rega,
                        0x18 => t.regb,
                        0x1c => t.regc,
                        0x20 => {
                            let s = t.status;
                            t.status &= 0x10000;
                            s
                        }
                        0x2c => t.irq_mask,
                        _ => 0,
                    }
                } else if o == 0xc4 {
                    self.tc_block_mode
                } else {
                    0
                }
            }
            0xf0 => match a & 0xff {
                0x08 => self.pio_enabled,
                0x18 => self.pio_output,
                0x38 => self.pio_out_data,
                0x3c => {
                    let d = 0x0000_0C00 & !self.pio_output & self.pio_enabled;
                    self.pio_pins(self.pio_data | d);
                    self.pio_data
                }
                0x48 => self.pio_irq_mask,
                0x4c => std::mem::take(&mut self.pio_irq_status),
                _ => 0,
            },
            0xf4 => {
                if a == 0xffff400c {
                    self.power
                } else {
                    0
                }
            }
            0xff => match a & 0xfff {
                0x100 => {
                    let act = self.aic_pending & self.aic_mask;
                    let mut d = None;
                    for i in 0..31 {
                        let p = self.prio[i] as u32;
                        if act & (1 << p) != 0 {
                            self.push_status();
                            self.aic_status = p;
                            self.aic_pending &= !(1 << p);
                            d = Some(self.aic_svr[p as usize]);
                            break;
                        }
                    }
                    let d = d.unwrap_or_else(|| {
                        self.aic_stack.push(0);
                        self.aic_spurious
                    });
                    self.pending_irq = false;
                    d
                }
                0x104 => self.aic_svr[0],
                0x108 => self.aic_status,
                0x10c => self.aic_pending,
                0x110 => self.aic_mask,
                0x114 => (self.pending_irq as u32) << 1,
                _ => 0,
            },
            _ => 0,
        }
    }

    fn push_status(&mut self) {
        if self.aic_stack.len() >= 31 {
            self.aic_stack.pop();
        }
        self.aic_stack.push(self.aic_status);
    }

    fn pio_pins(&mut self, d: u32) {
        let changed = d ^ self.pio_data;
        self.pio_data = d;
        let i = changed & self.pio_irq_mask;
        self.pio_irq_status |= i;
        if i != 0 {
            self.aic_pending |= 0x100;
        }
    }

    fn usart_baud(&mut self, n: usize) {
        let u = &mut self.us[n];
        if u.brgr != 0 {
            let usclks = (u.mr >> 4) & 3;
            let chrl = (u.mr >> 6) & 3;
            let sync = (u.mr >> 8) & 1;
            let nbstop = (u.mr >> 12) & 3;
            let mode9 = (u.mr >> 17) & 1;
            let div = if sync == 0 { 16 * u.brgr } else { u.brgr } as f64;
            let clk = match usclks {
                0 => CLOCK_HZ as f64 / div,
                1 => CLOCK_HZ as f64 / (8.0 * div),
                _ => 9600.0,
            };
            let bits = (if mode9 != 0 { 9 } else { chrl + 5 }) as f64 + nbstop as f64 / 2.0;
            u.period = CLOCK_HZ as f64 / (clk / bits);
            u.next = self.cycles as f64 + u.period;
        } else {
            u.period = 0.0;
        }
    }

    fn usart_w(&mut self, n: usize, a: u32, d: u32) {
        match (a & 0xff) / 4 {
            0x01 => {
                self.us[n].mr = d;
                self.usart_baud(n);
            }
            0x02 => {
                let u = &mut self.us[n];
                let fire = (!u.imr & d) & u.csr != 0;
                u.imr |= d;
                if fire {
                    self.fire_irq(3 - n as u32);
                }
            }
            0x03 => self.us[n].imr &= !d,
            0x07 => {
                let u = &mut self.us[n];
                u.pending_thr = true;
                u.thr = d;
                u.csr &= !(US_TXRDY | US_TXEMPTY);
            }
            0x08 => {
                self.us[n].brgr = d;
                self.usart_baud(n);
            }
            0x0c => self.us[n].rpr = d,
            0x0d => {
                self.us[n].rcr = d;
                if d > 0 {
                    self.us[n].csr &= !US_ENDRX;
                }
            }
            0x0e => self.us[n].tpr = d,
            0x0f => {
                self.us[n].tcr = d;
                if d > 0 {
                    self.us[n].csr &= !(US_ENDTX | US_TXRDY | US_TXEMPTY);
                }
            }
            _ => {}
        }
    }

    fn usart_r(&mut self, n: usize, a: u32) -> u32 {
        let u = &mut self.us[n];
        match (a & 0xff) / 4 {
            0x04 => u.imr,
            0x05 => u.csr,
            0x06 => {
                let b = u.rx.pop_front().unwrap_or(0) as u32;
                if u.rx.is_empty() {
                    u.csr &= !US_RXRDY;
                }
                b
            }
            0x08 => u.brgr,
            0x0c => u.rpr,
            0x0d => u.rcr,
            0x0e => u.tpr,
            0x0f => u.tcr,
            _ => 0,
        }
    }

    fn usart_tick(&mut self, n: usize) {
        let mut set = 0;
        if self.us[n].tcr > 0 {
            let p = self.us[n].tpr;
            let b = self.r8(p);
            self.tx[n].push(b);
            let u = &mut self.us[n];
            u.tpr = u.tpr.wrapping_add(1);
            u.tcr -= 1;
            if u.tcr == 0 {
                set |= !u.csr & US_ENDTX;
            }
        } else if self.us[n].pending_thr {
            self.us[n].pending_thr = false;
            let b = self.us[n].thr as u8;
            self.tx[n].push(b);
        }
        if self.us[n].tcr == 0 && !self.us[n].pending_thr {
            set |= !self.us[n].csr & (US_TXEMPTY | US_TXRDY);
        }
        if self.us[n].rcr > 0 && !self.us[n].rx.is_empty() {
            let b = self.us[n].rx.pop_front().unwrap();
            let p = self.us[n].rpr;
            self.w8(p, b);
            let u = &mut self.us[n];
            u.rpr = u.rpr.wrapping_add(1);
            u.rcr -= 1;
            if u.rcr == 0 {
                set |= !u.csr & US_ENDRX;
            }
            if u.rx.is_empty() {
                u.csr &= !US_RXRDY;
            }
        }
        self.us[n].csr |= set;
        if self.us[n].imr & set != 0 {
            self.fire_irq(3 - n as u32);
        }
    }

    /// The picture the display shows now (PinMAME sam_dmd mixer).
    pub fn picture(&self) -> [u8; 4096] {
        let mut out = [0u8; 4096];
        let o1 = 0x80000 + ((self.video_page[0] as usize & 0x7f) << 12);
        let o2 = 0x80000 + ((self.video_page[1] as usize & 0x7f) << 12);
        for (i, px) in out.iter_mut().enumerate() {
            let r1 = self.ram.get(o1 + i).copied().unwrap_or(0);
            let r2 = self.ram.get(o2 + i).copied().unwrap_or(0);
            let mix = r1 >> 4;
            *px = ((r2 & mix) | (r1 & (mix ^ 0xf))) & 0xf;
        }
        out
    }
}

impl Memory for Bus {
    fn r8(&mut self, a: u32) -> u8 {
        match a >> 20 {
            0x000 => self.page0[a as usize],
            0x003 => self.reset_ram[(a & 0xFFFFF) as usize],
            0x010 => self.ram[(a & 0xFFFFF) as usize],
            0x011 => self.switch_r(a) as u8,
            0x021 => self.nvram[(a & 0x1FFFF) as usize],
            0x024 => self.io_r(a) as u8,
            0x030 => self.usb[(a & 0xff) as usize],
            0x040..=0x07F => self.flash_byte(a),
            0xFFC..=0xFFF => self.internal_r(a) as u8,
            _ => 0,
        }
    }
    fn r16(&mut self, a: u32) -> u16 {
        let a = a & !1;
        match a >> 20 {
            0x000 => u16::from_le_bytes([self.page0[a as usize], self.page0[a as usize + 1]]),
            0x010 => {
                let i = (a & 0xFFFFF) as usize;
                u16::from_le_bytes([self.ram[i], self.ram[i + 1]])
            }
            0x011 => self.switch_r(a) as u16,
            0x024 => self.io_r(a) as u16,
            0xFFC..=0xFFF => self.internal_r(a) as u16,
            _ => u16::from_le_bytes([self.r8(a), self.r8(a + 1)]),
        }
    }
    fn r32(&mut self, a: u32) -> u32 {
        let a = a & !3;
        match a >> 20 {
            0x000 => {
                let i = a as usize;
                u32::from_le_bytes(self.page0[i..i + 4].try_into().unwrap())
            }
            0x010 => {
                let i = (a & 0xFFFFF) as usize;
                u32::from_le_bytes(self.ram[i..i + 4].try_into().unwrap())
            }
            0x040..=0x07F => {
                let off = if a & 0x800000 != 0 {
                    ((self.bank as usize) << 23) | (a as usize & 0x7FFFFF)
                } else {
                    a as usize & 0x7FFFFF
                };
                match self.flash.get(off..off + 4) {
                    Some(s) => u32::from_le_bytes(s.try_into().unwrap()),
                    None => 0,
                }
            }
            0x011 => self.switch_r(a),
            0x024 => self.io_r(a),
            0xFFC..=0xFFF => self.internal_r(a),
            _ => u32::from_le_bytes([self.r8(a), self.r8(a + 1), self.r8(a + 2), self.r8(a + 3)]),
        }
    }
    fn w8(&mut self, a: u32, v: u8) {
        match a >> 20 {
            0x000 => self.page0[a as usize] = v,
            0x003 => self.reset_ram[(a & 0xFFFFF) as usize] = v,
            0x010 => {
                if a < 0x109F000 {
                    self.ram[(a & 0xFFFFF) as usize] = v
                }
            }
            0x011..=0x01F => {
                let sh = (a & 3) * 8;
                self.dmdram_w(a & !3, 1 << (a & 3), (v as u32) << sh)
            }
            0x021 => self.nvram[(a & 0x1FFFF) as usize] = v,
            0x024..=0x02F => self.bank_w(a, v as u32),
            0x030 => self.usb[(a & 0xff) as usize] = v,
            0xFFC..=0xFFF => self.internal_w(a, v as u32),
            _ => {}
        }
    }
    fn w16(&mut self, a: u32, v: u16) {
        let a = a & !1;
        match a >> 20 {
            0x011..=0x01F => {
                let sh = (a & 2) * 8;
                self.dmdram_w(a & !3, 3 << (a & 2), (v as u32) << sh)
            }
            0x024..=0x02F => self.bank_w(a, v as u32),
            0xFFC..=0xFFF => self.internal_w(a, v as u32),
            _ => {
                let b = v.to_le_bytes();
                self.w8(a, b[0]);
                self.w8(a + 1, b[1]);
            }
        }
    }
    fn w32(&mut self, a: u32, v: u32) {
        let a = a & !3;
        match a >> 20 {
            0x000 => {
                let i = a as usize;
                self.page0[i..i + 4].copy_from_slice(&v.to_le_bytes())
            }
            0x010 => {
                if a < 0x109F000 {
                    let i = (a & 0xFFFFF) as usize;
                    self.ram[i..i + 4].copy_from_slice(&v.to_le_bytes())
                }
            }
            0x011..=0x01F => self.dmdram_w(a, 0xf, v),
            0x024..=0x02F => self.bank_w(a, v),
            0xFFC..=0xFFF => self.internal_w(a, v),
            _ => {
                let b = v.to_le_bytes();
                for (k, x) in b.iter().enumerate() {
                    self.w8(a + k as u32, *x);
                }
            }
        }
    }
}

/// Why `Sam::run` returned.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Stop {
    Done,
    Break(u32),
}

#[derive(Clone)]
pub struct Sam {
    pub cpu: Cpu,
    pub bus: Bus,
    next_fiq: f64,
    next_dmd: f64,
    next_zc: f64,
    /// stop when PC reaches one of these (checked before the instruction runs)
    pub breaks: Vec<u32>,
    /// skip the break check once (to resume from a breakpoint)
    resume: bool,
    pub instructions: u64,
    /// cycle at which to report the picture after a page flip (0 = none pending)
    cap_at: u64,
    last_cap: u64,
    /// pictures reported at page flips: (cycle stamp, picture), consecutive duplicates removed
    pub flips: Vec<(u64, [u8; 4096])>,
    pub record_flips: bool,
}

impl Sam {
    pub fn new(rom: Rc<Vec<u8>>, name: &str) -> Sam {
        let mut bus = Bus::new(rom);
        bus.block_timers = name.starts_with("csi_") || name.starts_with("ij4_");
        Sam {
            cpu: Cpu::new(),
            bus,
            next_fiq: CLOCK_HZ as f64 / FIQ_HZ,
            next_dmd: CLOCK_HZ as f64 / DMD_HZ,
            next_zc: CLOCK_HZ as f64 / ZC_HZ,
            breaks: Vec::new(),
            resume: false,
            instructions: 0,
            cap_at: 0,
            last_cap: 0,
            flips: Vec::new(),
            record_flips: false,
        }
    }

    pub fn reg(&self, r: u8) -> u32 {
        self.cpu.reg_get(self.cpu.mode(), r)
    }
    pub fn pc(&self) -> u32 {
        self.reg(reg::PC)
    }

    /// Run `cycles` CPU cycles; `on_dmd` gets the display picture at each scan (62.67 Hz).
    pub fn run(&mut self, cycles: u64, on_dmd: &mut dyn FnMut(&Sam)) -> Stop {
        let end = self.bus.cycles + cycles;
        while self.bus.cycles < end {
            // next event
            let mut next = end
                .min(if self.cap_at != 0 {
                    self.cap_at
                } else {
                    u64::MAX
                })
                .min(self.next_fiq.ceil() as u64)
                .min(self.next_dmd.ceil() as u64);
            next = next.min(self.next_zc.ceil() as u64);
            for t in &self.bus.tc {
                if t.next != 0 {
                    next = next.min(t.next);
                }
            }
            for u in &self.bus.us {
                if u.period > 0.0 {
                    next = next.min(u.next.ceil() as u64);
                }
            }
            while self.bus.cycles < next {
                if self.bus.pending_irq && self.cpu.irq_enable() {
                    self.cpu.exception(Exception::Interrupt);
                    self.bus.cycles += 3;
                }
                let pc = self.cpu.reg_get(Mode::User, reg::PC);
                if !self.breaks.is_empty() && !self.resume && self.breaks.contains(&pc) {
                    self.resume = true;
                    return Stop::Break(pc);
                }
                self.resume = false;
                let inst = self.bus.r32(pc);
                let cpsr = self.cpu.reg_get(Mode::User, reg::CPSR);
                self.bus.cycles += if cond_met(inst >> 28, cpsr) { 3 } else { 1 };
                self.cpu.step(&mut self.bus);
                self.instructions += 1;
            }
            let now = self.bus.cycles as f64;
            if now >= self.next_fiq {
                self.next_fiq += CLOCK_HZ as f64 / FIQ_HZ;
                if self.cpu.fiq_enable() {
                    self.cpu.exception(Exception::FastInterrupt);
                    self.bus.cycles += 3;
                }
            }
            if now >= self.next_zc {
                self.next_zc += CLOCK_HZ as f64 / ZC_HZ;
                self.bus.zc = !self.bus.zc;
            }
            for n in 0..3 {
                let t = &mut self.bus.tc[n];
                if t.next != 0 && self.bus.cycles >= t.next {
                    t.next += t.period;
                    t.counter = 0;
                    t.status |= 0x10;
                    if t.irq_mask & 0x10 != 0 && (!self.bus.block_timers || self.cpu.irq_enable()) {
                        self.bus.fire_irq(4 + n as u32);
                    }
                }
            }
            for n in 0..2 {
                let u = &self.bus.us[n];
                if u.period > 0.0 && now >= u.next {
                    self.bus.us[n].next += self.bus.us[n].period;
                    self.bus.usart_tick(n);
                }
            }
            if self.bus.flipped {
                self.bus.flipped = false;
                if self.cap_at == 0 {
                    // both page registers are written within a few instructions: look 0.5 ms
                    // later, and no sooner than 10 ms after the last look (an effect forced with
                    // bogus arguments can flip pages every millisecond; the panel would blend them)
                    self.cap_at =
                        (self.bus.cycles + CLOCK_HZ / 2000).max(self.last_cap + CLOCK_HZ / 100);
                }
            }
            if self.cap_at != 0 && self.bus.cycles >= self.cap_at {
                self.cap_at = 0;
                self.last_cap = self.bus.cycles;
                if self.record_flips {
                    let p = self.bus.picture();
                    if self.flips.last().map(|l| l.1 != p).unwrap_or(true) {
                        self.flips.push((self.bus.cycles, p));
                    }
                }
            }
            if now >= self.next_dmd {
                self.next_dmd += CLOCK_HZ as f64 / DMD_HZ;
                on_dmd(self);
            }
        }
        Stop::Done
    }
}

impl Sam {
    /// Read a little-endian u16/u32 from the CPU's view of memory.
    pub fn peek16(&mut self, a: u32) -> u16 {
        self.bus.r16(a)
    }
    pub fn peek32(&mut self, a: u32) -> u32 {
        self.bus.r32(a)
    }

    /// Run until PC reaches `pc` (or `limit` cycles pass); true if reached.
    pub fn run_to(&mut self, pc: u32, limit: u64) -> bool {
        self.breaks = vec![pc];
        let r = self.run(limit, &mut |_| {});
        self.breaks.clear();
        r == Stop::Break(pc)
    }

    /// Call `target(args)` from the context of a task stopped at `hook` (the OS sleep entry, which
    /// every task calls once per frame): the call returns to `hook`, where the saved registers are
    /// put back and the task's own sleep goes on. Returns the call's r0, or None if the call never
    /// came back within `limit` cycles.
    pub fn call_at(
        &mut self,
        hook: u32,
        target: u32,
        args: &[u32],
        limit: u64,
        ok: &dyn Fn(&mut Sam) -> bool,
    ) -> Option<u32> {
        let start = self.bus.cycles;
        loop {
            let left = (start + limit).saturating_sub(self.bus.cycles);
            if left == 0 || !self.run_to(hook, left) {
                return None;
            }
            if ok(self) {
                break;
            }
        }
        let mode = self.cpu.mode();
        let saved: Vec<u32> = (0..16).map(|r| self.cpu.reg_get(mode, r)).collect();
        let cpsr = self.cpu.reg_get(mode, reg::CPSR);
        for (k, a) in args.iter().enumerate() {
            self.cpu.reg_set(mode, k as u8, *a);
        }
        self.cpu.reg_set(mode, reg::LR, hook);
        self.cpu.reg_set(mode, reg::PC, target);
        let start = self.bus.cycles;
        loop {
            let left = (start + limit).saturating_sub(self.bus.cycles);
            if left == 0 || !self.run_to(hook, left) {
                return None;
            }
            if self.cpu.mode() == mode && self.reg(reg::SP) == saved[13] {
                break;
            }
            // another task's sleep: let it go
            self.resume = true;
        }
        let r0 = self.reg(0);
        let cur = self.cpu.mode();
        for (r, v) in saved.iter().enumerate() {
            self.cpu.reg_set(cur, r as u8, *v);
        }
        self.cpu.reg_set(cur, reg::CPSR, cpsr);
        self.resume = true;
        Some(r0)
    }
}

fn cond_met(cond: u32, cpsr: u32) -> bool {
    let n = cpsr >> 31 & 1 != 0;
    let z = cpsr >> 30 & 1 != 0;
    let c = cpsr >> 29 & 1 != 0;
    let v = cpsr >> 28 & 1 != 0;
    match cond {
        0x0 => z,
        0x1 => !z,
        0x2 => c,
        0x3 => !c,
        0x4 => n,
        0x5 => !n,
        0x6 => v,
        0x7 => !v,
        0x8 => c && !z,
        0x9 => !c || z,
        0xA => n == v,
        0xB => n != v,
        0xC => !z && n == v,
        0xD => z || n != v,
        _ => true,
    }
}

/// Load a SAM ROM: a .bin, or the first .bin inside a PinMAME .zip (via `unzip -p`).
pub fn load_rom(path: &str) -> Rc<Vec<u8>> {
    let data = if path.ends_with(".zip") {
        let out = std::process::Command::new("unzip")
            .args(["-p", path, "*.bin"])
            .output()
            .expect("unzip");
        out.stdout
    } else {
        std::fs::read(path).expect("rom")
    };
    Rc::new(data)
}

fn rom_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn write_frame(o: &mut impl std::io::Write, ms: u64, f: &[u8; 4096]) {
    writeln!(o, "0x{ms:08x}").unwrap();
    for row in f.chunks(128) {
        let s: String = row
            .iter()
            .map(|v| char::from_digit(*v as u32, 16).unwrap())
            .collect();
        writeln!(o, "{s}").unwrap();
    }
    writeln!(o).unwrap();
}

/// `rom2dmd sam-…` subcommands.
pub fn cli(args: &[String]) {
    match args[1].as_str() {
        // sam-boot <rom.zip|bin> <secs> <out.txt>: boot and dump every new displayed picture
        "sam-boot" => {
            let rom = load_rom(&args[2]);
            let mut s = Sam::new(rom, &rom_name(&args[2]));
            let secs: f64 = args[3].parse().unwrap();
            let mut o = std::io::BufWriter::new(std::fs::File::create(&args[4]).unwrap());
            let mut last = [0xffu8; 4096];
            let t = std::time::Instant::now();
            for k in 0..((secs * 10.0) as u64) {
                s.run(CLOCK_HZ / 10, &mut |s| {
                    let p = s.bus.picture();
                    if p != last {
                        write_frame(&mut o, s.bus.cycles / (CLOCK_HZ / 1000), &p);
                        last = p;
                    }
                });
                eprintln!(
                    "{k:4} pc {:08x} cyc {} mode {:?} pages {:?} instr {} ({:.1} s real)",
                    s.pc(),
                    s.bus.cycles,
                    s.cpu.mode(),
                    s.bus.video_page,
                    s.instructions,
                    t.elapsed().as_secs_f64()
                );
            }
        }
        // sam-deff <rom> <first> <last> <cap secs> <outdir>: boot BOOT s (default 30) once, then for
        // each id from that snapshot call START(id, A, B, 1) (the deff_start core; default A=0 B=1, B forces the effect over
        // the running one) from a task stopped at HOOK (OS sleep entry), and record every picture
        // flip until the current-effect word at CUR stops being the id (or the cap).
        // Env addresses in hex. Writes <outdir>/df_<id>.txt and prints one verdict line per id.
        "sam-deff" => {
            let env = |k: &str| std::env::var(k).ok();
            let hex = |k: &str| u32::from_str_radix(&env(k).expect(k), 16).unwrap();
            let (hook, start, cur) = (hex("HOOK"), hex("START"), hex("CUR"));
            let a: u32 = env("A").and_then(|v| v.parse().ok()).unwrap_or(0);
            let b: u32 = env("B").and_then(|v| v.parse().ok()).unwrap_or(1);
            let hold: f64 = env("HOLD").and_then(|v| v.parse().ok()).unwrap_or(5.0);
            let boot: f64 = env("BOOT").and_then(|v| v.parse().ok()).unwrap_or(30.0);
            let rom = load_rom(&args[2]);
            let first: u32 = args[3].parse().unwrap();
            let mut last: u32 = args[4].parse().unwrap();
            let cap: f64 = args[5].parse().unwrap();
            let out = &args[6];
            std::fs::create_dir_all(out).unwrap();
            let mut s0 = Sam::new(rom, &rom_name(&args[2]));
            s0.run((boot * CLOCK_HZ as f64) as u64, &mut |_| {});
            // NPTR: address of the effect count (ids are 1..count-1)
            if let Some(np) = env("NPTR") {
                let np = u32::from_str_radix(&np, 16).unwrap();
                let n = if env("NSIZE").as_deref() == Some("2") {
                    s0.peek16(np) as u32
                } else {
                    s0.peek32(np)
                };
                eprintln!("effect count {n}");
                last = last.min(n.saturating_sub(1));
            }
            for id in first..=last {
                let mut s = s0.clone();
                s.record_flips = true;
                // the picture on screen before the call
                let p = s.bus.picture();
                s.flips.push((s.bus.cycles, p));
                // call from a task that is not a display effect itself (CURTASK = RAM word holding the
                // current task, its flags u16 at +2, TFLAG = display-effect task flag)
                let curtask = env("CURTASK").map(|v| u32::from_str_radix(&v, 16).unwrap());
                let tflag: u16 = env("TFLAG")
                    .map(|v| u16::from_str_radix(&v, 16).unwrap())
                    .unwrap_or(4);
                let ok = move |s: &mut Sam| match curtask {
                    Some(ct) => {
                        let t = s.peek32(ct);
                        t == 0 || s.peek16(t + 2) & tflag == 0
                    }
                    None => true,
                };
                let r = s.call_at(hook, start, &[id, a, b, 1], CLOCK_HZ, &ok);
                let t0 = s.bus.cycles;
                let mut seen = false;
                let mut kind = "cap";
                let mut fell = None;
                while s.bus.cycles - t0 < (cap * CLOCK_HZ as f64) as u64 {
                    s.run(CLOCK_HZ / 100, &mut |_| {});
                    let c = s.peek16(cur) as u32;
                    if c == id {
                        seen = true;
                    } else if seen {
                        kind = "end";
                        fell = Some(s.bus.cycles);
                        break;
                    } else if s.bus.cycles - t0 > CLOCK_HZ / 2 {
                        kind = "none";
                        break;
                    }
                    // still the effect, but the picture has not changed for HOLD s (default 5)
                    let last = s.flips.last().map(|f| f.0).unwrap_or(t0).max(t0);
                    if seen && s.bus.cycles - last > (hold * CLOCK_HZ as f64) as u64 {
                        kind = "hold";
                        break;
                    }
                }
                let ms = |t: u64| t.saturating_sub(t0) / (CLOCK_HZ / 1000);
                println!(
                    "{id:3} {kind:5} ret {:08x} end {} frames {}",
                    r.unwrap_or(u32::MAX),
                    fell.map(|t| format!("{:.2}", ms(t) as f64 / 1000.0))
                        .unwrap_or("-".into()),
                    s.flips.len()
                );
                let mut o = std::io::BufWriter::new(
                    std::fs::File::create(format!("{out}/df_{id:03}.txt")).unwrap(),
                );
                for (t, f) in &s.flips {
                    write_frame(&mut o, ms(*t), f);
                }
            }
        }
        // sam-dump <rom> <secs> <outprefix>: boot and save page 0 RAM, U13 RAM and NVRAM
        "sam-dump" => {
            let rom = load_rom(&args[2]);
            let mut s = Sam::new(rom, &rom_name(&args[2]));
            s.run(
                (args[3].parse::<f64>().unwrap() * CLOCK_HZ as f64) as u64,
                &mut |_| {},
            );
            std::fs::write(format!("{}.page0", args[4]), &s.bus.page0).unwrap();
            std::fs::write(format!("{}.ram", args[4]), &s.bus.ram).unwrap();
            std::fs::write(format!("{}.nvram", args[4]), &s.bus.nvram).unwrap();
        }
        _ => eprintln!("unknown sam subcommand"),
    }
}
