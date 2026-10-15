//! Fuzzing events: arbitrary bytes → writer and reader systems and the sends, updates and reads
//! between frames, checked against the two-buffer model on 1 and 4 workers, as in
//! `tests/events.rs` (DECISIONS #30, #40).
#![no_main]

use libfuzzer_sys::fuzz_target;

// The same oracle as in the `pragma_ecs` property tests.
#[path = "../../crates/pragma_ecs/tests/common/mod.rs"]
mod common;

fuzz_target!(|data: &[u8]| common::events::check(&common::events::decode(data)));
