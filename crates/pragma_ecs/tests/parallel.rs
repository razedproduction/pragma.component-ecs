//! Property test: parallel query iteration gives the same result as sequential iteration, on 1
//! and N workers and for any chunk size (DECISIONS #15).
// Test code: a panic is a test failure.
#![allow(clippy::unwrap_used, clippy::panic, clippy::arithmetic_side_effects)]

use std::sync::{Mutex, OnceLock};

use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
use pragma_ecs::{EcsError, Entity, Query, Without, World};
use proptest::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pos(i64);

#[derive(Clone, Copy, Debug, PartialEq)]
struct Vel(i64);

struct Frozen;
struct Red;
struct Blue;

/// One entity: its position, optional velocity, and markers that spread it over tables.
#[derive(Clone, Debug)]
struct Spec {
    pos: i64,
    vel: Option<i64>,
    frozen: bool,
    red: bool,
    blue: bool,
}

fn spec() -> impl Strategy<Value = Spec> {
    (any::<i64>(), any::<Option<i64>>(), any::<[bool; 3]>()).prop_map(
        |(pos, vel, [frozen, red, blue])| Spec {
            pos,
            vel,
            frozen,
            red,
            blue,
        },
    )
}

/// The same world, built from `specs`, every time.
fn build(specs: &[Spec]) -> (World, Vec<Entity>) {
    let mut world = World::new();
    let mut entities = Vec::new();
    for s in specs {
        let e = world.spawn().unwrap();
        world.insert(e, Pos(s.pos)).unwrap();
        if let Some(v) = s.vel {
            world.insert(e, Vel(v)).unwrap();
        }
        if s.frozen {
            world.insert(e, Frozen).unwrap();
        }
        if s.red {
            world.insert(e, Red).unwrap();
        }
        if s.blue {
            world.insert(e, Blue).unwrap();
        }
        entities.push(e);
    }
    (world, entities)
}

/// A non-commutative per-entity update: any row visited twice or skipped changes the result.
fn step(pos: &mut Pos, vel: Option<&Vel>) {
    pos.0 = pos.0.wrapping_mul(31).wrapping_add(vel.map_or(7, |v| v.0));
}

type Movement = Query<(&'static mut Pos, Option<&'static Vel>), Without<Frozen>>;

fn pools() -> &'static [ThreadPool; 2] {
    static POOLS: OnceLock<[ThreadPool; 2]> = OnceLock::new();
    POOLS.get_or_init(|| {
        [1, 4].map(|workers| ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap())
    })
}

fn positions(world: &World, entities: &[Entity]) -> Vec<Option<Pos>> {
    entities
        .iter()
        .map(|e| world.get::<Pos>(*e).copied())
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Invariant: `par_for_each_mut` changes the world exactly as `iter_mut` does, and
    /// `par_for_each` visits exactly the rows `iter` yields, whatever the pool and the chunk size.
    #[test]
    fn parallel_equals_sequential(
        specs in proptest::collection::vec(spec(), 0..300),
        size in 1..64_usize,
        rounds in 1..3_usize,
    ) {
        let (mut sequential, entities) = build(&specs);
        let mut query = Movement::new().unwrap();
        for _ in 0..rounds {
            for (mut pos, vel) in query.iter_mut(&mut sequential).unwrap() {
                step(&mut pos, vel);
            }
        }
        let expected = positions(&sequential, &entities);
        let mut rows: Vec<(Entity, i64)> = Query::<(Entity, &Pos), Without<Frozen>>::new()
            .unwrap()
            .iter(&sequential)
            .unwrap()
            .map(|(e, p)| (e, p.0))
            .collect();
        rows.sort_unstable();

        for pool in pools() {
            for grain in [Grain::Fixed(size), Grain::Auto] {
                let (mut parallel, same) = build(&specs);
                prop_assert_eq!(&same, &entities);
                let mut query = Movement::new().unwrap();
                for _ in 0..rounds {
                    query
                        .par_for_each_mut(&mut parallel, pool, grain, |(mut pos, vel)| step(&mut pos, vel))
                        .unwrap();
                }
                prop_assert_eq!(&positions(&parallel, &entities), &expected);

                let seen = Mutex::new(Vec::new());
                Query::<(Entity, &Pos), Without<Frozen>>::new()
                    .unwrap()
                    .par_for_each(&parallel, pool, grain, |(e, p)| seen.lock().unwrap().push((e, p.0)))
                    .unwrap();
                let mut seen = seen.into_inner().unwrap();
                seen.sort_unstable();
                prop_assert_eq!(&seen, &rows);
            }
        }
    }
}

#[test]
fn a_panicking_closure_is_an_error_and_the_other_chunks_still_run() {
    let specs: Vec<Spec> = (0..100)
        .map(|i| Spec {
            pos: i,
            vel: None,
            frozen: false,
            red: false,
            blue: false,
        })
        .collect();
    let (mut world, entities) = build(&specs);
    let mut query = Query::<&mut Pos>::new().unwrap();
    let result = query.par_for_each_mut(&mut world, &pools()[1], Grain::Fixed(10), |mut pos| {
        if pos.0 == 42 {
            panic!("bad row");
        }
        pos.0 += 1000;
    });
    assert!(matches!(result, Err(EcsError::Job { .. })), "{result:?}");
    assert!(result.unwrap_err().to_string().contains("bad row"));
    // Every chunk but the one with the panicking row finished.
    let done = positions(&world, &entities)
        .iter()
        .filter(|p| p.is_some_and(|p| p.0 >= 1000))
        .count();
    assert!(done >= 90, "{done}");
}
