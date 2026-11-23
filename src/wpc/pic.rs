//! WPC-S / WPC-95 security PIC (U22), which sits between the CPU and the switch matrix.
//! Serial number algorithm from PinMAME `wpc.c` (via WPC-EMU `security-pic.js`).

const PIC_RESET: u8 = 0x00;
const PIC_COUNTER: u8 = 0x0d;
const PIC_UNLOCK: u8 = 0x20;
const SCRAMBLER_INIT: u8 = 0xa5;

#[derive(Clone)]
pub struct Pic {
    serial: [u8; 16],
    original: [u8; 16],
    scrambler: u8,
    last_write: u8,
    writes_until_unlock: u8,
    unlock_bytes_left: u8,
}

impl Pic {
    /// `machine` is the game number (e.g. 541 for Attack from Mars).
    pub fn new(machine: u16) -> Self {
        // "<machine> <serial> <serial 2> <unlock key>", as printed on the PIC label
        let label = format!("{machine:03} 123456 12345 123");
        let s = label.as_bytes();
        let d = |i: usize| s.get(i).map(|&c| c as i64 - b'0' as i64).unwrap_or(0);
        let mut n = [0u8; 16];
        n[10] = 0x12;
        n[2] = 0x34;
        let mut t = 100 * d(1) + 10 * d(8) + d(5) + 5 * n[10] as i64;
        t = t * 0x1bcd + 0x1f3f0;
        n[1] = (t >> 16) as u8;
        n[11] = (t >> 8) as u8;
        n[9] = t as u8;
        t = 10000 * d(2) + 1000 * d(18) + 100 * d(0) + 10 * d(9) + d(7);
        t += 2 * n[10] as i64 + n[2] as i64;
        t = t * 0x107f + 0x71e259;
        n[7] = (t >> 24) as u8;
        n[12] = (t >> 16) as u8;
        n[0] = (t >> 8) as u8;
        n[8] = t as u8;
        t = 1000 * d(19) + 100 * d(4) + 10 * d(6) + d(17) + n[2] as i64;
        t = t * 0x245 + 0x3d74;
        n[3] = (t >> 16) as u8;
        n[14] = (t >> 8) as u8;
        n[6] = t as u8;
        t = 10000 * (9 - d(15))
            + 1000 * (9 - d(14))
            + 100 * (9 - d(13))
            + 10 * (9 - d(12))
            + (9 - d(11));
        n[15] = (t >> 8) as u8;
        n[4] = t as u8;
        Pic {
            serial: n,
            original: n,
            scrambler: SCRAMBLER_INIT,
            last_write: 0xff,
            writes_until_unlock: 0x20,
            unlock_bytes_left: 0,
        }
    }

    pub fn reset(&mut self) {
        self.last_write = 0xff;
        self.writes_until_unlock = 0x20;
        self.serial = self.original;
    }

    /// Read of 0x3fe9; `row(col)` gives the switch rows of matrix column `col` (1..=10).
    pub fn read(&mut self, row: impl Fn(usize) -> u8) -> u8 {
        let w = self.last_write;
        if w == PIC_COUNTER {
            return self.writes_until_unlock;
        }
        if (0x16..=0x1f).contains(&w) {
            return row((w - 0x15) as usize);
        }
        if w & 0xf0 == 0x70 {
            let ret = self.serial[(w & 0x0f) as usize];
            self.scrambler = (self.scrambler >> 4) | (w << 4);
            self.serial[5] = (self.serial[5] ^ self.scrambler).wrapping_add(self.serial[13]);
            self.serial[13] = self.serial[13].wrapping_add(self.scrambler) ^ self.serial[5];
            return ret;
        }
        0
    }

    /// Write of 0x3fea.
    pub fn write(&mut self, val: u8) {
        self.last_write = val;
        if self.unlock_bytes_left > 0 {
            // the three unlock code bytes are not checked
            self.unlock_bytes_left -= 1;
            return;
        }
        match val {
            PIC_RESET => {
                self.scrambler = SCRAMBLER_INIT;
                self.serial[5] = self.serial[0] ^ self.serial[15];
                self.serial[13] = self.serial[2] ^ self.serial[12];
                self.writes_until_unlock = 0x20;
            }
            PIC_UNLOCK => self.unlock_bytes_left = 3,
            PIC_COUNTER => {
                self.writes_until_unlock = self.writes_until_unlock.wrapping_sub(1) & 0x1f
            }
            _ => {}
        }
    }
}
