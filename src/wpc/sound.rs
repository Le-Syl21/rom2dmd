//! Sound board stub: the CPU board side of the WPC sound interface (0x3fdc data, 0x3fdd
//! control/status), enough for games to boot. Follows WPC-EMU's `sound-serial-interface.js`:
//! commands are decoded only to answer the few queries a DCS game waits for.

use std::collections::VecDeque;

#[derive(Clone, Default)]
pub struct Sound {
    /// WPC-89 / Fliptronic boards take 8-bit commands; DCS boards take 16-bit ones.
    pub pre_dcs: bool,
    write_q: Vec<u8>,
    read_q: VecDeque<u8>,
    pub reset_count: u32,
    /// Every complete command received, with no decoding beyond the byte grouping.
    pub commands: Vec<u16>,
    /// Keep at most this many entries in `commands` (0 = do not log).
    pub log_limit: usize,
}

impl Sound {
    pub fn new(pre_dcs: bool) -> Self {
        Sound {
            pre_dcs,
            ..Default::default()
        }
    }

    pub fn reset(&mut self) {
        self.write_q.clear();
        self.read_q.clear();
        self.reset_count += 1;
    }

    fn log(&mut self, cmd: u16) {
        if self.commands.len() < self.log_limit {
            self.commands.push(cmd);
        }
    }

    pub fn write_control(&mut self, val: u8) {
        // Writing 0 resets the sound board (the 1 -> 0 transition on the real line).
        if val == 0 {
            self.reset();
        }
    }

    pub fn write_data(&mut self, val: u8) {
        // WPC-EMU quirk kept for parity: after many resets the first 0 is dropped.
        if self.reset_count > 20 && val == 0 {
            self.reset_count = 0;
            return;
        }
        self.write_q.push(val);
        if self.pre_dcs {
            self.process_pre_dcs();
        } else {
            self.process_dcs();
        }
    }

    fn process_pre_dcs(&mut self) {
        match self.write_q[0] {
            // volume: 0x79, value, ~value
            0x79 => {
                if self.write_q.len() == 3 {
                    self.write_q.clear();
                }
            }
            // extended command: 0x7a, id
            0x7a => {
                if self.write_q.len() > 1 {
                    let c = 0x7a00 | self.write_q[1] as u16;
                    self.log(c);
                    self.write_q.clear();
                }
            }
            c => {
                self.log(c as u16);
                self.write_q.clear();
            }
        }
    }

    fn process_dcs(&mut self) {
        if self.write_q.len() < 2 {
            return;
        }
        if self.write_q[0] == 0x55 {
            // volume: 0x55, 0xaa, value, ~value
            if self.write_q.len() == 4 {
                self.write_q.clear();
            }
            return;
        }
        let id = ((self.write_q[0] as u16) << 8) | self.write_q[1] as u16;
        match id {
            // queries a DCS game waits for: unknown 0x3d2/0x3d3, version major/minor
            0x3d2 | 0x3d3 => self.read_q.push_back(0x01),
            0x3e7 => self.read_q.push_back(0x10),
            0x3e8 => self.read_q.push_back(0x01),
            _ => self.log(id),
        }
        self.write_q.clear();
    }

    pub fn read_data(&mut self) -> u8 {
        self.read_q.pop_front().unwrap_or(0)
    }

    pub fn read_control(&self) -> u8 {
        if self.read_q.is_empty() { 0xff } else { 0x80 }
    }
}
