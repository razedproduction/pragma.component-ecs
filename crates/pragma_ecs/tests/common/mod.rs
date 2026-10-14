//! Oracles shared by the property tests and the fuzz targets (DECISIONS #30).
//!
//! Each module defines the operations of one property, a harness that applies them to the real
//! [`pragma_ecs::World`] and to a naive model and panics on the first disagreement, and a decoder
//! that turns arbitrary bytes (a fuzzer's input) into the same operations the property tests
//! generate. The test files and `fuzz/fuzz_targets/*.rs` include this module through `mod` /
//! `#[path]`; not every includer uses every module.
// Test code: a panic is a test failure; not every includer uses every item.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::unreachable
)]

pub mod bytes;
pub mod change;
pub mod commands;
pub mod world;
