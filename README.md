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

**M4 — commands, events, change detection.** A `World` of generational `Entity` ids,
components in archetype tables and resources; typed queries with declared access, sequential or
parallel on the pragma.core thread pool; and systems — plain functions of `View`, `Res`,
`ResMut`, `Commands`, `EventWriter` and `EventReader` parameters — run by a `Schedule` as tasks
of the pragma.core frame graph. Systems with disjoint access run in parallel; conflicting
systems without an explicit order are rejected when the schedule is built. Structural changes
from systems are deferred commands and events are published at the end of the frame, both in
a fixed order; `Added<T>` / `Changed<T>` filters see what changed since a query or system last
ran. [`examples/ecs_sim`](examples/ecs_sim) runs a simulation through all of it and checks that
the state hash of every tick is the same on 1, 2 and N workers. Hardening (fuzzing, baseline
benchmarks, semver checks) comes next. See the [roadmap](docs/roadmap.md) and the
[architecture](docs/architecture.md).

```rust
use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
use pragma_ecs::{Changed, Commands, EcsError, Entity, Res, ResMut, Schedule, View, Without, World};

struct Position(f32, f32);
struct Velocity(f32, f32);
struct Frozen;
struct Dt(f32);
struct Energy(f32);

// The access of a system is its parameters: writes `Position`, reads `Velocity` and `Dt`.
fn movement(
    mut bodies: View<(&mut Position, &Velocity), Without<Frozen>>,
    dt: Res<Dt>,
) -> Result<(), EcsError> {
    // `&mut Position` yields a `Mut<Position>`: writing it marks the position as changed.
    bodies.par_for_each_mut(Grain::Auto, |(mut position, velocity)| {
        position.0 += velocity.0 * dt.0;
        position.1 += velocity.1 * dt.0;
    })
}

// Reads `Velocity` only: runs in parallel with `movement`.
fn energy(mut velocities: View<&Velocity>, mut energy: ResMut<Energy>) -> Result<(), EcsError> {
    energy.0 = velocities.iter()?.map(|v| (v.0 * v.0 + v.1 * v.1) / 2.0).sum();
    Ok(())
}

fn render(_: View<&Position>) {}

// Positions that changed since this system last ran; entities that left the arena are
// despawned at the end of the frame, in a fixed order on any number of workers.
fn bounds(
    mut moved: View<(Entity, &Position), Changed<Position>>,
    mut commands: Commands,
) -> Result<(), EcsError> {
    for (entity, position) in moved.iter()? {
        if position.0.abs() > 1000.0 {
            commands.despawn(entity)?;
        }
    }
    Ok(())
}

let mut world = World::new();
let entity = world.spawn()?;
world.insert(entity, Position(0.0, 0.0))?;
world.insert(entity, Velocity(1.0, 0.5))?;
world.insert_resource(Dt(1.0 / 60.0));
world.insert_resource(Energy(0.0));

let mut builder = Schedule::builder();
builder.add_system("movement", movement)?;
builder.add_system("energy", energy)?;
// Without `after("movement")` the build fails with `EcsError::AmbiguousOrder`.
builder.add_system_with("render", render, |c| c.after("movement"))?;
builder.add_system_with("bounds", bounds, |c| c.after("movement"))?;
let mut schedule = builder.build()?;

let pool = ThreadPool::new(PoolConfig::default())?;
schedule.run(&mut world, &pool)?; // once per frame
```

## Building

```bash
cargo test --workspace --all-features
```

The vertical slice, with its determinism check on several worker counts:

```bash
cargo run --release -p ecs_sim -- --verify 1,2,4
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
