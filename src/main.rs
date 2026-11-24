mod alvg;
mod capcom;
mod de16;
mod de32;
mod de64;
mod export;
mod gts3;
mod sam;
mod scan;
mod wpc;

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

/// Frames kept by a run: (cycle stamp, 4096 levels), consecutive duplicates removed.
type Frames = Rc<RefCell<Vec<(u64, [u8; 4096])>>>;

fn write_dump(path: &str, frames: &[(u64, [u8; 4096])], t0: u64) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).expect("dump file"));
    for (t, fr) in frames {
        writeln!(f, "0x{:08x}", (t - t0) * 1000 / de32::CLOCK_HZ).unwrap();
        for row in fr.chunks(128) {
            let s: String = row.iter().map(|v| (b'0' + v) as char).collect();
            writeln!(f, "{s}").unwrap();
        }
        writeln!(f).unwrap();
    }
}

fn collector(board: &mut de32::Board) -> Frames {
    let frames: Frames = Rc::new(RefCell::new(Vec::new()));
    let fr = frames.clone();
    board.on_frame = Some(Box::new(move |t, f| {
        let mut v = fr.borrow_mut();
        if v.last().map(|l| &l.1 != f).unwrap_or(true) {
            v.push((t, *f));
        }
    }));
    frames
}

/// CPU cycles per vblank: BOARD=ws for the Sega/Stern Whitestar boards, else Data East.
fn frame_cycles() -> u64 {
    if std::env::var("BOARD").as_deref() == Ok("ws") {
        de32::CYCLES_PER_FRAME_WS
    } else {
        de32::CYCLES_PER_FRAME_DE
    }
}

fn scene_export_cli(args: &[String]) -> Result<(), String> {
    use std::path::Path;
    const USAGE: &str =
        "usage: rom2dmd scene-export <scandir> <scan.log> <outdir> [--blend] [--pal v,v,...]";
    if args.len() < 5 {
        return Err(USAGE.into());
    }
    let scenes = export::scenes::read_dumps(Path::new(&args[2]), Path::new(&args[3]))?;
    let (w, h) = scenes
        .iter()
        .find_map(|s| s.frames.first().map(|f| f.1.len()))
        .map_or((128, 32), |n| match n {
            2048 => (128, 16),
            12288 => (192, 64),
            8192 => (256, 32),
            _ => (128, 32),
        });
    let mut o = export::scenes::Opts {
        geometry: export::Geometry::linear(w, h, 4),
        blend: false,
    };
    let mut it = args[5..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--blend" => o.blend = true,
            "--pal" => {
                o.geometry.palette = it
                    .next()
                    .ok_or(USAGE)?
                    .split(',')
                    .map(|v| v.parse::<u8>().map_err(|_| USAGE.to_string()))
                    .collect::<Result<_, _>>()?
            }
            _ => return Err(USAGE.into()),
        }
    }
    let s = export::scenes::export(&scenes, &o, Path::new(&args[4]))?;
    println!("{:?} total {}", s.counts, s.total);
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // de32-boot <dmd.rom> <secs> <out.txt>: run the board alone, no command.
        Some("de32-boot") => {
            let rom = std::fs::read(&args[2]).expect("rom");
            let secs: f64 = args[3].parse().unwrap();
            let mut b = de32::Board::new(rom, frame_cycles());
            let frames = collector(&mut b);
            b.run((secs * de32::CLOCK_HZ as f64) as u64);
            let r = b.cpu.registers();
            eprintln!(
                "pc {:04x} frames {} latch reads {} status writes {:?} crtc {:02x?}",
                r.pc,
                frames.borrow().len(),
                b.bus.latch_reads,
                b.bus.status_writes.iter().take(20).collect::<Vec<_>>(),
                &b.bus.crtc[..16]
            );
            write_dump(&args[4], &frames.borrow(), 0);
        }
        // de32-cmd <dmd.rom> <boot secs> <hex packet,...> <secs> <out.txt>: boot, send packets, record.
        Some("de32-cmd") => {
            let rom = std::fs::read(&args[2]).expect("rom");
            let boot: f64 = args[3].parse().unwrap();
            let secs: f64 = args[5].parse().unwrap();
            let mut b = de32::Board::new(rom, frame_cycles());
            b.run((boot * de32::CLOCK_HZ as f64) as u64);
            let frames = collector(&mut b);
            let t0 = b.cpu.cycles();
            b.bus.watch = Some((0x1ff, 0x2ff));
            for pkt in args[4].split('/') {
                for byte in pkt.split(',').filter(|x| !x.is_empty()) {
                    while b.bus.busy {
                        b.run(200);
                    }
                    b.send(u8::from_str_radix(byte, 16).unwrap());
                    b.run(400);
                }
                b.run(40_000);
            }
            b.run((secs * de32::CLOCK_HZ as f64) as u64);
            let mut w = b.bus.watch_log.clone();
            w.sort();
            eprintln!(
                "latch reads {} cc {:02x} pc {:04x} frames {} watch {}",
                b.bus.latch_reads,
                b.cpu.registers().cc.to_byte(),
                b.cpu.registers().pc,
                frames.borrow().len(),
                w.iter()
                    .map(|(p, a)| format!("{p:04x}:{a:03x}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            write_dump(&args[6], &frames.borrow(), t0);
        }
        // de32-scan <dmd.rom> <cmd hex> <first> <last> <cap secs> <outdir>: one fresh board per id,
        // send [2, cmd, id], record until the busy bit (status bit 2) falls, or the cap.
        Some("de32-scan") => {
            let rom = std::fs::read(&args[2]).expect("rom");
            let cmd = u8::from_str_radix(&args[3], 16).unwrap();
            let (first, last): (u32, u32) = (args[4].parse().unwrap(), args[5].parse().unwrap());
            let cap: f64 = args[6].parse().unwrap();
            let out = &args[7];
            std::fs::create_dir_all(out).unwrap();
            let hz = de32::CLOCK_HZ as f64;
            for id in first..=last {
                let mut b = de32::Board::new(rom.clone(), frame_cycles());
                // BOOT: seconds before the first command (a Whitestar board ignores commands for ~5 s)
                let boot: f64 = std::env::var("BOOT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(3.0);
                b.run((boot * hz) as u64);
                let frames = collector(&mut b);
                // Scene-script markers (JP defaults, override by env). A scene ends when its counted
                // tracks reach 0; it is a mode loop when every counted track sits in an endless repeat
                // (count 0xff) that ran once, or in hold-forever.
                let h = |k: &str, d: &str| -> Vec<u16> {
                    std::env::var(k)
                        .unwrap_or(d.into())
                        .split(',')
                        .map(|x| u16::from_str_radix(x, 16).unwrap())
                        .collect()
                };
                let (rep_open, rep_close, hold_op) = (
                    h("REP_OPEN", "f350")[0],
                    h("REP_CLOSE", "f359")[0],
                    h("HOLD_OP", "f347")[0],
                );
                // A track is the current task when the interpreter initialises its script pointer.
                let start = h("TRACK_START", "ee57");
                let dec = h("TRACK_DONE", "ef22,f338,f33e"); // current task
                // Move-and-wait: the track waits until the sprite position equals the target; if the
                // sprite already moves away from it, it never arrives (X = sprite object here).
                let move_wait = h("MOVE_WAIT", "f211")[0];
                let counter =
                    usize::from_str_radix(&std::env::var("TRACK_COUNT").unwrap_or("5e".into()), 16)
                        .unwrap();
                let extra: Vec<u16> = std::env::var("PRINT_HOOKS")
                    .map(|v| {
                        v.split(',')
                            .map(|x| u16::from_str_radix(x, 16).unwrap())
                            .collect()
                    })
                    .unwrap_or_default();
                // The scene-script hooks are JP's addresses: another display ROM (IDLE_END) runs without
                // them, its scenes ending when the picture stops changing.
                if std::env::var("IDLE_END").is_err() {
                    b.hooks = [
                        vec![rep_open, rep_close, hold_op],
                        start.clone(),
                        dec.clone(),
                        extra.clone(),
                        vec![move_wait],
                    ]
                    .concat();
                }
                let t0 = b.cpu.cycles();
                // PACKET: the bytes sent for one scene, `{cmd}` and `{id}` filled in (JP: 2,{cmd},{id})
                let packet: Vec<u8> = std::env::var("PACKET")
                    .unwrap_or("2,{cmd},{id}".into())
                    .split(',')
                    .map(|x| match x {
                        "{cmd}" => cmd,
                        "{id}" => id as u8,
                        h => u8::from_str_radix(h, 16).unwrap(),
                    })
                    .collect();
                for byte in packet {
                    while b.bus.busy {
                        b.run(200);
                    }
                    b.send(byte);
                    b.run(400);
                }
                // End marker: the display program keeps the running scene id in RAM (JP: 0x5d)
                // and clears it when the scene script ends.
                let var =
                    usize::from_str_radix(&std::env::var("SCENE_VAR").unwrap_or("5d".into()), 16)
                        .unwrap();
                let (mut rose, mut fell, mut kind): (Option<u64>, Option<u64>, &str) =
                    (None, None, "cap");
                b.hook_log.clear();
                // Per counted track (task pointer): None = running, Some(Open(t)) = endless repeat
                // opened at t, Closed(t0, t1) = one period done, Held(t) = hold-forever.
                #[derive(Clone, Copy, PartialEq)]
                enum St {
                    Run,
                    Open(u64),
                    Closed(u64, u64),
                    Held(u64),
                    Call(u64),
                }
                let step = h("SCRIPT_STEP", "ee61")[0]; // U = script pointer of the current track
                let call_op = h("CALL_OPCODE", "87")[0] as u8; // script calls native code
                b.hooks.push(step);
                let mut tracks: Vec<(u16, St, u64)> = Vec::new(); // (task, state, birth)
                let mut scripts: Vec<(u16, u16, u64)> = Vec::new(); // (task, script start, start time)
                let mut loop_span: Option<(u64, u64)> = None;
                let mut pre_at: Option<u64> = None;
                let mut last_pre = 0u64;
                let mut native_since: Option<u64> = None;
                let mut loop_period: Vec<u64> = Vec::new();
                let mut mode_kind = "loop";
                let tail = std::env::var("HOLD_TAIL")
                    .ok()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(3.0);
                while b.cpu.cycles() - t0 < (cap * hz) as u64 {
                    b.run(2_000);
                    let now = b.cpu.cycles();
                    for (t, r, task) in b.hook_log.drain(..) {
                        let pc = r.pc;
                        if extra.contains(&pc) {
                            let sp = r.s as usize;
                            let ret = u16::from_be_bytes([
                                b.bus.sram[sp & 0x1fff],
                                b.bus.sram[(sp + 1) & 0x1fff],
                            ]);
                            eprintln!(
                                "  {:.3} pc {pc:04x} d {:04x} x {:04x} y {:04x} u {:04x} task {task:04x} [s] {ret:04x} cnt {}",
                                (t - t0) as f64 / hz,
                                r.d,
                                r.x,
                                r.y,
                                r.u,
                                b.bus.sram[counter]
                            );
                        }
                        if start.contains(&pc) {
                            scripts.push((task, r.u, t));
                            tracks.retain(|x| x.0 != task);
                            tracks.push((task, St::Run, t));
                        } else if dec.contains(&pc) {
                            tracks.retain(|x| x.0 != task);
                        } else if let Some(tr) = tracks.iter_mut().find(|x| x.0 == task) {
                            let endless = (r.d & 0xff) == 0xff;
                            if pc == step {
                                // Script opcodes live in the fixed upper 32 KiB or the banked window.
                                let u = r.u as usize;
                                let rom = &b.bus.rom;
                                let op = if u >= 0x8000 {
                                    rom[rom.len() - 0x8000 + u - 0x8000]
                                } else if u >= 0x4000 {
                                    rom[(b.bus.rom_bank * 0x4000 + u - 0x4000) % rom.len()]
                                } else {
                                    0
                                };
                                tr.1 = match tr.1 {
                                    St::Run if op == call_op => St::Call(t),
                                    St::Call(_) => St::Run,
                                    s => s,
                                };
                                continue;
                            }
                            if pc == move_wait {
                                let o = r.x as usize & 0x1fff;
                                let w = |k: usize| {
                                    i16::from_be_bytes([
                                        b.bus.sram[(o + k) & 0x1fff],
                                        b.bus.sram[(o + k + 1) & 0x1fff],
                                    ]) as i32
                                };
                                let away = |pos: i32, target: i32, vel: i32| {
                                    pos != target
                                        && (vel == 0 || (target - pos).signum() != vel.signum())
                                };
                                if (away(w(0x16), w(0x1d), w(0xd))
                                    || away(w(0x19), w(0x1f), w(0xf)))
                                    && !matches!(tr.1, St::Held(_))
                                {
                                    tr.1 = St::Held(t);
                                }
                                continue;
                            }
                            tr.1 = match (pc, tr.1) {
                                (p, St::Run) if p == rep_open && endless => St::Open(t),
                                // Each close ends one iteration: keep the latest [start, end].
                                (p, St::Open(t0)) if p == rep_close && endless => St::Closed(t0, t),
                                (p, St::Closed(_, z)) if p == rep_close && endless => {
                                    St::Closed(z, t)
                                }
                                (p, St::Run | St::Open(_)) if p == hold_op => St::Held(t),
                                (_, s) => s,
                            };
                        }
                    }
                    // Settled: every counted track is in an endless repeat, frozen, or parked in
                    // native code; then each endless repeat must run one whole iteration after that.
                    // Drop tracks whose task died (live task list: head 0x1300, next pointer at +0).
                    // A reused slot re-registers through the track-start hook.
                    {
                        let ram = &b.bus.sram;
                        let w = |a: usize| {
                            u16::from_be_bytes([ram[a & 0x1fff], ram[(a + 1) & 0x1fff]]) as usize
                        };
                        let mut live = Vec::new();
                        let mut t = 0x1300usize;
                        while t != 0 && live.len() < 256 {
                            live.push(t as u16);
                            t = w(t);
                        }
                        tracks.retain(|x| live.contains(&x.0));
                    }
                    // Respawn loop (spawn a copy of the script, then end): a running track whose script
                    // an earlier, now-dead track of this scene started closes one iteration.
                    for i in 0..tracks.len() {
                        let (k, st, born) = tracks[i];
                        if st != St::Run {
                            continue;
                        }
                        let Some(&(_, sc, _)) =
                            scripts.iter().rev().find(|x| x.0 == k && x.2 == born)
                        else {
                            continue;
                        };
                        let prev = scripts
                            .iter()
                            .filter(|x| {
                                x.1 == sc
                                    && x.2 < born
                                    && !tracks.iter().any(|y| y.0 == x.0 && y.2 == x.2)
                            })
                            .map(|x| x.2)
                            .max();
                        if let Some(tp) = prev {
                            tracks[i].1 = St::Closed(tp, born);
                        }
                    }
                    // Once settled, tracks born later (children spawned by a loop body) may run.
                    let born_before = pre_at.unwrap_or(u64::MAX);
                    // Counted tasks that never entered the script interpreter run native code; accept
                    // them only once the surplus has lasted 0.5 s (a respawn gap is shorter).
                    // JP's track counter means nothing in another display ROM (IDLE_END)
                    let cnt = if std::env::var("IDLE_END").is_ok() {
                        0
                    } else {
                        b.bus.sram[counter] as usize
                    };
                    native_since = if cnt > tracks.len() {
                        native_since.or(Some(now))
                    } else {
                        None
                    };
                    let native = native_since.is_some_and(|t| now - t > hz as u64 / 2);
                    let pre = cnt > 0
                        && (tracks.len() == cnt || native)
                        && tracks
                            .iter()
                            .all(|x| !matches!(x.1, St::Run) || x.2 >= born_before);
                    if std::env::var("LOOP_DEBUG2").is_ok() {
                        let st: Vec<String> = tracks
                            .iter()
                            .map(|x| {
                                format!(
                                    "{:04x}:{}",
                                    x.0,
                                    match x.1 {
                                        St::Run => "R",
                                        St::Open(_) => "O",
                                        St::Closed(..) => "C",
                                        St::Held(_) => "H",
                                        St::Call(_) => "W",
                                    }
                                )
                            })
                            .collect();
                        eprintln!(
                            "  {:.2} pre {pre} cnt {} [{}]",
                            (now - t0) as f64 / hz,
                            b.bus.sram[counter],
                            st.join(" ")
                        );
                    }
                    if pre {
                        last_pre = now;
                    }
                    // Debounce: a respawn leaves a brief gap (parent gone, child not started yet).
                    if !pre && now - last_pre > hz as u64 / 4 {
                        pre_at = None;
                        if loop_span.is_some() && b.bus.sram[var] == id as u8 {
                            loop_span = None; // a track left its loop: not a mode loop after all
                        }
                    } else if pre && pre_at.is_none() {
                        pre_at = Some(now);
                    }
                    if let (Some(p0), None, true) = (pre_at, loop_span, pre) {
                        let iters: Vec<(u64, u64)> = tracks
                            .iter()
                            .filter_map(|x| match x.1 {
                                St::Closed(a, z) if a >= p0 => Some((a, z)),
                                _ => None,
                            })
                            .collect();
                        let loops = tracks
                            .iter()
                            .filter(|x| matches!(x.1, St::Open(_) | St::Closed(..)))
                            .count();
                        if std::env::var("LOOP_DEBUG").is_ok() {
                            eprintln!(
                                "  p0 {:.2} loops {loops} iters {:?}",
                                (p0 - t0) as f64 / hz,
                                iters
                                    .iter()
                                    .map(|c| format!(
                                        "{:.2}..{:.2}",
                                        (c.0 - t0) as f64 / hz,
                                        (c.1 - t0) as f64 / hz
                                    ))
                                    .collect::<Vec<_>>()
                            );
                        }
                        let any_run = tracks.iter().any(|x| x.1 == St::Run);
                        if loops == 0 && !any_run {
                            mode_kind = if native {
                                "native"
                            } else if tracks.iter().any(|x| matches!(x.1, St::Call(_))) {
                                "wait"
                            } else {
                                "hold"
                            };
                            loop_span = Some((p0, p0 + (tail * hz) as u64));
                        } else if iters.len() == loops {
                            // Scene period = LCM of the track periods counted in refreshes. Start once
                            // every track is looping; beyond 60 s the rhythms are unrelated ("poly"):
                            // keep one iteration of the longest track.
                            let fc = b.frame_cycles;
                            let mut periods: Vec<u64> = iters
                                .iter()
                                .map(|c| ((c.1 - c.0 + fc / 2) / fc).max(1))
                                .collect();
                            // One iteration can be a refresh longer or shorter: snap near-equal periods.
                            let snap = periods.clone();
                            for p in periods.iter_mut() {
                                *p = snap
                                    .iter()
                                    .copied()
                                    .filter(|q| q.abs_diff(*p) <= 1)
                                    .max()
                                    .unwrap();
                            }
                            let gcd = |mut a: u64, mut b: u64| {
                                while b != 0 {
                                    (a, b) = (b, a % b);
                                }
                                a
                            };
                            let lcm = periods
                                .iter()
                                .fold(1u64, |l, &p| (l / gcd(l, p)).saturating_mul(p));
                            let start = iters.iter().map(|c| c.0).max().unwrap();
                            let longest = *periods.iter().max().unwrap();
                            let (len, kind_name) = if lcm * fc <= 60 * hz as u64 {
                                (lcm, "loop")
                            } else {
                                (longest, "poly")
                            };
                            mode_kind = kind_name;
                            loop_period = periods.clone();
                            loop_span = Some((start, start + len * fc));
                        }
                    }
                    // Confirmation: a controller track may still kill the loop on a timer, so the
                    // scene must also survive a guard window before it counts as a mode loop.
                    if let Some((from, to)) = loop_span {
                        let guard = (10 * hz as u64).max(to - from);
                        if now > to + guard {
                            kind = mode_kind;
                            fell = Some(to);
                            break;
                        }
                    }
                    if std::env::var("IDLE_END").is_ok() {
                        let last = frames.borrow().last().map(|f| f.0).unwrap_or(t0);
                        if now - last > 3 * de32::CLOCK_HZ {
                            fell = Some(last);
                            kind = "end";
                            break;
                        }
                        continue;
                    }
                    let cur = b.bus.sram[var];
                    if rose.is_none() && cur == id as u8 && id != 0 {
                        rose = Some(now);
                    }
                    if rose.is_some() && cur != id as u8 {
                        fell = Some(now);
                        kind = "end";
                        break;
                    }
                    if rose.is_none() && now - t0 > hz as u64 {
                        kind = "none";
                        break;
                    }
                }
                if std::env::var("LOOP_DEBUG").is_ok() {
                    let st: Vec<String> = tracks
                        .iter()
                        .map(|(t, s, _)| {
                            format!(
                                "{t:04x}:{}",
                                match s {
                                    St::Run => "run".into(),
                                    St::Open(a) => format!("open@{:.2}", (a - t0) as f64 / hz),
                                    St::Closed(..) => "closed".into(),
                                    St::Held(_) => "held".into(),
                                    St::Call(_) => "call".into(),
                                }
                            )
                        })
                        .collect();
                    eprintln!(
                        "  counter {} tracks [{}]",
                        b.bus.sram[counter],
                        st.join(" ")
                    );
                    if let Ok(v) = std::env::var("DUMP_RAM") {
                        let mut it = v.split(',').map(|x| usize::from_str_radix(x, 16).unwrap());
                        let (a, n) = (it.next().unwrap(), it.next().unwrap());
                        eprintln!(
                            "  ram {a:04x}: {}",
                            b.bus.sram[a..a + n]
                                .iter()
                                .map(|x| format!("{x:02x}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        );
                    }
                }
                b.run((0.3 * hz) as u64);
                // Repeat score: pixel agreement between the kept iteration and the next one (frames
                // recorded during the guard window). Low = the script loops but the content varies
                // (random sparkle, random picks).
                let mut repeat = None;
                if let (Some((from, to)), true) = (loop_span, matches!(kind, "loop" | "poly")) {
                    let f = frames.borrow();
                    let at = |t: u64| -> &[u8; 4096] {
                        let i = f.partition_point(|x| x.0 <= t);
                        &f[i.saturating_sub(1)].1
                    };
                    let p = to - from;
                    let n = 40u64;
                    let sum: f64 = (0..n)
                        .map(|k| {
                            let t = from + k * p / n;
                            let (a, z) = (at(t), at(t + p));
                            a.iter().zip(z.iter()).filter(|(x, y)| x == y).count() as f64 / 4096.0
                        })
                        .sum();
                    repeat = Some(sum / n as f64);
                }
                if matches!(kind, "loop" | "poly" | "hold" | "wait" | "native") {
                    let to = fell.unwrap();
                    frames.borrow_mut().retain(|x| x.0 <= to);
                }
                let f = frames.borrow();
                let lit = f.iter().filter(|x| x.1.iter().any(|&v| v != 0)).count();
                let s = |t: Option<u64>| {
                    t.map(|t| format!("{:.2}", (t - t0) as f64 / hz))
                        .unwrap_or("-".into())
                };
                let ls = loop_span
                    .filter(|_| kind != "end")
                    .map(|(a, z)| {
                        format!(
                            " loop {}..{} tracks {} periods {:?}{}",
                            s(Some(a)),
                            s(Some(z)),
                            tracks.len(),
                            loop_period,
                            repeat
                                .map(|r| format!(" repeat {r:.3}"))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                println!(
                    "{id:3} {kind:6} busy {}..{} frames {} lit {}{ls}",
                    s(rose),
                    s(fell),
                    f.len(),
                    lit
                );
                write_dump(&format!("{out}/{cmd:02x}_{id:03}.txt"), &f, t0);
            }
        }
        // de16-scan <dmd.rom> <first> <last> <cap secs> <outdir>: Data East 128x16 board, one fresh
        // board per id, send PACKET (default 2,1b,{id}), record until 3 s without a new picture.
        // de16-scan <dmd.rom> <first> <last> <cap secs> <outdir>: Data East 128x16 board (see scan.rs)
        Some("de16-scan") => {
            let rom = std::fs::read(&args[2]).expect("rom");
            scan::scan(
                || de16::Board::new(rom.clone()),
                args[3].parse().unwrap(),
                args[4].parse().unwrap(),
                args[5].parse().unwrap(),
                &args[6],
            );
        }
        // de64-scan <rom0> <rom3> <first> <last> <cap secs> <outdir>: Sega 192x64 board (see scan.rs)
        Some("de64-scan") => {
            let (r0, r3) = (
                std::fs::read(&args[2]).expect("rom0"),
                std::fs::read(&args[3]).expect("rom3"),
            );
            scan::scan(
                || de64::Board::new(&r0, &r3),
                args[4].parse().unwrap(),
                args[5].parse().unwrap(),
                args[6].parse().unwrap(),
                &args[7],
            );
        }
        // gts3-scan <dsprom.bin> <first> <last> <cap secs> <outdir>: Gottlieb System 3 board (see
        // scan.rs); COMBINER=a|b|5 picks PinMAME's frame combiner
        Some("gts3-scan") => {
            let rom = std::fs::read(&args[2]).expect("rom");
            let comb = std::env::var("COMBINER").unwrap_or("a".into());
            scan::scan(
                || gts3::Board::new(rom.clone(), &comb),
                args[3].parse().unwrap(),
                args[4].parse().unwrap(),
                args[5].parse().unwrap(),
                &args[6],
            );
        }
        // alvg-scan <u4 program rom> <u5 data rom> <first> <last> <cap secs> <outdir>: Alvin G.
        // PCA020A board (see scan.rs; packets have no length byte)
        // alvg-dis <u4> <u5> <hex start> <count> [bank]: disassemble the board's code
        Some("alvg-dis") => {
            let (code, data) = (
                std::fs::read(&args[2]).expect("u4"),
                std::fs::read(&args[3]).expect("u5"),
            );
            let mut b = alvg::Board::new(code, data);
            let bank = args.get(6).map(|v| v.parse().unwrap()).unwrap_or(0);
            for l in b.disassemble(
                u32::from_str_radix(&args[4], 16).unwrap(),
                args[5].parse().unwrap(),
                bank,
            ) {
                println!("{l}");
            }
        }
        // alvg-trace <u4> <u5> <boot secs> <bytes,...>: boot, send the bytes, print the state
        Some("alvg-trace") => {
            use scan::Dmd;
            let (code, data) = (
                std::fs::read(&args[2]).expect("u4"),
                std::fs::read(&args[3]).expect("u5"),
            );
            let mut b = alvg::Board::new(code, data);
            let boot: f64 = args[4].parse().unwrap();
            for _ in 0..10 {
                b.run((boot * alvg::CLOCK_HZ as f64 / 10.0) as u64);
                println!("{}", b.state());
            }
            for x in args[5].split(',') {
                b.send(u8::from_str_radix(x, 16).unwrap());
                for _ in 0..5 {
                    b.run(alvg::CLOCK_HZ / 1000);
                    println!("{x}: {}", b.state());
                }
            }
            for _ in 0..10 {
                b.run(alvg::CLOCK_HZ / 10);
                println!("{}", b.state());
            }
        }
        Some("alvg-scan") => {
            let (code, data) = (
                std::fs::read(&args[2]).expect("u4"),
                std::fs::read(&args[3]).expect("u5"),
            );
            scan::scan(
                || alvg::Board::new(code.clone(), data.clone()),
                args[4].parse().unwrap(),
                args[5].parse().unwrap(),
                args[6].parse().unwrap(),
                &args[7],
            );
        }
        // sam-*: Stern S.A.M. main board (see sam.rs)
        Some(c) if c.starts_with("sam-") => sam::cli(&args),
        // cap-*: Capcom main board (see capcom.rs)
        Some(c) if c.starts_with("cap-") => capcom::cli(&args),
        // wpc-export <scandir> <game> <outdir>: classify and export a wpc-scan output
        Some("wpc-export") => {
            if args.len() < 5 {
                eprintln!("usage: rom2dmd wpc-export <scandir> <game> <outdir>");
                std::process::exit(2);
            }
            let res = export::wpc::read_scan_dir(std::path::Path::new(&args[2]), &args[3])
                .and_then(|p| export::wpc::export(&p, std::path::Path::new(&args[4])));
            match res {
                Ok(s) => println!("{} {:?} total {}", args[3], s.counts, s.total),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        // scene-export <scandir> <scan.log> <outdir> [--blend] [--pal v,v,...]: export the text
        // dumps of a de16/de32/de64/gts3/alvg/cap/sam scan
        Some("scene-export") => {
            if let Err(e) = scene_export_cli(&args) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        // wpc-*: Williams WPC CPU board, DMD generation (see wpc/mod.rs)
        Some(c) if c.starts_with("wpc-") => wpc::cli(&args),
        _ => eprintln!("usage: rom2dmd de32-boot <dmd.rom> <secs> <out.txt>"),
    }
}
