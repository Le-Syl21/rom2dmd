//! WPC dot matrix controller (Williams A-14039): 16 pages of 512 bytes, 128x32 pixels.
//!
//! The CPU sees the video RAM through six 512-byte windows (0x3000-0x3bff): the two at 0x3800
//! (low page) and 0x3a00 (high page) exist on every DMD generation, the four at 0x3000-0x37ff
//! are added by WPC-95. The controller clocks one scanline (16 bytes) out every 512 CPU cycles
//! from the active page, so a full refresh takes 16384 cycles (122 Hz). A write to the active
//! page register takes effect just before scanline 0 is drawn. Writing the FIRQ line register
//! arms one FIRQ, raised when the given scanline has been drawn.

/// Bytes per page (128x32 bits).
pub const PAGE_SIZE: usize = 0x200;
/// CPU cycles per scanline.
pub const CYCLES_PER_LINE: u32 = 512;
const PAGES: usize = 16;
const LINE_BYTES: usize = 16;

#[derive(Clone)]
pub struct Dmd {
    pub vram: Vec<u8>,
    /// Page mapped in each CPU window: 0x3800, 0x3a00, 0x3000, 0x3200, 0x3400, 0x3600.
    pub map: [u8; 6],
    pub scanline: u8,
    pub active: u8,
    pub next_active: Option<u8>,
    pub request_firq: bool,
    ticks: u32,
    /// The picture being clocked out, one scanline at a time.
    drawing: [u8; PAGE_SIZE],
    /// The last three complete pictures, oldest first (WPC games shade with page flipping).
    pub history: [[u8; PAGE_SIZE]; 3],
    /// Scanlines clocked out since power-on (one every `CYCLES_PER_LINE` cycles): a time base
    /// that only moves with the display, as WPC-EMU's video tick counter.
    pub lines: u64,
    /// When set, every CPU write to the video RAM is appended to `writes` (the scene tracer
    /// drains it after each instruction).
    pub log_writes: bool,
    /// Logged writes: (page, offset in page, value).
    pub writes: Vec<(u8, u16, u8)>,
}

/// What a scanline tick produced.
pub struct LineEvent {
    /// The scanline about to be drawn next (0 after a complete refresh).
    pub scanline: u8,
    /// Set at the end of a refresh: the page that was shown.
    pub refreshed: Option<u8>,
}

impl Default for Dmd {
    fn default() -> Self {
        Dmd {
            vram: vec![0; PAGE_SIZE * PAGES],
            map: [0; 6],
            scanline: 0,
            active: 0,
            next_active: None,
            request_firq: true,
            ticks: 0,
            drawing: [0; PAGE_SIZE],
            history: [[0; PAGE_SIZE]; 3],
            lines: 0,
            log_writes: false,
            writes: Vec::new(),
        }
    }
}

impl Dmd {
    fn offset(&self, window: usize, off: usize) -> usize {
        self.map[window] as usize * PAGE_SIZE + off
    }

    /// CPU address 0x3000-0x3bff to (window, offset in page).
    pub fn window(addr: u16) -> (usize, usize) {
        let a = addr as usize - 0x3000;
        let off = a & (PAGE_SIZE - 1);
        match a / PAGE_SIZE {
            0 => (2, off),
            1 => (3, off),
            2 => (4, off),
            3 => (5, off),
            4 => (0, off),
            _ => (1, off),
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        let (w, off) = Self::window(addr);
        self.vram[self.offset(w, off)]
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        let (w, off) = Self::window(addr);
        let o = self.offset(w, off);
        self.vram[o] = val;
        if self.log_writes {
            self.writes.push((self.map[w], off as u16, val));
        }
    }

    pub fn select(&mut self, window: usize, val: u8) {
        self.map[window] = val & 0x0f;
    }

    /// The picture last shown (512 bytes, bit 0 = leftmost pixel).
    pub fn frame(&self) -> &[u8; PAGE_SIZE] {
        &self.history[2]
    }

    /// The last three refreshes summed per pixel: 4096 levels 0..3.
    pub fn shaded(&self) -> [u8; 4096] {
        let mut out = [0u8; 4096];
        for (i, px) in out.iter_mut().enumerate() {
            let (byte, bit) = (i / 8, i % 8);
            *px = self.history.iter().map(|h| (h[byte] >> bit) & 1).sum();
        }
        out
    }

    /// Advance by `ticks` CPU cycles; at most one scanline is drawn per call (instructions are
    /// shorter than a scanline).
    pub fn tick(&mut self, ticks: u32) -> Option<LineEvent> {
        self.ticks += ticks;
        if self.ticks < CYCLES_PER_LINE {
            return None;
        }
        self.ticks -= CYCLES_PER_LINE;
        self.lines += 1;
        let line = self.scanline as usize * LINE_BYTES;
        let src = self.active as usize * PAGE_SIZE + line;
        self.drawing[line..line + LINE_BYTES].copy_from_slice(&self.vram[src..src + LINE_BYTES]);
        self.scanline = (self.scanline + 1) & 0x1f;
        let mut refreshed = None;
        if self.scanline == 0 {
            refreshed = Some(self.active);
            self.history.rotate_left(1);
            self.history[2] = self.drawing;
            if let Some(p) = self.next_active.take() {
                self.active = p;
            }
        }
        Some(LineEvent {
            scanline: self.scanline,
            refreshed,
        })
    }
}
