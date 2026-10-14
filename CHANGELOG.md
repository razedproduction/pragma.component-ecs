# Changelog

The format is [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning is [SemVer](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-10-14

### Added

- Entities and component storage (M1): `Entity` (a generational id with `to_bits`/`from_bits`;
  stale ids are detected), `World` with `spawn`, `despawn`, `insert`, `remove`, `get`, `get_mut`,
  `contains`, `len`, `is_empty`, and the `Component` trait (blanket over `Send + Sync + 'static`).
  Storage is archetype tables of `Vec<T>` columns without `unsafe` (DECISIONS #8–#10).
- `EcsError::NoSuchEntity` and `EcsError::CapacityExhausted`; every storage growth goes through
  `try_reserve` and fails with `EcsError::OutOfMemory`, leaving the world unchanged.
- Tests: a property test against a `HashMap` oracle (`tests/oracle.rs`) and a failing-allocator
  test (`tests/oom.rs`, 7 `unsafe` in the budget, DECISIONS #11).
- Repository foundation (M0): the `pragma_ecs` crate skeleton with `EcsError`, workspace lints,
  pinned toolchain, `deny.toml`, `layers.toml` with `tools/audit_workspace.py`, CI (Windows +
  Linux: fmt, clippy, test, doc, MSRV, cargo-deny, audit), documentation and the decision log.
