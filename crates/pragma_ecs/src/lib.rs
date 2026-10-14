//! An Entity Component System for game engines and frameworks built on
//! [`pragma_core`](https://docs.rs/pragma_core).
//!
//! The crate is at milestone M0 (repository foundation): the public API does not exist yet.
//! See `docs/roadmap.md` for the plan and `docs/architecture.md` for the intended design.
#![forbid(unsafe_code)]

mod error;

pub use error::EcsError;
