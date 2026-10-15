# Changelog

The format is [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning is [SemVer](https://semver.org/).

## [Unreleased]

## [1.0.2] - 2026-10-16

A patch release: no change to the public API (DECISIONS #41–#44).

### Fixed

- `query_iter/query_8_tables` no longer depends on the heap layout (DECISIONS #42): column
  vectors still double, but once a block reaches 4096 bytes its size is rounded up to half a page
  past a page multiple, so the `Velocity` column no longer lands just below `Position` modulo
  4096 (4K aliasing). Over 8 paddings × 3 processes: 456–661 µs before, 294–312 µs after, the
  1-table speed; the bench gate against `main` shows 428 → 270 µs, with spawns level or faster
  and every other benchmark within ± 3 %. At most 2 KiB more capacity per column vector; no
  `unsafe`, out-of-memory errors unchanged, no public API change.

### Changed

- `Query` and `View` share one iteration path (DECISIONS #41): one internal sequential iterator
  behind `QueryIter`, `QueryIterMut` and `ViewIter` (table switch, row mask, `fold`) over three
  table sources (shared world, exclusive world, lent columns), and one parallel chunk splitter
  for `par_for_each(_mut)`. The per-row code is the one of #37; no public API change.
- CI: the bench gate builds both revisions with `-C llvm-args=-align-loops=64` (DECISIONS #43),
  so that where LLVM places a hot loop no longer decides a measurement
  (`change/mut_write_all` moved between ≈ 83 and ≈ 110 µs for the same source on AMD Zen 4 and
  Intel runners).
- CI: the bench gate runs each measurement in its own process and, on Linux, with
  `MALLOC_ARENA_MAX=1` (DECISIONS #44), so that glibc allocator state no longer flips
  `commands/spawn_commands` between ≈ 1.5 and ≈ 2.0 ms per process.

### Documentation

- The 8-table spread of `query_iter/query_8_tables` (#37) is a heap-layout effect, not code
  generation: the same binary takes 317–771 µs depending on where the columns land (4K aliasing
  between the `Velocity` loads and the `Position` stores). A current-table iterator without
  `Option` was tried and is ≈ 30 % slower on 8 tables, so it was not kept (#41,
  `docs/architecture.md` §6).

## [1.0.1] - 2026-10-15

A patch release: no change to the public API (DECISIONS #40).

### Fixed

- `World::send_event` for the first event of a type no longer aborts the process when memory
  runs out: the event's buffer is reserved before the `Events<T>` resource is created, so the
  call returns `EcsError::OutOfMemory`, as documented, and leaves the world without an empty
  `Events<T>`.

### Added

- Tests: events against a naive two-buffer model (`tests/common/events.rs`): writer and reader
  systems, readers skipping frames, `World::send_event`, manual updates and outside cursors,
  on 1 and 4 workers, as a property test and the new fuzz target `events` (60 s per PR,
  10 minutes nightly); `send_event` without memory in `tests/oom.rs`.

### Changed

- CI: the MSRV job runs the workspace tests on Rust 1.88 instead of only `cargo check`.
- Docs: the semver note in `docs/architecture.md` §6 reflects the published 1.0.0.

## [1.0.0] - 2026-10-15

The first crates.io release (M6, DECISIONS #39): everything since 0.1.0, milestones M2–M6
(0.2.0–0.5.0 were never released separately).

### Added

- Bundles (M6, DECISIONS #35): the sealed trait `Bundle`, implemented for tuples of 0 to 12
  components; `World::spawn_bundle` (an entity created directly in its final table) and
  `World::insert_bundle` (one table move; components of the same types are replaced),
  `Commands::spawn_bundle` (returns `EntityCommands`), `Commands::insert_bundle` and
  `EntityCommands::insert_bundle`; the new error `EcsError::DuplicateComponent` for a bundle
  with a type twice. Spawning 10 000 entities with two components: about 1.0 ms instead of
  1.85 ms with `spawn` + `insert`s (2.46 ms in M5; `bevy_ecs` 0.96 ms, `hecs` 0.55 ms,
  `baseline/`).
- `pub use pragma_core`: the `pragma_core` the crate is built with (#36).
- Crate documentation (#36, #38): an overview, the semver guarantees (items hidden from the
  documentation and the hidden methods of sealed traits are not covered) and an "Integrating
  into an engine" section with a compiled engine skeleton; rustdoc examples on the main methods
  (`World::spawn` / `despawn`, `Commands`, `EntityCommands`, `ScheduleBuilder::add_system` /
  `build`).
- Tests: bundles in the world, command and change-tick oracles (shared with the fuzz targets),
  bundles without memory (`tests/oom.rs`). Benchmarks `commands/spawn_bundle_direct` and
  `spawn_bundle_commands` (gated), `spawn/pragma_ecs_inserts` in the baseline.

- Hardening (M5, DECISIONS #29–#33):
  - the determinism suite `ecs_sim --suite [--bless] [--golden FILE]`: seven scenarios
    (default, odd count, empty world, single entity, several tables, a lethal hazard heavy with
    commands and events, a calm arena) on 1, 2 and N workers, checked against committed golden
    hashes (`examples/ecs_sim/golden.txt`); in CI on Windows and Linux for every PR (200 ticks)
    and nightly (3000 ticks). `ecs_sim` gains `SimConfig::lethality` / `fragments`, the
    `Hazard` resource, `--seed`, `--lethality`, `--fragments`, ms/tick in its report, and the
    suite API (`determinism_suite`, `run_suite`, `history`, golden parsing and checking);
  - fuzzing with cargo-fuzz in a separate `fuzz/` workspace: `world_ops`, `commands` and
    `change_ticks` over the oracles of the property tests, now shared in
    `crates/pragma_ecs/tests/common/` (60 s per target in PRs, 10 minutes nightly);
  - baseline benchmarks against `hecs` 0.11 and `bevy_ecs` 0.20 in a separate `baseline/`
    workspace (spawn, iteration, fragmented iteration, add/remove, despawn);
  - CI: a bench gate (`tools/bench_gate.py`, an interleaved A/B of the PR base and head),
    `cargo semver-checks` against `main`, a `changes` job that skips the heavy jobs on
    documentation-only PRs, and a nightly workflow.

- Deferred commands (M4, DECISIONS #23–#25): the system parameter `Commands` (`spawn`,
  `entity`, `despawn`, `insert`, `remove`, `len`, `is_empty`) and `EntityCommands` (`id`,
  `insert`, `remove`, `despawn`, chaining). Commands are kept per system, declare no access, and
  are applied at the sync point at the end of `Schedule::run`, system after system in
  registration order, so the world and its entity ids are the same on any number of workers;
  a new entity gets its id only then. A command on a dead entity is a no-op; any other failure
  while applying is the new `EcsError::CommandFailed { system, source }` (the rest is
  discarded); the commands of a failed or panicked system are discarded.
- Events (#26): `Event` (blanket over `Send + Sync + 'static`), the double-buffered resource
  `Events<T>` (`new`, `send`, `update`, `len`, `is_empty`, `iter`), `EventCursor` (`read`,
  `unread`), `EventIter`, the system parameters `EventWriter<T>` (published at the sync point, no
  access) and `EventReader<T>` (a read of `Events<T>`, a cursor per system), and
  `World::send_event`. A schedule creates and updates `Events<T>` for every event type its
  systems use.
- Change detection (#27, #28): `u64` added / changed ticks per component, stored next to the
  values; `World::change_tick`; the wrapper `Mut<T>` (`Deref`, `DerefMut`, `AsRef`, `AsMut`,
  `into_inner`) that marks a component changed when written; the row filters `Added<T>` and
  `Changed<T>`, also with `&mut T` of the same type in one query. A system's ticks follow the
  schedule's topological order, so change detection is deterministic too.
- `examples/ecs_sim` now despawns and respawns dying entities through commands and a `Death`
  event, counts critical health changes (`Changed<Health>`) and births (`Added<Age>`); its
  report has `births` and `alive`.
- Tests: commands against direct world operations on 1 and 4 workers (`tests/commands.rs`),
  events (`tests/events.rs`), change filters against a per-entity tick oracle and through
  schedules in both orders (`tests/change.rs`), command application without memory
  (`tests/oom.rs`). Benchmarks `benches/commands.rs` and `benches/change.rs`.

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

- The workspace version is 1.0.0 and `pragma_ecs` is publishable (`publish = false` removed),
  with its own README and LICENSE in the package and `documentation` pointing to docs.rs
  (DECISIONS #39). 1.0.0 only adds to the API of 0.5.0 (`cargo semver-checks` as a minor
  release passes). `ecs_sim`, `fuzz/` and `baseline/` stay unpublished.
- Faster table lookups: the maps keyed by `TypeId`s use a multiplicative hasher instead of
  SipHash (add/remove of a component about 14 % faster, #35). The per-row path of
  `QueryIter`, `QueryIterMut` and `ViewIter` is inlined (#37).
- `examples/ecs_sim` spawns through bundles; its golden hashes are unchanged.
- The workspace version was 0.5.0 in M5 (DECISIONS #33; never released, the last tag is v0.1.0).
- **Breaking:** `&mut T` in a `Query` or a `View` yields `Mut<'_, T>` instead of `&mut T`
  (DECISIONS #28); field access and methods work through auto-deref, a function taking `&mut T`
  needs `&mut *item`. `World::get_mut` and replacing a component with `World::insert` mark it
  changed.
- `QueryIter`, `QueryIterMut` and `ViewIter` take the query's filter as a type parameter
  (defaulting to `()`); `QueryFilter` has row filters; `SystemParam` covers `Commands`,
  `EventWriter` and `EventReader`.
- `Schedule::run` ends with the sync point (event update, deferred commands and events).
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
