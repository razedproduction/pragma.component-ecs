# pragma.component-ecs roadmap

The principle, as in pragma.core: **a vertical slice before breadth**. A small simulation example
should run through world → queries → systems on the frame graph as early as possible, and every
later milestone is verified on it.

Legend: ✅ done · 🚧 in progress · 📋 planned

| Milestone | Version | Essence | Status |
|---|---|---|---|
| M0 | 0.0.1 | Repository foundation | ✅ |
| M1 | 0.1.0 | Entities and component storage | 📋 |
| M2 | 0.2.0 | Queries | 📋 |
| M3 | 0.3.0 | Systems on the pragma.core frame graph, vertical slice | 📋 |
| M4 | 0.4.0 | Commands, events, change detection | 📋 |
| M5 | 0.5.0 | Hardening: determinism, fuzz, benches, semver | 📋 |
| M6 | 1.0.0 | Stable API, publication | 📋 |

---

## M0 — Foundation ✅

- [x] Workspace, edition 2024, pinned toolchain, MSRV 1.88, strict lints (no panics)
- [x] `deny.toml`, `layers.toml`, `tools/audit_workspace.py`
- [x] CI: Windows + Linux, fmt, clippy, test, doc, MSRV, deny, audit
- [x] `pragma_ecs` skeleton with `EcsError`, depending on `pragma_core` 1.x
- [x] Docs: README, architecture, roadmap, DECISIONS, CONTRIBUTING, CHANGELOG

## M1 — Entities and storage (0.1.0)

- [ ] **Decision:** storage model (archetype tables / sparse sets / hybrid) — see `docs/architecture.md`
- [ ] **Decision:** whether `unsafe` is needed for type-erased columns, and in which crate (DECISIONS #6)
- [ ] `Entity`: generational id (index + generation), stale ids are detected
- [ ] `World`: `spawn`, `despawn`, `insert`, `remove`, `get`, `get_mut`
- [ ] Bounded allocations: growth via `try_reserve` → `EcsError::OutOfMemory`
- [ ] Property tests against a naive `HashMap<Entity, components>` oracle

## M2 — Queries (0.2.0)

- [ ] Typed queries over `&T` / `&mut T` / `Option<&T>`, filters `With` / `Without`
- [ ] Declared access (read/write per component type), aliasing rejected at construction
- [ ] Parallel iteration via `pragma_core` `parallel_for`
- [ ] Iteration benchmark (`criterion`)

## M3 — Systems and the vertical slice (0.3.0)

- [ ] Systems as frame-graph tasks; access declared from their queries and resources
- [ ] Conflicting systems without an explicit order fail at build time (pragma.core `GraphError::AmbiguousOrder`)
- [ ] Resources (singletons)
- [ ] Example: N entities, several systems, a per-tick state hash identical on 1 and N workers

## M4 — Commands, events, change detection (0.4.0)

- [ ] Deferred commands (spawn/despawn/insert from systems), applied at a sync point
- [ ] Events (double-buffered)
- [ ] Added / changed tracking per component

## M5 — Hardening (0.5.0)

- [ ] Determinism suite in CI
- [ ] Fuzzing of world operations against the oracle
- [ ] Benchmarks against `hecs` / `bevy_ecs` as a baseline (dev only), bench gate
- [ ] `cargo-semver-checks` in CI

## M6 — 1.0 (1.0.0)

- [ ] API review, rustdoc with examples, an "Integrating into an engine" section
- [ ] Publication on crates.io

---

## Out of scope

Rendering, windowing, input, audio, asset loading, scene serialization formats, editor, App/plugin
framework, the game loop itself (DECISIONS #1). These belong to engines built on top.
