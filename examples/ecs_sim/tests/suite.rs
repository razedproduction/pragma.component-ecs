//! The determinism suite (DECISIONS #29) in a debug build, capped in size: every scenario gives
//! the same hashes on 1, 2 and 3 workers. The full-size suite in release, checked against the
//! golden file, is `ecs_sim --suite` in CI.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ecs_sim::{
    GoldenCheck, GoldenEntry, Verdict, bless, check_golden, determinism_suite, format_golden,
    parse_golden, run_suite,
};

#[test]
fn every_scenario_matches_on_1_2_and_3_workers() {
    let scenarios = determinism_suite(12, 1_500);
    let outcomes = run_suite(&scenarios, &[1, 2, 3]).unwrap();
    assert_eq!(outcomes.len(), scenarios.len());
    for outcome in &outcomes {
        assert_eq!(outcome.verdict, Verdict::Same, "{}", outcome.name);
        assert_eq!(outcome.elapsed.len(), 3);
    }
    // The scenarios really differ.
    let mut histories: Vec<u64> = outcomes.iter().map(|o| o.history).collect();
    histories.sort_unstable();
    histories.dedup();
    assert_eq!(histories.len(), outcomes.len());
    // The lethal hazard kills more than the normal one, the calm arena kills nobody.
    let dead = |name: &str| outcomes.iter().find(|o| o.name == name).unwrap().dead;
    assert!(dead("carnage") > dead("fragmented"));
    assert_eq!(dead("calm"), 0);
}

#[test]
fn golden_entries_round_trip_and_are_checked() {
    let outcomes = run_suite(&determinism_suite(3, 50), &[1]).unwrap();
    assert_eq!(check_golden(&[], &outcomes[0]), GoldenCheck::Missing);

    let old = GoldenEntry {
        name: "default".to_owned(),
        entities: 7,
        ticks: 9,
        history: 0xabc,
    };
    let blessed = bless(std::slice::from_ref(&old), &outcomes);
    assert_eq!(blessed.len(), outcomes.len() + 1, "other keys are kept");
    let parsed = parse_golden(&format_golden(&blessed)).unwrap();
    assert_eq!(parsed.len(), blessed.len());
    for outcome in &outcomes {
        assert_eq!(check_golden(&parsed, outcome), GoldenCheck::Matches);
    }
    assert!(parsed.contains(&old));

    // Blessing again replaces an entry rather than duplicating it.
    assert_eq!(bless(&parsed, &outcomes).len(), parsed.len());

    let mut wrong = parsed.clone();
    for entry in &mut wrong {
        entry.history ^= 1;
    }
    assert!(matches!(
        check_golden(&wrong, &outcomes[0]),
        GoldenCheck::Differs { .. }
    ));
    assert!(parse_golden("default 1 2").is_err());
    assert!(parse_golden("default 1 2 zz").is_err());
    assert!(parse_golden("default 1 2 ff extra").is_err());
    assert!(parse_golden("# only a comment\n\n").unwrap().is_empty());
}

/// The committed golden file covers every scenario at the tick counts CI runs.
#[test]
fn the_golden_file_covers_the_ci_tick_counts() {
    let golden = parse_golden(include_str!("../golden.txt")).unwrap();
    for ticks in [200, 3_000] {
        for scenario in determinism_suite(ticks, usize::MAX) {
            assert!(
                golden.iter().any(|e| e.name == scenario.name
                    && e.entities == scenario.config.entities
                    && e.ticks == ticks),
                "{} at {ticks} ticks",
                scenario.name
            );
        }
    }
}
