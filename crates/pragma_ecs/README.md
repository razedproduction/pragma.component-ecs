# pragma_ecs

[![crates.io](https://img.shields.io/crates/v/pragma_ecs.svg)](https://crates.io/crates/pragma_ecs)
[![docs.rs](https://img.shields.io/docsrs/pragma_ecs)](https://docs.rs/pragma_ecs)
![MSRV](https://img.shields.io/badge/MSRV-1.88-orange)

A pure-Rust Entity Component System for game engines and frameworks built on
[pragma.core](https://github.com/razedproduction/pragma.core) (`pragma_core` 1.x).

It owns no threads: systems — plain functions of `View`, `Res`, `ResMut`, `Commands`,
`EventWriter` and `EventReader` parameters — run as tasks of the pragma.core frame graph on the
engine's thread pool. Systems with disjoint access run in parallel; conflicting systems without
an explicit order are rejected when the `Schedule` is built, not discovered as a data race.
Components live in archetype tables; `spawn_bundle` puts an entity straight into its final
table. Structural changes from systems and events are applied at the end of the frame in a
fixed order, so a frame gives the same world on any number of workers. `Added<T>` /
`Changed<T>` filters see what changed since a query or system last ran.

```rust
use pragma_ecs::pragma_core::prelude::{PoolConfig, ThreadPool};
use pragma_ecs::{EcsError, Res, Schedule, View, World};

struct Position(f32, f32);
struct Velocity(f32, f32);
struct Dt(f32);

fn movement(mut bodies: View<(&mut Position, &Velocity)>, dt: Res<Dt>) -> Result<(), EcsError> {
    for (mut position, velocity) in bodies.iter_mut()? {
        position.0 += velocity.0 * dt.0;
        position.1 += velocity.1 * dt.0;
    }
    Ok(())
}

let mut world = World::new();
world.spawn_bundle((Position(0.0, 0.0), Velocity(1.0, 0.5)))?;
world.insert_resource(Dt(1.0 / 60.0));

let mut builder = Schedule::builder();
builder.add_system("movement", movement)?;
let mut schedule = builder.build()?;

let pool = ThreadPool::new(PoolConfig::default())?;
schedule.run(&mut world, &pool)?; // once per frame
```

No `unsafe`, no panics in the library (typed `#[non_exhaustive]` errors), bounded allocations
(`try_reserve` → `EcsError::OutOfMemory`), pure-Rust dependencies. The crate documentation has
an "Integrating into an engine" section; the design, the decision log and the benchmarks against
`hecs` and `bevy_ecs` are in the
[repository](https://github.com/razedproduction/pragma.component-ecs).

License: MIT.
