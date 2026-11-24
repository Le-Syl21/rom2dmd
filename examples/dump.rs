//! Extract a ROM set and write its PinMAME DMD dump: `cargo run --release --example dump --
//! t2_l8.zip t2_l8.txt`.

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: dump <rom.zip> <out.txt>".into());
    }
    let x = rom2dmd::extract(std::path::Path::new(&args[1]), &rom2dmd::Options::default())?;
    let shown = x.scenes.iter().filter(|s| !s.frames.is_empty()).count();
    println!(
        "{} ({}, {}x{}): {shown} scenes with pictures out of {}",
        x.set,
        x.family,
        x.width,
        x.height,
        x.scenes.len()
    );
    let f = std::fs::File::create(&args[2]).map_err(|e| format!("{}: {e}", args[2]))?;
    x.write_dump(std::io::BufWriter::new(f))
        .map_err(|e| format!("{}: {e}", args[2]))
}
