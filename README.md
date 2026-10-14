# pragma.component-ecs

[![CI](https://github.com/razedproduction/pragma.component-ecs/actions/workflows/ci.yml/badge.svg)](https://github.com/razedproduction/pragma.component-ecs/actions/workflows/ci.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.88-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

**pragma.component-ecs** is a pure-Rust Entity Component System for game engines and frameworks
built on [pragma.core](https://github.com/razedproduction/pragma.core).

It sits *on top of* the core: it owns no threads of its own. Systems run as tasks of the
pragma.core frame graph on its work-stealing pool, so conflicting data access between systems is
caught when the schedule is built, not as a data race at run time.

## Status

**M1 — entities and component storage.** A `World` of generational `Entity` ids and components in
archetype tables (`spawn`, `despawn`, `insert`, `remove`, `get`, `get_mut`); queries and systems
come next. See the [roadmap](docs/roadmap.md) and the [architecture](docs/architecture.md).

```rust
use pragma_ecs::World;

struct Position(f32, f32);

let mut world = World::new();
let entity = world.spawn()?;
world.insert(entity, Position(0.0, 0.0))?;
if let Some(position) = world.get_mut::<Position>(entity) {
    position.0 += 1.0;
}
world.despawn(entity)?;
```

## Building

```bash
cargo test --workspace --all-features
```

The toolchain is pinned in `rust-toolchain.toml` (1.99.0); the minimum supported Rust version is
1.88, the same as pragma.core. See [CONTRIBUTING.md](CONTRIBUTING.md) for the full check list.

## Reliability

The same rules as pragma.core, checked in CI on every PR:

- **Pure Rust.** No C/C++ compiled into the dependency tree (`cargo deny`, `tools/audit_workspace.py`).
- **No panics in the library.** `unwrap`/`expect`/`panic!`/unchecked indexing are denied by lints;
  failures are typed `#[non_exhaustive]` errors.
- **No `unsafe`** in the library, storage included ([DECISIONS #6, #9](DECISIONS.md)); the only
  `unsafe` is a failing test allocator, counted in the `layers.toml` budget (#11).
- **Bounded allocations.** Storage grows through `try_reserve`; out of memory is
  `EcsError::OutOfMemory` with the world unchanged, not an abort.
- **Layered.** Crates depend only on lower layers (`layers.toml`).

## Platforms

Windows and Linux (x86_64), tested in CI.

## Documentation

- [Architecture](docs/architecture.md)
- [Roadmap](docs/roadmap.md)
- [Decision log](DECISIONS.md)
- [Changelog](CHANGELOG.md)
- [Contributing](CONTRIBUTING.md)

## License

[MIT](LICENSE) © 2026 razed
