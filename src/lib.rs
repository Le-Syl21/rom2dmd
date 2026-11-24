//! Extract the display animations of a pinball ROM set by emulating its display board.
//!
//! `rom2dmd` reads a PinMAME ROM zip, identifies the board family and the game, finds the
//! per-game addresses in the code, then plays every display scene the game knows on an
//! emulated board, in parallel, and records what the dot matrix shows. Each scene comes back
//! as a series of pictures (one shade level per dot) with their durations, a verdict (did it
//! end, loop, hold?) and, for a loop, where its cycle starts and how long it is.
//!
//! Supported boards: Williams/Bally WPC (DMD, Fliptronics, DCS, Security, WPC-95), Data
//! East 128x32 and 128x16, Sega/Stern Whitestar 128x32, Sega 192x64, Gottlieb System 3, Alvin
//! G., Capcom and Stern SAM.
//!
//! The pictures are the format real displays take: a ZeDMD or any DMD device wants shade
//! levels per dot, a FullDMD renders them at any size. [`Extraction::write_dump`] writes the
//! same text dump as PinMAME's `-dmd_dump_dir` (and VPinMAME's DmdDump), which colorization
//! tools read.
//!
//! ```no_run
//! let x = rom2dmd::extract(std::path::Path::new("t2_l8.zip"), &rom2dmd::Options::default())?;
//! for s in x.scenes.iter().filter(|s| !s.frames.is_empty()) {
//!     println!("scene {} {}: {} pictures", s.id, s.verdict, s.frames.len());
//! }
//! # Ok::<(), String>(())
//! ```
//!
//! Progress goes through the [`log`] crate (`info` level).
//!
//! The `export` feature adds the file export of the `rom2dmd` command line (PNG pictures,
//! contact sheets, GIF animations, manifest).

// The emulated boards and the scans. Public for the command line's debug subcommands only:
// the supported interface is the items at the crate root.
#[doc(hidden)]
pub mod alvg;
#[doc(hidden)]
pub mod auto;
#[doc(hidden)]
pub mod capcom;
#[doc(hidden)]
pub mod de16;
#[doc(hidden)]
pub mod de32;
#[doc(hidden)]
pub mod de64;
#[doc(hidden)]
pub mod export;
#[doc(hidden)]
pub mod gts3;
#[doc(hidden)]
pub mod romset;
#[doc(hidden)]
pub mod sam;
#[doc(hidden)]
pub mod scan;
#[doc(hidden)]
pub mod wpc;

use std::io::Write;
use std::path::Path;

use serde_json::{Map, Value};

/// Extraction options.
#[derive(Clone, Debug)]
pub struct Options {
    /// Emulated boards run at once (default: the number of cores).
    pub threads: usize,
    /// Only these scene ids (default: every id of the board).
    pub ids: Option<Vec<u32>>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            threads: std::thread::available_parallelism().map_or(4, |n| n.get()),
            ids: None,
        }
    }
}

/// Everything extracted from one ROM set.
#[derive(Clone, Debug)]
pub struct Extraction {
    /// The PinMAME set name.
    pub set: String,
    /// The board family (`WPC`, `Data East 128x32`, `Stern SAM`...).
    pub family: String,
    /// Display size in dots.
    pub width: usize,
    pub height: usize,
    /// Grey value (0..255) of each shade level: a dot's value indexes it.
    pub palette: Vec<u8>,
    /// One entry per scene id tried, in id order.
    pub scenes: Vec<Scene>,
    /// The scan's report: what was probed and how each id went.
    pub note: String,
}

/// One display scene (a WPC display effect, a scene command of the other boards).
#[derive(Clone, Debug)]
pub struct Scene {
    pub id: u32,
    /// What the scene did:
    /// - `end` (other boards) or `finite` (WPC): it ended by itself; `cut` (WPC): another
    ///   task stopped it;
    /// - `loop`: still running at the end of the capture and repeating (see `cycle`);
    /// - `hold`: still running on one unchanging picture; `running`: still running, not
    ///   periodic;
    /// - no pictures: `blank` (only black), `none` (the command did nothing), and on WPC
    ///   `silent` (the effect never drew), `refused` (the game never started it), `missing`.
    pub verdict: String,
    /// The pictures, consecutive duplicates removed.
    pub frames: Vec<Frame>,
    /// For a loop: where the repeating part starts and how long one cycle lasts.
    pub cycle: Option<Cycle>,
    /// Every field of the scene's manifest record (verdict details).
    pub record: Map<String, Value>,
}

/// One picture of a scene.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Start, in milliseconds from the scene's first picture.
    pub start_ms: u32,
    /// How long it stays (the last picture of a scene: 1 s).
    pub duration_ms: u32,
    /// One shade level per dot, row by row (`width * height` values, indexing `palette`).
    pub dots: Vec<u8>,
}

/// The repeating part of a looping scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cycle {
    /// Start of the first cycle, in milliseconds from the scene's first picture.
    pub start_ms: u32,
    /// Length of one cycle in milliseconds.
    pub length_ms: u32,
}

/// One line about a ROM zip: set, family and what the probes found, without scanning (an
/// error when the set is not supported or a probe fails).
pub fn identify(zip: &Path) -> Result<String, String> {
    auto::identify(zip)
}

/// Extract every scene of a ROM zip.
pub fn extract(zip: &Path, o: &Options) -> Result<Extraction, String> {
    extract_set(&romset::open(zip)?, o)
}

/// Extract every scene of a ROM zip held in memory; `name` is the zip's file name (its stem
/// names the PinMAME set, e.g. `t2_l8.zip`).
pub fn extract_bytes(name: &str, zip: &[u8], o: &Options) -> Result<Extraction, String> {
    extract_set(&romset::open_bytes(name, zip)?, o)
}

fn extract_set(rs: &romset::RomSet, o: &Options) -> Result<Extraction, String> {
    let opts = auto::Opts {
        threads: o.threads.max(1),
        ids: o.ids.clone(),
        keep_work: None,
    };
    let (an, note) = auto::analyse(rs, &opts)?;
    Ok(Extraction::from_analysis(rs, an, note))
}

fn ms(s: f64) -> u32 {
    (s * 1000.0).round().clamp(0.0, u32::MAX as f64) as u32
}

impl Extraction {
    pub(crate) fn from_analysis(
        rs: &romset::RomSet,
        an: export::Analysis,
        note: String,
    ) -> Extraction {
        let scenes = an
            .items
            .into_iter()
            .map(|it| Scene {
                id: it.rec.get("id").and_then(Value::as_u64).unwrap_or(0) as u32,
                verdict: export::verdict(&it.rec).to_string(),
                cycle: export::cycle(&it.rec).map(|(a, c)| Cycle {
                    start_ms: ms(a),
                    length_ms: ms(c),
                }),
                frames: it
                    .frames
                    .into_iter()
                    .map(|f| Frame {
                        start_ms: ms(f.t),
                        duration_ms: ms(f.d).max(1),
                        dots: f.dots,
                    })
                    .collect(),
                record: it.rec,
            })
            .collect();
        Extraction {
            set: rs.set.clone(),
            family: rs.family.name().to_string(),
            width: an.geometry.width,
            height: an.geometry.height,
            palette: an.geometry.palette,
            scenes,
            note,
        }
    }

    /// Write the scenes as a PinMAME DMD dump (the `<set>.txt` of `-dmd_dump_dir`): for each
    /// picture a `0x%08x` millisecond time stamp line, one line per row with one hex digit
    /// per dot (the shade level), then an empty line.
    ///
    /// The scenes follow each other in id order, 1 s apart; a loop is written up to the end
    /// of its first cycle. As in PinMAME's dump, a picture equal to the one before is not
    /// written again.
    pub fn write_dump(&self, mut w: impl Write) -> std::io::Result<()> {
        let mut base: u64 = 0;
        let mut last: Option<&[u8]> = None;
        let mut line = String::with_capacity(self.width + 1);
        for s in &self.scenes {
            let stop = s.cycle.map_or(u32::MAX, |c| c.start_ms + c.length_ms);
            let mut end = 0u64;
            for f in s.frames.iter().filter(|f| f.start_ms < stop) {
                end = end.max(f.start_ms as u64 + f.duration_ms as u64);
                if last == Some(f.dots.as_slice()) {
                    continue;
                }
                last = Some(&f.dots);
                writeln!(w, "0x{:08x}", (base + f.start_ms as u64) as u32)?;
                for row in f.dots.chunks(self.width.max(1)) {
                    line.clear();
                    for &v in row {
                        line.push(char::from_digit(v.min(15) as u32, 16).unwrap_or('f'));
                    }
                    line.push('\n');
                    w.write_all(line.as_bytes())?;
                }
                writeln!(w)?;
            }
            if end > 0 {
                base += end.min(stop as u64) + 1000;
            }
        }
        w.flush()
    }

    /// [`Extraction::write_dump`] into a string.
    pub fn dump(&self) -> String {
        let mut v = Vec::new();
        // writing to a Vec cannot fail
        let _ = self.write_dump(&mut v);
        String::from_utf8(v).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_format() {
        let x = Extraction {
            set: "t".into(),
            family: "test".into(),
            width: 4,
            height: 2,
            palette: vec![0, 85, 170, 255],
            scenes: vec![
                Scene {
                    id: 0,
                    verdict: "end".into(),
                    frames: vec![
                        Frame {
                            start_ms: 0,
                            duration_ms: 100,
                            dots: vec![0, 1, 2, 3, 3, 2, 1, 0],
                        },
                        Frame {
                            start_ms: 100,
                            duration_ms: 1000,
                            dots: vec![3; 8],
                        },
                    ],
                    cycle: None,
                    record: Map::new(),
                },
                Scene {
                    id: 1,
                    verdict: "end".into(),
                    frames: vec![Frame {
                        start_ms: 0,
                        duration_ms: 1000,
                        dots: vec![1; 8],
                    }],
                    cycle: None,
                    record: Map::new(),
                },
            ],
            note: String::new(),
        };
        assert_eq!(
            x.dump(),
            "0x00000000\n0123\n3210\n\n0x00000064\n3333\n3333\n\n0x00000834\n1111\n1111\n\n"
        );
    }
}
