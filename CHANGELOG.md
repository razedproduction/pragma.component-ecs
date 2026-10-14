# Changelog

The format is [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning is [SemVer](https://semver.org/).

## [Unreleased]

### Added

- Systems and schedules (M3): systems are plain functions of up to 8 parameters — `View<Q, F>`
  (a query over the frame's world, with `iter`, `iter_mut`, `par_for_each`, `par_for_each_mut`
  and the iterator `ViewIter`), `Res<T>`, `ResMut<T>` (sealed `SystemParam`) — returning `()`
  or `Result<(), E>` for any error type (`SystemOutput`, `IntoSystem`). Their `Access` is the
  union of their parameters' (DECISIONS #17). `Schedule::builder()` → `ScheduleBuilder`
  (`add_system`, `add_system_with` with `SystemConfig::after` / `before`, `ordered`, `build`) →
  `Schedule` (`run`, `systems`, `order`, `edges`, `len`, `is_empty`): every system is a task of
  the `pragma_core` frame graph, systems with disjoint access run in parallel, and the columns of
  the component types systems use are lent to the frame graph without `unsafe` (#18).
- Conflicting systems without an explicit order fail `ScheduleBuilder::build` with the new
  `EcsError::AmbiguousOrder` (from pragma.core `GraphError::AmbiguousOrder`, #19).
- Resources (#20): the `Resource` trait (blanket over `Send + Sync + 'static`) and
  `World::insert_resource`, `resource`, `resource_mut`, `remove_resource`,
  `contains_resource`.
- New errors (#21): `EcsError::ConflictingResourceAccess`, `MissingResource`, `SystemFailed`,
  `SystemPanicked` and `Schedule` (other frame-graph errors, with pragma.core's `GraphError` as
  the source).
- `Access` covers resources too: `resource_kind_of`, `resources`; `conflict` compares both.
- The vertical slice `examples/ecs_sim` (a workspace crate, `publish = false`, #22): N entities,
  six systems, a per-tick state hash; its tests and `--verify` check the hashes on 1, 2 and N
  workers.
- Tests: random system sets build iff no unordered conflict and a frame equals a sequential run
  on 1 and 4 workers (`tests/schedule.rs`); a frame without memory for new tables lends nothing
  (`tests/oom.rs`). Benchmark `benches/schedule.rs`.
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
  count, or 0 when building a query's access or a schedule).
- `EcsError::Job` now also covers parallel views inside systems.
- The workspace includes `examples/*`.

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
