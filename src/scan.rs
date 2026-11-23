//! Scene scan shared by the boards emulated alone (`de16-scan`, `de64-scan`): one fresh board per
//! scene id, a boot, optional init bytes, the scene packet, then record until the picture stops
//! changing for 3 s (or the cap).
//!
//! Env: BOOT (s, default 3), PRE (init tokens sent before recording), PACKET (default `2,1b,{id}`),
//! GAP (CPU cycles after each byte), BOOTREC (keep the boot frames).
//! Tokens: hex bytes, `{id}`, `wNNN` = wait NNN ms.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

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

fn send_tokens<B: Dmd>(b: &mut B, tokens: &str, id: u32, gap: u64) -> u8 {
    let mut cmd = 0u8;
    let mut k = 0;
    for x in tokens.split(',').filter(|x| !x.is_empty()) {
        if let Some(ms) = x.strip_prefix('w') {
            b.run(ms.parse::<u64>().unwrap() * B::CLOCK_HZ / 1000);
            continue;
        }
        let byte = if x == "{id}" {
            id as u8
        } else {
            u8::from_str_radix(x, 16).unwrap()
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

pub fn scan<B: Dmd>(make: impl Fn() -> B, first: u32, last: u32, cap: f64, out: &str) {
    std::fs::create_dir_all(out).unwrap();
    let hz = B::CLOCK_HZ as f64;
    let env = |k: &str| std::env::var(k).ok();
    let boot: f64 = env("BOOT").and_then(|v| v.parse().ok()).unwrap_or(3.0);
    let pre = env("PRE").unwrap_or_default();
    let pk = env("PACKET").unwrap_or("2,1b,{id}".into());
    // about 0.9 ms between bytes, as the main CPU sends them
    let gap: u64 = env("GAP")
        .and_then(|v| v.parse().ok())
        .unwrap_or(B::CLOCK_HZ * 9 / 10_000);
    for id in first..=last {
        let mut b = make();
        let frames: Rc<RefCell<Recorded>> = Rc::new(RefCell::new(Vec::new()));
        let fr = frames.clone();
        b.set_on_frame(Box::new(move |t, f| {
            let mut v = fr.borrow_mut();
            if v.last().map(|l| l.1.as_slice() != f).unwrap_or(true) {
                v.push((t, f.to_vec()));
            }
        }));
        b.run((boot * hz) as u64);
        send_tokens(&mut b, &pre, id, gap);
        // the picture left by the boot stays as the first frame (the exporter drops it)
        if env("BOOTREC").is_none() {
            let mut v = frames.borrow_mut();
            let keep = v.pop();
            v.clear();
            v.extend(keep);
        }
        let t0 = b.cycles();
        let cmd = send_tokens(&mut b, &pk, id, gap);
        let (mut fell, mut kind) = (None, "cap");
        let mut args = 0;
        while b.cycles() - t0 < (cap * hz) as u64 {
            b.run(B::CLOCK_HZ / 1000);
            if args < 8 && b.wants_arg() {
                b.send(0);
                args += 1;
            }
            let last = frames.borrow().last().map(|f| f.0).unwrap_or(t0);
            if b.cycles() - last > 3 * B::CLOCK_HZ {
                fell = Some(last);
                kind = "end";
                break;
            }
        }
        let f = frames.borrow();
        let lit = f.iter().filter(|x| x.1.iter().any(|&v| v != 0)).count();
        let s = |t: Option<u64>| {
            t.map(|t| format!("{:.2}", t.saturating_sub(t0) as f64 / hz))
                .unwrap_or("-".into())
        };
        println!(
            "{id:3} {kind:6} busy -..{} frames {} lit {} args {args}",
            s(fell),
            f.len(),
            lit
        );
        let mut o = std::io::BufWriter::new(
            std::fs::File::create(format!("{out}/{cmd:02x}_{id:03}.txt")).unwrap(),
        );
        for (t, fr) in f.iter() {
            writeln!(o, "0x{:08x}", t.saturating_sub(t0) * 1000 / B::CLOCK_HZ).unwrap();
            for row in fr.chunks(B::WIDTH) {
                writeln!(
                    o,
                    "{}",
                    row.iter().map(|v| (b'0' + v) as char).collect::<String>()
                )
                .unwrap();
            }
            writeln!(o).unwrap();
        }
    }
}
