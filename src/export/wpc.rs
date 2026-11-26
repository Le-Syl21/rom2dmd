//! WPC effect export and classification (port of the reference `v5_export.py`, `segment.py`,
//! `render_eye.py` and `v5_classify.py`, generation v6).
//!
//! Input: the three passes of `wpc-scan` as text (their `.log`, `.effects.ndjson` and `.raw`
//! contents). Pictures are rebuilt from the refresh log: a WPC shaded picture is a page pair
//! cycled (high, high, low) refresh by refresh, so a window of 3 refreshes holding exactly
//! the two pages of one pair, one of them twice, is that picture; a window that straddles two
//! pairs (a page swap) is dropped instead of being mixed, and so is one whose neighbours do
//! not use the same pair. Levels are `2 * high + low` (0..3), a page shown alone is level 3.
//!
//! Verdicts (one per effect id):
//! - `finite`: the effect ended by itself (its task freed itself); `length_s`;
//! - `cut`: another task killed it (the protected re-run is used when it exists);
//! - `loop`: still running at the cap and repeating: `cycle_from_s`, `cycle_s`, `repeat_score`;
//! - `hold`: still running at the cap on one unchanging picture;
//! - `running`: still running at the cap, not periodic;
//! - `blank`: nothing but black; `silent`: the effect never drew nor started another one (what
//!   was captured is the game's other display); `refused`: the game never started it;
//!   `missing`: no capture.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde_json::{Map, Value, json};

use super::{
    Analysed, Analysis, Geometry, Periodic, Shot, io_err, par_map, periodicity, round_to,
    sample_20hz,
};

/// Ticks per second of the refresh log's time base.
const TPS: f64 = 2_000_000.0;
const PAGE: usize = 512;
const DOTS: usize = 128 * 32;

/// Which pass of the scan a text comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pass {
    /// Pass 1: started through the game's own starter.
    Normal,
    /// Pass 2: refused in pass 1, started with the starter's checks bypassed.
    Bypassed,
    /// Pass 3: killed by another task, run again protected from such kills.
    Protected,
}

impl Pass {
    fn name(self) -> &'static str {
        match self {
            Pass::Normal => "normal",
            Pass::Bypassed => "bypassed",
            Pass::Protected => "protected",
        }
    }
}

/// One output chunk of a pass (or several concatenated): the scan's text formats.
#[derive(Clone, Debug)]
pub struct PassText {
    pub pass: Pass,
    pub log: String,
    pub nd: String,
    pub raw: String,
}

/// What the tracer said about one effect run.
#[derive(Clone, Debug, Default)]
struct Info {
    kind: Option<String>,
    span: f64,
    killer: Option<String>,
    /// (non-zero bytes the effect's own task wrote, chained effects), from the ndjson records.
    own: Option<(u64, usize)>,
}

/// One effect's refresh log: distinct pages and (tick, page number, content index).
#[derive(Clone, Debug, Default)]
struct Effect {
    pages: Vec<[u8; PAGE]>,
    refreshes: Vec<(u64, u8, usize)>,
    raw: String,
}

fn bit(p: &[u8; PAGE], i: usize) -> u8 {
    (p[i / 8] >> (i % 8)) & 1
}

fn unhex(s: &str) -> Option<[u8; PAGE]> {
    let b = s.as_bytes();
    if b.len() != 2 * PAGE {
        return None;
    }
    let mut out = [0u8; PAGE];
    for (k, o) in out.iter_mut().enumerate() {
        let h = (b[2 * k] as char).to_digit(16)?;
        let l = (b[2 * k + 1] as char).to_digit(16)?;
        *o = (h * 16 + l) as u8;
    }
    Some(out)
}

/// Split a raw refresh log (`#id`, `=k hex`, `tick page k`) into effects.
fn parse_raw(text: &str, out: &mut BTreeMap<u32, Effect>) -> Result<(), String> {
    let mut cur: Option<(u32, Effect)> = None;
    let mut idx: Vec<Option<usize>> = Vec::new();
    let mut seen: HashMap<[u8; PAGE], usize> = HashMap::new();
    let bad = |l: &str| {
        format!(
            "bad raw log line: {}",
            l.chars().take(60).collect::<String>()
        )
    };
    for l in text.lines() {
        if let Some(id) = l.strip_prefix('#') {
            if let Some((id, e)) = cur.take() {
                out.insert(id, e);
            }
            let id = id.trim().parse().map_err(|_| bad(l))?;
            cur = Some((id, Effect::default()));
            idx.clear();
            seen.clear();
            if let Some((_, e)) = cur.as_mut() {
                e.raw.push_str(l);
                e.raw.push('\n');
            }
            continue;
        }
        let Some((_, e)) = cur.as_mut() else {
            continue;
        };
        e.raw.push_str(l);
        e.raw.push('\n');
        if let Some(rest) = l.strip_prefix('=') {
            let (k, h) = rest.split_once(' ').ok_or_else(|| bad(l))?;
            let k: usize = k.parse().map_err(|_| bad(l))?;
            let page = unhex(h.trim()).ok_or_else(|| bad(l))?;
            if idx.len() <= k {
                idx.resize(k + 1, None);
            }
            // identical contents share one entry (the scan already deduplicates them)
            let n = e.pages.len();
            let at = *seen.entry(page).or_insert(n);
            if at == n {
                e.pages.push(page);
            }
            idx[k] = Some(at);
        } else {
            let mut f = l.split_whitespace();
            let (Some(t), Some(p), Some(k), None) = (f.next(), f.next(), f.next(), f.next()) else {
                return Err(bad(l));
            };
            let t: u64 = t.parse().map_err(|_| bad(l))?;
            let p: u8 = p.parse().map_err(|_| bad(l))?;
            let k: usize = k.parse().map_err(|_| bad(l))?;
            let at = idx.get(k).copied().flatten().ok_or_else(|| bad(l))?;
            e.refreshes.push((t, p, at));
        }
    }
    if let Some((id, e)) = cur {
        out.insert(id, e);
    }
    Ok(())
}

/// `CAP id kind frames n ... novel span a-bs` and `YT died {json}` lines (the killer of the
/// next CAP line), plus the own-drawing counters of the ndjson records.
fn parse_info(log: &str, nd: &str, out: &mut BTreeMap<u32, Info>) {
    let mut own: BTreeMap<u32, (u64, usize)> = BTreeMap::new();
    for l in nd.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        let Some(id) = v.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let nz = v.get("ownNonzero").and_then(Value::as_u64).unwrap_or(0);
        let ch = v
            .get("chains")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        own.insert(id as u32, (nz, ch));
    }
    let mut killer: Option<String> = None;
    for l in log.lines() {
        if let Some(j) = l.strip_prefix("YT died ") {
            let j = j.split(" killerEntry").next().unwrap_or(j);
            let end = j.rfind('}').map_or(j.len(), |e| e + 1);
            killer = serde_json::from_str::<Value>(&j[..end])
                .ok()
                .and_then(|v| v.get("by").and_then(Value::as_str).map(str::to_string));
            continue;
        }
        let Some(rest) = l.strip_prefix("CAP ") else {
            continue;
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() < 4 || f[2] != "frames" {
            continue;
        }
        let Ok(id) = f[0].parse::<u32>() else {
            continue;
        };
        let Some(span) = rest
            .split("novel span ")
            .nth(1)
            .and_then(|s| s.split('s').next())
            .and_then(|s| s.split_once('-'))
            .and_then(|(_, b)| b.parse::<f64>().ok())
        else {
            continue;
        };
        out.insert(
            id,
            Info {
                kind: Some(f[1].to_string()),
                span,
                killer: killer.take(),
                own: own.get(&id).copied(),
            },
        );
    }
}

/// A stable picture: (start in seconds from the first one, duration in seconds, levels).
type Stable = (f64, f64, Vec<u8>);

/// Rebuild the displayed pictures of an effect (see the module documentation).
fn segment(e: &Effect) -> Vec<Stable> {
    let r = &e.refreshes;
    let n = r.len();
    let mut found: Vec<(u64, Vec<u8>)> = Vec::new();
    for i in 2..n {
        let w = [r[i - 2], r[i - 1], r[i]];
        let pages = [w[0].1, w[1].1, w[2].1];
        let mut set: Vec<u8> = pages.to_vec();
        set.sort_unstable();
        set.dedup();
        let img: Vec<u8> = match set.len() {
            1 => {
                if !(w[0].2 == w[1].2 && w[1].2 == w[2].2) {
                    continue;
                }
                let p = &e.pages[w[2].2];
                (0..DOTS).map(|j| 3 * bit(p, j)).collect()
            }
            2 => {
                let count = |p: u8| pages.iter().filter(|&&x| x == p).count();
                let (hi, lo) = if count(set[0]) > count(set[1]) {
                    (set[0], set[1])
                } else {
                    (set[1], set[0])
                };
                // the neighbouring windows must use the same pair (not a swap)
                let mut nb: Vec<u8> = r[i.saturating_sub(3)..(i + 2).min(n)]
                    .iter()
                    .map(|x| x.1)
                    .collect();
                nb.sort_unstable();
                nb.dedup();
                if nb != set {
                    continue;
                }
                let at = |p: u8| w.iter().find(|x| x.1 == p).map_or(0, |x| x.2);
                let (hp, lp) = (&e.pages[at(hi)], &e.pages[at(lo)]);
                (0..DOTS).map(|j| 2 * bit(hp, j) + bit(lp, j)).collect()
            }
            _ => continue,
        };
        if found.last().is_none_or(|l| l.1 != img) {
            found.push((r[i].0, img));
        }
    }
    // durations as the reference wrote them (4 decimals), the last one 1 s; starts from 0
    let mut out = Vec::with_capacity(found.len());
    let mut t = 0.0;
    for k in 0..found.len() {
        let next = found.get(k + 1).map_or(found[k].0 + 2_000_000, |f| f.0);
        let d = round_to(next.saturating_sub(found[k].0) as f64 / TPS, 4);
        out.push((t, d, std::mem::take(&mut found[k].1)));
        t += d;
    }
    out
}

/// The effect's verdict and its manifest fields.
fn classify(info: &Info, frames: &[Stable]) -> Map<String, Value> {
    let mut m = Map::new();
    let verdict = |m: &mut Map<String, Value>, v: &str| {
        m.insert("verdict".into(), json!(v));
    };
    if info.kind.as_deref() == Some("refused") {
        verdict(&mut m, "refused");
        return m;
    }
    if info.own == Some((0, 0)) {
        verdict(&mut m, "silent");
        return m;
    }
    if frames.is_empty() || !frames.iter().any(|f| f.2.iter().any(|&v| v != 0)) {
        verdict(&mut m, "blank");
        return m;
    }
    if info.kind.as_deref() == Some("finite") {
        let k = info.killer.as_deref();
        verdict(
            &mut m,
            if matches!(k, None | Some("self")) {
                "finite"
            } else {
                "cut"
            },
        );
        m.insert("killer".into(), json!(k));
        m.insert("length_s".into(), json!(round_to(info.span, 2)));
        return m;
    }
    // still running at the cap: sample at 20 Hz (pictures compared by content)
    let keyed: Vec<(f64, f64, usize)> = {
        let mut uniq: HashMap<&[u8], usize> = HashMap::new();
        frames
            .iter()
            .map(|(t, d, img)| {
                let n = uniq.len();
                (*t, *d, *uniq.entry(img.as_slice()).or_insert(n))
            })
            .collect()
    };
    let series = sample_20hz(&keyed, 0.0);
    match periodicity(&series) {
        Periodic::Hold => verdict(&mut m, "hold"),
        Periodic::Running => verdict(&mut m, "running"),
        Periodic::Loop { from, lag, agree } => {
            verdict(&mut m, "loop");
            m.insert(
                "cycle_from_s".into(),
                json!(round_to(from as f64 * 0.05, 2)),
            );
            m.insert("cycle_s".into(), json!(round_to(lag as f64 * 0.05, 2)));
            m.insert("repeat_score".into(), json!(round_to(agree, 3)));
        }
    }
    m
}

/// Python's `round` (ties to even) of `num / den`.
#[cfg(feature = "export")]
fn round_div(num: u32, den: u32) -> u32 {
    let (q, r) = (num / den, num % den);
    if 2 * r > den || (2 * r == den && q % 2 == 1) {
        q + 1
    } else {
        q
    }
}

/// The eye model: each refresh shows one page; the glass brightness is the mean of the last 6
/// refreshes, sampled at 60 fps from the first refresh up to `until` ticks after it.
#[cfg(feature = "export")]
fn eye(e: &Effect, until: Option<u64>) -> Vec<Vec<u8>> {
    const W: usize = 6;
    let r = &e.refreshes;
    let (Some(first), Some(last)) = (r.first(), r.last()) else {
        return Vec::new();
    };
    let stop = until.map_or(last.0, |u| last.0.min(first.0 + u));
    let mut acc = [0u32; DOTS];
    let (mut lo, mut hi) = (0usize, 0usize);
    let mut out = Vec::new();
    let mut k = 0u64;
    loop {
        let t = first.0 as f64 / TPS + k as f64 / 60.0;
        if t > stop as f64 / TPS {
            break;
        }
        while hi < r.len() && (r[hi].0 as f64 / TPS) <= t {
            let p = &e.pages[r[hi].2];
            for (j, a) in acc.iter_mut().enumerate() {
                *a += bit(p, j) as u32;
            }
            hi += 1;
        }
        while hi - lo > W {
            let p = &e.pages[r[lo].2];
            for (j, a) in acc.iter_mut().enumerate() {
                *a -= bit(p, j) as u32;
            }
            lo += 1;
        }
        let n = (hi - lo).max(1) as u32;
        out.push(
            acc.iter()
                .map(|&a| round_div(255 * a, n).min(255) as u8)
                .collect(),
        );
        k += 1;
    }
    out
}

/// Merge the passes and classify every effect.
pub fn analyse(passes: &[PassText]) -> Result<Analysis, String> {
    let mut info: BTreeMap<Pass, BTreeMap<u32, Info>> = BTreeMap::new();
    let mut effects: BTreeMap<Pass, BTreeMap<u32, Effect>> = BTreeMap::new();
    for p in passes {
        parse_info(&p.log, &p.nd, info.entry(p.pass).or_default());
        parse_raw(&p.raw, effects.entry(p.pass).or_default())?;
    }
    let empty = BTreeMap::new();
    let get = |p: Pass| info.get(&p).unwrap_or(&empty);
    let (normal, byp, prot) = (get(Pass::Normal), get(Pass::Bypassed), get(Pass::Protected));
    let ids: BTreeSet<u32> = normal.keys().chain(byp.keys()).copied().collect();
    let default = Info::default();
    let ids: Vec<u32> = ids.into_iter().collect();
    let done = par_map(&ids, |&i| -> Analysed {
        let (mut inf, mut src) = (normal.get(&i).unwrap_or(&default), Pass::Normal);
        if inf.kind.as_deref() == Some("refused")
            && let Some(b) = byp.get(&i)
        {
            (inf, src) = (b, Pass::Bypassed);
        }
        // cut short by another task: the protected run instead
        let mut cut = None;
        if inf.kind.as_deref() == Some("finite")
            && !matches!(inf.killer.as_deref(), None | Some("self"))
            && let Some(p) = prot.get(&i)
        {
            cut = Some((inf, src));
            (inf, src) = (p, Pass::Protected);
        }
        let run = |inf: &Info, src: Pass| {
            let e = effects
                .get(&src)
                .and_then(|m| m.get(&i))
                .filter(|e| !e.refreshes.is_empty());
            match e {
                Some(e) => {
                    let fr = segment(e);
                    let v = classify(inf, &fr);
                    (Some(e), fr, v)
                }
                None => {
                    let mut v = Map::new();
                    v.insert("verdict".into(), json!("missing"));
                    (None, Vec::new(), v)
                }
            }
        };
        let mut r = run(inf, src);
        // protected yet still changing at the cap with no period: the game's kill was the
        // effect's real end (a timed splash); keep the cut capture
        if let Some((ci, cs)) = cut
            && r.2.get("verdict").and_then(Value::as_str) == Some("running")
        {
            src = cs;
            r = run(ci, cs);
        }
        let (e, frames, v) = r;
        let mut rec = Map::new();
        rec.insert("id".into(), json!(i));
        rec.insert("started".into(), json!(src.name()));
        rec.extend(v);
        rec.insert("stable_frames".into(), json!(frames.len()));
        #[cfg(feature = "export")]
        let eye = e.map_or(Vec::new(), |e| {
            let until = super::cycle(&rec).map(|(a, c)| ((a + c) * TPS) as u64 + 2 * 16_384);
            eye(e, until)
        });
        #[cfg(not(feature = "export"))]
        let eye = Vec::new();
        let frames = if matches!(super::verdict(&rec), "blank") {
            Vec::new()
        } else {
            frames
                .into_iter()
                .map(|(t, d, dots)| Shot { t, d, dots })
                .collect()
        };
        Analysed {
            rec,
            frames,
            raw: e.map(|e| e.raw.clone()),
            eye,
        }
    });
    let mut done = done;
    trim_edges(&mut done);
    Ok(Analysis {
        geometry: Geometry::linear(128, 32, 4),
        folder: "effect",
        items: done,
    })
}

/// Drop what belongs to no effect: the black an effect starts on (the screen cleared before it
/// draws) and, once it ended, the game's own display coming back. That display (the score
/// screen, in each phase of its blinking) is found as the last pictures shared by at least a
/// tenth of the ended effects. An effect left with nothing lit only showed the game's display:
/// `silent`.
fn trim_edges(items: &mut [Analysed]) {
    let ended = |it: &Analysed| matches!(super::verdict(&it.rec), "finite" | "cut");
    let mut lasts: HashMap<&[u8], usize> = HashMap::new();
    let mut n_ended = 0usize;
    for it in items.iter().filter(|it| ended(it)) {
        n_ended += 1;
        // every picture of the closing run counts once, so each blink phase is seen
        let mut seen: BTreeSet<&[u8]> = BTreeSet::new();
        for f in it.frames.iter().rev().take(6) {
            if seen.insert(&f.dots) {
                *lasts.entry(&f.dots).or_default() += 1;
            }
        }
    }
    let floor = 5.max(n_ended.div_ceil(10));
    let rest: BTreeSet<Vec<u8>> = lasts
        .into_iter()
        .filter(|&(_, n)| n >= floor)
        .map(|(p, _)| p.to_vec())
        .collect();
    let black = |s: &Shot| s.dots.iter().all(|&v| v == 0);
    for it in items.iter_mut() {
        if super::verdict(&it.rec) == "silent" {
            // never drew: what the display showed is the game's own screen
            it.frames.clear();
            it.eye.clear();
            continue;
        }
        if it.frames.is_empty() {
            continue;
        }
        let was_ended = ended(it);
        let lead = it.frames.iter().take_while(|s| black(s)).count();
        let mut keep = it.frames.len();
        if was_ended {
            while keep > lead
                && (black(&it.frames[keep - 1]) || rest.contains(&it.frames[keep - 1].dots))
            {
                keep -= 1;
            }
        }
        if lead == 0 && keep == it.frames.len() {
            continue;
        }
        let t0 = it.frames.get(lead).map_or(0.0, |s| s.t);
        let tail_end = it.frames.get(keep).map(|s| s.t);
        let mut fr: Vec<Shot> = it.frames.drain(..).take(keep).skip(lead).collect();
        for s in &mut fr {
            s.t = round_to(s.t - t0, 4);
        }
        // the eye model runs from the first refresh at 60 fps: cut it to the same span
        if !it.eye.is_empty() {
            let a = ((t0 * 60.0).floor() as usize).min(it.eye.len());
            let b = tail_end.map_or(it.eye.len(), |e| {
                ((e * 60.0).ceil() as usize).clamp(a, it.eye.len())
            });
            it.eye = it.eye[a..b].to_vec();
        }
        if !fr.iter().any(|s| !black(s)) {
            it.rec.insert("verdict".into(), json!("silent"));
            for k in [
                "killer",
                "length_s",
                "cycle_from_s",
                "cycle_s",
                "repeat_score",
            ] {
                it.rec.remove(k);
            }
            it.rec.insert("stable_frames".into(), json!(0));
            it.frames = Vec::new();
            it.eye = Vec::new();
            continue;
        }
        if tail_end.is_some() {
            let len: f64 = fr.iter().map(|s| s.d).sum();
            it.rec.insert("length_s".into(), json!(round_to(len, 2)));
        }
        if let Some(a) = it.rec.get("cycle_from_s").and_then(Value::as_f64) {
            it.rec
                .insert("cycle_from_s".into(), json!(round_to((a - t0).max(0.0), 2)));
        }
        it.rec.insert("stable_frames".into(), json!(fr.len()));
        it.frames = fr;
    }
}

/// Read the chunk files `wpc-scan` wrote for `game` in `dir` (`<game>_<n>`, `<game>_byp<n>`,
/// `<game>_prot<n>` with `.log`, `.effects.ndjson`, `.raw`).
pub fn read_scan_dir(dir: &Path, game: &str) -> Result<Vec<PassText>, String> {
    let mut out = Vec::new();
    let rd = std::fs::read_dir(dir).map_err(|e| io_err(dir, e))?;
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter_map(|n| n.strip_suffix(".raw").map(str::to_string))
        .collect();
    names.sort();
    for base in names {
        let Some(tag) = base.strip_prefix(&format!("{game}_")) else {
            continue;
        };
        let (pass, num) = if let Some(n) = tag.strip_prefix("byp") {
            (Pass::Bypassed, n)
        } else if let Some(n) = tag.strip_prefix("prot") {
            (Pass::Protected, n)
        } else {
            (Pass::Normal, tag)
        };
        if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let read = |ext: &str| {
            let p = dir.join(format!("{base}.{ext}"));
            std::fs::read_to_string(&p).map_err(|e| io_err(&p, e))
        };
        out.push(PassText {
            pass,
            log: read("log")?,
            nd: read("effects.ndjson").unwrap_or_default(),
            raw: read("raw")?,
        });
    }
    if out.is_empty() {
        return Err(format!("{}: no {game}_*.raw scan output", dir.display()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(fill: u8) -> String {
        format!("{:02x}", fill).repeat(PAGE)
    }

    /// A shaded picture (pages 1,1,2 cycled) then a mono one: two stable pictures, and the
    /// windows at the swap are dropped.
    #[test]
    fn segment_shaded_and_mono() {
        let mut raw = format!(
            "#7\n=0 {}\n=1 {}\n=2 {}\n",
            page(0xff),
            page(0x0f),
            page(0x01)
        );
        let mut t = 0;
        for k in 0..12 {
            let (p, c) = if k % 3 == 2 { (2, 1) } else { (1, 0) };
            raw += &format!("{t} {p} {c}\n");
            t += 16_384;
        }
        for _ in 0..6 {
            raw += &format!("{t} 3 2\n");
            t += 16_384;
        }
        let mut eff = BTreeMap::new();
        parse_raw(&raw, &mut eff).expect("raw");
        let fr = segment(&eff[&7]);
        assert_eq!(fr.len(), 2);
        // pixel 0: high page bit 1, low page (0x0f) bit 1 -> 3; pixel 4: high 1, low 0 -> 2
        assert_eq!((fr[0].2[0], fr[0].2[4]), (3, 2));
        // mono page 0x01: pixel 0 lit (3), pixel 1 dark
        assert_eq!((fr[1].2[0], fr[1].2[1]), (3, 0));
        assert_eq!(fr[1].1, 1.0);
    }

    #[test]
    fn log_lines() {
        let log = "YT died {\"by\":\"a71\",\"ret\":\"9b32\"} killerEntry 46c7 stack 00\n\
                   CAP 5 finite frames 12 layer 0 pat {} novel-distinct 3 raw 12 novel span 0.1-2.35s \n\
                   CAP 6 cap frames 1 layer 0 novel span 0.0-0.0s \n";
        let nd = "{\"id\":6,\"ownNonzero\":0,\"chains\":[]}\n";
        let mut m = BTreeMap::new();
        parse_info(log, nd, &mut m);
        assert_eq!(m[&5].killer.as_deref(), Some("a71"));
        assert_eq!(m[&5].span, 2.35);
        assert_eq!(m[&6].killer, None);
        assert_eq!(m[&6].own, Some((0, 0)));
        assert_eq!(classify(&m[&6], &[])["verdict"], "silent");
    }

    #[cfg(feature = "export")]
    #[test]
    fn python_round() {
        assert_eq!(round_div(255, 6), 42); // 42.5 -> 42
        assert_eq!(round_div(255 * 3, 6), 128); // 127.5 -> 128
    }
}
