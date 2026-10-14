//! `pragma_ecs` next to `hecs` and `bevy_ecs` on the same workloads (DECISIONS #31). Each
//! criterion group is one workload, each function in it one library:
//!
//! - `spawn`: 10 000 entities with a position and a velocity into an empty world;
//! - `iter`: `(&mut Position, &Velocity)` over 100 000 entities in one table, writing every row;
//! - `frag_iter`: `&mut Data` over 20 entities in each of 26 tables;
//! - `add_remove`: add and then remove a component on 10 000 entities;
//! - `despawn`: despawn 10 000 entities.
//!
//! The libraries differ in what they do per write: `pragma_ecs` and `bevy_ecs` stamp a change
//! tick (`u64` / `u32`), `hecs` has no change detection. Every library spawns a bundle into its
//! final table (`pragma_ecs` since 1.0, DECISIONS #35); `spawn/pragma_ecs_inserts` keeps the
//! pre-bundle way, `spawn` plus one `insert` per component (each a table move).
//!
//! `cargo bench --manifest-path baseline/Cargo.toml` (report:
//! `baseline/target/criterion/report/index.html`).
#![allow(missing_docs, clippy::unwrap_used, clippy::cast_precision_loss)]

use std::hint::black_box;

use bevy_ecs::component::Component;
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};

const SPAWN: usize = 10_000;
const ITER: usize = 100_000;
const FRAG_PER_TABLE: usize = 20;

#[derive(Clone, Copy, Component)]
struct Position([f32; 3]);

#[derive(Clone, Copy, Component)]
struct Velocity([f32; 3]);

#[derive(Clone, Copy, Component)]
struct Extra(#[allow(dead_code)] u32);

#[derive(Clone, Copy, Component)]
struct Data(f32);

fn step(position: &mut Position, velocity: &Velocity) {
    for (p, v) in position.0.iter_mut().zip(velocity.0) {
        *p += v * 0.016;
    }
}

fn position(i: usize) -> Position {
    Position([i as f32; 3])
}

const VELOCITY: Velocity = Velocity([1.0, 2.0, 3.0]);

// Populated worlds of every library.

fn pragma_world(n: usize) -> (pragma_ecs::World, Vec<pragma_ecs::Entity>) {
    let mut world = pragma_ecs::World::new();
    let entities = (0..n)
        .map(|i| world.spawn_bundle((position(i), VELOCITY)).unwrap())
        .collect();
    (world, entities)
}

/// The same world without bundles: `spawn` plus one `insert` (a table move) per component.
fn pragma_world_inserts(n: usize) -> pragma_ecs::World {
    let mut world = pragma_ecs::World::new();
    for i in 0..n {
        let e = world.spawn().unwrap();
        world.insert(e, position(i)).unwrap();
        world.insert(e, VELOCITY).unwrap();
    }
    world
}

fn hecs_world(n: usize) -> (hecs::World, Vec<hecs::Entity>) {
    let mut world = hecs::World::new();
    let entities = (0..n)
        .map(|i| world.spawn((position(i), VELOCITY)))
        .collect();
    (world, entities)
}

fn bevy_world(n: usize) -> (bevy_ecs::world::World, Vec<bevy_ecs::entity::Entity>) {
    let mut world = bevy_ecs::world::World::new();
    let entities = (0..n)
        .map(|i| world.spawn((position(i), VELOCITY)).id())
        .collect();
    (world, entities)
}

fn spawn(c: &mut Criterion) {
    let mut g = c.benchmark_group("spawn");
    g.throughput(Throughput::Elements(SPAWN as u64));
    g.bench_function("pragma_ecs", |b| b.iter(|| pragma_world(SPAWN)));
    g.bench_function("pragma_ecs_inserts", |b| {
        b.iter(|| pragma_world_inserts(SPAWN));
    });
    g.bench_function("hecs", |b| b.iter(|| hecs_world(SPAWN)));
    g.bench_function("bevy_ecs", |b| b.iter(|| bevy_world(SPAWN)));
    g.finish();
}

fn iter(c: &mut Criterion) {
    let mut g = c.benchmark_group("iter");
    g.throughput(Throughput::Elements(ITER as u64));

    let (mut world, _) = pragma_world(ITER);
    let mut query = pragma_ecs::Query::<(&mut Position, &Velocity)>::new().unwrap();
    g.bench_function("pragma_ecs", |b| {
        b.iter(|| {
            for (mut p, v) in query.iter_mut(&mut world).unwrap() {
                step(&mut p, v);
            }
        });
    });

    let (mut world, _) = hecs_world(ITER);
    let mut query = hecs::PreparedQuery::<(&mut Position, &Velocity)>::new();
    g.bench_function("hecs", |b| {
        b.iter(|| {
            for (p, v) in query.query_mut(&mut world) {
                step(p, v);
            }
        });
    });

    let (mut world, _) = bevy_world(ITER);
    let mut query = world.query::<(&mut Position, &Velocity)>();
    g.bench_function("bevy_ecs", |b| {
        b.iter(|| {
            for (mut p, v) in query.iter_mut(&mut world) {
                step(&mut p, v);
            }
        });
    });

    // The reference: the same update over a plain Vec.
    let mut rows: Vec<(Position, Velocity)> = (0..ITER).map(|i| (position(i), VELOCITY)).collect();
    g.bench_function("vec", |b| {
        b.iter(|| rows.iter_mut().for_each(|(p, v)| step(p, v)));
    });
    g.finish();
}

/// 26 marker types that put `Data` into 26 tables.
macro_rules! markers {
    ($($name:ident),*) => {$(
        #[derive(Clone, Copy, Component)]
        struct $name;
    )*};
}
markers!(
    A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z
);

/// Calls `$spawn!(A, B, …, Z)` with every marker type.
macro_rules! with_markers {
    ($spawn:ident) => {
        $spawn!(
            A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z
        )
    };
}

fn frag_iter(c: &mut Criterion) {
    let mut g = c.benchmark_group("frag_iter");
    g.throughput(Throughput::Elements((26 * FRAG_PER_TABLE) as u64));

    let mut world = pragma_ecs::World::new();
    macro_rules! spawn_pragma {
        ($($marker:ident),*) => {$(
            for _ in 0..FRAG_PER_TABLE {
                world.spawn_bundle((Data(1.0), $marker)).unwrap();
            }
        )*};
    }
    with_markers!(spawn_pragma);
    let mut query = pragma_ecs::Query::<&mut Data>::new().unwrap();
    g.bench_function("pragma_ecs", |b| {
        b.iter(|| {
            for mut d in query.iter_mut(&mut world).unwrap() {
                d.0 *= 2.0;
            }
        });
    });

    let mut world = hecs::World::new();
    macro_rules! spawn_hecs {
        ($($marker:ident),*) => {$(
            for _ in 0..FRAG_PER_TABLE {
                world.spawn((Data(1.0), $marker));
            }
        )*};
    }
    with_markers!(spawn_hecs);
    let mut query = hecs::PreparedQuery::<&mut Data>::new();
    g.bench_function("hecs", |b| {
        b.iter(|| {
            for d in query.query_mut(&mut world) {
                d.0 *= 2.0;
            }
        });
    });

    let mut world = bevy_ecs::world::World::new();
    macro_rules! spawn_bevy {
        ($($marker:ident),*) => {$(
            for _ in 0..FRAG_PER_TABLE {
                world.spawn((Data(1.0), $marker));
            }
        )*};
    }
    with_markers!(spawn_bevy);
    let mut query = world.query::<&mut Data>();
    g.bench_function("bevy_ecs", |b| {
        b.iter(|| {
            for mut d in query.iter_mut(&mut world) {
                d.0 *= 2.0;
            }
        });
    });
    g.finish();
}

fn add_remove(c: &mut Criterion) {
    let mut g = c.benchmark_group("add_remove");
    g.throughput(Throughput::Elements(SPAWN as u64));

    let (mut world, entities) = pragma_world(SPAWN);
    g.bench_function("pragma_ecs", |b| {
        b.iter(|| {
            for &e in &entities {
                world.insert(e, Extra(1)).unwrap();
            }
            for &e in &entities {
                black_box(world.remove::<Extra>(e).unwrap());
            }
        });
    });

    let (mut world, entities) = hecs_world(SPAWN);
    g.bench_function("hecs", |b| {
        b.iter(|| {
            for &e in &entities {
                world.insert_one(e, Extra(1)).unwrap();
            }
            for &e in &entities {
                black_box(world.remove_one::<Extra>(e).unwrap());
            }
        });
    });

    let (mut world, entities) = bevy_world(SPAWN);
    g.bench_function("bevy_ecs", |b| {
        b.iter(|| {
            for &e in &entities {
                world.entity_mut(e).insert(Extra(1));
            }
            for &e in &entities {
                world.entity_mut(e).remove::<Extra>();
            }
        });
    });
    g.finish();
}

fn despawn(c: &mut Criterion) {
    let mut g = c.benchmark_group("despawn");
    g.throughput(Throughput::Elements(SPAWN as u64));
    g.bench_function("pragma_ecs", |b| {
        b.iter_batched(
            || pragma_world(SPAWN),
            |(mut world, entities)| {
                for e in entities {
                    world.despawn(e).unwrap();
                }
                world
            },
            BatchSize::LargeInput,
        );
    });
    g.bench_function("hecs", |b| {
        b.iter_batched(
            || hecs_world(SPAWN),
            |(mut world, entities)| {
                for e in entities {
                    world.despawn(e).unwrap();
                }
                world
            },
            BatchSize::LargeInput,
        );
    });
    g.bench_function("bevy_ecs", |b| {
        b.iter_batched(
            || bevy_world(SPAWN),
            |(mut world, entities)| {
                for e in entities {
                    world.despawn(e);
                }
                world
            },
            BatchSize::LargeInput,
        );
    });
    g.finish();
}

criterion_group!(benches, spawn, iter, frag_iter, add_remove, despawn);
criterion_main!(benches);
