//! Scene analysis: turns what a scan recorded into scenes of timed pictures with a verdict
//! (always built), and, with the `export` feature, into the files a person looks at and a
//! manifest.
//!
//! Every effect (WPC) or scene (the other boards) gets a folder with:
//! - `frames/fNNNN.png`: each distinct picture, one grey level per shade (linear, `k/N`);
//! - `sheet.png`: a contact sheet of up to 36 pictures spread over the whole capture;
//! - `anim.gif`: the pictures with their real durations (for a loop: intro and one cycle);
//! - `loop.gif`: one cycle of a loop only;
//! - WPC only: `eye.png`, an animated PNG at 60 fps of what the glass shows (mean of the last
//!   6 refreshes, the eye model), and `raw.txt`, the effect's refresh log as the scan wrote it.
//!
//! The manifest (`manifest.json`) holds one record per id with its verdict.

pub mod scenes;
pub mod wpc;

#[cfg(feature = "export")]
use std::borrow::Cow;
#[cfg(feature = "export")]
use std::fs::File;
#[cfg(feature = "export")]
use std::io::BufWriter;
use std::path::Path;

use serde_json::{Map, Value};

/// A picture with its timing, in seconds from the scene's first picture.
#[derive(Clone, Debug)]
pub struct Shot {
    pub t: f64,
    pub d: f64,
    /// One shade level per dot, row by row.
    pub dots: Vec<u8>,
}

/// One scene (or WPC effect) after analysis.
#[derive(Clone, Debug)]
pub struct Analysed {
    /// The manifest record: `id`, `verdict` and the verdict's fields.
    pub rec: Map<String, Value>,
    /// The pictures; empty for `none` and `blank`.
    pub frames: Vec<Shot>,
    /// WPC: the effect's refresh log as the scan wrote it (`raw.txt`).
    pub raw: Option<String>,
    /// WPC: what the glass shows at 60 fps (`eye.png`); built for the file export only.
    pub eye: Vec<Vec<u8>>,
}

/// Every scene of a ROM set after analysis.
#[derive(Clone, Debug)]
pub struct Analysis {
    pub geometry: Geometry,
    /// Folder name prefix: `scene` or `effect`.
    pub folder: &'static str,
    pub items: Vec<Analysed>,
}

impl Analysis {
    /// Verdict counts.
    pub fn summary(&self) -> Summary {
        let mut counts = std::collections::BTreeMap::new();
        for it in &self.items {
            *counts.entry(verdict(&it.rec).to_string()).or_default() += 1;
        }
        Summary {
            counts,
            total: self.items.len(),
        }
    }
}

/// The record's verdict (`none` when absent).
pub fn verdict(rec: &Map<String, Value>) -> &str {
    rec.get("verdict").and_then(Value::as_str).unwrap_or("none")
}

/// The record's loop: (start, length) in seconds.
pub fn cycle(rec: &Map<String, Value>) -> Option<(f64, f64)> {
    match (
        rec.get("cycle_from_s").and_then(Value::as_f64),
        rec.get("cycle_s").and_then(Value::as_f64),
    ) {
        (Some(a), Some(c)) => Some((a, c)),
        _ => None,
    }
}

/// The part of `frames` inside [a, b) seconds, durations clipped.
pub fn window(frames: &[Shot], a: f64, b: f64) -> Vec<(f64, &[u8])> {
    frames
        .iter()
        .filter(|f| f.t + f.d > a && f.t < b)
        .map(|f| ((f.t + f.d).min(b) - f.t.max(a), f.dots.as_slice()))
        .filter(|(d, _)| *d > 0.0)
        .collect()
}

/// Write `outdir/<folder>_NNN/` for every scene that has something to show, and
/// `outdir/manifest.json`.
#[cfg(feature = "export")]
pub fn write(an: &Analysis, outdir: &Path) -> Result<Summary, String> {
    std::fs::create_dir_all(outdir).map_err(|e| io_err(outdir, e))?;
    let g = &an.geometry;
    let mut man = Vec::new();
    for it in &an.items {
        man.push(Value::Object(it.rec.clone()));
        if it.raw.is_none() && (matches!(verdict(&it.rec), "none" | "blank")) {
            continue;
        }
        let id = it.rec.get("id").and_then(Value::as_u64).unwrap_or(0);
        let dir = outdir.join(format!("{}_{id:03}", an.folder));
        std::fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        if let Some(raw) = &it.raw {
            let rp = dir.join("raw.txt");
            std::fs::write(&rp, raw).map_err(|e| io_err(&rp, e))?;
        }
        let imgs: Vec<&[u8]> = it.frames.iter().map(|f| f.dots.as_slice()).collect();
        if imgs.is_empty() {
            continue;
        }
        write_frames(&dir.join("frames"), g, &imgs)?;
        write_sheet(&dir.join("sheet.png"), g, &imgs)?;
        let cyc = cycle(&it.rec);
        let end = cyc.map_or(f64::INFINITY, |(a, c)| a + c);
        write_gif(&dir.join("anim.gif"), g, &window(&it.frames, 0.0, end))?;
        if let Some((a, c)) = cyc {
            write_gif(&dir.join("loop.gif"), g, &window(&it.frames, a, a + c))?;
        }
        if !it.eye.is_empty() {
            write_apng(&dir.join("eye.png"), g.width, g.height, &it.eye)?;
        }
    }
    let mp = outdir.join("manifest.json");
    let text = serde_json::to_string_pretty(&man).map_err(|e| io_err(&mp, e))?;
    std::fs::write(&mp, text).map_err(|e| io_err(&mp, e))?;
    Ok(an.summary())
}

/// Summary of an export: verdict counts.
pub struct Summary {
    pub counts: std::collections::BTreeMap<String, usize>,
    pub total: usize,
}

/// Run `f` on every item on all cores; results in item order.
pub fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    par_map_threads(items, threads, f)
}

/// Run `f` on every item on `threads` threads; results in item order.
pub fn par_map_threads<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<R>>> =
        items.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|sc| {
        for _ in 0..threads.min(items.len()).max(1) {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(it) = items.get(i) else { break };
                    let r = f(it);
                    if let Ok(mut s) = slots[i].lock() {
                        *s = Some(r);
                    }
                }
            });
        }
    });
    slots
        .into_iter()
        .filter_map(|m| m.into_inner().ok().flatten())
        .collect()
}

/// Display size and grey value of each shade level.
#[derive(Clone, Debug)]
pub struct Geometry {
    pub width: usize,
    pub height: usize,
    pub palette: Vec<u8>,
}

impl Geometry {
    /// `levels` evenly spaced grey levels (level k = round(255 k / (levels - 1))).
    pub fn linear(width: usize, height: usize, levels: usize) -> Self {
        let top = levels.max(2) - 1;
        let palette = (0..levels.max(2))
            .map(|k| ((255 * k + top / 2) / top) as u8)
            .collect();
        Geometry {
            width,
            height,
            palette,
        }
    }

    #[cfg(feature = "export")]
    fn grey(&self, level: u8) -> u8 {
        self.palette.get(level as usize).copied().unwrap_or(u8::MAX)
    }
}

pub(crate) fn io_err(p: &Path, e: impl std::fmt::Display) -> String {
    format!("{}: {e}", p.display())
}

#[cfg(feature = "export")]
/// Write an 8-bit greyscale PNG.
pub fn write_png(path: &Path, width: usize, height: usize, pixels: &[u8]) -> Result<(), String> {
    let f = File::create(path).map_err(|e| io_err(path, e))?;
    let mut enc = png::Encoder::new(BufWriter::new(f), width as u32, height as u32);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| io_err(path, e))?;
    w.write_image_data(pixels).map_err(|e| io_err(path, e))?;
    w.finish().map_err(|e| io_err(path, e))
}

#[cfg(feature = "export")]
/// Nearest-neighbour upscale of greyscale pixels.
fn upscale(px: &[u8], w: usize, h: usize, s: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * s * h * s];
    for y in 0..h * s {
        let row = &px[(y / s) * w..(y / s + 1) * w];
        for x in 0..w * s {
            out[y * w * s + x] = row[x / s];
        }
    }
    out
}

#[cfg(feature = "export")]
/// One frame of levels as grey pixels.
fn to_grey(g: &Geometry, levels: &[u8]) -> Vec<u8> {
    levels.iter().map(|&v| g.grey(v)).collect()
}

#[cfg(feature = "export")]
/// Write every picture as `dir/fNNNN.png` (native size, grey per level).
pub fn write_frames(dir: &Path, g: &Geometry, frames: &[&[u8]]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    for (k, f) in frames.iter().enumerate() {
        write_png(
            &dir.join(format!("f{k:04}.png")),
            g.width,
            g.height,
            &to_grey(g, f),
        )?;
    }
    Ok(())
}

#[cfg(feature = "export")]
/// Contact sheet: up to 36 pictures picked evenly (picture `j * n / k`), 3x, a 4-pixel grey
/// border around each, 3 per row (the layout of the reference tools).
pub fn write_sheet(path: &Path, g: &Geometry, frames: &[&[u8]]) -> Result<(), String> {
    const S: usize = 3;
    const PAD: usize = 4;
    const COLS: usize = 3;
    let n = frames.len();
    if n == 0 {
        return Ok(());
    }
    let k = n.min(36);
    let (cw, ch) = (g.width * S + 2 * PAD, g.height * S + 2 * PAD);
    let rows = k.div_ceil(COLS);
    let (pw, ph) = (COLS * cw, rows * ch);
    let mut buf = vec![128u8; pw * ph];
    for j in 0..k {
        let px = upscale(&to_grey(g, frames[j * n / k]), g.width, g.height, S);
        let (ox, oy) = ((j % COLS) * cw + PAD, (j / COLS) * ch + PAD);
        for y in 0..g.height * S {
            let dst = (oy + y) * pw + ox;
            buf[dst..dst + g.width * S]
                .copy_from_slice(&px[y * g.width * S..(y + 1) * g.width * S]);
        }
    }
    write_png(path, pw, ph, &buf)
}

#[cfg(feature = "export")]
/// Animated GIF of pictures with their durations in seconds, scaled 4x. GIF delays are in
/// hundredths of a second: frame boundaries are rounded on the cumulated time (no drift), and
/// a picture shown less than 2/100 s (browsers stretch shorter delays) is merged into the next.
pub fn write_gif(path: &Path, g: &Geometry, frames: &[(f64, &[u8])]) -> Result<(), String> {
    const S: usize = 4;
    if frames.is_empty() {
        return Ok(());
    }
    let (w, h) = (g.width * S, g.height * S);
    let mut palette = Vec::with_capacity(3 * g.palette.len());
    for &v in &g.palette {
        palette.extend_from_slice(&[v, v, v]);
    }
    let f = File::create(path).map_err(|e| io_err(path, e))?;
    let mut enc = gif::Encoder::new(BufWriter::new(f), w as u16, h as u16, &palette)
        .map_err(|e| io_err(path, e))?;
    enc.set_repeat(gif::Repeat::Infinite)
        .map_err(|e| io_err(path, e))?;
    let top = g.palette.len().saturating_sub(1) as u8;
    let (mut t, mut shown) = (0.0f64, 0u64); // seconds elapsed, hundredths written
    for (k, (d, levels)) in frames.iter().enumerate() {
        t += d;
        let end = (t * 100.0).round().max(0.0) as u64;
        let delay = end.saturating_sub(shown);
        if delay < 2 && k + 1 < frames.len() {
            continue;
        }
        let delay = delay.clamp(2, u16::MAX as u64);
        shown += delay;
        let idx: Vec<u8> = levels.iter().map(|&v| v.min(top)).collect();
        let fr = gif::Frame {
            width: w as u16,
            height: h as u16,
            delay: delay as u16,
            buffer: Cow::Owned(upscale(&idx, g.width, g.height, S)),
            ..gif::Frame::default()
        };
        enc.write_frame(&fr).map_err(|e| io_err(path, e))?;
    }
    Ok(())
}

#[cfg(feature = "export")]
/// Animated PNG of greyscale pictures (`width` x `height` before the 4x scale), each with a
/// duration in 1/60 s; identical consecutive pictures are merged into one longer frame.
pub fn write_apng(
    path: &Path,
    width: usize,
    height: usize,
    frames: &[Vec<u8>],
) -> Result<(), String> {
    const S: usize = 4;
    let mut runs: Vec<(&[u8], u32)> = Vec::new();
    for f in frames {
        match runs.last_mut() {
            Some((p, n)) if *p == f.as_slice() && *n < u16::MAX as u32 => *n += 1,
            _ => runs.push((f, 1)),
        }
    }
    if runs.is_empty() {
        return Ok(());
    }
    let f = File::create(path).map_err(|e| io_err(path, e))?;
    let mut enc = png::Encoder::new(BufWriter::new(f), (width * S) as u32, (height * S) as u32);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_animated(runs.len() as u32, 0)
        .map_err(|e| io_err(path, e))?;
    let mut w = enc.write_header().map_err(|e| io_err(path, e))?;
    for (px, n) in runs {
        w.set_frame_delay(n as u16, 60)
            .map_err(|e| io_err(path, e))?;
        w.write_image_data(&upscale(px, width, height, S))
            .map_err(|e| io_err(path, e))?;
    }
    w.finish().map_err(|e| io_err(path, e))
}

/// Python's `round(x, nd)` (correctly rounded from the binary value, as Rust's formatting).
pub fn round_to(x: f64, nd: usize) -> f64 {
    format!("{x:.nd$}").parse().unwrap_or(x)
}

/// Result of the periodicity test on a picture series sampled every 50 ms.
#[derive(Clone, Debug, PartialEq)]
pub enum Periodic {
    /// One unchanging picture over the second half.
    Hold,
    /// Repeats: first sample of the cycle, cycle length in samples, agreement.
    Loop {
        from: usize,
        lag: usize,
        agree: f64,
    },
    Running,
}

/// The reference classifiers' periodicity test: over the second half of the series, the first
/// period (from 4 samples) whose samples agree at 95 % or more, with one sample (50 ms) of
/// jitter allowed; then the earliest sample from which the series repeats with it.
pub fn periodicity<T: PartialEq>(series: &[T]) -> Periodic {
    let tail = &series[series.len() / 2..];
    if tail.first().is_some_and(|f| tail.iter().all(|x| x == f)) {
        return Periodic::Hold;
    }
    let n = tail.len();
    for lag in 4..n / 2 {
        let m = n - lag - 1;
        let hit = (0..m)
            .filter(|&i| {
                let x = &tail[i];
                *x == tail[i + lag - 1]
                    || *x == tail[i + lag]
                    || *x == tail[(n - 1).min(i + lag + 1)]
            })
            .count();
        let agree = hit as f64 / m as f64;
        if agree >= 0.95 {
            let mut from = 0;
            while from + lag < series.len() && series[from] != series[from + lag] {
                from += 1;
            }
            return Periodic::Loop { from, lag, agree };
        }
    }
    Periodic::Running
}

/// Sample `(start, duration, key)` pictures every 50 ms from `t0` up to the end of the last.
pub fn sample_20hz<K: Copy>(frames: &[(f64, f64, K)], t0: f64) -> Vec<K> {
    let Some(last) = frames.last() else {
        return Vec::new();
    };
    let end = last.0 + last.1;
    let mut out = Vec::new();
    let (mut j, mut t) = (0usize, t0);
    while t < end {
        while j + 1 < frames.len() && frames[j + 1].0 <= t {
            j += 1;
        }
        out.push(frames[j].2);
        t += 0.05;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodicity_finds_cycles() {
        // the reference test accepts one sample of jitter: a clean 10-sample cycle is found at
        // lag 9 (sample i+10 matches the lag-9 window's third candidate), and then no sample
        // starts an exact lag-9 repeat (the start search runs to the end), as in Python
        let s: Vec<u8> = (0..400).map(|i| (i % 10) as u8).collect();
        assert_eq!(
            periodicity(&s),
            Periodic::Loop {
                from: 391,
                lag: 9,
                agree: 1.0
            }
        );
        let mut h = vec![1u8, 2, 3];
        h.extend(std::iter::repeat_n(7, 50));
        assert_eq!(periodicity(&h), Periodic::Hold);
        let r: Vec<u32> = (0..100).collect();
        assert_eq!(periodicity(&r), Periodic::Running);
    }

    #[test]
    fn palette_and_rounding() {
        assert_eq!(Geometry::linear(128, 32, 4).palette, vec![0, 85, 170, 255]);
        assert_eq!(
            Geometry::linear(128, 32, 5).palette,
            vec![0, 64, 128, 191, 255]
        );
        assert_eq!(round_to(0.15000000000000002, 2), 0.15);
        assert_eq!(round_to(0.9984, 3), 0.998);
    }

    #[test]
    fn sampling() {
        let f = [(0.0, 0.1, 'a'), (0.1, 0.1, 'b')];
        assert_eq!(sample_20hz(&f, 0.0), vec!['a', 'a', 'b', 'b']);
    }
}
