//! Schedule benchmarks (DECISIONS #16, #18): one frame of four systems over 100 000 entities on
//! the frame graph, against the same updates as sequential queries on the world (the
//! reference), and the fixed cost of a frame (lending the columns, running the graph) on an
//! empty world.
//!
//! `cargo bench -p pragma_ecs --bench schedule` (report: `target/criterion/report/index.html`).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss
)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
use pragma_ecs::{EcsError, Query, Res, ResMut, Schedule, View, World};

const N: usize = 100_000;

#[derive(Clone, Copy)]
struct Position([f32; 3]);

#[derive(Clone, Copy)]
struct Velocity([f32; 3]);

struct Age(u32);

struct Health(f32);

struct Dt(f32);

struct Energy(f32);

fn advance(position: &mut Position, velocity: &Velocity, dt: f32) {
    for (p, v) in position.0.iter_mut().zip(velocity.0) {
        *p += v * dt;
    }
}

fn kinetic(velocity: &Velocity) -> f32 {
    velocity.0.iter().map(|v| v * v).sum::<f32>() / 2.0
}

fn regenerate(health: &mut Health) {
    health.0 = (health.0 + 0.1).min(100.0);
}

fn movement(mut bodies: View<(&mut Position, &Velocity)>, dt: Res<Dt>) -> Result<(), EcsError> {
    let dt = dt.0;
    bodies.par_for_each_mut(Grain::Auto, |(mut p, v)| advance(&mut p, v, dt))
}

fn aging(mut ages: View<&mut Age>) -> Result<(), EcsError> {
    ages.par_for_each_mut(Grain::Auto, |mut a| a.0 += 1)
}

fn health(mut healths: View<&mut Health>) -> Result<(), EcsError> {
    healths.par_for_each_mut(Grain::Auto, |mut h| regenerate(&mut h))
}

fn energy(mut velocities: View<&Velocity>, mut energy: ResMut<Energy>) -> Result<(), EcsError> {
    energy.0 = velocities.iter()?.map(kinetic).sum();
    Ok(())
}

fn populate(entities: usize) -> World {
    let mut world = World::new();
    for i in 0..entities {
        let e = world.spawn().unwrap();
        world.insert(e, Position([i as f32; 3])).unwrap();
        world.insert(e, Velocity([1.0, 2.0, 3.0])).unwrap();
        world.insert(e, Age(0)).unwrap();
        world.insert(e, Health(50.0)).unwrap();
    }
    world.insert_resource(Dt(0.016));
    world.insert_resource(Energy(0.0));
    world
}

fn schedule() -> Schedule {
    let mut builder = Schedule::builder();
    builder
        .add_system("movement", movement)
        .unwrap()
        .add_system("aging", aging)
        .unwrap()
        .add_system("health", health)
        .unwrap()
        .add_system("energy", energy)
        .unwrap();
    builder.build().unwrap()
}

fn workers() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get())
}

fn bench_frame(c: &mut Criterion) {
    let pool = ThreadPool::new(PoolConfig::default().with_workers(workers())).unwrap();
    let mut g = c.benchmark_group("schedule_frame");
    g.throughput(Throughput::Elements(N as u64));

    // The reference: the same four updates as sequential queries on the world.
    let mut reference = populate(N);
    let mut bodies = Query::<(&mut Position, &Velocity)>::new().unwrap();
    let mut ages = Query::<&mut Age>::new().unwrap();
    let mut healths = Query::<&mut Health>::new().unwrap();
    let mut velocities = Query::<&Velocity>::new().unwrap();
    g.bench_function(BenchmarkId::new("queries", N), |b| {
        b.iter(|| {
            let dt = reference.resource::<Dt>().unwrap().0;
            for (mut p, v) in bodies.iter_mut(&mut reference).unwrap() {
                advance(&mut p, v, dt);
            }
            ages.iter_mut(&mut reference)
                .unwrap()
                .for_each(|mut a| a.0 += 1);
            healths
                .iter_mut(&mut reference)
                .unwrap()
                .for_each(|mut h| regenerate(&mut h));
            let total = velocities.iter(&reference).unwrap().map(kinetic).sum();
            reference.resource_mut::<Energy>().unwrap().0 = total;
        });
    });

    let mut world = populate(N);
    let mut frame = schedule();
    g.bench_function(BenchmarkId::new("schedule", N), |b| {
        b.iter(|| frame.run(&mut world, &pool).unwrap());
    });
    g.finish();

    // The fixed cost of a frame: lend and return the columns, run four systems.
    let mut empty = populate(0);
    let mut frame = schedule();
    c.bench_function("schedule_frame/empty_world", |b| {
        b.iter(|| frame.run(&mut empty, &pool).unwrap());
    });
}

criterion_group!(benches, bench_frame);
criterion_main!(benches);
