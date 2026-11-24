//! Scene scan shared by the boards emulated alone (`de16-scan`, `de64-scan`, `gts3-scan`,
//! `alvg-scan`, and the Data East / Whitestar 128x32 board in the single command): one fresh
//! board per scene id, a boot, optional init bytes, the scene packet, then record until the
//! picture stops changing for 3 s (or the cap).
//!
//! Subcommand env: BOOT (s, default 3), PRE (init tokens sent before recording), PACKET
//! (default `2,1b,{id}`), GAP (CPU cycles after each byte), BOOTREC (keep the boot frames).
//! Tokens: hex bytes, `{id}`, `wNNN` = wait NNN ms.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use crate::export::scenes::Scene;

/// Callback receiving each new picture: (cycle stamp, one level per dot).
pub type FrameSink = Box<dyn FnMut(u64, &[u8])>;

/// Pictures kept by a scan: (cycle stamp, levels), consecutive duplicates removed.
type Recorded = Vec<(u64, Vec<u8>)>;

pub trait Dmd {
    const CLOCK_HZ: u64;
    const WIDTH: usize;
    fn run(&mut self, cycles: u64);
    fn send(&mut self, byte: u8);
    fn busy(&self) -> bool;
    fn cycles(&self) -> u64;
    fn set_on_frame(&mut self, f: FrameSink);
    /// The board is waiting for an argument byte of the command being run (boards whose
    /// commands have no length byte); the scan then sends a 0.
    fn wants_arg(&self) -> bool {
        false
    }
}

/// How one scene is played.
#[derive(Clone, Debug)]
pub struct Cfg {
    /// Seconds from power-on to the first byte.
    pub boot_s: f64,
    /// Tokens sent before recording (the game's own initialisation).
    pub pre: String,
    /// Tokens of the scene command.
    pub packet: String,
    /// CPU cycles after each byte (None: about 0.9 ms, as the main CPU sends them).
    pub gap: Option<u64>,
    /// Keep the frames shown during the boot and PRE (else only the last one).
    pub bootrec: bool,
    /// Seconds before a scene still running is cut.
    pub cap_s: f64,
}

impl Cfg {
    /// The subcommands' settings from the environment (see the module documentation).
    pub fn from_env(cap_s: f64) -> Result<Self, String> {
        let env = |k: &str| std::env::var(k).ok();
        let gap = match env("GAP") {
            Some(v) => Some(v.parse().map_err(|_| format!("bad GAP {v}"))?),
            None => None,
        };
        Ok(Cfg {
            boot_s: env("BOOT").and_then(|v| v.parse().ok()).unwrap_or(3.0),
            pre: env("PRE").unwrap_or_default(),
            packet: env("PACKET").unwrap_or("2,1b,{id}".into()),
            gap,
            bootrec: env("BOOTREC").is_some(),
            cap_s,
        })
    }
}

/// Check a token list: hex bytes, `{id}` or `wNNN`.
pub fn check_tokens(tokens: &str) -> Result<(), String> {
    for x in tokens.split(',').filter(|x| !x.is_empty()) {
        let ok = x == "{id}"
            || x.strip_prefix('w')
                .is_some_and(|ms| ms.parse::<u64>().is_ok())
            || u8::from_str_radix(x, 16).is_ok();
        if !ok {
            return Err(format!("bad token {x} in {tokens}"));
        }
    }
    Ok(())
}

/// Send tokens (checked by `check_tokens`); returns the second byte (the command).
fn send_tokens<B: Dmd>(b: &mut B, tokens: &str, id: u32, gap: u64) -> u8 {
    let mut cmd = 0u8;
    let mut k = 0;
    for x in tokens.split(',').filter(|x| !x.is_empty()) {
        if let Some(ms) = x.strip_prefix('w') {
            b.run(ms.parse::<u64>().unwrap_or(0) * B::CLOCK_HZ / 1000);
            continue;
        }
        let byte = if x == "{id}" {
            id as u8
        } else {
            u8::from_str_radix(x, 16).unwrap_or(0)
        };
        if k == 1 {
            cmd = byte;
        }
        k += 1;
        let mut w = 0;
        while b.busy() && w < 100 {
            b.run(B::CLOCK_HZ / 10_000);
            w += 1;
        }
        b.send(byte);
        b.run(gap);
    }
    cmd
}

/// Play one scene on a fresh board. Returns the scene (frames in ms since the command), the
/// command byte (for the dump file name) and the number of argument bytes fed.
pub fn scan_one<B: Dmd>(mut b: B, id: u32, c: &Cfg) -> (Scene, u8, u32) {
    let hz = B::CLOCK_HZ as f64;
    let gap = c.gap.unwrap_or(B::CLOCK_HZ * 9 / 10_000);
    let frames: Rc<RefCell<Recorded>> = Rc::new(RefCell::new(Vec::new()));
    let fr = frames.clone();
    b.set_on_frame(Box::new(move |t, f| {
        let mut v = fr.borrow_mut();
        if v.last().map(|l| l.1.as_slice() != f).unwrap_or(true) {
            v.push((t, f.to_vec()));
        }
    }));
    b.run((c.boot_s * hz) as u64);
    send_tokens(&mut b, &c.pre, id, gap);
    // the picture left by the boot stays as the first frame (the exporter drops it)
    if !c.bootrec {
        let mut v = frames.borrow_mut();
        let keep = v.pop();
        v.clear();
        v.extend(keep);
    }
    let t0 = b.cycles();
    let cmd = send_tokens(&mut b, &c.packet, id, gap);
    let mut kind = "cap";
    let mut args = 0;
    while b.cycles() - t0 < (c.cap_s * hz) as u64 {
        b.run(B::CLOCK_HZ / 1000);
        if args < 8 && b.wants_arg() {
            b.send(0);
            args += 1;
        }
        // Silence is counted from the command at the earliest: the boot picture kept as
        // the first frame may be older than the 3 s (a still boot screen), which ended
        // the scene before the board had read the command.
        let last = frames.borrow().last().map_or(t0, |f| f.0.max(t0));
        if b.cycles() - last > 3 * B::CLOCK_HZ {
            kind = "end";
            break;
        }
    }
    let f = std::mem::take(&mut *frames.borrow_mut());
    let frames = f
        .into_iter()
        .map(|(t, px)| (t.saturating_sub(t0) * 1000 / B::CLOCK_HZ, px))
        .collect();
    (
        Scene {
            id,
            kind: kind.into(),
            frames,
            extra: Default::default(),
        },
        cmd,
        args,
    )
}

/// Play every id on all cores (`threads`), each on a fresh board from `make`.
pub fn scan_par<B: Dmd>(
    make: impl Fn() -> B + Sync,
    ids: &[u32],
    c: &Cfg,
    threads: usize,
    progress: &(dyn Fn(&Scene) + Sync),
) -> Vec<Scene> {
    crate::export::par_map_threads(ids, threads, |&id| {
        let s = scan_one(make(), id, c).0;
        progress(&s);
        s
    })
}

/// The subcommands' scan: ids `first..=last`, one text dump per id in `out`
/// (`<cmd>_<id>.txt`: `0x<ms>` then one row of level digits per line) and one line per id
/// on stdout.
pub fn scan<B: Dmd>(make: impl Fn() -> B, first: u32, last: u32, cap: f64, out: &str) {
    if let Err(e) = scan_files(make, first, last, cap, out) {
        log::info!("error: {e}");
        std::process::exit(1);
    }
}

fn scan_files<B: Dmd>(
    make: impl Fn() -> B,
    first: u32,
    last: u32,
    cap: f64,
    out: &str,
) -> Result<(), String> {
    let c = Cfg::from_env(cap)?;
    check_tokens(&c.pre)?;
    check_tokens(&c.packet)?;
    std::fs::create_dir_all(out).map_err(|e| format!("{out}: {e}"))?;
    for id in first..=last {
        let (s, cmd, args) = scan_one(make(), id, &c);
        let lit = s
            .frames
            .iter()
            .filter(|x| x.1.iter().any(|&v| v != 0))
            .count();
        let fell = if s.kind == "end" {
            s.frames
                .last()
                .map_or("-".into(), |f| format!("{:.2}", f.0 as f64 / 1000.0))
        } else {
            "-".into()
        };
        println!(
            "{id:3} {:6} busy -..{fell} frames {} lit {lit} args {args}",
            s.kind,
            s.frames.len()
        );
        let p = format!("{out}/{cmd:02x}_{id:03}.txt");
        let mut o =
            std::io::BufWriter::new(std::fs::File::create(&p).map_err(|e| format!("{p}: {e}"))?);
        let mut w = || -> std::io::Result<()> {
            for (t, fr) in &s.frames {
                writeln!(o, "0x{t:08x}")?;
                for row in fr.chunks(B::WIDTH) {
                    let line: String = row
                        .iter()
                        .map(|&v| char::from_digit(v as u32, 16).unwrap_or('?'))
                        .collect();
                    writeln!(o, "{line}")?;
                }
                writeln!(o)?;
            }
            o.flush()
        };
        w().map_err(|e| format!("{p}: {e}"))?;
    }
    Ok(())
}
