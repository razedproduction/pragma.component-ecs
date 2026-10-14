//! The CLI of the `pragma.component-ecs` vertical slice.
//!
//! ```text
//! ecs_sim [--entities N] [--ticks N] [--workers N] [--seed N] [--lethality PERCENT]
//!         [--fragments 1..4] [--verify W1,W2,...] [--suite [--bless] [--golden FILE]]
//! ```
//!
//! `--verify` runs the scenario on each worker count and compares the state hashes of all
//! ticks; a mismatch means exit code 1.
//!
//! `--suite` is the mandatory determinism suite ([`ecs_sim::determinism_suite`], DECISIONS
//! #29): every scenario on 1, 2 and N workers (or the `--verify` list) with `--ticks` ticks;
//! a scenario fails if the worker counts disagree or its history hash differs from (or is
//! missing in) the golden file (`examples/ecs_sim/golden.txt` unless `--golden` says
//! otherwise). `--bless` writes the hashes of this run into the golden file instead of
//! comparing them.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use ecs_sim::{
    GoldenCheck, SimConfig, SimError, Verdict, bless, check_golden, compare, default_workers,
    determinism_suite, format_golden, parse_golden, run, run_suite,
};

/// The golden file next to this crate's manifest.
const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/golden.txt");

const USAGE: &str = "usage: ecs_sim [--entities N] [--ticks N] [--workers N] [--seed N] \
                     [--lethality PERCENT] [--fragments 1..4] [--verify W1,W2,...] \
                     [--suite [--bless] [--golden FILE]]";

struct Args {
    config: SimConfig,
    /// `--ticks` was given (the suite's default is 200).
    ticks_given: bool,
    verify: Vec<usize>,
    suite: bool,
    bless: bool,
    golden: PathBuf,
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
        ticks_given: false,
        verify: Vec::new(),
        suite: false,
        bless: false,
        golden: PathBuf::from(GOLDEN),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--entities" => args.config.entities = parse_number(&flag, it.next())?,
            "--ticks" => {
                args.config.ticks = parse_number(&flag, it.next())?;
                args.ticks_given = true;
            }
            "--workers" => args.config.workers = parse_number(&flag, it.next())?,
            "--seed" => args.config.seed = parse_number(&flag, it.next())?,
            "--lethality" => args.config.lethality = parse_number(&flag, it.next())?,
            "--fragments" => args.config.fragments = parse_number(&flag, it.next())?,
            "--verify" => {
                let list = it.next().ok_or("--verify: missing value")?;
                args.verify = list
                    .split(',')
                    .map(|w| parse_number(&flag, Some(w.to_owned())))
                    .collect::<Result<_, _>>()?;
            }
            "--suite" => args.suite = true,
            "--bless" => args.bless = true,
            "--golden" => {
                args.golden = PathBuf::from(it.next().ok_or("--golden: missing value")?);
            }
            "--help" | "-h" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    Ok(args)
}

/// The suite's default worker counts: 1, 2 and N without oversubscription.
fn suite_workers() -> Vec<usize> {
    let n = default_workers();
    let mut workers = vec![1, 2.min(n), n];
    workers.dedup();
    workers
}

/// Runs the determinism suite; `Ok(false)` if a scenario failed.
fn suite(args: &Args) -> Result<bool, SimError> {
    let ticks = if args.ticks_given {
        args.config.ticks
    } else {
        200
    };
    let workers = if args.verify.is_empty() {
        suite_workers()
    } else {
        args.verify.clone()
    };
    let scenarios = determinism_suite(ticks, usize::MAX);
    let outcomes = run_suite(&scenarios, &workers)?;
    let text = std::fs::read_to_string(&args.golden).unwrap_or_default();
    let golden = parse_golden(&text)?;
    let mut ok = true;
    for outcome in &outcomes {
        let golden_check = check_golden(&golden, outcome);
        let times: Vec<String> = outcome
            .elapsed
            .iter()
            .map(|t| format!("{:.1} ms", t.as_secs_f64() * 1e3))
            .collect();
        println!(
            "[{}] {} entities, {} ticks, {} died, history {:016x}, {} on {workers:?} workers",
            outcome.name,
            outcome.entities,
            outcome.ticks,
            outcome.dead,
            outcome.history,
            times.join(" / "),
        );
        if let Verdict::Differs { workers, tick } = outcome.verdict {
            eprintln!(
                "MISMATCH: [{}] {workers} workers differ from the first run at tick {tick}",
                outcome.name
            );
            ok = false;
        }
        if args.bless {
            continue;
        }
        match golden_check {
            GoldenCheck::Matches => {}
            GoldenCheck::Missing => {
                eprintln!(
                    "MISSING: [{}] has no golden hash for {} entities and {} ticks in {} \
                     (run with --bless)",
                    outcome.name,
                    outcome.entities,
                    outcome.ticks,
                    args.golden.display()
                );
                ok = false;
            }
            GoldenCheck::Differs { expected } => {
                eprintln!(
                    "GOLDEN: [{}] history {:016x}, the golden file expects {expected:016x}",
                    outcome.name, outcome.history
                );
                ok = false;
            }
        }
    }
    if args.bless && ok {
        let blessed = format_golden(&bless(&golden, &outcomes));
        if let Err(error) = std::fs::write(&args.golden, blessed) {
            eprintln!("error: writing {}: {error}", args.golden.display());
            return Ok(false);
        }
        println!("blessed {}", args.golden.display());
    }
    println!(
        "suite: {} scenarios, {ticks} ticks, workers {workers:?}: {}",
        outcomes.len(),
        if ok { "ok" } else { "FAILED" }
    );
    Ok(ok)
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    if args.suite {
        return match suite(&args) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::from(1)
            }
        };
    }
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
            // `tools/bench_gate.py` reads the elapsed time from this line.
            let ticks = u32::try_from(args.config.ticks).unwrap_or(u32::MAX);
            println!(
                "{} entities, {} ticks on {} workers (default {}): {:?}, {:.3} ms/tick, {} died, {} born, {} alive, final hash {last:016x}",
                args.config.entities,
                args.config.ticks,
                args.config.workers,
                default_workers(),
                report.elapsed,
                report.elapsed.as_secs_f64() * 1e3 / f64::from(ticks),
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
