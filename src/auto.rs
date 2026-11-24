//! `rom2dmd <rom.zip> <outdir>`: identify the board family and the game, find the per-game
//! addresses in the code, scan every display scene on all cores and write the export.
//!
//! What each family needs, all found from the ROM set:
//! - WPC: the effect starter, task creator/free, protect and bypass points, priority and skip
//!   bytes (`wpc::probe`); switch names from WPC-EMU's database when the game is in it.
//! - Data East / Whitestar 128x32, Data East 128x16, Sega 192x64: nothing (the main CPU only
//!   sends packets: `02 9b id`, `02 1b id`).
//! - Gottlieb System 3: the start sequence carries a key, the 16-bit byte sum of the main
//!   CPU ROM (`bb 01 hi lo`); the shading combiner per game is PinMAME's.
//! - Alvin G.: the "next byte" routine and the main loop's return address
//!   (`alvg::Cfg::probe`).
//! - Capcom: DM_ADD, the current-entry word or the slot table, and the scenes (DM_ADD call
//!   sites) (`capcom::probe`), plus an NVRAM made by a first boot.
//! - Stern SAM: deff_start, current effect and task, task_sleep, effect count (`sam::probe`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::export::scenes::{self, Scene};
use crate::export::{Analysis, Geometry, wpc as xwpc};
use crate::romset::{Family, RomSet};
use crate::scan::{self, Cfg as ScanCfg};

pub const USAGE: &str =
    "usage: rom2dmd <rom.zip> <outdir> [--threads n] [--ids a-b,c,...] [--keep-work]";

/// Options of a scan.
#[derive(Clone, Debug)]
pub struct Opts {
    pub threads: usize,
    pub ids: Option<Vec<u32>>,
    /// Write the WPC scan's chunk files into this folder.
    pub keep_work: Option<PathBuf>,
}

#[cfg(feature = "export")]
fn parse_ids(s: &str) -> Option<Vec<u32>> {
    let mut v = Vec::new();
    for p in s.split(',').filter(|x| !x.is_empty()) {
        match p.split_once('-') {
            Some((a, b)) => v.extend(a.parse::<u32>().ok()?..=b.parse::<u32>().ok()?),
            None => v.push(p.parse().ok()?),
        }
    }
    Some(v)
}

#[cfg(feature = "export")]
pub fn cli(args: &[String]) -> Result<(), String> {
    if args.len() < 3 {
        return Err(USAGE.into());
    }
    let mut o = Opts {
        threads: std::thread::available_parallelism().map_or(4, |n| n.get()),
        ids: None,
        keep_work: None,
    };
    let mut it = args[3..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--threads" => {
                o.threads = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|&n| n > 0)
                    .ok_or(USAGE)?
            }
            "--ids" => o.ids = Some(it.next().and_then(|v| parse_ids(v)).ok_or(USAGE)?),
            "--keep-work" => o.keep_work = Some(Path::new(&args[2]).join("scan")),
            _ => return Err(format!("unknown option {a}\n{USAGE}")),
        }
    }
    run(Path::new(&args[1]), Path::new(&args[2]), &o)
}

/// `rom2dmd identify <rom.zip>...`: family, game and the addresses each scan would use,
/// without scanning.
pub fn identify_cli(args: &[String]) -> Result<(), String> {
    let mut failed = 0;
    for z in &args[2..] {
        match identify(Path::new(z)) {
            Ok(l) => println!("{l}"),
            Err(e) => {
                failed += 1;
                println!("{z}: ERROR {e}");
            }
        }
    }
    if failed > 0 {
        return Err(format!("{failed} of {} not automatic", args.len() - 2));
    }
    Ok(())
}

/// One line: set, family and what the probes found (an error when a probe fails).
pub fn identify(zip: &Path) -> Result<String, String> {
    let rs = crate::romset::open(zip)?;
    let what = match rs.family {
        Family::Wpc(_) => {
            let rom = if rs.roms.is_empty() {
                crate::wpc::load_rom_files(&rs.files)?
            } else {
                rs.rom(0)?.to_vec()
            };
            let p = crate::wpc::probe::probe(&rom);
            let missing = p.missing();
            if !missing.is_empty() {
                return Err(format!(
                    "{}: WPC probe incomplete, missing {missing:?}",
                    rs.set
                ));
            }
            let db = crate::wpc::scan::resolve_game(&rs.set, None)?.0.is_some();
            format!(
                "{}{}",
                p.line(),
                if db {
                    ""
                } else {
                    " (no switch database entry)"
                }
            )
        }
        Family::De32 | Family::Ws32 | Family::De16 | Family::De64 => {
            for k in 0..rs.roms.len() {
                rs.rom(k)?;
            }
            "no addresses needed".into()
        }
        Family::Gts3 => {
            rs.rom(0)?;
            let key = rs
                .rom(1)?
                .iter()
                .fold(0u16, |s, &b| s.wrapping_add(b as u16));
            format!("key {key:04x} combiner {}", gts3_combiner(&rs.set))
        }
        Family::Alvg | Family::AlvgSplit => {
            let d = rs.rom(0)?;
            let split = rs.family == Family::AlvgSplit;
            let code = if split {
                let hi = &d[d.len().saturating_sub(0x8000)..];
                [hi, hi].concat()
            } else {
                d.to_vec()
            };
            let c = crate::alvg::Cfg::probe(&code, split)
                .ok_or_else(|| format!("{}: Alvin G. command routine not found", rs.set))?;
            format!("get byte {:04x} idle return {:04x}", c.get_byte, c.idle_ret)
        }
        Family::Capcom => {
            let p = crate::capcom::probe(&crate::capcom::image(&rs.files, &rs.set))
                .ok_or_else(|| format!("{}: DM_ADD not found", rs.set))?;
            if !crate::capcom::known_set(&rs.set) {
                return Err(format!("Capcom set {} has no hardware entry", rs.set));
            }
            format!(
                "DM_ADD {:08x} {:x?} {} scenes",
                p.dmadd,
                p.running,
                p.scenes.len()
            )
        }
        Family::Sam => {
            let p = crate::sam::probe(rs.rom(0)?)
                .ok_or_else(|| format!("{}: SAM display-effect API not found", rs.set))?;
            format!("{p:x?}")
        }
    };
    Ok(format!("{}: {} {what}", rs.set, rs.family.name()))
}

/// Scan and analyse an opened set: the analysis and the scan note (`scan.txt`).
pub fn analyse(rs: &RomSet, o: &Opts) -> Result<(Analysis, String), String> {
    log::info!(
        "{}: {} ({})",
        rs.set,
        rs.family.name(),
        if rs.roms.is_empty() {
            "found by content".to_string()
        } else {
            rs.roms.join(", ")
        }
    );
    match rs.family {
        Family::Wpc(g) => wpc(rs, g, o),
        _ => {
            let (scenes, opts, note) = board_scenes(rs, o)?;
            log::info!("{}: analysing", rs.set);
            let note = format!(
                "rom2dmd {} set {} family {}\n{note}",
                env!("CARGO_PKG_VERSION"),
                rs.set,
                rs.family.name()
            );
            Ok((scenes::analyse(&scenes, &opts), note))
        }
    }
}

/// Identify, scan and export one ROM zip into `outdir`: the scene folders, `manifest.json`,
/// `scan.txt` and the DMD dump `<set>.txt`.
#[cfg(feature = "export")]
pub fn run(zip: &Path, outdir: &Path, o: &Opts) -> Result<(), String> {
    let t = std::time::Instant::now();
    let rs = crate::romset::open(zip)?;
    let (an, note) = analyse(&rs, o)?;
    log::info!("{}: writing", rs.set);
    let summary = crate::export::write(&an, outdir)?;
    let p = outdir.join("scan.txt");
    std::fs::write(&p, note).map_err(|e| format!("{}: {e}", p.display()))?;
    let x = crate::Extraction::from_analysis(&rs, an, String::new());
    let p = outdir.join(format!("{}.txt", rs.set));
    let f = std::fs::File::create(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    x.write_dump(std::io::BufWriter::new(f))
        .map_err(|e| format!("{}: {e}", p.display()))?;
    log::info!(
        "{}: {:?} total {} in {:.0} s -> {}",
        rs.set,
        summary.counts,
        summary.total,
        t.elapsed().as_secs_f64(),
        outdir.display()
    );
    Ok(())
}

/// A progress line per finished id.
fn progress(set: &str, total: usize) -> impl Fn(&Scene) + Sync {
    let done = AtomicUsize::new(0);
    let set = set.to_string();
    move |s: &Scene| {
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        log::info!("{set}: id {} {} ({n}/{total})", s.id, s.kind);
    }
}

fn ids_or(o: &Opts, default: impl Iterator<Item = u32>) -> Vec<u32> {
    o.ids.clone().unwrap_or_else(|| default.collect())
}

/// The boards whose scans record finished pictures: scenes, export options and a note.
fn board_scenes(rs: &RomSet, o: &Opts) -> Result<(Vec<Scene>, scenes::Opts, String), String> {
    let lin = |w, h, n| scenes::Opts {
        geometry: Geometry::linear(w, h, n),
        blend: false,
    };
    let sc = |boot_s: f64, pre: &str, packet: &str, gap: Option<u64>| ScanCfg {
        boot_s,
        pre: pre.into(),
        packet: packet.into(),
        gap,
        bootrec: false,
        cap_s: 30.0,
    };
    Ok(match rs.family {
        Family::De32 | Family::Ws32 => {
            let mut rom = Vec::new();
            for k in 0..rs.roms.len() {
                rom.extend_from_slice(rs.rom(k)?);
            }
            let ws = rs.family == Family::Ws32;
            let fc = if ws {
                crate::de32::CYCLES_PER_FRAME_WS
            } else {
                crate::de32::CYCLES_PER_FRAME_DE
            };
            // a Whitestar board ignores commands for ~5 s after reset
            let c = sc(if ws { 7.0 } else { 3.0 }, "", "2,9b,{id}", Some(400));
            let ids = ids_or(o, 0..=255);
            let p = progress(&rs.set, ids.len());
            let s = scan::scan_par(
                || crate::de32::Board::new(rom.clone(), fc),
                &ids,
                &c,
                o.threads,
                &p,
            );
            (s, lin(128, 32, 4), "scenes: packet 02 9b id\n".into())
        }
        Family::De16 => {
            let rom = rs.rom(0)?.to_vec();
            let c = sc(1.0, "0,w3000,1,1a,w500", "2,1b,{id}", None);
            let ids = ids_or(o, 0..=255);
            let p = progress(&rs.set, ids.len());
            let s = scan::scan_par(
                || crate::de16::Board::new(rom.clone()),
                &ids,
                &c,
                o.threads,
                &p,
            );
            (s, lin(128, 16, 4), "scenes: packet 02 1b id\n".into())
        }
        Family::De64 => {
            let (r0, r3) = (rs.rom(0)?, rs.rom(1)?);
            let c = sc(3.0, "", "2,9b,{id}", None);
            let ids = ids_or(o, 0..=255);
            let p = progress(&rs.set, ids.len());
            let s = scan::scan_par(|| crate::de64::Board::new(r0, r3), &ids, &c, o.threads, &p);
            (s, lin(192, 64, 4), "scenes: packet 02 9b id\n".into())
        }
        Family::Gts3 => {
            let (dsp, main) = (rs.rom(0)?.to_vec(), rs.rom(1)?);
            // the start sequence's key: the main CPU ROM's 16-bit byte sum
            let key = main.iter().fold(0u16, |s, &b| s.wrapping_add(b as u16));
            let comb = gts3_combiner(&rs.set);
            let pre = format!(
                "bb,23{},bb,25{},bb,26,0,0,0,0,cc,21,w900,bb,1,{:x},{:x},w1000,cc,29,bb,4,0,bb,26,0,0,0,0,w1000",
                ",0".repeat(24),
                ",0".repeat(10),
                key >> 8,
                key & 0xff
            );
            let c = sc(0.2, &pre, "cc,{id}", None);
            let ids = ids_or(o, 0..=255);
            let p = progress(&rs.set, ids.len());
            let s = scan::scan_par(
                || crate::gts3::Board::new(dsp.clone(), comb),
                &ids,
                &c,
                o.threads,
                &p,
            );
            let mut opts = lin(128, 32, if comb == "5" { 5 } else { 4 });
            opts.blend = true;
            (
                s,
                opts,
                format!("scenes: packet cc id; start key {key:04x}; combiner {comb}\n"),
            )
        }
        Family::Alvg | Family::AlvgSplit => {
            let split = rs.family == Family::AlvgSplit;
            let code = if split {
                // the program is the upper 32 KiB of the ROM (seen twice in the 64 KiB space)
                let d = rs.rom(0)?;
                let hi = &d[d.len().saturating_sub(0x8000)..];
                [hi, hi].concat()
            } else {
                rs.rom(0)?.to_vec()
            };
            let mut data = Vec::new();
            for k in 1..rs.roms.len() {
                data.extend_from_slice(rs.rom(k)?);
            }
            let cfg = crate::alvg::Cfg::probe(&code, split).ok_or_else(|| {
                format!(
                    "{}: Alvin G. command routine not found in the program ROM",
                    rs.set
                )
            })?;
            let pre = if split {
                "ff,w300,90,00,w1500"
            } else {
                "ff,w300,90,00,09,18,08,2e,w1500"
            };
            let c = sc(6.0, pre, "{id}", None);
            let ids = ids_or(o, 0x80..=0xff);
            let p = progress(&rs.set, ids.len());
            let s = scan::scan_par(
                || crate::alvg::Board::with_cfg(code.clone(), data.clone(), cfg),
                &ids,
                &c,
                o.threads,
                &p,
            );
            (
                s,
                lin(128, 32, 5),
                format!(
                    "scenes: command bytes 80-ff; get byte {:04x}, idle return {:04x}\n",
                    cfg.get_byte, cfg.idle_ret
                ),
            )
        }
        Family::Capcom => capcom(rs, o)?,
        Family::Sam => sam(rs, o)?,
        Family::Wpc(_) => return Err("WPC is not a scene board".into()),
    })
}

/// PinMAME's shading combiner per Gottlieb System 3 game (gts3.c).
fn gts3_combiner(set: &str) -> &'static str {
    let s = set.to_lowercase();
    if s.starts_with("smb") || s.starts_with("cueball") {
        "5"
    } else if [
        "stargat", "bighurt", "waterwl", "andrett", "barbwire", "brooks", "snspare",
    ]
    .iter()
    .any(|p| s.starts_with(p))
    {
        "b"
    } else {
        "a"
    }
}

fn capcom(rs: &RomSet, o: &Opts) -> Result<(Vec<Scene>, scenes::Opts, String), String> {
    let rom = crate::capcom::image(&rs.files, &rs.set);
    let p = crate::capcom::probe(&rom)
        .ok_or_else(|| format!("{}: Capcom display manager (DM_ADD) not found", rs.set))?;
    // a first boot on a fresh NVRAM gets past the "NVRAM corrupted" message; every scene
    // then starts from that NVRAM
    let boot = crate::capcom::nvram_boot_s(&rs.set);
    log::info!(
        "{}: DM_ADD {:08x} {:?}, {} scenes; first boot {boot} s for the NVRAM",
        rs.set,
        p.dmadd,
        p.running,
        p.scenes.len()
    );
    let mut b = crate::capcom::board_for_set(rom.clone(), &rs.set, None)?;
    let (w, h) = (b.width, b.height);
    b.run((boot * crate::capcom::CLOCK_HZ as f64) as u64);
    let nv = b.bus.nvram.clone();
    drop(b);
    let ids = ids_or(o, 0..p.scenes.len() as u32);
    let pr = progress(&rs.set, ids.len());
    let out = crate::export::par_map_threads(&ids, o.threads, |&n| -> Result<Scene, String> {
        let scene = p
            .scenes
            .get(n as usize)
            .ok_or_else(|| format!("no scene {n}"))?;
        let b = crate::capcom::board_for_set(rom.clone(), &rs.set, Some(&nv))?;
        let s = crate::capcom::scan_scene(b, n, scene, &p, 3.0, 30.0);
        pr(&s);
        Ok(s)
    });
    let scenes = out.into_iter().collect::<Result<Vec<_>, _>>()?;
    let mut g = Geometry::linear(w, h, 4);
    g.palette = vec![0, 64, 128, 255];
    Ok((
        scenes,
        scenes::Opts {
            geometry: g,
            blend: false,
        },
        format!(
            "scenes: DM_ADD functions; DM_ADD {:08x} {:?}\n",
            p.dmadd, p.running
        ),
    ))
}

fn sam(rs: &RomSet, o: &Opts) -> Result<(Vec<Scene>, scenes::Opts, String), String> {
    let flash = rs.rom(0)?;
    let p = crate::sam::probe(flash)
        .ok_or_else(|| format!("{}: SAM display-effect API not found", rs.set))?;
    log::info!("{}: {p:x?}; booting 30 s", rs.set);
    let s0 = crate::sam::boot_for_deffs(Arc::new(flash.to_vec()), &rs.set, 30.0);
    let c = crate::sam::DeffCfg::from_probe(&p);
    let ids = ids_or(o, 1..p.n.max(1));
    let pr = progress(&rs.set, ids.len());
    let scenes = crate::export::par_map_threads(&ids, o.threads, |&id| {
        let s = crate::sam::deff_one(&s0, id, &c).0;
        pr(&s);
        s
    });
    Ok((
        scenes,
        scenes::Opts {
            geometry: Geometry::linear(128, 32, 16),
            blend: false,
        },
        format!("scenes: display effects 1..{}; {p:x?}\n", p.n),
    ))
}

/// WPC: probe, boot, the three passes, then the effect analysis.
fn wpc(
    rs: &RomSet,
    generation: crate::wpc::Generation,
    o: &Opts,
) -> Result<(Analysis, String), String> {
    use crate::wpc::scan as ws;
    let rom = if rs.roms.is_empty() {
        crate::wpc::load_rom_files(&rs.files)?
    } else {
        rs.rom(0)?.to_vec()
    };
    let (game, name) = ws::resolve_game(&rs.set, None)?;
    if game.is_none() {
        log::info!(
            "{}: not in the WPC-EMU switch database: no random play, attract-mode snapshot",
            rs.set
        );
    }
    let opts = ws::Opts {
        generation: Some(generation),
        ids: o.ids.clone(),
        threads: o.threads,
        ..ws::Opts::default()
    };
    let out = ws::run_scan(rom, game.as_ref(), &name, &opts)?;
    let mut passes = Vec::new();
    for (tag, chunks) in &out.passes {
        let pass = match *tag {
            "byp" => xwpc::Pass::Bypassed,
            "prot" => xwpc::Pass::Protected,
            _ => xwpc::Pass::Normal,
        };
        let (mut log, mut nd, mut raw) = (out.header.clone(), String::new(), String::new());
        for r in chunks.iter().flatten() {
            log.push_str(&r.log);
            nd.push_str(&r.nd);
            raw.push_str(&r.raw);
        }
        if let Some(work) = &o.keep_work {
            std::fs::create_dir_all(work).map_err(|e| format!("{}: {e}", work.display()))?;
            for (ext, text) in [("log", &log), ("effects.ndjson", &nd), ("raw", &raw)] {
                let p = work.join(format!("{name}_{tag}1.{ext}"));
                std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
            }
        }
        passes.push(xwpc::PassText { pass, log, nd, raw });
    }
    log::info!("{}: analysing", rs.set);
    Ok((xwpc::analyse(&passes)?, out.summary))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A set from the ROM collection ($ROM2DMD_ROMS, default ~/roms-full); None (the test is
    /// skipped) when absent.
    fn set(name: &str) -> Option<RomSet> {
        let dir = std::env::var("ROM2DMD_ROMS")
            .unwrap_or_else(|_| format!("{}/roms-full", std::env::var("HOME").unwrap_or_default()));
        let p = PathBuf::from(dir).join(format!("{name}.zip"));
        if !p.exists() {
            log::info!("{} absent, skipped", p.display());
            return None;
        }
        Some(crate::romset::open(&p).expect("ROM set"))
    }

    #[test]
    fn identifies_families() {
        for (name, fam) in [
            ("t2_l8", Family::Wpc(crate::wpc::Generation::Dmd)),
            ("lah_112", Family::De32),
            ("ckpt_a17", Family::De16),
            ("frankst", Family::De64),
            ("sfight2", Family::Gts3),
            ("wrldtour", Family::AlvgSplit),
            ("pmv112", Family::Capcom),
        ] {
            if let Some(rs) = set(name) {
                assert_eq!(rs.family, fam, "{name}");
            }
        }
    }

    /// The addresses found by hand during the first extractions, now found in the code.
    #[test]
    fn probes_find_the_hand_found_addresses() {
        if let Some(rs) = set("mystcast") {
            let c = crate::alvg::Cfg::probe(rs.rom(0).expect("u4"), false).expect("alvg");
            assert_eq!((c.get_byte, c.idle_ret), (0x0ae9, 0x006a));
        }
        if let Some(rs) = set("sfight2") {
            let main = rs.rom(1).expect("gprom");
            let key = main.iter().fold(0u16, |s, &b| s.wrapping_add(b as u16));
            assert_eq!(key, 0xa094); // the key PinMAME's main CPU sends (bb 01 a0 94)
        }
        if let Some(rs) = set("kpb105") {
            let p = crate::capcom::probe(&crate::capcom::image(&rs.files, &rs.set)).expect("cap");
            assert_eq!(p.dmadd, 0x1009_bfba);
            assert_eq!(
                p.running,
                crate::capcom::Running::Slots([0x336b0, 0x52, 0x10, 0x2a])
            );
            assert_eq!(p.scenes.len(), 98);
        }
        if let Some(rs) = set("pmv112") {
            let p = crate::capcom::probe(&crate::capcom::image(&rs.files, &rs.set)).expect("cap");
            assert_eq!(
                (p.dmadd, p.running),
                (0x1009_ca54, crate::capcom::Running::Cur(0xa996))
            );
        }
        if let Some(rs) = set("wpt_140a") {
            let p = crate::sam::probe(rs.rom(0).expect("flash")).expect("sam");
            assert_eq!(
                (p.start, p.hook, p.cur, p.n),
                (0x20668, 0x8ce0, 0x33d32, 0xa8)
            );
        }
    }

    #[test]
    fn combiners_follow_pinmame() {
        assert_eq!(gts3_combiner("cueball"), "5");
        assert_eq!(gts3_combiner("stargat5"), "b");
        assert_eq!(gts3_combiner("sfight2"), "a");
    }
}
