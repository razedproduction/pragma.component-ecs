//! Fuzzing change detection: arbitrary bytes → world operations interleaved with `Added` /
//! `Changed` queries, checked against the per-entity tick oracle of `tests/change.rs`
//! (DECISIONS #30).
#![no_main]

use libfuzzer_sys::fuzz_target;

// The same oracle as in the `pragma_ecs` property tests.
#[path = "../../crates/pragma_ecs/tests/common/mod.rs"]
mod common;

fuzz_target!(|data: &[u8]| common::change::check(common::change::decode(data)));
