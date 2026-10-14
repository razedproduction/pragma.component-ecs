//! Change detection (DECISIONS #27, #28).
//!
//! The property test applies random world operations (spawning, inserting, replacing, removing,
//! `get_mut`, writing through `Mut` in a query, moving rows between tables, despawning) and
//! asks long-lived queries with `Added` / `Changed` filters at random points. A naive oracle
//! keeps, per entity, the sequence numbers of the operations that added and last changed its
//! component, and per query the sequence number of its previous call: every answer must match.
//! The oracle (`common/change.rs`) is shared with the `change_ticks` fuzz target (DECISIONS
//! #30). A second test runs writers and readers through a schedule in both orders on 1 and 4
//! workers.
// Test code: a panic is a test failure.
#![allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

mod common;

use common::change::{A, Mover, OBSERVERS, Op, check, decode};
use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
use pragma_ecs::{Changed, EcsError, ResMut, Schedule, View, Without, World};
use proptest::prelude::*;

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::Spawn),
        1 => any::<usize>().prop_map(Op::Despawn),
        4 => (any::<usize>(), any::<u32>()).prop_map(|(i, v)| Op::Insert(i, v)),
        1 => any::<usize>().prop_map(Op::Remove),
        2 => any::<usize>().prop_map(Op::GetMut),
        1 => Just(Op::WriteOdd),
        2 => any::<usize>().prop_map(Op::Move),
        4 => (0..OBSERVERS).prop_map(Op::Observe),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Invariant: every `Added` / `Changed` query yields exactly the entities whose component
    /// the oracle saw added / changed since that query's previous call.
    #[test]
    fn change_filters_match_a_per_entity_oracle(ops in proptest::collection::vec(op(), 0..200)) {
        check(ops);
    }

    /// The fuzz target's decoder: arbitrary bytes are a valid sequence of operations.
    #[test]
    fn decoded_bytes_match_the_tick_oracle(data in proptest::collection::vec(any::<u8>(), 0..1024)) {
        check(decode(&data));
    }
}

// Through a schedule.

#[derive(Default)]
struct Seen(Vec<Vec<u32>>);

#[derive(Default)]
struct Frame(u32);

/// Writes `A` of the entities whose value equals the frame number modulo 4.
fn write_some(mut values: View<&mut A>, frame: ResMut<Frame>) -> Result<(), EcsError> {
    let target = frame.0 % 4;
    values.par_for_each_mut(Grain::Fixed(3), |mut a| {
        if a.0 % 4 == target {
            a.0 += 4;
        }
    })
}

fn read_changed(
    mut changed: View<&A, (Changed<A>, Without<Mover>)>,
    mut seen: ResMut<Seen>,
) -> Result<(), EcsError> {
    let mut values: Vec<u32> = changed.iter()?.map(|a| a.0).collect();
    values.sort_unstable();
    seen.0.push(values);
    Ok(())
}

fn next_frame(mut frame: ResMut<Frame>) {
    frame.0 += 1;
}

fn run(reader_first: bool, workers: usize) -> Vec<Vec<u32>> {
    let pool = ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap();
    let mut world = World::new();
    for i in 0..40 {
        let e = world.spawn().unwrap();
        world.insert(e, A(i)).unwrap();
    }
    world.insert_resource(Seen::default());
    world.insert_resource(Frame::default());
    let mut builder = Schedule::builder();
    builder.add_system("write", write_some).unwrap();
    if reader_first {
        builder
            .add_system_with("read", read_changed, |c| c.before("write"))
            .unwrap();
    } else {
        builder
            .add_system_with("read", read_changed, |c| c.after("write"))
            .unwrap();
    }
    builder
        .add_system_with("next", next_frame, |c| c.after("write"))
        .unwrap();
    let mut schedule = builder.build().unwrap();
    for _ in 0..5 {
        schedule.run(&mut world, &pool).unwrap();
    }
    world.remove_resource::<Seen>().unwrap().0
}

#[test]
fn a_system_sees_every_change_exactly_once_in_either_order() {
    let all: Vec<u32> = (0..40).collect();
    // After the writer: everything on the first run, then the frame's own writes.
    let after = run(false, 1);
    assert_eq!(after[0].len(), all.len());
    assert_eq!(
        after[1],
        (1..40).step_by(4).map(|v| v + 4).collect::<Vec<_>>()
    );
    // Before the writer: everything on the first run, then the previous frame's writes.
    let before = run(true, 1);
    assert_eq!(before[0], all);
    assert_eq!(
        before[1],
        (0..40).step_by(4).map(|v| v + 4).collect::<Vec<_>>()
    );
    assert_eq!(before[2], after[1]);
    // Every write is seen once in both orders.
    let count = |frames: &[Vec<u32>]| frames.iter().skip(1).map(Vec::len).sum::<usize>();
    assert_eq!(count(&after[..4]), count(&before[1..5]));
    assert_eq!(run(false, 4), after);
    assert_eq!(run(true, 4), before);
}
