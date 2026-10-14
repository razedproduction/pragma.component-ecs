//! Fuzzing deferred commands: arbitrary bytes → an initial world and the command lists of
//! several systems, run through a schedule on 1 and 4 workers and compared with the same
//! operations applied directly, as in `tests/commands.rs` (DECISIONS #30).
#![no_main]

use libfuzzer_sys::fuzz_target;

// The same oracle as in the `pragma_ecs` property tests.
#[path = "../../crates/pragma_ecs/tests/common/mod.rs"]
mod common;

fuzz_target!(|data: &[u8]| common::commands::check(&common::commands::decode(data)));
