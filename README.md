# rom2dmd

rom2dmd extracts the dot-matrix display (DMD) scenes of a pinball machine from its ROM
set. It runs the game's own code on an emulation of the board that drives the display. It
plays every display effect or scene the program knows, records what the display shows, and
gives each scene back as pictures with their durations, ready for a ZeDMD, a FullDMD or a
PinMAME-style DMD dump.

It does not need Python, Node, PinMAME or any address you type in. It recognises the board
family and the game from the ROM set. It finds the addresses it needs in the game's code
by signature. It runs the scan on all cores.

It comes in two parts:

- the **`rom2dmd` library** ([crates.io](https://crates.io/crates/rom2dmd)): the
  extraction alone. A ROM zip goes in; scenes of pictures (one shade level per dot), their
  durations, a verdict and the loop points come out, plus the DMD dump.
- the **`rom2dmd` command line** (`cli/`, binaries on the
  [releases page](https://github.com/Le-Syl21/rom2dmd/releases)): the library plus the file
  export (PNG, GIF, manifest) and the debug tools.

## Library

```toml
[dependencies]
rom2dmd = "0.1"
```

```rust
let x = rom2dmd::extract(std::path::Path::new("t2_l8.zip"), &rom2dmd::Options::default())?;
for s in x.scenes.iter().filter(|s| !s.frames.is_empty()) {
    // s.frames[k].dots: x.width * x.height shade levels, x.palette[level] is its grey
    println!("scene {} {}: {} pictures", s.id, s.verdict, s.frames.len());
}
x.write_dump(std::fs::File::create("t2_l8.txt").map_err(|e| e.to_string())?)
    .map_err(|e| e.to_string())?;
```

`extract_bytes` takes a zip already in memory. Progress goes through the
[`log`](https://crates.io/crates/log) crate. The `export` feature adds the command line's
file export. `examples/dump.rs` writes the dump of a ROM set.

## Command line

```
rom2dmd <rom.zip> <outdir> [--threads n] [--ids a-b,c,...] [--keep-work]
```

- `<rom.zip>`: a PinMAME ROM set (the zip name is the PinMAME set name, e.g. `t2_l8.zip`).
  A zip with another name is recognised by the names of the files inside it.
- `<outdir>`: where the export goes.
- `--threads`: number of emulation threads (default: all cores).
- `--ids`: scan only these effect or scene ids.
- `--keep-work`: WPC only, keep the scan's raw logs in `outdir/scan/`.

`rom2dmd identify <rom.zip>...` shows the family, the game and the addresses each scan
would use, without scanning.

## Output of the command line

`<set>.txt` is the DMD dump, in the format of PinMAME's `-dmd_dump_dir` (and VPinMAME's
DmdDump): for each picture a `0x%08x` time stamp in milliseconds, one line per row with one
hex digit per dot (the shade level), then an empty line. The scenes follow each other in id
order, 1 s apart; a loop is written up to the end of its first cycle.

There is one folder per effect (`effect_NNN`, WPC) or scene (`scene_NNN`, the other boards):

- `frames/fNNNN.png`: every distinct picture. Each shade level is a linear grey
  (level k of N is k/(N-1) of full white). No gamma is applied.
- `sheet.png`: a contact sheet of up to 36 pictures spread over the scene.
- `anim.gif`: the pictures with their real durations. For a loop, this is the intro and
  one cycle.
- `loop.gif`: one cycle of a loop.
- WPC only: `eye.png`, an animated PNG at 60 fps of what the glass shows. Each pixel is
  the mean of the last 6 display refreshes, so the shading and the crossfades look as they
  do on the machine. WPC only: `raw.txt`, the effect's refresh log.

`manifest.json` has one record per id with a verdict:

- WPC: `finite` (the effect ended by itself, `length_s`), `cut` (another task killed it),
  `loop` (`cycle_from_s`, `cycle_s`, `repeat_score`), `hold`, `running`, `blank`,
  `silent` (the effect never drew anything itself), `refused`, `missing`.
- The other boards: `end`, `loop` (`cycle_from_s`, `cycle_s`), `hold`, `running`,
  `blank`, `none` (the command did nothing).

`scan.txt` records how the scan ran: the addresses found and the verdicts of each pass.

## Supported families

| Family | Board emulated | What is found in the ROM |
| --- | --- | --- |
| Williams WPC (DMD, Fliptronic, DCS, Security, WPC-95) | CPU board (6809) | effect starter, task creator/free, protect and bypass points, priority and skip bytes |
| Data East 128x32, Sega/Stern Whitestar 128x32 | display board (6809) | nothing (scene = packet `02 9b id`) |
| Data East 128x16 | display board (Z80) | nothing (scene = packet `02 1b id`) |
| Sega 192x64 | display board (68000) | nothing (scene = packet `02 9b id`) |
| Gottlieb System 3 | display board (65C02) | start-sequence key (byte sum of the main CPU ROM) |
| Alvin G. PCA020A / PCA020 | display board (8031) | command-byte routine and main loop return |
| Capcom | main board (68306) | display manager (DM_ADD), running-scene test, scenes (DM_ADD call sites) |
| Stern S.A.M. | main board (AT91, ARM7) | deff_start, current effect and task, task_sleep, effect count |

The WPC scan plays a game at random to get an in-game snapshot. It needs the game's switch
names to do that. They come from the game database of
[WPC-EMU](https://github.com/neophob/wpc-emu) (Apache-2.0), reduced to the facts the scan
needs in `src/wpc/games.txt`. Games that are not in it are
scanned from attract mode. Stern Spike and other boards are not supported.

## Your ROMs only

rom2dmd is an extractor. Run it only on ROM sets you own. The pictures it extracts belong
to the makers of the games. Never redistribute them. This repository contains no ROM data
and no extracted frames. The tests read ROM sets from `$ROM2DMD_ROMS` (default
`~/roms-full`) and skip when they are missing.

## Debug subcommands

The per-family scanners are still available on their own for investigation. See
`cli/src/main.rs` for the list. Examples: `wpc-scan`, `wpc-probe`, `wpc-export`, `sam-probe`,
`sam-deff`, `cap-probe`, `cap-scan`, `de32-scan`, `de16-scan`, `de64-scan`, `gts3-scan`,
`alvg-scan` and `scene-export`.

## Building

```
cargo build --release      # the command line, target/release/rom2dmd
cargo test --release       # the board tests read $ROM2DMD_ROMS (default ~/roms-full)
```

## License

BSD-3-Clause.
