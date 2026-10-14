//! Fuzzing world operations and queries: arbitrary bytes → a sequence of spawns, despawns,
//! inserts, removes, `get_mut` and sequential / parallel mutating queries, checked after every
//! operation against the `HashMap` oracle of `tests/oracle.rs` (DECISIONS #30).
#![no_main]

use libfuzzer_sys::fuzz_target;

// The same oracle as in the `pragma_ecs` property tests.
#[path = "../../crates/pragma_ecs/tests/common/mod.rs"]
mod common;

fuzz_target!(|data: &[u8]| common::world::check(common::world::decode(data)));
