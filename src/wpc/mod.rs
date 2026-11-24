//! Williams WPC CPU board, DMD generation (WPC-89, Fliptronic, DCS, WPC-S, WPC-95).
//!
//! Ported from WPC-EMU (the reference emulator our WPC DMD scene tracer ran on) and checked
//! against PinMAME `wpc.c`. The board runs a game ROM (the U6 image) from power-on to its DMD
//! output: 6809 at 2 MHz (mc6809-core), ROM banking, RAM protection, the ASIC (bit shifter,
//! watchdog, zero-cross, periodic IRQ at 976 Hz, FIRQ on a DMD scanline, clock), the switch
//! matrix and cabinet inputs, the DMD controller and, on WPC-S / WPC-95, the security PIC.
//! The sound board is a stub that only answers what a game waits for.
//!
//! The whole machine is `Clone`: a clone is a snapshot (the ROM is shared), assigning it
//! back restores it.

// Board API for the scene tracer (next stage); the CLI does not use all of it yet.
#![allow(dead_code)]

pub mod bus;
pub mod dmd;
pub mod games;
pub mod pic;
pub mod probe;
pub mod scan;
pub mod sound;
pub mod timing;

use std::io::{Read, Write};
use std::sync::Arc;

use mc6809_core::{Cpu, Memory};

use bus::Bus;
use pic::Pic;
use timing::Entry;

pub const CLOCK_HZ: u64 = 2_000_000;

/// Board generation: decides the sound interface and whether the security PIC is fitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generation {
    /// WPC-89 with DMD (pre-DCS sound).
    Dmd,
    /// WPC Fliptronic (pre-DCS sound).
    Fliptronic,
    /// WPC DCS.
    Dcs,
    /// WPC Security: DCS sound and the security PIC.
    Security,
    /// WPC-95: as WPC-S, with the extra DMD windows and flipper ports.
    Wpc95,
}

impl Generation {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "dmd" | "wpc89" => Self::Dmd,
            "flip" | "fliptronic" => Self::Fliptronic,
            "dcs" => Self::Dcs,
            "wpcs" | "security" => Self::Security,
            "wpc95" => Self::Wpc95,
            _ => return None,
        })
    }

    pub fn has_pic(self) -> bool {
        matches!(self, Self::Security | Self::Wpc95)
    }

    pub fn pre_dcs(self) -> bool {
        matches!(self, Self::Dmd | Self::Fliptronic)
    }
}

/// Options that make a run replay WPC-EMU exactly; all off reproduces the hardware.
#[derive(Clone, Copy, Debug, Default)]
pub struct Compat {
    /// Clock the board with WPC-EMU's cycle counts (see `timing.rs`).
    pub timing: bool,
    /// Patch the ROM checksum correction word (0xffec) so the boot ROM test is skipped, as
    /// WPC-EMU's `skipWpcRomCheck` does.
    pub skip_rom_check: bool,
    /// WPC-EMU's security PIC always holds Medieval Madness' serial (559) and the game ID
    /// read is patched to match; off, the PIC holds the game's own number.
    pub pic_559: bool,
    /// Banks beyond the game part of the ROM read 0, as in WPC-EMU (the hardware maps the
    /// system part there).
    pub zero_top_banks: bool,
    /// CPU writes to the fixed ROM region are stored, as WPC-EMU does (the hardware ignores
    /// them). AFM pushes a return address with S = 0 while booting, which overwrites the
    /// reset vector in WPC-EMU, so its next reset starts at 0x8ebf instead of 0x8dd4.
    pub rom_writes: bool,
    /// WPC-EMU's 6809 flag quirks: NEG sets carry from bit 7 of the result (the 6809 sets it
    /// when the result is not 0) and DAA clears a carry it does not produce (the 6809 keeps
    /// it). Games use DAA for BCD scores, so a played game drifts without this.
    pub cpu_quirks: bool,
}

impl Compat {
    pub fn wpc_emu() -> Self {
        Compat {
            timing: true,
            skip_rom_check: true,
            pic_559: true,
            zero_top_banks: true,
            rom_writes: true,
            cpu_quirks: true,
        }
    }
}

/// The game ID: the second "EC 9F hi lo 83 12 34" sequence in the ROM points (through the
/// system ROM) at a word holding the game number + 0x1234. Returns (address, game number).
pub fn find_game_id(rom: &[u8]) -> Option<(u16, u16)> {
    let (game, sys) = rom.split_at(rom.len().checked_sub(0x8000)?);
    let mut found = 0;
    for x in 0..game.len().saturating_sub(7) {
        let w = &game[x..x + 7];
        if w[0] == 0xec && w[1] == 0x9f && w[4..7] == [0x83, 0x12, 0x34] {
            found += 1;
            if found == 2 {
                let p = (((w[2] as usize) << 8) | w[3] as usize).checked_sub(0x8000)?;
                let loc = ((*sys.get(p)? as u16) << 8) | *sys.get(p + 1)? as u16;
                let off = (loc as usize).checked_sub(0x8000)?;
                let id = ((*sys.get(off)? as u16) << 8) | *sys.get(off + 1)? as u16;
                return Some((loc, id.wrapping_sub(0x1234)));
            }
        }
    }
    None
}

/// One complete DMD refresh.
pub struct Refresh<'a> {
    /// Refreshes since power-on.
    pub index: u64,
    /// CPU cycle at which the last scanline was clocked out.
    pub cycle: u64,
    /// The page that was shown.
    pub page: u8,
    /// The picture as clocked out (512 bytes, bit 0 = leftmost pixel).
    pub frame: &'a [u8; dmd::PAGE_SIZE],
}

#[derive(Clone)]
pub struct Board {
    pub cpu: Cpu,
    pub bus: Bus,
    pub generation: Generation,
    pub compat: Compat,
    /// CPU cycles since reset (in WPC-EMU cycles when `compat.timing` is set).
    pub cycles: u64,
    pub refreshes: u64,
    ticks_irq: u32,
    irq_pending: bool,
    firq_pending: bool,
}

impl Board {
    /// Build the board around a U6 game ROM (128 KiB to 1 MiB) and power it on.
    pub fn new(rom: Vec<u8>, generation: Generation, compat: Compat) -> Result<Self, String> {
        let page_mask = match rom.len() {
            0x20000 => 0x07,
            0x40000 => 0x0f,
            0x80000 => 0x1f,
            0x100000 => 0x3f,
            n => return Err(format!("not a WPC game ROM size: {n} bytes")),
        };
        let game_id = find_game_id(&rom);
        let mut patches = Vec::new();
        if compat.skip_rom_check {
            patches.extend([(0xffec, 0x00), (0xffed, 0xff)]);
        }
        let pic = if generation.has_pic() {
            let machine = if compat.pic_559 {
                559
            } else {
                game_id
                    .map(|g| g.1)
                    .ok_or("no game ID in the ROM, needed by the security PIC")?
            };
            Some(Pic::new(machine))
        } else {
            None
        };
        if compat.pic_559
            && let Some((loc, _)) = game_id
        {
            // 559 + 0x1234
            patches.extend([(loc, 0x14), (loc.wrapping_add(1), 0x63)]);
        }
        let mut bus = Bus::new(Arc::new(rom), page_mask, pic, generation.pre_dcs());
        bus.patches = patches;
        bus.zero_top_banks = compat.zero_top_banks;
        bus.rom_writes = compat.rom_writes;
        let mut b = Board {
            cpu: Cpu::new(),
            bus,
            generation,
            compat,
            cycles: 0,
            refreshes: 0,
            ticks_irq: 0,
            irq_pending: false,
            firq_pending: false,
        };
        b.reset();
        Ok(b)
    }

    /// CPU reset line: CPU and ASIC restart, RAM and the DMD keep their contents.
    pub fn reset(&mut self) {
        self.ticks_irq = 0;
        self.irq_pending = false;
        self.firq_pending = false;
        self.cpu.set_irq(false);
        self.cpu.set_firq(false);
        self.bus.reset();
        self.cpu.reset(&mut self.bus);
        self.cycles = 0;
        self.bus.now = 0;
    }

    /// Close (or open) switch `n` in WPC numbering (column * 10 + row, e.g. 13 = start).
    pub fn set_switch(&mut self, n: u8, closed: bool) {
        self.bus.set_switch(n, closed);
    }

    /// Coin door inputs (bit 0 escape, 1 down, 2 up, 3 enter, 4-7 coin slots); stays set
    /// until changed.
    pub fn set_cabinet(&mut self, bits: u8) {
        self.bus.switches[0] = bits;
    }

    /// Fliptronic switch F1..F8 (`n` = 1..=8).
    pub fn set_fliptronic(&mut self, n: u8, closed: bool) {
        if (1..=8).contains(&n) {
            let bit = 1 << (n - 1);
            if closed {
                self.bus.switches[9] |= bit;
            } else {
                self.bus.switches[9] &= !bit;
            }
        }
    }

    /// Execute one instruction (or interrupt entry) and clock the board accordingly.
    pub fn step(&mut self, on_refresh: &mut impl FnMut(&Refresh)) -> u64 {
        let cc = self.cpu.registers().cc;
        // Interrupt requests are latched and dropped once the CPU takes them (WPC-EMU; MAME's
        // HOLD_LINE): FIRQ first, then IRQ.
        let entry = if self.firq_pending && !cc.firq_inhibit() {
            Entry::Firq
        } else if self.irq_pending && !cc.irq_inhibit() {
            Entry::Irq
        } else {
            Entry::None
        };
        let model = if self.compat.timing {
            let bus = &self.bus;
            timing::cycles(|a| bus.peek(a), self.cpu.registers(), entry)
        } else {
            0
        };
        // WPC-EMU flag quirks (see `Compat::cpu_quirks`): opcode and DAA inputs before the step
        let quirk = if self.compat.cpu_quirks && entry == Entry::None {
            let g = self.cpu.registers();
            Some((self.bus.peek(g.pc), g.a(), g.cc.to_byte()))
        } else {
            None
        };
        let real = self.cpu.step(&mut self.bus);
        if let Some((op, a, cc)) = quirk {
            const C: u8 = 0x01;
            const V: u8 = 0x02;
            const Z: u8 = 0x04;
            const N: u8 = 0x08;
            const H: u8 = 0x20;
            match op {
                // NEG direct / NEGA / NEGB / NEG indexed / NEG extended: C = N
                0x00 | 0x40 | 0x50 | 0x60 | 0x70 => {
                    let mut g = self.cpu.registers_mut();
                    let f = g.cc.to_byte();
                    let c = if f & N != 0 { C } else { 0 };
                    g.cc = mc6809_core::ConditionCodes::from_byte((f & !C) | c);
                }
                0x19 => {
                    let (hi, lo) = (a & 0xf0, a & 0x0f);
                    let mut corr = 0u16;
                    if lo > 9 || cc & H != 0 {
                        corr |= 0x06;
                    }
                    if (hi > 0x80 && lo > 9) || hi > 0x90 || cc & C != 0 {
                        corr |= 0x60;
                    }
                    let r = corr + a as u16;
                    let v = r as u8;
                    let mut f = cc & !(C | N | Z | V);
                    if r & 0x100 != 0 {
                        f |= C;
                    }
                    if v == 0 {
                        f |= Z;
                    }
                    if v & 0x80 != 0 {
                        f |= N;
                    }
                    let mut g = self.cpu.registers_mut();
                    g.set_a(v);
                    g.cc = mc6809_core::ConditionCodes::from_byte(f);
                }
                _ => {}
            }
        }
        if self.cpu.illegal() {
            self.cpu.clear_illegal();
        }
        match entry {
            Entry::Firq => {
                self.firq_pending = false;
                self.cpu.set_firq(false);
            }
            Entry::Irq => {
                self.irq_pending = false;
                self.cpu.set_irq(false);
            }
            Entry::None => {}
        }
        // never 0 (a halted CPU, or an opcode WPC-EMU has no count for), so time moves on
        let ticks = if self.compat.timing { model } else { real }.max(1);
        self.cycles += ticks;
        self.bus.now = self.cycles;
        let t = ticks as u32;
        self.ticks_irq += t;
        if self.ticks_irq >= bus::IRQ_CYCLES {
            self.ticks_irq -= bus::IRQ_CYCLES;
            // The periodic IRQ fires whatever the enable bit says (as WPC-EMU, which notes
            // that some games, e.g. Indiana Jones, need it).
            self.irq_pending = true;
            self.cpu.set_irq(true);
        }
        if let Some(ev) = self.bus.dmd.tick(t) {
            if let Some(page) = ev.refreshed {
                let r = Refresh {
                    index: self.refreshes,
                    cycle: self.cycles,
                    page,
                    frame: self.bus.dmd.frame(),
                };
                on_refresh(&r);
                self.refreshes += 1;
            }
            if self.bus.dmd.request_firq && ev.scanline == self.bus.ram[bus::DMD_FIRQ_LINE as usize]
            {
                self.firq_pending = true;
                self.cpu.set_firq(true);
                self.bus.firq_source_dmd = true;
                self.bus.dmd.request_firq = false;
            }
        }
        self.bus.tick(t);
        ticks
    }

    /// Run for at least `cycles` CPU cycles.
    pub fn run(&mut self, cycles: u64, mut on_refresh: impl FnMut(&Refresh)) {
        let end = self.cycles + cycles;
        while self.cycles < end {
            self.step(&mut on_refresh);
        }
    }

    /// Load `snapshot` the way WPC-EMU's `setState` does, for `--compat` replays of the scene
    /// tracer: WPC-EMU does not save the CPU's pending interrupt lines, the DMD's active page,
    /// the watchdog counter, the security PIC or the sound interface, so those keep the values
    /// of the machine being overwritten (`self`), and a missing pending DMD page becomes page 0
    /// (`undefined & 0xf`). Everything else comes from the snapshot.
    pub fn restore_like_wpc_emu(&mut self, snapshot: &Board) {
        let mut b = snapshot.clone();
        b.irq_pending = self.irq_pending;
        b.firq_pending = self.firq_pending;
        b.cpu.set_irq(self.irq_pending);
        b.cpu.set_firq(self.firq_pending);
        b.bus.dmd.active = self.bus.dmd.active;
        b.bus.dmd.next_active = Some(snapshot.bus.dmd.next_active.unwrap_or(0));
        b.bus.watchdog_ticks = self.bus.watchdog_ticks;
        b.bus.pic = self.bus.pic.take();
        b.bus.sound = std::mem::take(&mut self.bus.sound);
        *self = b;
    }

    /// Read memory as the CPU would, without side effects on hardware registers.
    pub fn peek(&self, addr: u16) -> u8 {
        self.bus.peek(addr)
    }

    /// Write memory as the CPU would (RAM protection applies; hardware registers react).
    pub fn poke(&mut self, addr: u16, val: u8) {
        self.bus.write(addr, val);
    }
}

/// Load the U6 game ROM: a plain image, or the member of a zip that carries a WPC game ID
/// (or `member` when given).
pub fn load_rom(path: &str, member: Option<&str>) -> Result<Vec<u8>, String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if !data.starts_with(b"PK\x03\x04") {
        return Ok(data);
    }
    let mut zip =
        zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| format!("{path}: {e}"))?;
    let mut members = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| format!("{path}: {e}"))?;
        let name = f.name().map_err(|e| format!("{path}: {e}"))?.to_string();
        let mut v = Vec::new();
        f.read_to_end(&mut v)
            .map_err(|e| format!("{path}:{name}: {e}"))?;
        members.push((name, v));
    }
    if let Some(m) = member {
        return members
            .into_iter()
            .find(|(n, _)| n == m)
            .map(|(_, v)| v)
            .ok_or_else(|| format!("{path}: no member {m}"));
    }
    load_rom_files(&members).map_err(|e| format!("{path}: {e}"))
}

/// The U6 game ROM among a set's files: the one that carries a WPC game ID, else (older
/// games such as T2 have none) one whose name ends in .rom or holds "u6".
pub fn load_rom_files(members: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
    let mut fallback = None;
    for (n, v) in members {
        if !matches!(v.len(), 0x20000 | 0x40000 | 0x80000 | 0x100000) {
            continue;
        }
        // reset vector in the fixed ROM
        let reset = ((v[v.len() - 2] as u16) << 8) | v[v.len() - 1] as u16;
        if reset < 0x8000 {
            continue;
        }
        if find_game_id(v).is_some() {
            return Ok(v.clone());
        }
        let l = n.to_ascii_lowercase();
        if fallback.is_none() && (l.ends_with(".rom") || l.contains("u6")) {
            fallback = Some(v.clone());
        }
    }
    fallback.ok_or_else(|| format!("no game ROM among {names:?} (use --member)"))
}

/// One line per refresh: `index page data`, where data is the frame in hex, or `=` when it
/// is the same as the previous refresh's.
pub struct RefreshLog<W: Write> {
    out: W,
    last: Option<[u8; dmd::PAGE_SIZE]>,
    pub err: Option<std::io::Error>,
}

impl<W: Write> RefreshLog<W> {
    pub fn new(out: W) -> Self {
        RefreshLog {
            out,
            last: None,
            err: None,
        }
    }

    pub fn push(&mut self, r: &Refresh) {
        if self.err.is_some() {
            return;
        }
        let res = if self.last.as_ref() == Some(r.frame) {
            writeln!(self.out, "{} {} =", r.index, r.page)
        } else {
            self.last = Some(*r.frame);
            let hex: String = r.frame.iter().map(|b| format!("{b:02x}")).collect();
            writeln!(self.out, "{} {} {hex}", r.index, r.page)
        };
        if let Err(e) = res {
            self.err = Some(e);
        }
    }
}

/// `rom2dmd wpc-run <rom.zip|u6.rom> <seconds> <out> [--gen dmd|flip|dcs|wpcs|wpc95]
/// [--compat] [--member name]`: power on, no input, log every DMD refresh (see `RefreshLog`).
/// `--compat` replays WPC-EMU (cycle counts, ROM check skip, PIC serial 559, top banks 0,
/// writable ROM).
pub fn cli(args: &[String]) {
    let res = match args.get(1).map(String::as_str) {
        Some("wpc-scan") => scan::cli(args),
        Some("wpc-probe") => probe::cli(args),
        _ => run_cli(args),
    };
    if let Err(e) = res {
        log::info!("error: {e}");
        std::process::exit(1);
    }
}

fn run_cli(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: rom2dmd wpc-run <rom.zip|u6.rom> <seconds> <out> \
        [--gen dmd|flip|dcs|wpcs|wpc95] [--compat] [--wpcemu-timing] [--skip-rom-check] \
        [--pic-559] [--zero-top-banks] [--rom-writes] [--member name] [--reset-at secs] [--closed n,n,...] \
        [--trace from,count]";
    if args.get(1).map(String::as_str) != Some("wpc-run") || args.len() < 5 {
        return Err(USAGE.into());
    }
    let mut generation = Generation::Dmd;
    let mut compat = Compat::default();
    let mut member = None;
    let mut reset_at = None;
    let mut closed = Vec::new();
    let mut trace = None;
    let mut it = args[5..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--gen" => {
                let g = it.next().ok_or(USAGE)?;
                generation = Generation::parse(g).ok_or(format!("unknown generation {g}"))?;
            }
            "--compat" => compat = Compat::wpc_emu(),
            "--wpcemu-timing" => compat.timing = true,
            "--skip-rom-check" => compat.skip_rom_check = true,
            "--pic-559" => compat.pic_559 = true,
            "--zero-top-banks" => compat.zero_top_banks = true,
            "--rom-writes" => compat.rom_writes = true,
            "--reset-at" => {
                let v = it.next().ok_or(USAGE)?;
                reset_at = Some(v.parse::<f64>().map_err(|_| USAGE)?);
            }
            "--trace" => {
                // --trace from,count: print the CPU state before each of `count` steps from
                // cycle `from` (counted from the last reset)
                let v = it.next().ok_or(USAGE)?;
                let (a, b) = v.split_once(',').ok_or(USAGE)?;
                trace = Some((
                    a.parse::<u64>().map_err(|_| USAGE)?,
                    b.parse::<u64>().map_err(|_| USAGE)?,
                ));
            }
            "--closed" => {
                for n in it.next().ok_or(USAGE)?.split(',') {
                    closed.push(n.parse::<u8>().map_err(|_| USAGE)?);
                }
            }
            "--member" => member = Some(it.next().ok_or(USAGE)?.as_str()),
            _ => return Err(USAGE.into()),
        }
    }
    let secs: f64 = args[3].parse().map_err(|_| USAGE)?;
    let rom = load_rom(&args[2], member)?;
    let mut board = Board::new(rom, generation, compat)?;
    let f = std::fs::File::create(&args[4]).map_err(|e| format!("{}: {e}", args[4]))?;
    let mut log = RefreshLog::new(std::io::BufWriter::new(f));
    for n in closed {
        board.set_switch(n, true);
    }
    let total = (secs * CLOCK_HZ as f64) as u64;
    let mut elapsed = 0;
    if let Some(r) = reset_at {
        // a reset (RAM kept): leaves the first-boot "factory settings restored" screen
        let first = ((r * CLOCK_HZ as f64) as u64).min(total);
        board.run(first, |r| log.push(r));
        elapsed = board.cycles;
        board.reset();
    }
    let rest = total.saturating_sub(elapsed);
    match trace {
        None => board.run(rest, |r| log.push(r)),
        Some((from, count)) => {
            let mut push = |r: &Refresh| log.push(r);
            let mut left = count;
            while board.cycles < rest {
                if board.cycles >= from && left > 0 {
                    left -= 1;
                    let g = board.cpu.registers();
                    println!(
                        "{} {:04x} {:02x} {:02x} {:04x} {:04x} {:04x} {:04x} {:02x} {:02x} {}",
                        board.cycles,
                        g.pc,
                        g.a(),
                        g.b(),
                        g.x,
                        g.y,
                        g.u,
                        g.s,
                        g.dp,
                        g.cc.to_byte(),
                        board.bus.rom_bank
                    );
                }
                board.step(&mut push);
            }
        }
    }
    if let Some(e) = log.err.take() {
        return Err(format!("{}: {e}", args[4]));
    }
    log.out.flush().map_err(|e| format!("{}: {e}", args[4]))?;
    log::info!(
        "{} refreshes, pc {:04x}, bank {}, watchdog expiries {}, protected writes {}",
        board.refreshes,
        board.cpu.registers().pc,
        board.bus.rom_bank,
        board.bus.watchdog_expired,
        board.bus.protected_write_attempts
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A game ROM zip from the ROM collection ($ROM2DMD_ROMS, default ~/roms-full); None (the
    /// test is skipped) when absent.
    fn rom(zip: &str) -> Option<Vec<u8>> {
        let dir = std::env::var("ROM2DMD_ROMS")
            .unwrap_or_else(|_| format!("{}/roms-full", std::env::var("HOME").unwrap_or_default()));
        let path = format!("{dir}/{zip}");
        if !std::path::Path::new(&path).exists() {
            log::info!("{path} absent, skipped");
            return None;
        }
        Some(load_rom(&path, None).expect("game ROM"))
    }

    fn fnv1a(data: &[u8]) -> u64 {
        data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// Run `secs` seconds (with a reset at `reset_at`) and return the refresh log.
    fn log(board: &mut Board, secs: f64, reset_at: Option<f64>) -> Vec<u8> {
        let mut log = RefreshLog::new(Vec::new());
        let total = (secs * CLOCK_HZ as f64) as u64;
        let mut elapsed = 0;
        if let Some(r) = reset_at {
            board.run((r * CLOCK_HZ as f64) as u64, |f| log.push(f));
            elapsed = board.cycles;
            board.reset();
        }
        board.run(total - elapsed, |f| log.push(f));
        log.out
    }

    /// T2 attract with WPC-EMU's options: the refresh log digest is the one of WPC-EMU's own
    /// log for the same run (wpc_dump.js, 30 s, reset at 6 s), so the board replays it bit
    /// for bit.
    #[test]
    fn t2_compat_matches_wpc_emu() {
        let Some(rom) = rom("t2_l8.zip") else { return };
        let mut b = Board::new(rom, Generation::Dmd, Compat::wpc_emu()).unwrap();
        let l = log(&mut b, 30.0, Some(6.0));
        assert_eq!(b.refreshes, 3662);
        assert_eq!(fnv1a(&l), 0x8a9a_ebee_1805_c465);
    }

    /// With the hardware's timing, T2 plays its attract mode (many different pictures).
    #[test]
    fn t2_hardware_reaches_attract() {
        let Some(rom) = rom("t2_l8.zip") else { return };
        let mut b = Board::new(rom, Generation::Dmd, Compat::default()).unwrap();
        let l = log(&mut b, 40.0, Some(20.0));
        let pictures: std::collections::HashSet<&[u8]> = l
            .split(|&c| c == b'\n')
            .filter_map(|line| line.rsplit(|&c| c == b' ').next())
            .filter(|d| d.len() == 1024)
            .collect();
        assert!(pictures.len() > 100, "{} pictures", pictures.len());
    }

    /// A clone is a snapshot: running from it again gives the same output.
    #[test]
    fn snapshot_restore_replays() {
        let Some(rom) = rom("t2_l8.zip") else { return };
        let mut b = Board::new(rom, Generation::Dmd, Compat::default()).unwrap();
        log(&mut b, 8.0, Some(6.0));
        let snap = b.clone();
        let first = log(&mut b, 3.0, None);
        b = snap;
        assert_eq!(log(&mut b, 3.0, None), first);
    }

    #[test]
    fn game_id_and_pic() {
        let Some(rom) = rom("afm_113.zip") else {
            return;
        };
        assert_eq!(find_game_id(&rom), Some((0xe885, 541)));
    }

    #[test]
    fn civil_dates() {
        // 2024-06-01 12:00 UTC, a Saturday
        assert_eq!(bus::civil(1_717_243_200), (2024, 6, 1, 6, 12, 0));
        assert_eq!(bus::civil(0), (1970, 1, 1, 4, 0, 0));
        assert_eq!(bus::civil(951_782_400), (2000, 2, 29, 2, 0, 0));
    }

    #[test]
    fn protect_masks() {
        assert_eq!(bus::protect_mask(0x00), 0x1000);
        assert_eq!(bus::protect_mask(0x1f), 0x2f00);
        assert_eq!(bus::protect_mask(0xff), 0x0f00);
    }
}
