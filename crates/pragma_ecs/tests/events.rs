//! Events published at the sync point (DECISIONS #24, #26).
//!
//! The property test runs random writer and reader systems (readers skipping frames) together
//! with sends, manual updates and reads from outside the schedule, and checks every read and the
//! kept events against a naive model of the two buffers, on 1 and 4 workers. The oracle
//! (`common/events.rs`) is shared with the `events` fuzz target (DECISIONS #30, #40).
// Test code: a panic is a test failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

mod common;

use common::events::{Case, Step, check, decode};
use pragma_core::prelude::{PoolConfig, ThreadPool};
use pragma_ecs::{
    EcsError, EventCursor, EventReader, EventWriter, Events, Res, ResMut, Schedule, World,
};
use proptest::prelude::*;

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => Just(Step::Frame),
        1 => any::<u32>().prop_map(Step::Send),
        1 => Just(Step::Update),
        1 => Just(Step::Read),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Invariant: every reader, inside or outside systems, reads exactly the kept events it has
    /// not read yet, in sending order, and the world keeps exactly the events of the last two
    /// updates, on 1 and 4 workers.
    #[test]
    fn events_match_a_two_buffer_model(
        writers in proptest::collection::vec(
            proptest::collection::vec(proptest::collection::vec(any::<u32>(), 0..4), 1..4),
            0..4,
        ),
        readers in proptest::collection::vec(proptest::collection::vec(any::<bool>(), 1..5), 0..4),
        steps in proptest::collection::vec(step(), 0..64),
    ) {
        check(&Case { writers, readers, steps });
    }

    /// The fuzz target's decoder: arbitrary bytes are a valid case.
    #[test]
    fn decoded_bytes_match_the_two_buffer_model(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        check(&decode(&data));
    }
}

// Behaviour of single schedules.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ping(u32);

/// What each reader has seen, frame by frame.
#[derive(Default)]
struct Seen(Vec<Vec<u32>>);

#[derive(Default)]
struct Late(Vec<Vec<u32>>);

fn pool(workers: usize) -> ThreadPool {
    ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap()
}

fn writer<const BASE: u32>(
    mut pings: EventWriter<Ping>,
    frame: Res<Frame>,
) -> Result<(), EcsError> {
    pings.send(Ping(BASE + frame.0))?;
    pings.send(Ping(BASE + frame.0 + 1))?;
    assert_eq!(pings.len(), 2);
    Ok(())
}

struct Frame(u32);

struct Frame2(u32);

fn second_writer(mut pings: EventWriter<Ping>, frame: Res<Frame2>) -> Result<(), EcsError> {
    pings.send(Ping(1000 + frame.0))
}

fn tick(mut frame: ResMut<Frame>, mut other: ResMut<Frame2>) {
    frame.0 += 10;
    other.0 += 10;
}

fn reader(mut pings: EventReader<Ping>, mut seen: ResMut<Seen>) {
    let unread = pings.len();
    assert_eq!(pings.is_empty(), unread == 0);
    seen.0.push(pings.read().map(|p| p.0).collect());
    assert!(pings.is_empty());
}

fn late_reader(mut pings: EventReader<Ping>, mut seen: ResMut<Late>) {
    seen.0.push(pings.read().map(|p| p.0).collect());
}

fn world() -> World {
    let mut world = World::new();
    world.insert_resource(Frame(0));
    world.insert_resource(Frame2(0));
    world.insert_resource(Seen::default());
    world.insert_resource(Late::default());
    world
}

fn schedule() -> Schedule {
    let mut builder = Schedule::builder();
    // Registration order decides the publishing order: `second` before `first`.
    builder.add_system("second", second_writer).unwrap();
    builder.add_system("first", writer::<0>).unwrap();
    builder.add_system("reader", reader).unwrap();
    builder
        .add_system_with("tick", tick, |c| c.after("first").after("second"))
        .unwrap();
    builder.build().unwrap()
}

#[test]
fn events_arrive_once_next_frame_in_registration_order() {
    for workers in [1, 4] {
        let pool = pool(workers);
        let mut world = world();
        let mut schedule = schedule();
        // Writers and readers never conflict: no explicit order between them was needed.
        for _ in 0..3 {
            schedule.run(&mut world, &pool).unwrap();
        }
        let seen = &world.resource::<Seen>().unwrap().0;
        assert_eq!(
            seen,
            &[vec![], vec![1000, 0, 1], vec![1010, 10, 11]],
            "{workers} workers"
        );
        // The events of the last frame are kept for the next one.
        let events = world.resource::<Events<Ping>>().unwrap();
        assert_eq!(
            events.iter().map(|p| p.0).collect::<Vec<_>>(),
            [1010, 10, 11, 1020, 20, 21]
        );
    }
}

#[test]
fn a_reader_added_later_or_skipping_a_frame_still_sees_kept_events() {
    let pool = pool(2);
    let mut world = world();
    let mut schedule = schedule();
    schedule.run(&mut world, &pool).unwrap();
    schedule.run(&mut world, &pool).unwrap();

    // A second schedule with a reader that runs every other frame: it sees the events of the
    // last two sync points, each once.
    let mut builder = Schedule::builder();
    builder.add_system("late", late_reader).unwrap();
    let mut late = builder.build().unwrap();
    late.run(&mut world, &pool).unwrap();
    // `late`'s own sync point updated the buffers once more, as a second schedule does.
    schedule.run(&mut world, &pool).unwrap();
    late.run(&mut world, &pool).unwrap();
    let seen = &world.resource::<Late>().unwrap().0;
    assert_eq!(seen[0], [1000, 0, 1, 1010, 10, 11]);
    assert_eq!(seen[1], [1020, 20, 21]);
}

#[test]
fn events_sent_from_outside_and_schedules_creating_the_resource() {
    let pool = pool(1);
    let mut world = world();
    let mut schedule = schedule();
    // The schedule creates `Events<Ping>` before the first frame.
    assert!(!world.contains_resource::<Events<Ping>>());
    world.send_event(Ping(7)).unwrap();
    schedule.run(&mut world, &pool).unwrap();
    assert_eq!(world.resource::<Seen>().unwrap().0, [vec![7]]);

    // A cursor outside systems.
    let mut cursor = EventCursor::<Ping>::default();
    let events = world.resource::<Events<Ping>>().unwrap();
    assert_eq!(cursor.unread(&events), 4);
    assert_eq!(cursor.read(&events).count(), 4);

    // A reader over another world starts from the beginning there.
    let mut other = self::world();
    other.send_event(Ping(42)).unwrap();
    schedule.run(&mut other, &pool).unwrap();
    assert_eq!(other.resource::<Seen>().unwrap().0, [vec![42]]);
}

fn failing_writer(mut pings: EventWriter<Ping>) -> Result<(), String> {
    pings.send(Ping(99)).map_err(|e| e.to_string())?;
    Err("refused".to_owned())
}

#[test]
fn events_of_a_failed_system_are_discarded() {
    let pool = pool(2);
    let mut world = world();
    let mut builder = Schedule::builder();
    builder.add_system("failing", failing_writer).unwrap();
    builder.add_system("first", writer::<0>).unwrap();
    let mut schedule = builder.build().unwrap();
    assert!(matches!(
        schedule.run(&mut world, &pool),
        Err(EcsError::SystemFailed { .. })
    ));
    let events = world.resource::<Events<Ping>>().unwrap();
    assert_eq!(events.iter().map(|p| p.0).collect::<Vec<_>>(), [0, 1]);
}
