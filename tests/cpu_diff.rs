//! Differential test: mc6809-core vs the CPU of WPC-EMU, single-step on random states.
//! Reference file produced by `diff/gen.js` (see ROM2DMD_CPU_REF); skipped when absent.
use mc6809_core::{Cpu, Memory};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    i: u32,
    op: u8,
    #[serde(default)]
    illegal: bool,
    #[serde(default)]
    init: Vec<u32>,
    #[serde(default)]
    cycles: u64,
    #[serde(default)]
    fin: Vec<u32>,
    #[serde(default)]
    writes: Vec<(u16, u8)>,
}

struct Ram {
    m: Vec<u8>,
    w: Vec<(u16, u8)>,
}
impl Memory for Ram {
    fn read(&mut self, a: u16) -> u8 {
        self.m[a as usize]
    }
    fn write(&mut self, a: u16, v: u8) {
        self.w.push((a, v));
        self.m[a as usize] = v;
    }
}

fn xorshift(seed: u32) -> impl FnMut() -> u32 {
    let mut s = if seed == 0 { 1 } else { seed };
    move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    }
}

/// Known, understood deviations of WPC-EMU's CPU from the 6809 datasheet (the crate is right):
/// - NEG (00/40/50/60/70) does not set C; DAA (19) computes C/V differently;
/// - CWAI (3C) is flagged "broken" in WPC-EMU itself;
/// - undefined indexed postbytes xxx01111 without indirect bit;
/// - cycle counts: indexed modes (-3) and 10/11 prefixes (-1) are counted differently;
/// - 16-bit stores/pushes write the two bytes in a different order.
fn explained(c: &Case, got: &[u32], cyc: u64, w: &[(u16, u8)], m: &[u8]) -> bool {
    if c.op == 0x3c {
        return true;
    }
    let indexed = matches!(c.op, 0x30..=0x33 | 0x60..=0x6f | 0xa0..=0xaf | 0xe0..=0xef);
    if indexed && m[(c.init[7] as usize + 1) & 0xffff] & 0x8f == 0x8f {
        return true;
    }
    let (mut a, mut b) = (w.to_vec(), c.writes.clone());
    a.sort();
    b.sort();
    if a != b {
        return false;
    }
    let cc_ok = got[8] == c.fin[8]
        || (matches!(c.op, 0x00 | 0x40 | 0x50 | 0x60 | 0x70) && (got[8] ^ c.fin[8]) == 1)
        || c.op == 0x19;
    got[..8] == c.fin[..8]
        && cc_ok
        && (cyc == c.cycles || cyc + 3 == c.cycles || cyc + 1 == c.cycles)
}

#[test]
fn matches_wpc_emu_cpu() {
    let Ok(path) = std::env::var("ROM2DMD_CPU_REF") else {
        eprintln!("ROM2DMD_CPU_REF not set, skipped");
        return;
    };
    let Ok(txt) = std::fs::read_to_string(&path) else {
        eprintln!("no reference at {path}, skipped");
        return;
    };
    let cases: Vec<Case> = serde_json::from_str(&txt).unwrap();
    let (mut total, mut unexplained) = (0u32, 0u32);
    for c in cases.iter().filter(|c| !c.illegal) {
        let mut r = xorshift(c.i.wrapping_add(1).wrapping_mul(2654435761));
        let m: Vec<u8> = (0..65536).map(|_| (r() & 0xff) as u8).collect();
        let (a, b, x, y, u, s, dp, pc, cc) = (
            r() & 255,
            r() & 255,
            r() & 65535,
            r() & 65535,
            r() & 65535,
            r() & 65535,
            r() & 255,
            r() & 65535,
            r() & 0xff & !0x80,
        );
        assert_eq!(
            c.init,
            vec![a, b, x, y, u, s, dp, pc, cc],
            "rng desync on case {}",
            c.i
        );
        let mut ram = Ram { m, w: vec![] };
        let mut cpu = Cpu::new();
        {
            let mut g = cpu.registers_mut();
            g.d = ((a << 8) | b) as u16;
            g.x = x as u16;
            g.y = y as u16;
            g.u = u as u16;
            g.s = s as u16;
            g.dp = dp as u8;
            g.pc = pc as u16;
            g.cc = mc6809_core::ConditionCodes::from_byte(cc as u8);
        }
        let cyc = cpu.step(&mut ram);
        let g = cpu.registers();
        let got = vec![
            g.a() as u32,
            g.b() as u32,
            g.x as u32,
            g.y as u32,
            g.u as u32,
            g.s as u32,
            g.dp as u32,
            g.pc as u32,
            g.cc.to_byte() as u32,
        ];
        total += 1;
        if !explained(c, &got, cyc, &ram.w, &ram.m) {
            unexplained += 1;
            eprintln!(
                "op {:02X} case {} init {:02X?}: got {got:02X?} want {:02X?} cyc {cyc}/{}",
                c.op, c.i, c.init, c.fin, c.cycles
            );
        }
    }
    eprintln!("{unexplained} unexplained differences in {total} legal single-step cases");
    assert_eq!(unexplained, 0);
}
