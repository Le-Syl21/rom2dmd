//! WPC DMD scene tracer: plays every display effect ("deff") of a WPC game through the game's
//! own effect starter and logs what the display shows, refresh by refresh.
//!
//! Port of the Node tracer that ran on WPC-EMU (`trace56.js`, v6) and of its 3-pass runner:
//!
//! 1. Boot the game, go through attract and start a game (coins, Start, balls served and
//!    played at random; WPC-95 games start from Start alone), and snapshot the machine at a
//!    natural call of the effect starter (A = effect id).
//! 2. Pass 1, for every effect id: restore the snapshot with A = id, zero the running
//!    effect's priority byte (so the starter accepts the effect and retires the running one)
//!    and the "skip this effect" flag, and run. The effect's task is learnt when the task
//!    creator returns; the game's own effect requests are refused while it plays (MUTE); its
//!    death is the moment the OS frees its task (FREEPC with U = task), with the killer.
//! 3. Pass 2: the effects refused in pass 1, with the starter's refusal checks bypassed for
//!    the first 0.1 s and the effect protected from group kills by other tasks.
//! 4. Pass 3: the effects another task killed, again with the protection.
//!
//! Output, per pass and chunk, in the reference tracer's formats (the Python export and
//! classifier read them unchanged): `<game>_<k>.raw` (`#id`, `=k hex` page contents,
//! `tick page k` per refresh, ticks of 1/2 MHz since injection), `<game>_<k>.log` (`YT died
//! {...}` and `CAP id verdict ...` lines) and `<game>_<k>.effects.ndjson`.
//!
//! Timing: by default the board runs on the hardware's cycle counts and every effect starts
//! from an exact copy of the snapshot, on as many threads as there are cores. `--compat`
//! replays the reference instead: WPC-EMU's cycle counts and CPU flag quirks, its lossy
//! snapshot restore (see `Board::restore_like_wpc_emu`), chunks run in order on 4 threads, a
//! 6 s first boot. With a fixed real-time clock and coin-door keys released 100 emulated ms
//! after a press, the reference tracer then gives the same CAP lines and raw logs byte for
//! byte (its own runs read the wall clock, so their snapshots drift a little from run to run).
//!
//! Never jump into the task creator to start an effect: the display manager then never gives
//! the effect the screen and other tasks draw over it (truncated frames).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use mc6809_core::ConditionCodes;

use super::dmd::PAGE_SIZE;
use super::games::{self, Action, Game};
use super::probe::{self, Probe};
use super::{Board, Compat, Generation, Refresh};

/// Ticks per second of the display time base (CPU cycles).
const TPS: u64 = 2_000_000;
/// Ticks per DMD scanline.
const LINE_TICKS: u64 = 512;
/// The reference runs the machine in slices of at least this many cycles and checks its stop
/// conditions between slices.
const SLICE: u64 = 100_000;
/// Offset of the entry point in a task structure.
const TENT: u16 = 0x22;
/// RAM bytes printed by the reference's play report (game running, ball, score).
const RUNADDR: usize = 0x80;
/// Words that keep a switch out of the random play (flippers, start, tilt, coin door, ball
/// trough...): the reference tracer's list, matched as substrings of the names.
const PLAY_SKIP: &[&str] = &[
    "FLIPPER", "START", "TILT", "SLAM", "COIN", "ADJUST", "HI_SCORE", "CHAMP", "TICKED", "TROUGH",
    "SHOOTER", "OUTHOLE", "ENTER", "ESCAPE", "BACK", "UP", "DOWN", "BEGIN",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Attract,
    Play,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Attract => "attract",
            Phase::Play => "play",
        }
    }
}

/// Everything a scan needs, resolved from the probe, the game database and the options.
#[derive(Clone, Debug)]
pub struct Cfg {
    pub game: String,
    pub starter: u16,
    pub create: u16,
    pub freepc: u16,
    pub protect: u16,
    pub bypass: Option<[u16; 2]>,
    pub tbase: u16,
    pub toff: usize,
    pub nids: usize,
    pub curtask: usize,
    pub prio: Option<u16>,
    pub zero: Vec<u16>,
    pub compat: bool,
    /// WPC-95 start: Start alone (else coins, Start and balls played).
    pub wpc95_flow: bool,
    /// Seconds of the serve/play/drain loop after the start.
    pub secs: f64,
    pub seed: u32,
    /// Seconds from power-on to the reset that ends the first boot.
    pub reset_s: f64,
    pub snapn: u32,
    pub snapidle: bool,
    pub snapearly: bool,
    pub attract_s: u32,
    pub phase: Option<Phase>,
    pub starts: u32,
    pub closed_extra: Vec<u8>,
    pub closed: Vec<u8>,
    pub actions: Vec<(u32, Action)>,
    pub trough: Vec<u8>,
    pub shooter: Vec<u8>,
    pub outhole: Vec<u8>,
    pub play: Vec<u8>,
    /// Coin-door keys read as released this many cycles after being set: a short press, as
    /// WPC-EMU made them (it releases them 100 ms of wall-clock time after they are set; a
    /// key held for the whole second the tracer waits keeps some games from starting).
    pub cab_release: Option<u64>,
}

/// JavaScript's `Math.round`.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// JavaScript's `Number.prototype.toFixed` (ties away from zero, which `{:.p}` does not do).
fn fixed(x: f64, p: usize) -> String {
    let m = 10f64.powi(p as i32);
    let s = x * m;
    if (s - s.trunc()).abs() == 0.5 {
        let r = (s.trunc() + s.signum()) / m;
        return format!("{r:.p$}");
    }
    format!("{x:.p$}")
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for v in b {
        let _ = write!(s, "{v:02x}");
    }
    s
}

fn popcount(b: &[u8]) -> u32 {
    b.iter().map(|v| v.count_ones()).sum()
}

/// Share of the lit pixels of `a` also lit in `b`.
fn share(a: &[u8], b: &[u8]) -> f64 {
    let n = popcount(a);
    if n == 0 {
        return 0.0;
    }
    let both: u32 = a.iter().zip(b).map(|(x, y)| (x & y).count_ones()).sum();
    both as f64 / n as f64
}

/// A displayed picture: the (high, low) page pair of a shaded frame, level = 2*high + low
/// (a mono frame has the same page twice).
type Pic = [u8; 2 * PAGE_SIZE];

fn pic(hi: &[u8], lo: &[u8]) -> Pic {
    let mut p = [0u8; 2 * PAGE_SIZE];
    p[..PAGE_SIZE].copy_from_slice(hi);
    p[PAGE_SIZE..].copy_from_slice(lo);
    p
}

/// The injected effect, as the tracer follows it.
struct Yt {
    entry: u16,
    /// Its task structure, -1 until known.
    e: i32,
    injected: bool,
    create_ret: Option<u16>,
    last_u: i32,
    died: Option<u64>,
    killer: Option<String>,
    killer_other: bool,
    last_resume: Option<String>,
    armed: bool,
    writes: u64,
    nz: u64,
    chains: Vec<(String, i64, &'static str)>,
    seen_u: Vec<(u16, u16)>,
}

/// Frame capture state of one effect run.
struct Cap {
    t0: u64,
    evs: Vec<(u64, Box<Pic>)>,
    evl: Vec<Option<Box<Pic>>>,
    win: VecDeque<(u64, u8, [u8; PAGE_SIZE])>,
    hist: VecDeque<u8>,
    mono_run: u32,
    last_ref: Option<Box<Pic>>,
    pat: (u64, u64, u64),
    raw: std::io::BufWriter<std::fs::File>,
    raw_idx: HashMap<[u8; PAGE_SIZE], usize>,
    raw_err: Option<std::io::Error>,
}

/// Options of one pass.
#[derive(Clone, Copy, Debug)]
pub struct PassOpts {
    pub caps: u32,
    pub bypass: bool,
    pub protect: bool,
}

/// What one effect run produced.
pub struct IdResult {
    pub id: u32,
    pub kind: &'static str,
    pub killer_other: bool,
    pub log: String,
    pub nd: String,
    pub raw_part: PathBuf,
}

/// The board with the tracer's instrumentation around every instruction.
pub struct Machine<'c> {
    pub b: Board,
    c: &'c Cfg,
    pub log: String,
    go: bool,
    phase: Phase,
    req_n: u32,
    pub snaps: [Option<Board>; 2],
    yt: Option<Yt>,
    protect_on: bool,
    byp: bool,
    byp_hits: u64,
    prot_hits: u64,
    muted: u64,
    cap: Option<Cap>,
    last_w: u64,
    t0: u64,
    bootlog: bool,
    nrun: u64,
    dbg: Debug,
}

/// Debugging aids, from the environment (read once):
/// - `ROM2DMD_WPC_BOOTLOG=1|dump`: RAM digest (or contents) after every boot run;
/// - `ROM2DMD_WATCH=hex`: log every change of that RAM byte (cycle, PC, values, bank);
/// - `ROM2DMD_TRACE=from,count`: CPU state before each instruction in that cycle window.
#[derive(Clone, Copy, Default)]
struct Debug {
    watch: Option<usize>,
    trace: Option<(u64, u64)>,
}

impl Debug {
    fn from_env() -> Self {
        let watch = std::env::var("ROM2DMD_WATCH")
            .ok()
            .and_then(|v| usize::from_str_radix(&v, 16).ok());
        let trace = std::env::var("ROM2DMD_TRACE").ok().and_then(|v| {
            let (a, b) = v.split_once(',')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        });
        Debug { watch, trace }
    }
}

impl<'c> Machine<'c> {
    pub fn new(b: Board, c: &'c Cfg) -> Self {
        Machine {
            b,
            c,
            log: String::new(),
            go: false,
            phase: Phase::Attract,
            req_n: 0,
            snaps: [None, None],
            yt: None,
            protect_on: false,
            byp: false,
            byp_hits: 0,
            prot_hits: 0,
            muted: 0,
            cap: None,
            last_w: 0,
            t0: 0,
            bootlog: std::env::var_os("ROM2DMD_WPC_BOOTLOG").is_some(),
            nrun: 0,
            dbg: Debug::from_env(),
        }
    }

    fn now(&self) -> u64 {
        self.b.bus.dmd.lines * LINE_TICKS
    }

    fn ram(&self, a: u32) -> u8 {
        self.b.bus.ram[(a & 0x3fff) as usize]
    }

    fn ram_word(&self, a: u32) -> u16 {
        ((self.ram(a) as u16) << 8) | self.ram(a.wrapping_add(1)) as u16
    }

    fn curtask(&self) -> i32 {
        self.ram_word(self.c.curtask as u32) as i32
    }

    /// Return from the routine about to run: PC = the word at S, S += 2, then `f` on the
    /// registers.
    fn force_return(&mut self, f: impl FnOnce(&mut mc6809_core::Registers)) {
        let s = self.b.cpu.registers().s;
        let ret = self.ram_word(s as u32);
        let mut g = self.b.cpu.registers_mut();
        g.pc = ret;
        g.s = s.wrapping_add(2);
        f(&mut g);
    }

    /// The tracer's per-instruction hook, before the instruction (or interrupt entry) runs.
    fn hook(&mut self) {
        let c = self.c;
        let r = *self.b.cpu.registers();
        let pc = r.pc;
        // PROTECT: the routine that unlinks one task of a group kill (X = the task) skips the
        // effect's task when another task kills its group
        if self.protect_on
            && let Some(y) = &self.yt
            && y.e >= 0
            && pc == c.protect
            && r.x as i32 == y.e
            && self.curtask() != y.e
        {
            self.force_return(|g| g.u = g.x);
            self.prot_hits += 1;
            return;
        }
        // BYPASS: right after the injection, the starter's refusal checks return 'allowed'
        if self.byp
            && let Some(b) = c.bypass
            && b.contains(&pc)
        {
            self.force_return(|g| g.cc = ConditionCodes::from_byte(g.cc.to_byte() & 0xfe));
            self.byp_hits += 1;
            return;
        }
        // MUTE: while the effect plays, effect requests from any other task are refused; the
        // first starter call after the injection is the injection itself
        if pc == c.starter && self.yt.is_some() {
            let cur = self.curtask();
            if let Some(y) = self.yt.as_mut() {
                if !y.injected {
                    y.injected = true;
                } else if cur != y.e || y.e < 0 {
                    self.force_return(|g| g.cc = ConditionCodes::from_byte(g.cc.to_byte() | 1));
                    self.muted += 1;
                    return;
                }
            }
        }
        // the effect's task is known as soon as the creator returns (U = the new task)
        if self.yt.as_ref().is_some_and(|y| y.e < 0) {
            let s_word = self.ram_word(r.s as u32);
            let ent_u = self.ram_word(r.u as u32 + TENT as u32);
            if let Some(y) = self.yt.as_mut() {
                if pc == c.create && y.create_ret.is_none() {
                    y.create_ret = Some(s_word);
                } else if Some(pc) == y.create_ret {
                    if ent_u == y.entry {
                        y.e = r.u as i32;
                    }
                    y.create_ret = None;
                }
            }
        }
        // snapshot at a natural starter call (the n-th; SNAPIDLE: only while no effect runs)
        let pi = self.phase as usize;
        if pc == c.starter && self.go && self.snaps[pi].is_none() {
            self.req_n += 1;
            let idle = c.prio.is_none_or(|p| self.ram(p as u32) == 0);
            if self.req_n >= c.snapn && (!c.snapidle || idle) {
                self.snaps[pi] = Some(self.b.clone());
                let prio = c
                    .prio
                    .map(|p| self.ram(p as u32).to_string())
                    .unwrap_or_else(|| "-".into());
                let _ = writeln!(
                    self.log,
                    "SNAP {} A= {} prio {prio}",
                    self.phase.name(),
                    r.a()
                );
            }
        }
        if self.yt.as_ref().is_some_and(|y| y.died.is_none()) {
            self.follow(&r);
        }
    }

    /// Follow the effect's task: its death (and killer), its resumes, the effects it starts.
    fn follow(&mut self, r: &mc6809_core::Registers) {
        let c = self.c;
        let pc = r.pc;
        let u = self.curtask();
        let now = self.now();
        let bank = self.b.bus.rom_bank;
        let Some(e) = self.yt.as_ref().map(|y| y.e) else {
            return;
        };
        // death: the OS pushes the effect's task onto the free list (U = the task)
        if e >= 0 && pc == c.freepc && r.u as i32 == e {
            let sp = r.s as u32;
            let ret = self.ram_word(sp + 3);
            let ret2 = self.ram_word(sp + 5);
            let kentry = self.ram_word(u as u32 + TENT as u32);
            let stack: Vec<String> = (0..24)
                .map(|k| format!("{:02x}", self.ram(sp + k)))
                .collect();
            if let Some(y) = self.yt.as_mut() {
                y.died = Some(now);
                let by = if u == e {
                    "self".to_string()
                } else {
                    format!("{u:x}")
                };
                y.killer_other = u != e;
                let mut j = format!(
                    "{{\"by\":\"{by}\",\"ret\":\"{ret:x}\",\"ret2\":\"{ret2:x}\",\"bank\":\"{bank:x}\""
                );
                if let Some(rs) = &y.last_resume {
                    let _ = write!(j, ",\"resume\":\"{rs}\"");
                }
                j.push('}');
                let _ = writeln!(
                    self.log,
                    "YT died {j} killerEntry {kentry:x} stack {}",
                    stack.join(" ")
                );
                y.killer = Some(j);
            }
        }
        let ent_u = self.ram_word(u as u32 + TENT as u32);
        let t0 = self.t0;
        let Some(y) = self.yt.as_mut() else {
            return;
        };
        if y.e < 0 && u != y.last_u {
            if y.seen_u.len() < 64 && !y.seen_u.iter().any(|s| s.0 as i32 == u) {
                y.seen_u.push((u as u16, ent_u));
            }
            if ent_u == y.entry {
                y.e = u;
            }
        }
        // where the effect's own code resumes after each task switch (for the killer report)
        if u == y.e && y.last_u != y.e && y.e >= 0 {
            y.armed = true;
        }
        if y.armed && u == y.e && (0x4000..0x8000).contains(&pc) {
            y.armed = false;
            y.last_resume = Some(format!("{bank:x}:{pc:x}"));
        }
        // effects this effect starts itself (starter with A = id, creator with X = record)
        if y.e >= 0 && u == y.e && (pc == c.starter || pc == c.create) {
            let cid = if pc == c.starter {
                r.a() as i64
            } else {
                js_round((r.x as f64 - c.tbase as f64) / 5.0) as i64
            };
            let tt = fixed((now - t0) as f64 / TPS as f64, 2);
            if !y.chains.iter().any(|ch| ch.1 == cid) {
                y.chains
                    .push((tt, cid, if pc == c.starter { "starter" } else { "create" }));
            }
        }
        y.last_u = u;
    }

    /// One instruction with the instrumentation; returns its cycles.
    fn step(&mut self) -> u64 {
        self.hook();
        let watch = self.dbg.watch;
        let wv = watch.map(|a| self.b.bus.ram[a & 0x3fff]);
        let wpc = self.b.cpu.registers().pc;
        if let Some((f, n)) = self.dbg.trace
            && self.b.cycles >= f
            && self.b.cycles < f + n
        {
            let g = self.b.cpu.registers();
            eprintln!(
                "T {} {:x} {:02x} {:02x} {:04x} {:04x} {:02x}",
                self.b.cycles,
                g.pc,
                g.a(),
                g.b(),
                g.x,
                g.u,
                g.cc.to_byte()
            );
        }
        let before = self.now();
        let mut refreshed = None;
        let t = self.b.step(&mut |r: &Refresh| refreshed = Some(r.page));
        if let (Some(a), Some(v)) = (watch, wv)
            && self.b.bus.ram[a & 0x3fff] != v
        {
            eprintln!(
                "W {} {wpc:x} {v} -> {} bank {:x}",
                self.b.cycles,
                self.b.bus.ram[a & 0x3fff],
                self.b.bus.rom_bank
            );
        }
        if !self.b.bus.dmd.writes.is_empty() {
            let w = std::mem::take(&mut self.b.bus.dmd.writes);
            let cur = self.curtask();
            for &(_, _, v) in &w {
                self.last_w = before;
                if let Some(y) = self.yt.as_mut()
                    && y.died.is_none()
                    && y.e >= 0
                    && cur == y.e
                {
                    y.writes += 1;
                    if v != 0 {
                        y.nz += 1;
                    }
                }
            }
            self.b.bus.dmd.writes = w;
            self.b.bus.dmd.writes.clear();
        }
        if let Some(p) = refreshed
            && self.cap.is_some()
        {
            self.on_refresh(p);
        }
        t
    }

    /// A complete refresh while capturing: raw log, then the frame builder (a shaded frame is
    /// a (hi, hi, lo) page cycle; a page shown 3 times running is a mono frame).
    fn on_refresh(&mut self, was: u8) {
        let now = self.now();
        let vram = &self.b.bus.dmd.vram;
        let o = was as usize * PAGE_SIZE;
        let mut buf = [0u8; PAGE_SIZE];
        buf.copy_from_slice(&vram[o..o + PAGE_SIZE]);
        let p14: [u8; PAGE_SIZE] = vram[14 * PAGE_SIZE..15 * PAGE_SIZE]
            .try_into()
            .unwrap_or([0; PAGE_SIZE]);
        let p15: [u8; PAGE_SIZE] = vram[15 * PAGE_SIZE..16 * PAGE_SIZE]
            .try_into()
            .unwrap_or([0; PAGE_SIZE]);
        let Some(cap) = self.cap.as_mut() else {
            return;
        };
        if cap.raw_err.is_none() {
            let n = cap.raw_idx.len();
            let k = *cap.raw_idx.entry(buf).or_insert(n);
            let mut res = Ok(());
            if k == n {
                res = writeln!(cap.raw, "={k} {}", hex(&buf));
            }
            if res.is_ok() {
                res = writeln!(cap.raw, "{} {was} {k}", now - cap.t0);
            }
            if let Err(e) = res {
                cap.raw_err = Some(e);
            }
        }
        cap.win.push_back((now, was, buf));
        if cap.win.len() > 3 {
            cap.win.pop_front();
        }
        cap.pat.0 += 1;
        cap.hist.push_back(was);
        if cap.hist.len() > 6 {
            cap.hist.pop_front();
        }
        let flips = cap
            .hist
            .iter()
            .zip(cap.hist.iter().skip(1))
            .filter(|(a, b)| a != b)
            .count();
        if flips >= 3
            && cap.win.len() == 3
            && cap.win[0].1 == cap.win[1].1
            && cap.win[2].1 != cap.win[0].1
            && cap.win[0].2 == cap.win[1].2
        {
            let (t, hi, lo) = (cap.win[2].0, cap.win[0].2, cap.win[2].2);
            emit(cap, t, &hi, &lo, &p14, &p15);
            cap.mono_run = 0;
            cap.pat.1 += 1;
            return;
        }
        if cap.win.len() >= 2 && cap.win[cap.win.len() - 2].1 == was {
            cap.mono_run += 1;
            if cap.mono_run >= 3 {
                emit(cap, now, &buf, &buf, &p14, &p15);
                cap.pat.2 += 1;
            }
        } else {
            cap.mono_run = 0;
        }
    }

    /// Run one slice (at least `SLICE` cycles).
    fn slice(&mut self) {
        let mut done = 0;
        while done < SLICE {
            done += self.step();
        }
    }

    /// Run `s` seconds as the reference does (`Math.round(s * 20)` slices).
    fn run(&mut self, s: f64) {
        for _ in 0..js_round(s * 20.0) as u64 {
            self.slice();
        }
        if self.bootlog {
            // ROM2DMD_WPC_BOOTLOG: RAM digest after every run, to compare a boot with the
            // reference tracer's (FNV-1a over 0x0000-0x2fff)
            self.nrun += 1;
            let h = self.b.bus.ram[..0x3000]
                .iter()
                .fold(0x811c_9dc5u32, |h, &v| {
                    (h ^ v as u32).wrapping_mul(0x0100_0193)
                });
            eprintln!(
                "R {} {} {:x} {h:x}",
                self.nrun,
                self.b.cycles,
                self.b.cpu.registers().pc
            );
            if std::env::var("ROM2DMD_WPC_BOOTLOG").is_ok_and(|v| v == "dump") {
                eprintln!("RAM {}", hex(&self.b.bus.ram[..0x3000]));
            }
        }
    }

    fn set(&mut self, ids: &[u8], v: bool) {
        for &i in ids {
            self.b.set_switch(i, v);
        }
    }

    fn cabinet(&mut self, v: u8) {
        self.b.set_cabinet(v);
        self.b.bus.cabinet_release_at = self.c.cab_release.map(|d| self.b.cycles + d);
    }

    /// Power-on to the snapshots: attract, coins, start, served and played balls.
    pub fn boot(&mut self) {
        let c = self.c;
        self.run(c.reset_s);
        self.b.reset();
        if c.compat {
            // the reference resets twice (board reset, then emulator start)
            self.b.reset();
        }
        self.run(10.0);
        for &s in &c.closed {
            self.b.set_switch(s, true);
        }
        for (delay, a) in &c.actions {
            self.run(*delay as f64 / 1000.0);
            match *a {
                Action::Cabinet(v) => {
                    self.cabinet(v);
                    self.run(0.5);
                    self.cabinet(0);
                }
                Action::Memory(o, v) => self.b.bus.ram[o as usize & 0x3fff] = v,
                Action::Switch(_) => {}
            }
        }
        self.go = true;
        let mut k = 0;
        while k < c.attract_s && self.snaps[0].is_none() {
            self.run(1.0);
            k += 1;
        }
        self.go = false;
        let _ = writeln!(self.log, "attract done game {}", self.ram(RUNADDR as u32));
        let closed_extra = c.closed_extra.clone();
        if c.wpc95_flow {
            self.set(&closed_extra, true);
            self.run(3.0);
            for _ in 0..3 {
                self.cabinet(1);
                self.run(0.6);
                self.cabinet(0);
                self.run(0.6);
            }
            for _ in 0..c.starts {
                self.b.set_switch(13, true);
                self.run(0.4);
                self.b.set_switch(13, false);
                self.run(4.0);
            }
            self.phase = Phase::Play;
            self.go = true;
            let mut k = 0;
            while k < 90 && self.snaps[1].is_none() {
                self.run(1.0);
                k += 1;
            }
            self.go = false;
        } else {
            self.set(&closed_extra, true);
            self.run(1.0);
            if c.snapearly {
                self.phase = Phase::Play;
                self.go = true;
            }
            for _ in 0..3 {
                self.cabinet(1);
                self.run(1.0);
                self.cabinet(0);
                self.run(1.0);
            }
            self.b.set_switch(13, true);
            self.run(0.5);
            self.b.set_switch(13, false);
            self.run(2.0);
        }
        let mut seed = c.seed;
        let mut rnd = || {
            // the reference's LCG in JavaScript doubles (the product loses its low bits)
            let x = seed as f64 * 1103515245.0 + 12345.0;
            seed = (x as u64 & 0x7fff_ffff) as u32;
            seed as f64 / 0x7fff_ffff as f64
        };
        let trough1: Vec<u8> = c.trough.iter().take(1).copied().collect();
        let mut t = 0.0f64;
        while t < c.secs {
            // serve: ball in the shooter lane
            self.set(&trough1, false);
            self.set(&c.shooter, true);
            self.run(2.0);
            self.set(&c.shooter, false);
            self.run(1.0);
            t += 3.0;
            // play: random playfield switches, a flipper tap every 5
            self.go = t > 20.0;
            self.phase = Phase::Play;
            let mut k = 0;
            while k < 60 && t < c.secs {
                let r = rnd();
                let id = c
                    .play
                    .get((r * c.play.len() as f64).floor() as usize)
                    .copied();
                if let Some(id) = id {
                    self.b.set_switch(id, true);
                }
                self.run(0.15);
                if let Some(id) = id {
                    self.b.set_switch(id, false);
                }
                self.run(0.25);
                t += 0.4;
                if k % 5 == 0 {
                    self.cabinet(0);
                    self.b.set_switch(11, true);
                    self.run(0.1);
                    self.b.set_switch(11, false);
                    t += 0.1;
                }
                k += 1;
            }
            let score = hex(&self.b.bus.ram[0x172f..0x1735]);
            let _ = writeln!(
                self.log,
                "t {} run {} ball {} score {score}",
                fixed(t, 0),
                self.ram(RUNADDR as u32),
                self.ram(0x410)
            );
            // drain: outhole, then back in the trough
            self.set(&c.outhole, true);
            self.run(1.5);
            self.set(&c.outhole, false);
            self.set(&trough1, true);
            self.run(6.0);
            t += 7.5;
        }
    }

    fn entry(&self, id: u32) -> u16 {
        let o = self.c.toff + id as usize * 5 + 2;
        let r = &self.b.bus.rom;
        match (r.get(o), r.get(o + 1)) {
            (Some(&h), Some(&l)) => ((h as u16) << 8) | l as u16,
            _ => 0,
        }
    }

    /// Play effect `id` from `snap` and capture it. In `--compat` mode the snapshot is loaded
    /// over the current machine as WPC-EMU does (see `Board::restore_like_wpc_emu`).
    pub fn capture(
        &mut self,
        id: u32,
        snap: &Board,
        o: PassOpts,
        raw_part: &Path,
    ) -> Result<IdResult, String> {
        let c = self.c;
        let f =
            std::fs::File::create(raw_part).map_err(|e| format!("{}: {e}", raw_part.display()))?;
        let mut raw = std::io::BufWriter::new(f);
        writeln!(raw, "#{id}").map_err(|e| format!("{}: {e}", raw_part.display()))?;
        if c.compat {
            self.b.restore_like_wpc_emu(snap);
        } else {
            self.b = snap.clone();
        }
        self.b.cpu.registers_mut().set_a(id as u8);
        if let Some(p) = c.prio {
            self.b.bus.ram[p as usize & 0x3fff] = 0;
        }
        for &z in &c.zero {
            self.b.bus.ram[z as usize & 0x3fff] = 0;
        }
        self.b.bus.dmd.vram.fill(0);
        self.b.bus.dmd.history = [[0; PAGE_SIZE]; 3];
        self.b.bus.dmd.log_writes = true;
        self.b.bus.dmd.writes.clear();
        self.log.clear();
        let t0 = self.now();
        self.t0 = t0;
        self.last_w = t0;
        self.byp = o.bypass && c.bypass.is_some();
        self.protect_on = o.protect;
        self.byp_hits = 0;
        self.prot_hits = 0;
        self.muted = 0;
        self.go = false;
        let entry = self.entry(id);
        self.yt = Some(Yt {
            entry,
            e: -1,
            injected: false,
            create_ret: None,
            last_u: -1,
            died: None,
            killer: None,
            killer_other: false,
            last_resume: None,
            armed: false,
            writes: 0,
            nz: 0,
            chains: Vec::new(),
            seen_u: Vec::new(),
        });
        self.cap = Some(Cap {
            t0,
            evs: Vec::new(),
            evl: Vec::new(),
            win: VecDeque::new(),
            hist: VecDeque::new(),
            mono_run: 0,
            last_ref: None,
            pat: (0, 0, 0),
            raw,
            raw_idx: HashMap::new(),
            raw_err: None,
        });
        let mut kind = "cap";
        for _ in 0..o.caps as u64 * 20 {
            self.slice();
            let now = self.now();
            if self.byp && now - t0 > TPS / 10 {
                self.byp = false;
                if self.byp_hits > 0 {
                    let _ = writeln!(self.log, "BYPASS hits {}", self.byp_hits);
                }
            }
            let Some(y) = self.yt.as_ref() else { break };
            if let Some(d) = y.died
                && now - d > TPS * 3 / 10
            {
                kind = "finite";
                break;
            }
            if y.e < 0 && now - t0 > 2 * TPS {
                kind = "refused";
                break;
            }
        }
        self.b.bus.dmd.log_writes = false;
        let cap = self.cap.take().ok_or("capture state lost")?;
        let y = self.yt.take().ok_or("effect state lost")?;
        let Cap {
            evs,
            evl,
            pat,
            mut raw,
            raw_err,
            ..
        } = cap;
        if let Some(e) = raw_err {
            return Err(format!("{}: {e}", raw_part.display()));
        }
        raw.flush()
            .map_err(|e| format!("{}: {e}", raw_part.display()))?;
        // every frame is "novel" (no background reference): first/last new picture
        let mut seen: HashSet<&Pic> = HashSet::new();
        let (mut first_novel, mut last_novel) = (None, None);
        for (t, p) in &evs {
            if seen.insert(p) {
                first_novel.get_or_insert(*t);
                last_novel = Some(*t);
            }
        }
        let from = first_novel.unwrap_or(t0);
        let lim = match kind {
            "cap" => u64::MAX,
            "finite" => self.last_w + TPS * 6 / 10,
            _ => last_novel.map_or(TPS - 1, |l| l + TPS),
        };
        let (mut frames, mut layers) = (0usize, 0usize);
        let (mut last_f, mut last_l): (Option<&Pic>, Option<&Pic>) = (None, None);
        for (k, (t, p)) in evs.iter().enumerate() {
            if *t < from {
                continue;
            }
            if *t > lim {
                break;
            }
            if last_f != Some(p) {
                frames += 1;
                last_f = Some(p);
            }
            if let Some(Some(l)) = evl.get(k)
                && last_l != Some(l)
            {
                layers += 1;
                last_l = Some(l);
            }
        }
        let secs = |t: Option<u64>| t.map_or(-1.0, |t| (t - t0) as f64 / TPS as f64);
        let mut log = std::mem::take(&mut self.log);
        let chains: Vec<String> = y
            .chains
            .iter()
            .map(|(tt, cid, how)| format!("[\"{tt}\",{cid},\"{how}\"]"))
            .collect();
        let chains = format!("[{}]", chains.join(","));
        let alive = y.e >= 0 && y.died.is_none();
        let _ = writeln!(log, "YT chains {chains}");
        let _ = writeln!(
            log,
            "YT alive {alive} diedAt {} ownWrites {} nonzero {}",
            y.died
                .map_or("-".into(), |d| fixed((d - t0) as f64 / TPS as f64, 3)),
            y.writes,
            y.nz
        );
        let tasks: Vec<String> = y
            .seen_u
            .iter()
            .map(|(u, e)| format!("\"{u:x}>{e:x}\""))
            .collect();
        let e_hex = if y.e < 0 {
            "-1".to_string()
        } else {
            format!("{:x}", y.e)
        };
        let _ = writeln!(
            log,
            "YT id {id} entry {:x} E {e_hex} tasks [{}]",
            y.entry,
            tasks.join(",")
        );
        let _ = writeln!(
            log,
            "CAP {id} {kind} frames {frames} layer {layers} pat {{\"n\":{},\"shaded\":{},\"mono\":{}}} novel-distinct {} raw {} novel span {}-{}s ",
            pat.0,
            pat.1,
            pat.2,
            seen.len(),
            evs.len(),
            fixed(secs(first_novel), 1),
            fixed(secs(last_novel), 1)
        );
        if self.prot_hits > 0 {
            let _ = writeln!(log, "PROTECT hits {}", self.prot_hits);
        }
        if self.muted > 0 {
            let _ = writeln!(log, "MUTED game requests {}", self.muted);
        }
        let nd = format!(
            "{{\"id\":{id},\"kind\":\"{kind}\",\"firstNovelS\":{},\"lastNovelS\":{},\"unique\":{},\"rawEvents\":{},\"died\":{},\"created\":{},\"ownWrites\":{},\"ownNonzero\":{},\"chains\":{chains},\"killer\":{}}}\n",
            secs(first_novel),
            secs(last_novel),
            seen.len(),
            evs.len(),
            y.died.map_or("null".into(), |d| (d - t0).to_string()),
            y.e >= 0,
            y.writes,
            y.nz,
            y.killer.clone().unwrap_or_else(|| "null".into())
        );
        Ok(IdResult {
            id,
            kind,
            killer_other: kind == "finite" && y.killer_other,
            log,
            nd,
            raw_part: raw_part.to_path_buf(),
        })
    }
}

/// Add a frame to the capture (consecutive duplicates dropped), with the off-screen layer
/// (pages 14/15, the scene the effect composes into) when those pages hold anything.
fn emit(cap: &mut Cap, t: u64, hi: &[u8], lo: &[u8], p14: &[u8], p15: &[u8]) {
    let f = Box::new(pic(hi, lo));
    if cap.last_ref.as_deref() == Some(&*f) {
        return;
    }
    cap.last_ref = Some(f.clone());
    cap.evs.push((t, f));
    if popcount(p14) + popcount(p15) > 0 {
        let l = if share(p14, hi) >= share(p15, hi) {
            pic(p14, p15)
        } else {
            pic(p15, p14)
        };
        cap.evl.push(Some(Box::new(l)));
    } else {
        cap.evl.push(None);
    }
}

/// Split ids in up to 4 contiguous chunks (the reference's pass 1).
fn contiguous(ids: &[u32]) -> Vec<Vec<u32>> {
    if ids.is_empty() {
        return Vec::new();
    }
    let q = ids.len().div_ceil(4);
    ids.chunks(q).map(<[u32]>::to_vec).collect()
}

/// Deal ids round-robin in up to 4 chunks (the reference's passes 2 and 3).
fn dealt(ids: &[u32]) -> Vec<Vec<u32>> {
    let mut g: Vec<Vec<u32>> = vec![Vec::new(); 4];
    for (i, &id) in ids.iter().enumerate() {
        g[i % 4].push(id);
    }
    g.retain(|v| !v.is_empty());
    g
}

/// A capture's result, once its thread has delivered it.
type Slot = Option<Result<IdResult, String>>;

/// Run one pass: `groups` become the output chunks `<game>_<tag><k>`. Without `--compat`
/// every effect starts from an exact copy of the snapshot and runs on any of `threads`
/// threads; with it, each chunk runs in order on one thread, each effect loaded over the
/// previous one as WPC-EMU does (the first over `carrier`).
#[allow(clippy::too_many_arguments)]
fn run_pass(
    c: &Cfg,
    snap: &Board,
    carrier: &Board,
    groups: &[Vec<u32>],
    o: PassOpts,
    threads: usize,
    parts: &Path,
    tag: &str,
) -> Result<Vec<Vec<IdResult>>, String> {
    let total: usize = groups.iter().map(Vec::len).sum();
    let slots: Mutex<Vec<Vec<Slot>>> = Mutex::new(
        groups
            .iter()
            .map(|g| (0..g.len()).map(|_| None).collect())
            .collect(),
    );
    let done = AtomicUsize::new(0);
    let report = |gi: usize, pi: usize, r: Result<IdResult, String>| {
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        match &r {
            Ok(x) => eprintln!(
                "{} pass {tag}: id {} {} ({n}/{total})",
                c.game, x.id, x.kind
            ),
            Err(e) => eprintln!("{} pass {tag}: error {e}", c.game),
        }
        if let Ok(mut s) = slots.lock() {
            s[gi][pi] = Some(r);
        }
    };
    let part = |id: u32| parts.join(format!("{tag}_{id}.raw"));
    let work: Vec<(usize, usize, u32)> = groups
        .iter()
        .enumerate()
        .flat_map(|(gi, g)| g.iter().enumerate().map(move |(pi, &id)| (gi, pi, id)))
        .collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        if c.compat {
            for (gi, g) in groups.iter().enumerate() {
                let report = &report;
                sc.spawn(move || {
                    let mut m = Machine::new(carrier.clone(), c);
                    for (pi, &id) in g.iter().enumerate() {
                        report(gi, pi, m.capture(id, snap, o, &part(id)));
                    }
                });
            }
        } else {
            for _ in 0..threads.max(1).min(work.len().max(1)) {
                let (report, work, next) = (&report, &work, &next);
                sc.spawn(move || {
                    let mut m = Machine::new(snap.clone(), c);
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(gi, pi, id)) = work.get(i) else {
                            break;
                        };
                        report(gi, pi, m.capture(id, snap, o, &part(id)));
                    }
                });
            }
        }
    });
    let slots = slots
        .into_inner()
        .map_err(|_| "a capture thread panicked")?;
    let mut out = Vec::new();
    for g in slots {
        let mut v = Vec::new();
        for r in g {
            v.push(r.ok_or("a capture did not finish")??);
        }
        out.push(v);
    }
    Ok(out)
}

/// Write the chunk files of a pass: `<game>_<tag><k>.{log,raw,effects.ndjson}`.
fn write_chunks(
    outdir: &Path,
    game: &str,
    tag: &str,
    header: &str,
    res: &[Vec<IdResult>],
) -> Result<(), String> {
    let err = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    for (k, g) in res.iter().enumerate() {
        let base = outdir.join(format!("{game}_{tag}{}", k + 1));
        let lp = base.with_extension("log");
        let rp = base.with_extension("raw");
        let np = base.with_extension("effects.ndjson");
        let mut log = header.to_string();
        let mut nd = String::new();
        let rf = std::fs::File::create(&rp).map_err(|e| err(&rp, e))?;
        let mut raw = std::io::BufWriter::new(rf);
        for r in g {
            log.push_str(&r.log);
            nd.push_str(&r.nd);
            let mut part = std::fs::File::open(&r.raw_part).map_err(|e| err(&r.raw_part, e))?;
            std::io::copy(&mut part, &mut raw).map_err(|e| err(&rp, e))?;
            std::fs::remove_file(&r.raw_part).map_err(|e| err(&r.raw_part, e))?;
        }
        raw.flush().map_err(|e| err(&rp, e))?;
        std::fs::write(&lp, log).map_err(|e| err(&lp, e))?;
        std::fs::write(&np, nd).map_err(|e| err(&np, e))?;
    }
    Ok(())
}

/// Resolve the game, the probe and the options into a scan configuration.
pub fn configure(
    rom: &[u8],
    game: Option<&Game>,
    name: &str,
    opts: &Opts,
) -> Result<(Cfg, Probe), String> {
    let p = probe::probe(rom);
    let missing = p.missing();
    if !missing.is_empty() {
        return Err(format!(
            "probe incomplete, missing {missing:?}: {}",
            p.line()
        ));
    }
    let set = games::settings(name);
    let generation = opts
        .generation
        .or(game.map(Game::generation))
        .unwrap_or(Generation::Dmd);
    let t2flow = opts.t2flow.unwrap_or(set.t2flow);
    let wpc95_flow = generation == Generation::Wpc95 && !t2flow;
    let playskip: Vec<String> = opts.playskip.clone().unwrap_or(set.playskip);
    let (closed, actions, trough, shooter, outhole, play) = match game {
        Some(g) => {
            let skip2: Vec<&str> = playskip.iter().map(String::as_str).collect();
            let play = g
                .switches
                .iter()
                .filter(|(_, n)| {
                    !games::contains_any(n, PLAY_SKIP) && !games::contains_any(n, &skip2)
                })
                .map(|&(id, _)| id)
                .collect();
            (
                g.closed.clone(),
                g.actions.clone(),
                g.named(&["TROUGH"]),
                g.named(&["SHOOTER"]),
                g.named(&["OUTHOLE"]),
                play,
            )
        }
        None => Default::default(),
    };
    let zero = match &opts.zero {
        Some(z) => z.clone(),
        None if !set.zero.is_empty() => set.zero.clone(),
        None => p.zero.into_iter().collect(),
    };
    let (Some(starter), Some(create), Some(freepc), Some(protect), Some(toff), Some(nids)) =
        (p.starter, p.create, p.freepc, p.protect, p.toff, p.nids)
    else {
        return Err("probe incomplete".into());
    };
    let cfg = Cfg {
        game: name.to_string(),
        starter,
        create,
        freepc,
        protect,
        bypass: p.bypass,
        tbase: p.tbase.unwrap_or(0),
        toff,
        nids,
        curtask: opts.curtask.or(p.curtask).unwrap_or(0x53) as usize,
        prio: p.prio,
        zero,
        compat: opts.compat,
        wpc95_flow,
        secs: opts.secs.unwrap_or(if wpc95_flow { 1.0 } else { 50.0 }),
        seed: opts.seed,
        reset_s: opts.reset_s.unwrap_or(if opts.compat { 6.0 } else { 20.0 }),
        snapn: opts.snapn.unwrap_or(set.snapn),
        snapidle: opts.snapidle,
        snapearly: opts.snapearly.unwrap_or(set.snapearly),
        attract_s: opts.attract_s.or(set.attract_s).unwrap_or(120),
        phase: opts.phase,
        starts: opts.starts,
        closed_extra: opts.closed.clone(),
        closed,
        actions,
        trough,
        shooter,
        outhole,
        play,
        cab_release: (opts.cab_release_ms > 0).then_some(opts.cab_release_ms * TPS / 1000),
    };
    Ok((cfg, p))
}

/// Command-line options of `wpc-scan`.
#[derive(Clone, Debug)]
pub struct Opts {
    pub game: Option<String>,
    pub member: Option<String>,
    pub generation: Option<Generation>,
    pub ids: Option<Vec<u32>>,
    pub caps: u32,
    pub caps3: u32,
    pub passes: Vec<u8>,
    pub compat: bool,
    pub threads: usize,
    pub t2flow: Option<bool>,
    pub playskip: Option<Vec<String>>,
    pub snapn: Option<u32>,
    pub snapidle: bool,
    pub snapearly: Option<bool>,
    pub attract_s: Option<u32>,
    pub zero: Option<Vec<u16>>,
    pub phase: Option<Phase>,
    pub starts: u32,
    pub closed: Vec<u8>,
    pub secs: Option<f64>,
    pub reset_s: Option<f64>,
    pub seed: u32,
    pub curtask: Option<u8>,
    pub cab_release_ms: u64,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            game: None,
            member: None,
            generation: None,
            ids: None,
            caps: 120,
            caps3: 60,
            passes: vec![1, 2, 3],
            compat: false,
            threads: std::thread::available_parallelism().map_or(4, |n| n.get()),
            t2flow: None,
            playskip: None,
            snapn: None,
            snapidle: false,
            snapearly: None,
            attract_s: None,
            zero: None,
            phase: None,
            starts: 1,
            closed: Vec::new(),
            secs: None,
            reset_s: None,
            seed: 3,
            curtask: None,
            cab_release_ms: 100,
        }
    }
}

const USAGE: &str = "usage: rom2dmd wpc-scan <rom.zip|u6.rom> <outdir> [--game name] \
[--member name] [--gen dmd|flip|dcs|wpcs|wpc95] [--ids 0-9,12,...] [--caps secs] [--caps3 secs] \
[--passes 1,2,3] [--compat] [--threads n] [--t2flow|--no-t2flow] [--playskip WORD,...] \
[--snapn n] [--snapidle] [--snapearly] [--attract-s secs] [--zero hex,...|none] \
[--phase attract|play] [--starts n] [--closed n,...] [--secs s] [--reset-s s] [--seed n] \
[--curtask hex] \
[--cab-release-ms ms]";

fn parse_ids(s: &str) -> Option<Vec<u32>> {
    let mut v = Vec::new();
    for p in s.split(',').filter(|x| !x.is_empty()) {
        match p.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
                v.extend(a..=b);
            }
            None => v.push(p.parse().ok()?),
        }
    }
    Some(v)
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.iter();
    let bad = |a: &str| format!("bad value for {a}\n{USAGE}");
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| bad(a));
        match a.as_str() {
            "--game" => o.game = Some(val()?.clone()),
            "--member" => o.member = Some(val()?.clone()),
            "--gen" => {
                let g = val()?;
                o.generation = Some(Generation::parse(g).ok_or_else(|| bad(a))?);
            }
            "--ids" => o.ids = Some(parse_ids(val()?).ok_or_else(|| bad(a))?),
            "--caps" => o.caps = val()?.parse().map_err(|_| bad(a))?,
            "--caps3" => o.caps3 = val()?.parse().map_err(|_| bad(a))?,
            "--passes" => {
                o.passes = val()?
                    .split(',')
                    .map(|x| x.parse().map_err(|_| bad(a)))
                    .collect::<Result<_, _>>()?
            }
            "--compat" => o.compat = true,
            "--threads" => o.threads = val()?.parse().map_err(|_| bad(a))?,
            "--t2flow" => o.t2flow = Some(true),
            "--no-t2flow" => o.t2flow = Some(false),
            "--playskip" => {
                o.playskip = Some(val()?.split(',').map(str::to_string).collect());
            }
            "--snapn" => o.snapn = Some(val()?.parse().map_err(|_| bad(a))?),
            "--snapidle" => o.snapidle = true,
            "--snapearly" => o.snapearly = Some(true),
            "--attract-s" => o.attract_s = Some(val()?.parse().map_err(|_| bad(a))?),
            "--zero" => {
                let v = val()?;
                o.zero = Some(if v == "none" {
                    Vec::new()
                } else {
                    v.split(',')
                        .map(|x| u16::from_str_radix(x, 16).map_err(|_| bad(a)))
                        .collect::<Result<_, _>>()?
                });
            }
            "--phase" => {
                o.phase = Some(match val()?.as_str() {
                    "attract" => Phase::Attract,
                    "play" => Phase::Play,
                    _ => return Err(bad(a)),
                })
            }
            "--starts" => o.starts = val()?.parse().map_err(|_| bad(a))?,
            "--closed" => {
                o.closed = val()?
                    .split(',')
                    .map(|x| x.parse().map_err(|_| bad(a)))
                    .collect::<Result<_, _>>()?
            }
            "--secs" => o.secs = Some(val()?.parse().map_err(|_| bad(a))?),
            "--reset-s" => o.reset_s = Some(val()?.parse().map_err(|_| bad(a))?),
            "--seed" => o.seed = val()?.parse().map_err(|_| bad(a))?,
            "--curtask" => {
                o.curtask = Some(u8::from_str_radix(val()?, 16).map_err(|_| bad(a))?);
            }
            "--cab-release-ms" => o.cab_release_ms = val()?.parse().map_err(|_| bad(a))?,
            _ => return Err(format!("unknown option {a}\n{USAGE}")),
        }
    }
    Ok(o)
}

/// `rom2dmd wpc-scan`: see `USAGE` and the module documentation.
pub fn cli(args: &[String]) -> Result<(), String> {
    if args.len() < 4 {
        return Err(USAGE.into());
    }
    let (rom_path, outdir) = (&args[2], PathBuf::from(&args[3]));
    let o = parse_opts(&args[4..])?;
    let rom = super::load_rom(rom_path, o.member.as_deref())?;
    let stem = Path::new(rom_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let game = match &o.game {
        Some(g) => Some(games::find(g).ok_or_else(|| format!("unknown game {g}"))?),
        None => games::find(&stem),
    };
    let name = game
        .as_ref()
        .map(|g| g.name.clone())
        .or(o.game.clone())
        .unwrap_or(stem);
    if game.is_none() {
        eprintln!("warning: {name} is not in the game database: no switch names, no random play");
    }
    let (cfg, p) = configure(&rom, game.as_ref(), &name, &o)?;
    let generation = o
        .generation
        .or(game.as_ref().map(Game::generation))
        .unwrap_or(Generation::Dmd);
    std::fs::create_dir_all(&outdir).map_err(|e| format!("{}: {e}", outdir.display()))?;
    let parts = outdir.join(format!(".{name}_parts"));
    std::fs::create_dir_all(&parts).map_err(|e| format!("{}: {e}", parts.display()))?;
    let compat = if o.compat {
        Compat::wpc_emu()
    } else {
        Compat::default()
    };
    let mut board = Board::new(rom, generation, compat)?;
    if o.compat {
        // WPC-EMU's tracer never told the emulator the sound board generation: DCS interface
        board.bus.sound.pre_dcs = false;
    }
    let fresh = board.clone();
    let mut header = format!(
        "rom2dmd wpc-scan {name} gen {generation:?}{}\nPROBE {}\nZERO {}\n",
        if o.compat { " compat" } else { "" },
        p.line(),
        cfg.zero
            .iter()
            .map(|z| format!("{z:x}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    eprintln!("{header}");
    let mut m = Machine::new(board, &cfg);
    m.boot();
    header.push_str(&m.log);
    eprint!("{}", m.log);
    let ph = cfg.phase.unwrap_or(if m.snaps[1].is_some() {
        Phase::Play
    } else {
        Phase::Attract
    });
    let snap = m.snaps[ph as usize]
        .take()
        .ok_or_else(|| format!("NO SNAP {} (no effect request seen)", ph.name()))?;
    let after_boot = m.b.clone();
    drop(m);
    let ids: Vec<u32> = o
        .ids
        .clone()
        .unwrap_or_else(|| (0..cfg.nids as u32).collect());
    let mut summary = header.clone();
    let mut p1: Vec<IdResult> = Vec::new();
    let mut p2: Vec<IdResult> = Vec::new();
    if o.passes.contains(&1) {
        let r = run_pass(
            &cfg,
            &snap,
            &after_boot,
            &contiguous(&ids),
            PassOpts {
                caps: o.caps,
                bypass: false,
                protect: false,
            },
            o.threads,
            &parts,
            "",
        )?;
        write_chunks(&outdir, &name, "", &header, &r)?;
        p1 = r.into_iter().flatten().collect();
    }
    let refused: Vec<u32> = p1
        .iter()
        .filter(|r| r.kind == "refused")
        .map(|r| r.id)
        .collect();
    if o.passes.contains(&2) && !refused.is_empty() {
        let r = run_pass(
            &cfg,
            &snap,
            &fresh,
            &dealt(&refused),
            PassOpts {
                caps: o.caps,
                bypass: true,
                protect: true,
            },
            o.threads,
            &parts,
            "byp",
        )?;
        write_chunks(&outdir, &name, "byp", &header, &r)?;
        p2 = r.into_iter().flatten().collect();
    }
    let mut cut: Vec<u32> = p1
        .iter()
        .chain(&p2)
        .filter(|r| r.killer_other)
        .map(|r| r.id)
        .collect();
    cut.sort_unstable();
    cut.dedup();
    let mut p3: Vec<IdResult> = Vec::new();
    if o.passes.contains(&3) && !cut.is_empty() {
        let r = run_pass(
            &cfg,
            &snap,
            &fresh,
            &dealt(&cut),
            PassOpts {
                caps: o.caps3,
                bypass: true,
                protect: true,
            },
            o.threads,
            &parts,
            "prot",
        )?;
        write_chunks(&outdir, &name, "prot", &header, &r)?;
        p3 = r.into_iter().flatten().collect();
    }
    let _ = std::fs::remove_dir(&parts);
    // verdict counts of a pass
    let count = |v: &[IdResult]| {
        let mut k: HashMap<&str, usize> = HashMap::new();
        for r in v {
            *k.entry(r.kind).or_default() += 1;
        }
        let mut k: Vec<_> = k.into_iter().collect();
        k.sort();
        k
    };
    let _ = writeln!(
        summary,
        "pass 1: {} effects {:?}\npass 2 (refused, bypassed): {} {:?} {refused:?}\npass 3 (cut, protected): {} {:?} {cut:?}",
        p1.len(),
        count(&p1),
        p2.len(),
        count(&p2),
        p3.len(),
        count(&p3)
    );
    let sp = outdir.join(format!("{name}.scan.txt"));
    std::fs::write(&sp, &summary).map_err(|e| format!("{}: {e}", sp.display()))?;
    eprint!("{summary}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_number_formatting() {
        assert_eq!(fixed(0.25, 1), "0.3");
        assert_eq!(fixed(-1.0, 1), "-1.0");
        assert_eq!(fixed(28.000000000000004, 0), "28");
        assert_eq!(fixed(1.005, 2), "1.00"); // 1.005 is below the tie in binary, as in JS
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(0.15 * 20.0), 3.0);
    }

    #[test]
    fn id_lists_and_chunks() {
        assert_eq!(parse_ids("0-3,7"), Some(vec![0, 1, 2, 3, 7]));
        assert_eq!(parse_ids("x"), None);
        let ids: Vec<u32> = (0..75).collect();
        let c = contiguous(&ids);
        assert_eq!(
            c.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![19, 19, 19, 18]
        );
        assert_eq!(
            dealt(&[5, 6, 7, 8, 9]),
            vec![vec![5, 9], vec![6], vec![7], vec![8]]
        );
    }

    /// The reference tracer's random play: JavaScript doubles lose the product's low bits.
    #[test]
    fn lcg_matches_javascript() {
        let mut seed = 3u32;
        let mut v = Vec::new();
        for _ in 0..3 {
            let x = seed as f64 * 1103515245.0 + 12345.0;
            seed = (x as u64 & 0x7fff_ffff) as u32;
            v.push(seed);
        }
        // node: s=3; [0,0,0].map(() => s = (s * 1103515245 + 12345) & 0x7fffffff)
        assert_eq!(v, vec![1163074432, 465823232, 1719475776]);
    }

    /// T2 from its ROM zip ($ROM2DMD_ROMS, default ~/roms-full; skipped when absent): the
    /// probe, the boot to a play snapshot and one effect (17, a 5 s animation) that ends by
    /// itself with its frames in the raw log.
    #[test]
    fn t2_scan_one_effect() {
        let dir = std::env::var("ROM2DMD_ROMS")
            .unwrap_or_else(|_| format!("{}/roms-full", std::env::var("HOME").unwrap_or_default()));
        let path = format!("{dir}/t2_l8.zip");
        if !Path::new(&path).exists() {
            eprintln!("{path} absent, skipped");
            return;
        }
        let rom = super::super::load_rom(&path, None).expect("t2 ROM");
        let game = games::find("t2_l8").expect("t2 in the database");
        let (cfg, _) = configure(&rom, Some(&game), "t2", &Opts::default()).expect("probe");
        assert!(!cfg.wpc95_flow);
        assert_eq!(cfg.zero, vec![0x488]);
        let board = Board::new(rom, game.generation(), Compat::default()).expect("board");
        let mut m = Machine::new(board, &cfg);
        m.boot();
        let snap = m.snaps[1].take().expect("play snapshot");
        let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/wpc-scan-test");
        std::fs::create_dir_all(&out).expect("test dir");
        let raw = out.join("t2_17.raw");
        let o = PassOpts {
            caps: 20,
            bypass: false,
            protect: false,
        };
        let r = m.capture(17, &snap, o, &raw).expect("capture");
        assert_eq!(r.kind, "finite", "{}", r.log);
        let text = std::fs::read_to_string(&raw).expect("raw log");
        assert!(text.starts_with("#17\n"));
        let pages = text.lines().filter(|l| l.starts_with('=')).count();
        assert!(pages > 30, "{pages} distinct pages");
        let _ = std::fs::remove_file(&raw);
    }
}
