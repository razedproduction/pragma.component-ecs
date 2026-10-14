# pragma.component-ecs roadmap

The principle, as in pragma.core: **a vertical slice before breadth**. A small simulation example
should run through world → queries → systems on the frame graph as early as possible, and every
later milestone is verified on it.

Legend: ✅ done · 🚧 in progress · 📋 planned

| Milestone | Version | Essence | Status |
|---|---|---|---|
| M0 | 0.0.1 | Repository foundation | ✅ |
| M1 | 0.1.0 | Entities and component storage | ✅ |
| M2 | 0.2.0 | Queries | 🚧 |
| M3 | 0.3.0 | Systems on the pragma.core frame graph, vertical slice | 🚧 |
| M4 | 0.4.0 | Commands, events, change detection | 🚧 |
| M5 | 0.5.0 | Hardening: determinism, fuzz, benches, semver | 🚧 |
| M6 | 1.0.0 | Stable API, publication | 📋 |

---

## M0 — Foundation ✅

- [x] Workspace, edition 2024, pinned toolchain, MSRV 1.88, strict lints (no panics)
- [x] `deny.toml`, `layers.toml`, `tools/audit_workspace.py`
- [x] CI: Windows + Linux, fmt, clippy, test, doc, MSRV, deny, audit
- [x] `pragma_ecs` skeleton with `EcsError`, depending on `pragma_core` 1.x
- [x] Docs: README, architecture, roadmap, DECISIONS, CONTRIBUTING, CHANGELOG

## M1 — Entities and storage (0.1.0) ✅


- [x] **Decision:** storage model — archetype tables (DECISIONS #8, `docs/architecture.md` §3)
- [x] **Decision:** no `unsafe`: columns are `Box<dyn Column>` over `Vec<T>` (DECISIONS #9); own entity allocator (#10)
- [x] `Entity`: generational id (index + generation), stale ids are detected
- [x] `World`: `spawn`, `despawn`, `insert`, `remove`, `get`, `get_mut`
- [x] Bounded allocations: growth via `try_reserve` → `EcsError::OutOfMemory` (`tests/oom.rs`, DECISIONS #11)
- [x] Property tests against a naive `HashMap<Entity, components>` oracle (`tests/oracle.rs`)

## M2 — Queries (0.2.0) 🚧

- [x] **Decisions:** `Query<Q, F>` with an archetype cache (DECISIONS #12), column borrows without
  `unsafe` (#13), declared access (#14), chunks of rows for parallel iteration (#15), `criterion` (#16)
- [x] Typed queries over `&T` / `&mut T` / `Option<&T>` (and `Entity`), filters `With` / `Without`
- [x] Declared access (read/write per component type), aliasing rejected at construction
  (`EcsError::ConflictingAccess`)
- [x] Parallel iteration via `pragma_core` `parallel_for`, identical to sequential on 1 and N workers
  (`tests/parallel.rs`)
- [x] Iteration benchmark (`criterion`, `benches/query.rs`)
- [x] Property tests of queries against the oracle (`tests/oracle.rs`)

## M3 — Systems and the vertical slice (0.3.0) 🚧

- [x] **Decisions:** function systems with sealed parameters (DECISIONS #17), columns lent to the
  frame graph without `unsafe` (#18), strict ordering plus an `ordered()` mode (#19), resources in
  the world (#20), schedule lifetime and error mapping (#21), the `ecs_sim` example crate (#22)
- [x] Systems as frame-graph tasks (`Schedule`); access declared from their queries and resources
  (`View`, `Res`, `ResMut`)
- [x] Conflicting systems without an explicit order fail at build time (pragma.core
  `GraphError::AmbiguousOrder` → `EcsError::AmbiguousOrder`)
- [x] Resources (singletons): `World::insert_resource` and friends, `EcsError::MissingResource`
- [x] Example: N entities, several systems, a per-tick state hash identical on 1, 2 and N workers
  (`examples/ecs_sim`, its tests and `--verify`)
- [x] Property tests of schedules: random system sets build iff no unordered conflict, a frame
  equals a sequential run (`tests/schedule.rs`); frame benchmark (`benches/schedule.rs`)

## M4 — Commands, events, change detection (0.4.0) 🚧

- [x] **Decisions:** deferred commands with ids allocated at the sync point (DECISIONS #23), one
  sync point at the end of `Schedule::run` in registration order (#24), lenient command failures
  and `EcsError::CommandFailed` (#25), events published at the sync point (#26), `u64` change
  ticks next to the values (#27), `Mut<T>` and row filters through a mask, no `unsafe` (#28)
- [x] Deferred commands (spawn/despawn/insert/remove from systems: `Commands`,
  `EntityCommands`), applied at the sync point, identical on 1 and N workers (`tests/commands.rs`
  against direct world operations, `tests/oom.rs`)
- [x] Events (double-buffered): `Events<T>`, `EventWriter`, `EventReader`, `EventCursor`,
  `World::send_event` (`tests/events.rs`)
- [x] Added / changed tracking per component: `Mut<T>`, `Added<T>`, `Changed<T>`
  (`tests/change.rs` against a per-entity tick oracle)
- [x] `examples/ecs_sim`: deaths through commands and a `Death` event, respawning, systems
  reacting to `Changed<Health>` and `Added<Age>`; the per-tick hash identical on 1, 2 and N
  workers
- [x] Benchmarks: command application (`benches/commands.rs`), change-detection overhead
  (`benches/change.rs`)

## M5 — Hardening (0.5.0) 🚧

- [x] **Decisions:** a determinism suite with committed golden hashes (DECISIONS #29), cargo-fuzz
  in a separate workspace over the shared oracles (#30), a separate `baseline/` workspace with
  `hecs` and `bevy_ecs` (#31), an A/B bench gate (#32), semver-checks against `main` and the
  version 0.5.0 (#33), no change to storage or ticks after the benchmarks, bundles to M6 (#34)
- [x] Determinism suite in CI: `ecs_sim --suite`, seven scenarios on 1, 2 and N workers against
  `examples/ecs_sim/golden.txt`, Windows and Linux on every PR (200 ticks) and nightly (3000)
- [x] Fuzzing of world operations against the oracle: `fuzz/` targets `world_ops`, `commands`,
  `change_ticks` over the oracles in `crates/pragma_ecs/tests/common/`, shared with proptest
  (60 s per PR, 10 min nightly with a kept corpus)
- [x] Benchmarks against `hecs` / `bevy_ecs` as a baseline (dev only, `baseline/`), bench gate
  (`tools/bench_gate.py`, PRs on Windows and Linux)
- [x] `cargo-semver-checks` in CI (PRs, baseline `main`)

## M6 — 1.0 (1.0.0)

- [ ] API review, rustdoc with examples, an "Integrating into an engine" section
- [ ] Bundle spawns into the final table, for `World` and `Commands` (DECISIONS #34: spawning
  is the largest gap to `hecs` / `bevy_ecs` in the M5 baseline)
- [ ] Publication on crates.io

---

## Out of scope

Rendering, windowing, input, audio, asset loading, scene serialization formats, editor, App/plugin
framework, the game loop itself (DECISIONS #1). These belong to engines built on top.
