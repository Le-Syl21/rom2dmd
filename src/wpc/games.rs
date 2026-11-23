//! Per-game facts for the WPC scene tracer: the WPC-EMU game database entries it needs
//! (switch numbers and names, switches closed at power-on, start-up actions; `games.txt`)
//! and the start-up differences found by hand while extracting every WPC game (`SETTINGS`).
//! No ROM data.

use super::Generation;

const DB: &str = include_str!("games.txt");

/// A start-up action of the WPC-EMU database, run after `delay_ms`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Press coin-door keys (bit mask) for half a second.
    Cabinet(u8),
    /// Write a RAM byte (e.g. free play).
    Memory(u16, u8),
    /// A switch action; the tracer only waits its delay (as the reference tracer did).
    Switch(u8),
}

#[derive(Clone, Debug)]
pub struct Game {
    pub name: String,
    pub known: Vec<String>,
    pub features: Vec<String>,
    pub closed: Vec<u8>,
    pub actions: Vec<(u32, Action)>,
    pub switches: Vec<(u8, String)>,
}

impl Game {
    /// The board generation from WPC-EMU's features.
    pub fn generation(&self) -> Generation {
        let has = |f: &str| self.features.iter().any(|x| x == f);
        if has("wpc95") {
            Generation::Wpc95
        } else if has("securityPic") {
            Generation::Security
        } else if has("wpcDcs") {
            Generation::Dcs
        } else if has("wpcFliptronics") {
            Generation::Fliptronic
        } else {
            Generation::Dmd
        }
    }

    /// Switch numbers whose name contains one of `words` (case-insensitive), in database order.
    pub fn named(&self, words: &[&str]) -> Vec<u8> {
        self.switches
            .iter()
            .filter(|(_, n)| contains_any(n, words))
            .map(|&(id, _)| id)
            .collect()
    }
}

/// Case-insensitive substring test against several words.
pub fn contains_any(name: &str, words: &[&str]) -> bool {
    let n = name.to_ascii_uppercase();
    words
        .iter()
        .any(|w| !w.is_empty() && n.contains(&w.to_ascii_uppercase()))
}

fn parse_line(l: &str) -> Option<Game> {
    let f: Vec<&str> = l.split('\t').collect();
    if f.len() != 6 {
        return None;
    }
    let list = |s: &str| -> Vec<String> {
        s.split(',')
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .collect()
    };
    let mut actions = Vec::new();
    for a in f[4].split(',').filter(|x| !x.is_empty()) {
        let p: Vec<&str> = a.split(':').collect();
        let delay = p.first()?.parse().ok()?;
        let act = match (p.get(1).copied(), p.len()) {
            (Some("cabinet"), 3) => Action::Cabinet(p[2].parse().ok()?),
            (Some("memory"), 4) => Action::Memory(p[2].parse().ok()?, p[3].parse().ok()?),
            (Some("switch"), 3) => Action::Switch(p[2].parse().ok()?),
            _ => return None,
        };
        actions.push((delay, act));
    }
    let mut switches = Vec::new();
    for s in f[5].split('|').filter(|x| !x.is_empty()) {
        let (id, name) = s.split_once('=')?;
        switches.push((id.parse().ok()?, name.to_string()));
    }
    Some(Game {
        name: f[0].to_string(),
        known: list(f[1]),
        features: list(f[2]),
        closed: f[3]
            .split(',')
            .filter(|x| !x.is_empty())
            .map(|x| x.parse().ok())
            .collect::<Option<_>>()?,
        actions,
        switches,
    })
}

/// Every game of the database.
pub fn all() -> Vec<Game> {
    DB.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(parse_line)
        .collect()
}

/// A game by its database name or one of its PinMAME names (e.g. "t2" or "t2_l8").
pub fn find(name: &str) -> Option<Game> {
    let n = name.to_ascii_lowercase();
    let games = all();
    if let Some(g) = games.iter().find(|g| g.name == n) {
        return Some(g.clone());
    }
    games.into_iter().find(|g| g.known.contains(&n))
}

/// Start-up settings of one game (defaults: none of them).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Start through coins, Start and played balls even on a WPC-95 machine (which otherwise
    /// starts from Start alone).
    pub t2flow: bool,
    /// Switch-name words kept out of the random play (their native effect masks the others).
    pub playskip: Vec<String>,
    /// Snapshot at this effect request (1 = the first).
    pub snapn: u32,
    /// Snapshot at the first effect request made while coins go in (a game that never starts
    /// in the emulator).
    pub snapearly: bool,
    /// Seconds of attract mode watched for an attract snapshot (default 120).
    pub attract_s: Option<u32>,
    /// RAM bytes cleared at injection (when empty, the probe's skip flag).
    pub zero: Vec<u16>,
}

/// Per-game differences found by hand (the reference runner's `wpc_games.tsv`):
/// - T2FLOW: WPC-95 games that do not start from Start alone (tom, ss, jy, totan).
/// - ss: random play hit the leapers and the native "LEAPER MANIA" effect masked every request.
/// - nofear never starts a game in the emulator and attract makes no request: snapshot at the
///   first request while coins go in.
/// - fishtales: the first play request is effect 16, whose companion task kills any effect
///   that replaces it, so the second request is the snapshot. fishtales/tpz zero their "skip
///   this effect" flag (the probe finds it on every game now).
pub fn settings(game: &str) -> Settings {
    let mut s = Settings {
        snapn: 1,
        ..Default::default()
    };
    match game {
        "tom" | "jy" | "totan" => s.t2flow = true,
        "ss" => {
            s.t2flow = true;
            s.playskip = vec!["LEAPER".into()];
        }
        "nofear" => {
            s.snapearly = true;
            s.attract_s = Some(20);
        }
        "fishtales" => {
            s.snapn = 2;
            s.zero = vec![0x432];
        }
        "tpz" => s.zero = vec![0x486],
        _ => {}
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_parses() {
        let g = all();
        assert!(g.len() >= 49, "{} games", g.len());
        let t2 = find("t2_l8").expect("t2");
        assert_eq!(t2.name, "t2");
        assert_eq!(t2.closed, vec![15, 16, 17, 22, 23]);
        assert_eq!(
            t2.actions,
            vec![(1000, Action::Cabinet(16)), (3000, Action::Memory(7070, 1))]
        );
        assert_eq!(t2.named(&["TROUGH"]), vec![15, 16, 17]);
        assert_eq!(find("afm").map(|g| g.generation()), Some(Generation::Wpc95));
        assert_eq!(
            find("tom").map(|g| g.generation()),
            Some(Generation::Security)
        );
        assert_eq!(settings("fishtales").snapn, 2);
    }
}
