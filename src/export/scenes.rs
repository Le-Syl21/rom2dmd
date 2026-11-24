//! Scene export of the boards whose scans record finished pictures (Data East, Whitestar, Sega
//! 192x64, Gottlieb System 3, Alvin G., Capcom, Stern SAM): port of the reference
//! `de_export.py` / `sam_export.py`.
//!
//! The picture on screen when a command arrives (the resting screen: the most common first
//! picture of all scenes) belongs to no scene and is dropped from their start, also when only
//! half redrawn, and so are the other phases of an animated resting screen (the same dots in
//! other shades, in the first 0.1 s of at least half the scenes at the same time). With `blend`
//! (Gottlieb's temporal combiner) the mixtures of the resting screen at both ends of a scene are
//! dropped too, and a single still picture shared by 5 or more ids is the picture under the
//! score layer that an ignored command leaves (`none`). An ended scene also loses the black or
//! resting pictures it ends on (the board clearing the screen, the boot picture coming back).
//!
//! Verdicts: `end` (the scene ended), `loop` (ran to the cap and repeats: `cycle_from_s`,
//! `cycle_s`), `hold` (ran to the cap on one picture), `running` (ran to the cap, not
//! periodic), `blank` (only black), `none` (the command did nothing).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde_json::{Map, Value, json};

use super::{
    Analysed, Analysis, Geometry, Periodic, Shot, io_err, periodicity, round_to, sample_20hz,
};

/// What a scan recorded for one scene id.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub id: u32,
    /// The scan's verdict: `end` when the scene ended, anything else when it ran to the cap
    /// (`none`: the command was not taken).
    pub kind: String,
    /// (milliseconds since the command, levels), consecutive duplicates removed.
    pub frames: Vec<(u64, Vec<u8>)>,
    /// Extra manifest fields (e.g. the display-manager function of a Capcom scene).
    pub extra: Map<String, Value>,
}

/// Export options.
#[derive(Clone, Debug)]
pub struct Opts {
    pub geometry: Geometry,
    /// Gottlieb combiner clean-up (see the module documentation).
    pub blend: bool,
}

type Pic<'a> = (f64, &'a [u8]);

/// Trim, classify and time every scene.
pub fn analyse(scenes: &[Scene], o: &Opts) -> Analysis {
    let mut g = o.geometry.clone();
    let top = scenes
        .iter()
        .flat_map(|s| s.frames.iter())
        .flat_map(|f| f.1.iter())
        .copied()
        .max()
        .unwrap_or(0) as usize;
    if top >= g.palette.len() {
        g = Geometry::linear(g.width, g.height, top + 1);
    }
    let (w, dots) = (g.width, g.width * g.height);
    // the resting screen: the most common first picture (first seen wins a tie)
    let mut firsts: Vec<(&[u8], usize)> = Vec::new();
    for s in scenes {
        if let Some(f) = s.frames.first() {
            match firsts.iter_mut().find(|x| x.0 == f.1.as_slice()) {
                Some(x) => x.1 += 1,
                None => firsts.push((&f.1, 1)),
            }
        }
    }
    let mut rest: Option<&[u8]> = None;
    let mut best = 0;
    for (p, n) in &firsts {
        if *n > best {
            (rest, best) = (Some(*p), *n);
        }
    }
    // An animated resting screen (Baywatch's shimmering title) shows a second phase before the
    // scene starts. The boot is the same for every id, so that phase comes at the same time in
    // each scene: a picture of the first 0.1 s (before the board answers the command) found in
    // at least half the scenes at the same time, lighting the same dots as the resting screen in
    // other shades, is resting too.
    let mut openers: Vec<(u64, &[u8], usize)> = Vec::new();
    for s in scenes {
        let start = s.frames.first().map_or(0, |f| f.0);
        for f in s.frames.iter().take(3).filter(|f| f.0 - start < 100) {
            match openers
                .iter_mut()
                .find(|x| x.0 == f.0 && x.1 == f.1.as_slice())
            {
                Some(x) => x.2 += 1,
                None => openers.push((f.0, &f.1, 1)),
            }
        }
    }
    let with_frames = scenes.iter().filter(|s| !s.frames.is_empty()).count();
    let same_dots = |p: &[u8]| {
        rest.is_some_and(|r| {
            r.len() == p.len() && r.iter().zip(p).all(|(a, b)| (*a == 0) == (*b == 0))
        })
    };
    let resting: Vec<(u64, &[u8])> = openers
        .iter()
        .filter(|x| x.2 >= 2 && 2 * x.2 >= with_frames && same_dots(x.1))
        .map(|x| (x.0, x.1))
        .collect();
    let rest_lit: Vec<usize> = rest.map_or(Vec::new(), |r| {
        (0..r.len()).filter(|&i| r[i] != 0).collect()
    });
    // a picture still carrying most of the resting screen's dots (plus others): a mixture
    let blend_of_rest = |img: &[u8]| -> bool {
        let Some(r) = rest else { return false };
        if rest_lit.is_empty() || img == r {
            return false;
        }
        let kept = rest_lit.iter().filter(|&&i| img[i] != 0).count();
        let extra = (0..img.len()).filter(|&i| img[i] != 0 && r[i] == 0).count();
        (extra == 0 && kept > 0) || (kept as f64 >= 0.8 * rest_lit.len() as f64 && extra > 0)
    };
    // also a picture still half the resting one (the 128x16 board redraws one half at a time)
    let half_rest = |img: &[u8]| -> bool {
        let Some(r) = rest else { return false };
        if img == r {
            return true;
        }
        if r.iter().all(|&v| v == 0) || img.len() != r.len() {
            return false;
        }
        let half = |a: usize, b: usize| {
            img.chunks(w)
                .zip(r.chunks(w))
                .all(|(x, y)| x[a..b] == y[a..b])
        };
        half(0, w / 2) || half(w / 2, w)
    };
    let mut recs: Vec<(Map<String, Value>, Vec<Pic>)> = Vec::new();
    for s in scenes {
        let mut fr: Vec<Pic> = s
            .frames
            .iter()
            .filter(|f| f.1.len() == dots)
            .map(|f| (f.0 as f64 / 1000.0, f.1.as_slice()))
            .collect();
        let t0 = fr.first().map_or(0.0, |f| f.0);
        let resting_pic = |f: &Pic| {
            half_rest(f.1)
                || resting
                    .iter()
                    .any(|r| r.1 == f.1 && r.0 as f64 == (f.0 * 1000.0).round())
        };
        while fr.len() > 1 && resting_pic(&fr[0]) {
            fr.remove(0);
        }
        if fr.len() == 1 && resting_pic(&fr[0]) {
            fr.clear();
        }
        if o.blend && rest.is_some() {
            while fr.len() > 1
                && (blend_of_rest(fr[0].1) || Some(fr[0].1) == rest)
                && fr[0].0 - t0 < 0.25
            {
                fr.remove(0);
            }
            while fr.len() > 1 && fr.last().map(|f| f.1) == rest {
                fr.pop();
            }
            while fr.len() > 1 && fr.last().is_some_and(|f| blend_of_rest(f.1)) {
                fr.pop();
            }
        }
        // a flash of under 0.1 s that ends black: the command only cleared the screen
        if let (Some(a), Some(z)) = (fr.first(), fr.last())
            && z.0 - a.0 < 0.1
            && z.1.iter().all(|&v| v == 0)
        {
            fr.clear();
        }
        // An ended scene hands the screen back: the black or resting pictures it ends on (the
        // board clearing the screen, the boot picture coming back) are not part of it.
        if s.kind == "end" {
            while fr.len() > 1
                && fr.last().is_some_and(|f| {
                    f.1.iter().all(|&v| v == 0)
                        || Some(f.1) == rest
                        || resting.iter().any(|r| r.1 == f.1)
                })
            {
                fr.pop();
            }
        }
        // only the resting screen, still or shimmering (its dots in any shades): the command
        // did nothing
        if !resting.is_empty() && fr.iter().all(|f| same_dots(f.1)) {
            fr.clear();
        }
        let mut rec = Map::new();
        rec.insert("id".into(), json!(s.id));
        let lit = fr.iter().any(|f| f.1.iter().any(|&v| v != 0));
        if fr.is_empty() || (s.kind == "none" && fr.len() <= 1) {
            rec.insert("verdict".into(), json!("none"));
        } else if !lit {
            rec.insert("verdict".into(), json!("blank"));
        } else {
            let (a, z) = (fr[0].0, fr[fr.len() - 1].0);
            rec.insert("frames".into(), json!(fr.len()));
            rec.insert("length_s".into(), json!(round_to(z - a, 2)));
            if s.kind == "end" {
                rec.insert("verdict".into(), json!("end"));
            } else {
                let mut uniq: HashMap<&[u8], usize> = HashMap::new();
                let keyed: Vec<(f64, f64, usize)> = fr
                    .iter()
                    .map(|f| {
                        let n = uniq.len();
                        (f.0, 0.0, *uniq.entry(f.1).or_insert(n))
                    })
                    .collect();
                // sampled up to the start of the last picture
                let series = sample_20hz(&keyed, a);
                match periodicity(&series) {
                    Periodic::Hold => {
                        rec.insert("verdict".into(), json!("hold"));
                    }
                    Periodic::Running => {
                        rec.insert("verdict".into(), json!("running"));
                    }
                    Periodic::Loop { from, lag, .. } => {
                        rec.insert("verdict".into(), json!("loop"));
                        rec.insert("cycle_s".into(), json!(round_to(lag as f64 * 0.05, 2)));
                        rec.insert(
                            "cycle_from_s".into(),
                            json!(round_to(from as f64 * 0.05, 2)),
                        );
                    }
                }
            }
        }
        rec.extend(s.extra.clone());
        recs.push((rec, fr));
    }
    if o.blend {
        // a single still picture shared by many ids: the underlay, not a scene
        let mut still: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
        for (k, (rec, fr)) in recs.iter().enumerate() {
            let n = rec.get("frames").and_then(Value::as_u64).unwrap_or(0);
            if (1..=2).contains(&n) && fr.iter().all(|f| f.1 == fr[0].1) {
                still.entry(fr[0].1.to_vec()).or_default().push(k);
            }
        }
        for ks in still.values().filter(|v| v.len() >= 5) {
            for &k in ks {
                let id = recs[k].0.get("id").cloned().unwrap_or(Value::Null);
                let mut m = Map::new();
                m.insert("id".into(), id);
                m.insert("verdict".into(), json!("none"));
                m.insert("note".into(), json!("underlay"));
                recs[k] = (m, Vec::new());
            }
        }
    }
    let items = recs
        .into_iter()
        .map(|(rec, fr)| {
            let frames = if matches!(super::verdict(&rec), "none" | "blank") {
                Vec::new()
            } else {
                timed(&fr)
            };
            Analysed {
                rec,
                frames,
                raw: None,
                eye: Vec::new(),
            }
        })
        .collect();
    Analysis {
        geometry: g,
        folder: "scene",
        items,
    }
}

/// Pictures with their start from the first picture and their duration (next start minus
/// own start, the last 1 s), in seconds.
fn timed(fr: &[Pic]) -> Vec<Shot> {
    let t0 = fr.first().map_or(0.0, |f| f.0);
    fr.iter()
        .enumerate()
        .map(|(k, f)| {
            let s = f.0 - t0;
            let e = fr.get(k + 1).map_or(s + 1.0, |n| n.0 - t0).max(s + 0.001);
            Shot {
                t: s,
                d: e - s,
                dots: f.1.to_vec(),
            }
        })
        .collect()
}

/// Read a scan folder of text dumps (`<cmd>_<id>.txt`: `0x<ms>` then one row of level digits
/// per line, a blank line after each picture) and its log (`id verdict ...` per line).
pub fn read_dumps(dir: &Path, log: &Path) -> Result<Vec<Scene>, String> {
    let text = std::fs::read_to_string(log).map_err(|e| io_err(log, e))?;
    let mut kind: HashMap<u32, String> = HashMap::new();
    for l in text.lines() {
        let mut f = l.split_whitespace();
        if let (Some(id), Some(k)) = (f.next(), f.next())
            && let Ok(id) = id.parse()
        {
            kind.insert(id, k.to_string());
        }
    }
    let mut out = Vec::new();
    let rd = std::fs::read_dir(dir).map_err(|e| io_err(dir, e))?;
    for e in rd.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(id) = name
            .strip_suffix(".txt")
            .and_then(|n| n.rsplit('_').next())
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let p = e.path();
        let t = std::fs::read_to_string(&p).map_err(|err| io_err(&p, err))?;
        let mut frames = Vec::new();
        for block in t.split("\n\n").filter(|b| !b.trim().is_empty()) {
            let mut rows = block.lines();
            let Some(ms) = rows
                .next()
                .and_then(|h| u64::from_str_radix(h.trim().trim_start_matches("0x"), 16).ok())
            else {
                continue;
            };
            let px: Vec<u8> = rows
                .flat_map(|r| r.chars().map(|c| c.to_digit(16).unwrap_or(0) as u8))
                .collect();
            frames.push((ms, px));
        }
        out.push(Scene {
            id,
            kind: kind.get(&id).cloned().unwrap_or_default(),
            frames,
            extra: Map::new(),
        });
    }
    out.sort_by_key(|s| s.id);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture whose lit dots depend on `v` (every (v+1)th dot, in shade v)
    fn pic(v: u8) -> Vec<u8> {
        (0..128 * 32)
            .map(|i| if i % (v as usize + 1) == 0 { v } else { 0 })
            .collect()
    }

    #[test]
    fn resting_screen_dropped_and_verdicts() {
        let rest = pic(1);
        let mut scenes = Vec::new();
        for id in 0..3 {
            scenes.push(Scene {
                id,
                kind: "end".into(),
                frames: vec![(0, rest.clone()), (100, pic(2 + id as u8)), (900, pic(3))],
                extra: Map::new(),
            });
        }
        scenes.push(Scene {
            id: 3,
            kind: "none".into(),
            frames: vec![(0, rest.clone())],
            extra: Map::new(),
        });
        let o = Opts {
            geometry: Geometry::linear(128, 32, 4),
            blend: false,
        };
        let a = analyse(&scenes, &o);
        let s = a.summary();
        assert_eq!(s.counts.get("end"), Some(&3));
        assert_eq!(s.counts.get("none"), Some(&1));
        assert_eq!(a.items[0].frames.len(), 2);
    }

    #[test]
    fn animated_resting_screen_dropped() {
        // the resting screen shimmers: phase b lights the same dots in another shade
        let (a, mut b) = (pic(1), pic(1));
        b[4] = 3;
        let scenes: Vec<Scene> = (0..3)
            .map(|id| Scene {
                id,
                kind: "end".into(),
                frames: vec![(0, a.clone()), (24, b.clone()), (100, pic(5 + id as u8))],
                extra: Map::new(),
            })
            .collect();
        let o = Opts {
            geometry: Geometry::linear(128, 32, 4),
            blend: false,
        };
        let a = analyse(&scenes, &o);
        let s = a.summary();
        assert_eq!(s.counts.get("end"), Some(&3));
        assert_eq!(a.items[0].frames.len(), 1);
    }
}
