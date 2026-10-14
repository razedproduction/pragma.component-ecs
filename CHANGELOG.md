# Changelog

The format is [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning is [SemVer](https://semver.org/).

## [Unreleased]

### Added

- Queries (M2): `Query<Q, F = ()>` with `new`, `access`, `iter`, `iter_mut`, `par_for_each`,
  `par_for_each_mut`, and the iterators `QueryIter` / `QueryIterMut`. Query data (`QueryData`,
  `ReadOnlyQueryData`, sealed): `&T`, `&mut T`, `Option<&T>`, `Entity` and tuples of up to 8;
  filters (`QueryFilter`, sealed): `With<T>`, `Without<T>` and tuples. A query caches its matching
  archetype tables and checks only new ones on each call (DECISIONS #12); columns are borrowed
  without `unsafe` (#13).
- Declared access: `Access` and `AccessKind`, with `Access::conflict` for comparing two
  accesses; a component written and also accessed elsewhere in a query is the new
  `EcsError::ConflictingAccess` at construction (#14).
- Parallel iteration on the `pragma_core` thread pool via `parallel_for`, in chunks of rows that
  do not depend on the worker count (#15); a task failure is the new `EcsError::Job`.
- Tests: queries against the oracle (`tests/oracle.rs`), parallel = sequential on 1 and 4 workers
  (`tests/parallel.rs`), queries without memory (`tests/oom.rs`).
- Benchmark `benches/query.rs` with `criterion` (dev dependency; its `alloca` is accepted in
  `[purity.accepted]`, DECISIONS #16).

### Changed

- `EcsError::OutOfMemory::entities` is documented for query allocations (the world's entity
  count, or 0 when building a query's access).

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
