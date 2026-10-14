//! The CLI of the `pragma.component-ecs` vertical slice.
//!
//! ```text
//! ecs_sim [--entities N] [--ticks N] [--workers N] [--verify W1,W2,...]
//! ```
//!
//! `--verify` runs the scenario on each worker count and compares the state hashes of all
//! ticks; a mismatch means exit code 1.
#![forbid(unsafe_code)]

use std::process::ExitCode;

use ecs_sim::{SimConfig, Verdict, compare, default_workers, run};

struct Args {
    config: SimConfig,
    verify: Vec<usize>,
}

fn parse_number<T: std::str::FromStr>(flag: &str, value: Option<String>) -> Result<T, String> {
    let value = value.ok_or_else(|| format!("{flag}: missing value"))?;
    value
        .parse()
        .map_err(|_| format!("{flag}: `{value}` is not a number"))
}

fn parse() -> Result<Args, String> {
    let mut args = Args {
        config: SimConfig::default(),
        verify: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--entities" => args.config.entities = parse_number(&flag, it.next())?,
            "--ticks" => args.config.ticks = parse_number(&flag, it.next())?,
            "--workers" => args.config.workers = parse_number(&flag, it.next())?,
            "--verify" => {
                let list = it.next().ok_or("--verify: missing value")?;
                args.verify = list
                    .split(',')
                    .map(|w| parse_number(&flag, Some(w.to_owned())))
                    .collect::<Result<_, _>>()?;
            }
            "--help" | "-h" => {
                return Err(
                    "usage: ecs_sim [--entities N] [--ticks N] [--workers N] [--verify W1,W2,...]"
                        .to_owned(),
                );
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    if !args.verify.is_empty() {
        return match compare(&args.config, &args.verify) {
            Ok(Verdict::Same) => {
                println!(
                    "deterministic: {} ticks of {} entities give the same hashes on {:?} workers",
                    args.config.ticks, args.config.entities, args.verify
                );
                ExitCode::SUCCESS
            }
            Ok(Verdict::Differs { workers, tick }) => {
                eprintln!("MISMATCH: {workers} workers differ from the first run at tick {tick}");
                ExitCode::from(1)
            }
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::from(1)
            }
        };
    }
    match run(&args.config) {
        Ok(report) => {
            let last = report.hashes.last().copied().unwrap_or_default();
            println!(
                "{} entities, {} ticks on {} workers (default {}): {:?}, {} died, {} born, {} alive, final hash {last:016x}",
                args.config.entities,
                args.config.ticks,
                args.config.workers,
                default_workers(),
                report.elapsed,
                report.dead,
                report.births,
                report.alive,
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}
