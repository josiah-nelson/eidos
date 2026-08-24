use eidos_sync::soak::{run_seed, SoakFailure};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let seeds: u64 = args
        .next()
        .as_deref()
        .unwrap_or("1000000")
        .replace('_', "")
        .parse()?;
    let output = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "sync-soak-failures.jsonl".into()),
    );
    let max_failures: usize = args.next().as_deref().unwrap_or("20").parse()?;
    let mut failures = Vec::<SoakFailure>::new();
    for seed in 0..seeds {
        if let Err(failure) = run_seed(seed) {
            eprintln!(
                "::error title=DST failing seed {}::{}",
                failure.seed, failure.reproducer
            );
            failures.push(failure);
            if failures.len() >= max_failures {
                break;
            }
        }
        if seed > 0 && seed % 100_000 == 0 {
            eprintln!("DST progress: {seed}/{seeds} seeds");
        }
    }

    let file = File::create(&output)?;
    let mut writer = BufWriter::new(file);
    for failure in &failures {
        serde_json::to_writer(&mut writer, failure)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    if failures.is_empty() {
        println!("DST soak passed {seeds} seeds");
        Ok(())
    } else {
        Err(format!("DST soak found {} failing seeds", failures.len()).into())
    }
}
