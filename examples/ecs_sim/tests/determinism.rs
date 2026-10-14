//! The hash of every tick does not depend on the number of workers (DECISIONS #22).
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ecs_sim::{SimConfig, Verdict, compare, default_workers, run};

fn small() -> SimConfig {
    SimConfig {
        entities: 5_000,
        ticks: 150,
        ..SimConfig::default()
    }
}

#[test]
fn hashes_match_on_1_2_and_n_workers() {
    let n = default_workers().max(4);
    assert_eq!(compare(&small(), &[1, 2, n]).unwrap(), Verdict::Same);
}

#[test]
fn the_simulation_changes_state_and_moves_entities_between_tables() {
    let report = run(&small()).unwrap();
    assert_eq!(report.hashes.len(), 150);
    // Every tick changes the state.
    assert!(report.hashes.windows(2).all(|w| w[0] != w[1]));
    // Some entities died inside the hazard and moved to the `Dead` table, some did not.
    assert!(report.dead > 0 && report.dead < 5_000, "{}", report.dead);
    // Another seed gives another history.
    let other = run(&SimConfig { seed: 7, ..small() }).unwrap();
    assert_ne!(other.hashes, report.hashes);
}

#[test]
fn invalid_configs_are_errors() {
    assert!(
        run(&SimConfig {
            ticks: 0,
            ..small()
        })
        .is_err()
    );
    assert!(
        run(&SimConfig {
            workers: 0,
            ..small()
        })
        .is_err()
    );
}
