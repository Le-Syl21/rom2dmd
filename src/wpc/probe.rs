//! Finds, by code signatures, every address the WPC scene tracer needs in a game ROM, so a
//! scan needs no hand-given address. Port of the `wpcprobe.py` research tool (same searches,
//! same order, same output line), plus the "skip this effect" flag.
//!
//! All WPC DMD games share the same operating system, moved around between revisions: the
//! signatures are the OS routines' instruction sequences with their operand addresses left
//! as wildcards.

/// One byte of a signature: a fixed value or any byte.
#[derive(Clone, Copy)]
enum Tok {
    B(u8),
    Any,
}

use Tok::{Any, B};

/// `ANY` in a pattern string: "34 76 .. 1c" (`..` = any byte).
fn pat(s: &str) -> Vec<Tok> {
    s.split_whitespace()
        .map(|t| {
            if t == ".." {
                Any
            } else {
                // patterns are compile-time literals of this file
                B(u8::from_str_radix(t, 16).unwrap_or(0))
            }
        })
        .collect()
}

fn matches_at(data: &[u8], at: usize, p: &[Tok]) -> bool {
    at + p.len() <= data.len()
        && p.iter().zip(&data[at..]).all(|(t, &b)| match t {
            B(v) => *v == b,
            Any => true,
        })
}

/// First position >= `from` where one of the alternatives matches (tried in order at each
/// position, as a regex with an optional group does); returns (position, length).
fn find(data: &[u8], from: usize, alts: &[&[Tok]]) -> Option<(usize, usize)> {
    (from..data.len()).find_map(|i| {
        alts.iter()
            .find(|p| matches_at(data, i, p))
            .map(|p| (i, p.len()))
    })
}

fn word(d: &[u8], at: usize) -> u16 {
    ((d[at] as u16) << 8) | d[at + 1] as u16
}

/// What the probe found. Addresses are CPU addresses (the fixed ROM is at 0x8000-0xffff);
/// `toff` is a file offset in the ROM image.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// The effect starter (A = effect id).
    pub starter: Option<u16>,
    /// The starter's two refusal checks (each returns carry set to refuse).
    pub bypass: Option<[u16; 2]>,
    /// The task creator the starter calls (X = effect record, B = its bank).
    pub create: Option<u16>,
    /// The instruction that pushes a dead task onto the free list (U = the task).
    pub freepc: Option<u16>,
    /// The routine that unlinks one task of a group kill (X = the task).
    pub protect: Option<u16>,
    /// The effect table: CPU address, bank, file offset, number of records.
    pub tbase: Option<u16>,
    pub tb: Option<u8>,
    pub toff: Option<usize>,
    pub nids: Option<usize>,
    /// Direct-page address of the word holding the running task.
    pub curtask: Option<u8>,
    /// The running effect's priority byte, read by the second refusal check.
    pub prio: Option<u16>,
    /// The "skip this effect" flag a flipper press sets and each effect's watcher polls.
    pub zero: Option<u16>,
}

impl Probe {
    /// The fields the tracer cannot run without, by their `wpcprobe.py` names.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.starter.is_none() {
            m.push("STARTER");
        }
        if self.create.is_none() {
            m.push("CREATE");
        }
        if self.freepc.is_none() {
            m.push("FREEPC");
        }
        if self.protect.is_none() {
            m.push("PROTECT");
        }
        if self.toff.is_none() {
            m.push("TOFF");
        }
        m
    }

    /// The `wpcprobe.py` output line (docker `-e` arguments), then ` ZERO=` when found.
    pub fn line(&self) -> String {
        let mut a = Vec::new();
        let h = |k: &str, v: Option<u32>| v.map(|v| format!("-e {k}={v:x}"));
        a.extend(h("STARTER", self.starter.map(u32::from)));
        a.extend(h("CREATE", self.create.map(u32::from)));
        a.extend(h("FREEPC", self.freepc.map(u32::from)));
        a.extend(h("PROTECT", self.protect.map(u32::from)));
        a.extend(h("TBASE", self.tbase.map(u32::from)));
        a.extend(h("TB", self.tb.map(u32::from)));
        a.extend(h("CURTASK", self.curtask.map(u32::from)));
        a.extend(h("PRIO", self.prio.map(u32::from)));
        if let Some([x, y]) = self.bypass {
            a.push(format!("-e BYPASS={x:x},{y:x}"));
        }
        if let Some(t) = self.toff {
            a.push(format!("-e TOFF={t}"));
        }
        if let Some(n) = self.nids {
            a.push(format!("-e NIDS={n}"));
        }
        let mut s = a.join(" ");
        let m = self.missing();
        if !m.is_empty() {
            let q: Vec<String> = m.iter().map(|k| format!("'{k}'")).collect();
            s.push_str(&format!("   # MISSING [{}]", q.join(", ")));
        }
        s
    }
}

/// Probe a U6 game ROM image.
pub fn probe(r: &[u8]) -> Probe {
    let mut out = Probe::default();
    let n = r.len();
    if n < 0x10000 {
        return out;
    }
    let nb = n / 0x4000;
    let first_bank = 0x40_usize.saturating_sub(nb);
    let fixed = &r[n - 0x8000..];
    let fa = |off: usize| (0x8000 + off) as u16;

    // STARTER: PSHS U,Y,X,B,A; [LEAS -1,S;] ANDCC #FE; JSR x; SUBA #x; BCC +3; JMP x — then,
    // in its first 0x100 bytes, three JSR/BCS pairs: the two refusal checks and the creator
    let s1 = pat("34 76 32 7f 1c fe bd .. .. 80 .. 24 03 7e");
    let s2 = pat("34 76 1c fe bd .. .. 80 .. 24 03 7e");
    let c = pat("bd .. .. 25 .. bd .. .. 25 .. bd .. .. 25 ..");
    let mut from = 0;
    while let Some((at, len)) = find(fixed, from, &[&s1, &s2]) {
        let body = &fixed[at..(at + 0x100).min(fixed.len())];
        if let Some((k, _)) = find(body, 0, &[&c]) {
            out.starter = Some(fa(at));
            out.bypass = Some([word(body, k + 1), word(body, k + 6)]);
            out.create = Some(word(body, k + 11));
            break;
        }
        from = at + len;
    }

    // FREEPC: LDX free; STX ,U; STU free; INC count -> the STU
    let f = pat("be .. .. af c4 ff .. .. 7c");
    let mut from = 0;
    while let Some((at, _)) = find(fixed, from, &[&f]) {
        if fixed[at + 6..at + 8] == fixed[at + 1..at + 3] {
            out.freepc = Some(fa(at + 5));
            break;
        }
        from = at + 1;
    }

    // PROTECT: PSHS U,B,A; [BSR x;] LDD ,X; STD ,U; LEAU ,X
    let p1 = pat("34 46 8d .. ec 84 ed c4 33 84");
    let p2 = pat("34 46 ec 84 ed c4 33 84");
    if let Some((at, _)) = find(fixed, 0, &[&p1, &p2]) {
        out.protect = Some(fa(at));
    }

    // effect table: the longest run of 5-byte records (.., .., entry in the bank window, bank
    // number) that starts on a record whose entry is in the fixed ROM (bank 0xff)
    let ok = |o: usize| {
        let e = word(r, o + 2);
        let b = r[o + 4] as usize;
        if (0x8000..0xff00).contains(&e) && b == 0xff && r[o] != 0xff && r[o + 1] != 0xff {
            return true;
        }
        (0x4000..0x8000).contains(&e) && first_bank <= b && b < first_bank + nb - 2
    };
    let lim = n - 0x8000 - 5;
    let mut run = vec![0usize; lim + 5];
    for o in (0..lim).rev() {
        run[o] = if ok(o) { 1 + run[o + 5] } else { 0 };
    }
    let mut best = (0usize, 0usize);
    for o in 0..lim {
        if run[o] > best.1 && r[o + 4] == 0xff {
            best = (o, run[o]);
        }
    }
    let (toff, nids) = best;
    if nids > 20 {
        out.toff = Some(toff);
        out.nids = Some(nids);
        out.tb = Some((first_bank + toff / 0x4000) as u8);
        out.tbase = Some((0x4000 + toff % 0x4000) as u16);
    }

    // CURTASK: the direct-page word the scheduler stores the running task in:
    // LDD #0; STD ,X; LDU #list; STU <cur; STU >..  (or STU <cur; STU <..; LDB $13,U)
    let k1 = pat("cc 00 00 ed 84 ce .. .. df .. ff");
    let k2 = pat("df .. df .. e6 c8 13");
    if let Some((at, _)) = find(fixed, 0, &[&k1]) {
        out.curtask = Some(fixed[at + 9]);
    } else if let Some((at, _)) = find(fixed, 0, &[&k2]) {
        out.curtask = Some(fixed[at + 1]);
    }

    // PRIO: in the second refusal check, LDA prio; CMPA 1,S; BHI refuse; BLO allow
    if let Some([_, b2]) = out.bypass {
        let b2 = (b2 as usize).wrapping_sub(0x8000);
        if b2 < fixed.len() {
            let win = &fixed[b2..(b2 + 0x60).min(fixed.len())];
            if let Some((at, _)) = find(win, 0, &[&pat("b6 .. .. a1 61 22 .. 25 ..")]) {
                out.prio = Some(word(win, at + 1));
            }
        }
    }

    // ZERO: the effect watcher's skip test, PSHS A; LDA flag; BEQ +3; CLR flag; CMPA #1;
    // PULS A,PC (a flipper press sets the flag; the watcher kills its effect when set)
    let z = pat("34 02 b6 .. .. 27 03 7f .. .. 81 01 35 82");
    let mut from = 0;
    while let Some((at, _)) = find(fixed, from, &[&z]) {
        if fixed[at + 3..at + 5] == fixed[at + 8..at + 10] {
            out.zero = Some(word(fixed, at + 3));
            break;
        }
        from = at + 1;
    }
    out
}

/// `rom2dmd wpc-probe <rom.zip|u6.rom>...`: one line per ROM, `name <wpcprobe.py line>` then
/// ` ZERO=<hex>` when the skip flag was found.
pub fn cli(args: &[String]) -> Result<(), String> {
    if args.len() < 3 {
        return Err("usage: rom2dmd wpc-probe <rom.zip|u6.rom>...".into());
    }
    for path in &args[2..] {
        let rom = super::load_rom(path, None)?;
        let p = probe(&rom);
        let name = std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let z = p.zero.map(|z| format!(" ZERO={z:x}")).unwrap_or_default();
        println!("{name} {}{z}", p.line());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom(zip: &str) -> Option<Vec<u8>> {
        let dir = std::env::var("ROM2DMD_ROMS")
            .unwrap_or_else(|_| format!("{}/roms-full", std::env::var("HOME").unwrap_or_default()));
        let path = format!("{dir}/{zip}");
        if !std::path::Path::new(&path).exists() {
            eprintln!("{path} absent, skipped");
            return None;
        }
        super::super::load_rom(&path, None).ok()
    }

    /// The addresses found by hand on T2 and AFM (and by `wpcprobe.py`).
    #[test]
    fn t2_and_afm_addresses() {
        if let Some(r) = rom("t2_l8.zip") {
            let p = probe(&r);
            assert_eq!(
                p.line(),
                "-e STARTER=c791 -e CREATE=c9f2 -e FREEPC=9ae3 -e PROTECT=9b24 -e TBASE=5d1d \
                 -e TB=3d -e CURTASK=53 -e PRIO=481 -e BYPASS=c8af,c8cb -e TOFF=482589 -e NIDS=75"
            );
            assert_eq!(p.zero, Some(0x488));
        }
        if let Some(r) = rom("afm_113.zip") {
            let p = probe(&r);
            assert_eq!(p.starter, Some(0xc7de));
            assert_eq!(p.create, Some(0xca68));
            assert_eq!(p.nids, Some(138));
            assert_eq!(p.prio, Some(0x443));
        }
    }

    #[test]
    fn incomplete_probe_names_missing_fields() {
        let p = probe(&vec![0u8; 0x20000]);
        assert_eq!(
            p.missing(),
            vec!["STARTER", "CREATE", "FREEPC", "PROTECT", "TOFF"]
        );
        assert!(p.line().contains("MISSING"));
    }
}
