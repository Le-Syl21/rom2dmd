//! A ROM set: the files of a PinMAME zip, identified as a board family.
//!
//! Identification: the zip's name is looked up in `sets.txt` (PinMAME set name, family, the
//! ROM file names the display scan needs; generated from PinMAME's driver ROM macros, names
//! only). A zip with another name is matched by its file names (the set whose files are all
//! there, the one with most files winning), then by content (a Stern SAM image is one file of
//! 16 MiB or more; a WPC game ROM carries the WPC game ID).

use std::io::Read;
use std::path::Path;

const SETS: &str = include_str!("sets.txt");

/// Board families the single command knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// Williams WPC (DMD, Fliptronic, DCS, Security, WPC-95).
    Wpc(crate::wpc::Generation),
    /// Data East 128x32 display board.
    De32,
    /// Sega / Stern Whitestar 128x32 display board.
    Ws32,
    /// Data East 128x16 display board.
    De16,
    /// Sega 192x64 display board.
    De64,
    /// Gottlieb System 3 display board.
    Gts3,
    /// Alvin G. PCA020A display board.
    Alvg,
    /// Alvin G. PCA020 display board (program in the upper half of its ROM).
    AlvgSplit,
    /// Capcom main board.
    Capcom,
    /// Stern S.A.M. main board.
    Sam,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Family::Wpc(_) => "Williams WPC",
            Family::De32 => "Data East 128x32",
            Family::Ws32 => "Sega/Stern Whitestar 128x32",
            Family::De16 => "Data East 128x16",
            Family::De64 => "Sega 192x64",
            Family::Gts3 => "Gottlieb System 3",
            Family::Alvg => "Alvin G. PCA020A",
            Family::AlvgSplit => "Alvin G. PCA020",
            Family::Capcom => "Capcom",
            Family::Sam => "Stern S.A.M.",
        }
    }

    fn parse(s: &str) -> Option<Family> {
        use crate::wpc::Generation as G;
        Some(match s {
            "de32" => Family::De32,
            "ws32" => Family::Ws32,
            "de16" => Family::De16,
            "de64" => Family::De64,
            "gts3" => Family::Gts3,
            "alvg" => Family::Alvg,
            "alvgsplit" => Family::AlvgSplit,
            "cap" => Family::Capcom,
            "sam" => Family::Sam,
            w => Family::Wpc(G::parse(w.strip_prefix("wpc:")?)?),
        })
    }
}

/// One row of the set table.
#[derive(Clone, Debug)]
pub struct SetInfo {
    pub set: String,
    pub family: Family,
    pub roms: Vec<String>,
}

fn table() -> Vec<SetInfo> {
    SETS.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (set, fam, roms) = (f.next()?, f.next()?, f.next()?);
            Some(SetInfo {
                set: set.to_string(),
                family: Family::parse(fam)?,
                roms: roms.split(',').map(str::to_lowercase).collect(),
            })
        })
        .collect()
}

/// The set table entry of a PinMAME set name.
#[cfg(test)]
pub fn lookup(set: &str) -> Option<SetInfo> {
    let s = set.to_lowercase();
    table().into_iter().find(|i| i.set == s)
}

/// The files of a zip and what they are.
pub struct RomSet {
    /// The PinMAME set name (the table's, else the zip's name).
    pub set: String,
    pub family: Family,
    /// The display scan's ROM files, in the table's order (empty when found by content).
    pub roms: Vec<String>,
    /// Every file: (name, contents).
    pub files: Vec<(String, Vec<u8>)>,
}

impl RomSet {
    /// A file by name (case-insensitive).
    pub fn file(&self, name: &str) -> Result<&[u8], String> {
        self.files
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, d)| d.as_slice())
            .ok_or_else(|| format!("{}: ROM file {name} missing from the zip", self.set))
    }

    /// The `k`-th ROM file of the table entry.
    pub fn rom(&self, k: usize) -> Result<&[u8], String> {
        let n = self
            .roms
            .get(k)
            .ok_or_else(|| format!("{}: no ROM file #{k} in the set table", self.set))?;
        self.file(n)
    }
}

/// Read every file of a zip.
pub fn read_zip(path: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    read_zip_from(std::io::BufReader::new(f), &path.display().to_string())
}

/// Read every file of a zip from any reader; `label` names it in errors.
pub fn read_zip_from<R: std::io::Read + std::io::Seek>(
    r: R,
    label: &str,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let err = |e: &dyn std::fmt::Display| format!("{label}: {e}");
    let mut zip = zip::ZipArchive::new(r).map_err(|e| err(&e))?;
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| err(&e))?;
        if f.is_dir() {
            continue;
        }
        let name = f.name().map_err(|e| err(&e))?;
        let name = name.rsplit('/').next().unwrap_or(&name).to_string();
        let mut v = Vec::new();
        f.read_to_end(&mut v).map_err(|e| err(&e))?;
        out.push((name, v));
    }
    Ok(out)
}

/// Open and identify a ROM zip.
pub fn open(path: &Path) -> Result<RomSet, String> {
    let files = read_zip(path)?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    identify_files(&stem, files, &path.display().to_string())
}

/// Open and identify a ROM zip held in memory; `name` is the zip's file name (its stem is
/// the PinMAME set name when it is one).
pub fn open_bytes(name: &str, zip: &[u8]) -> Result<RomSet, String> {
    let files = read_zip_from(std::io::Cursor::new(zip), name)?;
    let stem = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    identify_files(&stem, files, name)
}

/// Identify a set from its files; `stem` is the zip's name without extension, `label` names
/// the zip in errors.
fn identify_files(
    stem: &str,
    files: Vec<(String, Vec<u8>)>,
    label: &str,
) -> Result<RomSet, String> {
    let stem = stem.to_string();
    let names: Vec<String> = files.iter().map(|(n, _)| n.to_lowercase()).collect();
    let has = |i: &SetInfo| i.roms.iter().all(|r| names.contains(r));
    let tab = table();
    let by_name = tab.iter().find(|i| i.set == stem);
    let found = match by_name {
        Some(i) if has(i) => Some(i.clone()),
        _ => tab
            .iter()
            .filter(|i| has(i))
            .max_by_key(|i| i.roms.len())
            .cloned(),
    };
    if let Some(i) = found {
        return Ok(RomSet {
            set: if by_name.is_some() {
                stem
            } else {
                i.set.clone()
            },
            family: i.family,
            roms: i.roms,
            files,
        });
    }
    if let Some(i) = by_name {
        return Err(format!(
            "{}: the zip lacks ROM files of set {} ({}); a clone set may need its parent's files",
            label,
            i.set,
            i.roms.join(", ")
        ));
    }
    // by content
    if let Some((n, _)) = files.iter().find(|(_, d)| d.len() >= 16 << 20) {
        return Ok(RomSet {
            set: stem,
            family: Family::Sam,
            roms: vec![n.to_lowercase()],
            files,
        });
    }
    if let Some((n, _)) = files
        .iter()
        .find(|(_, d)| crate::wpc::find_game_id(d).is_some())
    {
        return Ok(RomSet {
            set: stem,
            family: Family::Wpc(crate::wpc::Generation::Dmd),
            roms: vec![n.to_lowercase()],
            files,
        });
    }
    Err(format!(
        "{}: not a known PinMAME set and no supported board recognised (files: {})",
        label,
        names.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_parses() {
        let t = table();
        assert!(t.len() > 1000, "{} sets", t.len());
        let t2 = lookup("t2_l8").expect("t2");
        assert_eq!(t2.family, Family::Wpc(crate::wpc::Generation::Dmd));
        assert_eq!(lookup("lah_112").map(|i| i.family), Some(Family::De32));
        assert_eq!(lookup("gldneye").map(|i| i.family), Some(Family::Ws32));
        let sf2 = lookup("sfight2").expect("sf2");
        assert_eq!((sf2.family, sf2.roms.len()), (Family::Gts3, 2));
    }
}
