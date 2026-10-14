//! Command benchmarks (DECISIONS #23, #24): spawning 10 000 entities with two components and
//! despawning them through `Commands` at the sync point, against the same operations applied
//! directly to the world (the reference).
//!
//! `cargo bench -p pragma_ecs --bench commands` (report: `target/criterion/report/index.html`).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss
)]

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pragma_core::prelude::{PoolConfig, ThreadPool};
use pragma_ecs::{Commands, EcsError, Entity, Schedule, View, World};

const N: usize = 10_000;

/// Payloads: stored and dropped, never read.
#[derive(Clone, Copy)]
struct Position(#[allow(dead_code)] [f32; 3]);

#[derive(Clone, Copy)]
struct Velocity(#[allow(dead_code)] [f32; 3]);

fn spawn_all(mut commands: Commands<'_>) -> Result<(), EcsError> {
    for i in 0..N {
        commands
            .spawn()?
            .insert(Position([i as f32; 3]))?
            .insert(Velocity([1.0, 2.0, 3.0]))?;
    }
    Ok(())
}

fn despawn_all(mut entities: View<Entity>, mut commands: Commands<'_>) -> Result<(), EcsError> {
    for entity in entities.iter()? {
        commands.despawn(entity)?;
    }
    Ok(())
}

fn populated() -> World {
    let mut world = World::new();
    for i in 0..N {
        let e = world.spawn().unwrap();
        world.insert(e, Position([i as f32; 3])).unwrap();
        world.insert(e, Velocity([1.0, 2.0, 3.0])).unwrap();
    }
    world
}

fn schedule<M>(system: impl pragma_ecs::IntoSystem<M>) -> Schedule {
    let mut builder = Schedule::builder();
    builder.add_system("system", system).unwrap();
    builder.build().unwrap()
}

fn bench_commands(c: &mut Criterion) {
    let pool = ThreadPool::new(PoolConfig::default().with_workers(2)).unwrap();
    let mut g = c.benchmark_group("commands");
    g.throughput(Throughput::Elements(N as u64));

    g.bench_function(BenchmarkId::new("spawn_direct", N), |b| {
        b.iter_batched(
            World::new,
            |mut world| {
                for i in 0..N {
                    let e = world.spawn().unwrap();
                    world.insert(e, Position([i as f32; 3])).unwrap();
                    world.insert(e, Velocity([1.0, 2.0, 3.0])).unwrap();
                }
                world
            },
            BatchSize::LargeInput,
        );
    });
    // The queue stays warm between iterations: the schedule (and its system's queue) is reused.
    let mut spawner = schedule(spawn_all);
    g.bench_function(BenchmarkId::new("spawn_commands", N), |b| {
        b.iter_batched(
            World::new,
            |mut world| {
                spawner.run(&mut world, &pool).unwrap();
                world
            },
            BatchSize::LargeInput,
        );
    });

    g.bench_function(BenchmarkId::new("despawn_direct", N), |b| {
        b.iter_batched(
            || {
                let world = populated();
                let entities: Vec<Entity> = pragma_ecs::Query::<Entity>::new()
                    .unwrap()
                    .iter(&world)
                    .unwrap()
                    .collect();
                (world, entities)
            },
            |(mut world, entities)| {
                for e in entities {
                    world.despawn(e).unwrap();
                }
                world
            },
            BatchSize::LargeInput,
        );
    });
    let mut despawner = schedule(despawn_all);
    g.bench_function(BenchmarkId::new("despawn_commands", N), |b| {
        b.iter_batched(
            populated,
            |mut world| {
                despawner.run(&mut world, &pool).unwrap();
                world
            },
            BatchSize::LargeInput,
        );
    });
    g.finish();
}

criterion_group!(benches, bench_commands);
criterion_main!(benches);
