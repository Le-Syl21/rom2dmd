//! WPC-EMU's 6809 cycle counts, for the diagnostic mode that replays its timing exactly.
//!
//! mc6809-core counts cycles per the datasheet; WPC-EMU charges 3 more for indirect indexed
//! modes and 1 more for every 0x10/0x11-prefixed instruction. Interrupts land on different
//! instructions as soon as the counts differ, so a run is only comparable instruction for
//! instruction with WPC-EMU when the board is clocked with these counts. This is a measuring
//! tool for parity tests, not the default: the crate's counts are the hardware's.

use mc6809_core::Registers;

#[rustfmt::skip]
const CYCLES: [u8; 256] = [
    6, 0, 0, 6, 6, 0, 6, 6, 6, 6, 6, 0, 6, 6, 3, 6,
    1, 1, 2, 2, 0, 0, 5, 9, 0, 2, 3, 0, 3, 2, 8, 7,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    4, 4, 4, 4, 5, 5, 5, 5, 0, 5, 3, 6, 21, 11, 0, 19,
    2, 0, 0, 2, 2, 0, 2, 2, 2, 2, 2, 0, 2, 2, 0, 2,
    2, 0, 0, 2, 2, 0, 2, 2, 2, 2, 2, 0, 2, 2, 0, 2,
    6, 0, 0, 6, 6, 0, 6, 6, 6, 6, 6, 0, 6, 6, 3, 6,
    7, 0, 0, 7, 7, 0, 7, 7, 7, 7, 7, 0, 7, 7, 4, 7,
    2, 2, 2, 4, 2, 2, 2, 0, 2, 2, 2, 2, 4, 7, 3, 0,
    4, 4, 4, 6, 4, 4, 4, 4, 4, 4, 4, 4, 6, 7, 5, 5,
    4, 4, 4, 6, 4, 4, 4, 4, 4, 4, 4, 4, 6, 7, 5, 5,
    5, 5, 5, 7, 5, 5, 5, 5, 5, 5, 5, 5, 7, 8, 6, 6,
    2, 2, 2, 4, 2, 2, 2, 0, 2, 2, 2, 2, 3, 0, 3, 0,
    4, 4, 4, 6, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5,
    4, 4, 4, 6, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5,
    5, 5, 5, 7, 5, 5, 5, 5, 5, 5, 5, 5, 6, 6, 6, 6,
];

#[rustfmt::skip]
const CYCLES2: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 20,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 4, 0,
    0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 7, 0, 6, 6,
    0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 7, 0, 6, 6,
    0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 8, 0, 7, 7,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 6,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 6,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7, 7,
];

fn indexed(op: u8) -> bool {
    matches!(op, 0x30..=0x33 | 0x60..=0x6f | 0xa0..=0xaf | 0xe0..=0xef)
}

fn postbyte_cycles(pb: u8) -> u64 {
    if pb & 0x80 == 0 {
        return 1;
    }
    let mode = match pb & 0x0f {
        0x0 | 0x2 => 2,
        0x1 | 0x3 => 3,
        0x5 | 0x6 | 0x8 | 0xc => 1,
        0x9 | 0xb => 4,
        0xd | 0xf => 5,
        _ => 0,
    };
    mode + if pb & 0x10 != 0 { 3 } else { 0 }
}

fn stack_bytes(mask: u8) -> u64 {
    (0..8)
        .filter(|b| mask & (1 << b) != 0)
        .map(|b| if b >= 4 { 2 } else { 1 })
        .sum()
}

fn long_branch_taken(op: u8, cc: u8) -> bool {
    let (c, v, z, n) = (cc & 1 != 0, cc & 2 != 0, cc & 4 != 0, cc & 8 != 0);
    match op {
        0x22 => !(c || z),
        0x23 => c || z,
        0x24 => !c,
        0x25 => c,
        0x26 => !z,
        0x27 => z,
        0x28 => !v,
        0x29 => v,
        0x2a => !n,
        0x2b => n,
        0x2c => n == v,
        0x2d => n != v,
        0x2e => n == v && !z,
        0x2f => n != v || z,
        _ => false,
    }
}

/// Interrupt about to be taken by the next step, if any.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    None,
    Firq,
    Irq,
}

/// Cycles WPC-EMU charges for the next step: the instruction at PC, or an interrupt entry.
/// `peek` must read memory without side effects.
pub fn cycles(peek: impl Fn(u16) -> u8, r: &Registers, entry: Entry) -> u64 {
    match entry {
        Entry::Firq => return 10,
        Entry::Irq => return 19,
        Entry::None => {}
    }
    let pc = r.pc;
    let at = |k: u16| peek(pc.wrapping_add(k));
    let op = at(0);
    let mut n = CYCLES[op as usize] as u64;
    match op {
        0x10 | 0x11 => {
            let op2 = at(1);
            n += CYCLES2[op2 as usize] as u64;
            if op == 0x10 && long_branch_taken(op2, r.cc.to_byte()) {
                n += 1;
            }
            if matches!(op2, 0xa0..=0xaf | 0xe0..=0xef) {
                n += postbyte_cycles(at(2));
            }
        }
        0x34..=0x37 => n += stack_bytes(at(1)),
        // RTI: 9 more when the stacked CC has E set (entire state)
        0x3b => {
            if peek(r.s) & 0x80 != 0 {
                n += 9;
            }
        }
        _ if indexed(op) => n += postbyte_cycles(at(1)),
        _ => {}
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use mc6809_core::ConditionCodes;
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

    /// The cycle model must reproduce every count of WPC-EMU's single-step reference file
    /// (tests/cpu_diff.rs explains how it is made); skipped when absent.
    #[test]
    fn matches_wpc_emu_cycle_counts() {
        let Ok(path) = std::env::var("ROM2DMD_CPU_REF") else {
            log::info!("ROM2DMD_CPU_REF not set, skipped");
            return;
        };
        let Ok(txt) = std::fs::read_to_string(&path) else {
            log::info!("no reference at {path}, skipped");
            return;
        };
        let cases: Vec<Case> = serde_json::from_str(&txt).unwrap();
        let mut bad = 0;
        let mut total = 0;
        for c in cases.iter().filter(|c| !c.illegal) {
            let mut r = xorshift(c.i.wrapping_add(1).wrapping_mul(2654435761));
            let m: Vec<u8> = (0..65536).map(|_| (r() & 0xff) as u8).collect();
            let i = &c.init;
            let mut regs = Registers::new();
            regs.d = ((i[0] << 8) | i[1]) as u16;
            regs.x = i[2] as u16;
            regs.y = i[3] as u16;
            regs.u = i[4] as u16;
            regs.s = i[5] as u16;
            regs.dp = i[6] as u8;
            regs.pc = i[7] as u16;
            regs.cc = ConditionCodes::from_byte(i[8] as u8);
            // undefined indexed modes throw in WPC-EMU
            let pb = m[(regs.pc as usize + 1) & 0xffff];
            if indexed(c.op) && pb & 0x80 != 0 && matches!(pb & 0x0f, 0x7 | 0xa | 0xe) {
                continue;
            }
            total += 1;
            let got = cycles(|a| m[a as usize], &regs, Entry::None);
            if got != c.cycles {
                bad += 1;
                if bad < 20 {
                    log::info!(
                        "op {:02x} case {}: model {got} wpc-emu {}",
                        c.op,
                        c.i,
                        c.cycles
                    );
                }
            }
        }
        log::info!("{bad} mismatches in {total} cases");
        assert_eq!(bad, 0);
    }
}
