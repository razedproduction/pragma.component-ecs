//! Query iteration benchmarks (DECISIONS #16): a sequential and a parallel query over one table
//! and over tables fragmented by marker components, against the same update on a plain `Vec`
//! as the reference (docs/perf.md §3 of pragma.core: a reference the change does not affect).
//!
//! `cargo bench -p pragma_ecs` (report: `target/criterion/report/index.html`).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss
)]

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
use pragma_ecs::{Query, World};

const N: usize = 100_000;

#[derive(Clone, Copy)]
struct Position([f32; 3]);

#[derive(Clone, Copy)]
struct Velocity([f32; 3]);

/// A marker that puts entities into different tables.
struct Fragment<const K: usize>;

/// Cheap on purpose: the benchmark measures the iteration, not the update.
fn step(position: &mut Position, velocity: &Velocity) {
    for (p, v) in position.0.iter_mut().zip(velocity.0) {
        *p += v * 0.016;
    }
}

/// `N` entities with a position and a velocity, spread round-robin over `tables` tables (1..=8).
fn world(tables: usize) -> World {
    let mut world = World::new();
    for i in 0..N {
        let e = world.spawn().unwrap();
        world.insert(e, Position([i as f32; 3])).unwrap();
        world.insert(e, Velocity([1.0, 2.0, 3.0])).unwrap();
        match i % tables {
            0 => {}
            1 => drop(world.insert(e, Fragment::<1>).unwrap()),
            2 => drop(world.insert(e, Fragment::<2>).unwrap()),
            3 => drop(world.insert(e, Fragment::<3>).unwrap()),
            4 => drop(world.insert(e, Fragment::<4>).unwrap()),
            5 => drop(world.insert(e, Fragment::<5>).unwrap()),
            6 => drop(world.insert(e, Fragment::<6>).unwrap()),
            _ => drop(world.insert(e, Fragment::<7>).unwrap()),
        }
    }
    world
}

fn workers() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get())
}

fn bench_iter(c: &mut Criterion) {
    let pool = ThreadPool::new(PoolConfig::default().with_workers(workers())).unwrap();
    let mut g = c.benchmark_group("query_iter");
    g.throughput(Throughput::Elements(N as u64));

    // The reference: the same update over a plain Vec, sequential and on the same pool.
    let mut rows: Vec<(Position, Velocity)> = (0..N)
        .map(|i| (Position([i as f32; 3]), Velocity([1.0, 2.0, 3.0])))
        .collect();
    g.bench_function(BenchmarkId::new("vec", N), |b| {
        b.iter(|| rows.iter_mut().for_each(|(p, v)| step(p, v)));
    });
    g.bench_function(BenchmarkId::new("vec_parallel", N), |b| {
        b.iter(|| {
            pool.parallel_for(&mut rows, Grain::Auto, |(p, v)| step(p, v))
                .unwrap();
        });
    });

    for tables in [1, 8] {
        let mut world = world(tables);
        let mut query = Query::<(&mut Position, &Velocity)>::new().unwrap();
        g.bench_function(BenchmarkId::new(format!("query_{tables}_tables"), N), |b| {
            b.iter(|| {
                for (p, v) in query.iter_mut(&mut world).unwrap() {
                    step(p, v);
                }
            });
        });
        g.bench_function(
            BenchmarkId::new(format!("query_parallel_{tables}_tables"), N),
            |b| {
                b.iter(|| {
                    query
                        .par_for_each_mut(&mut world, &pool, Grain::Auto, |(p, v)| step(p, v))
                        .unwrap();
                });
            },
        );
        g.bench_function(
            BenchmarkId::new(format!("query_read_{tables}_tables"), N),
            |b| {
                let mut read = Query::<&Position>::new().unwrap();
                b.iter(|| {
                    let sum: f32 = read.iter(&world).unwrap().map(|p| p.0[0]).sum();
                    black_box(sum)
                });
            },
        );
    }
    g.finish();
}

criterion_group!(benches, bench_iter);
criterion_main!(benches);
