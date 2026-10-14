//! Change-detection benchmarks (DECISIONS #27, #28) over 100 000 entities: the cost of a
//! `Changed<T>` row filter (the mask pass) when no row, 10 % of the rows and every row changed,
//! against a plain read of the same column (the reference), and the cost of writing through
//! `Mut<T>` (stamping the tick) against reading through `&mut T` without writing.
//!
//! `cargo bench -p pragma_ecs --bench change` (report: `target/criterion/report/index.html`).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss
)]

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pragma_ecs::{Changed, Query, World};

const N: usize = 100_000;

#[derive(Clone, Copy)]
struct Position([f32; 3]);

fn world() -> World {
    let mut world = World::new();
    for i in 0..N {
        let e = world.spawn().unwrap();
        world.insert(e, Position([i as f32; 3])).unwrap();
    }
    world
}

/// Writes every `step`-th row through `Mut` (the others are only read).
fn touch(world: &mut World, writer: &mut Query<&'static mut Position>, step: usize) {
    for (i, mut p) in writer.iter_mut(world).unwrap().enumerate() {
        if i % step == 0 {
            p.0[0] += 1.0;
        }
    }
}

fn bench_change(c: &mut Criterion) {
    let mut g = c.benchmark_group("change");
    g.throughput(Throughput::Elements(N as u64));

    let mut world = world();
    let mut read = Query::<&Position>::new().unwrap();
    g.bench_function(BenchmarkId::new("read_plain", N), |b| {
        b.iter(|| black_box(read.iter(&world).unwrap().map(|p| p.0[0]).sum::<f32>()));
    });

    let mut changed = Query::<&Position, Changed<Position>>::new().unwrap();
    let mut writer = Query::<&mut Position>::new().unwrap();
    for (name, step) in [("none", 0), ("10_percent", 10), ("all", 1)] {
        g.bench_function(BenchmarkId::new(format!("read_changed_{name}"), N), |b| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    if step > 0 {
                        touch(&mut world, &mut writer, step);
                    }
                    let start = std::time::Instant::now();
                    black_box(changed.iter(&world).unwrap().map(|p| p.0[0]).sum::<f32>());
                    total += start.elapsed();
                }
                total
            });
        });
    }

    // `&mut T` without writing: no tick is stamped.
    g.bench_function(BenchmarkId::new("mut_read_only", N), |b| {
        b.iter(|| {
            black_box(
                writer
                    .iter_mut(&mut world)
                    .unwrap()
                    .map(|p| p.0[0])
                    .sum::<f32>(),
            )
        });
    });
    // Writing every row: one tick store per row.
    g.bench_function(BenchmarkId::new("mut_write_all", N), |b| {
        b.iter(|| {
            for mut p in writer.iter_mut(&mut world).unwrap() {
                p.0[0] += 1.0;
            }
        });
    });
    g.finish();
}

criterion_group!(benches, bench_change);
criterion_main!(benches);
