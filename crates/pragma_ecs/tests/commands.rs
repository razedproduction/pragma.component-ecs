//! Deferred commands (DECISIONS #23–#25).
//!
//! The property test gives several systems random command lists (spawning, also with a bundle,
//! with commands on the new entity, despawning, inserting components and bundles and removing
//! on live, stale and same-frame-despawned
//! entities), runs them through a schedule on 1 and 4 workers, and checks that the world (entity
//! ids included) equals the same operations applied directly to the world, system after system
//! in registration order, with operations on dead entities ignored. The oracle
//! (`common/commands.rs`) is shared with the `commands` fuzz target (DECISIONS #30).
// Test code: a panic is a test failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::needless_pass_by_value
)]

mod common;

use common::commands::{A, B, Case, Cmd, On, check, decode, pools};
use pragma_ecs::{Commands, EcsError, Query, ResMut, Schedule, View, World};
use proptest::prelude::*;

fn on() -> impl Strategy<Value = On> {
    prop_oneof![
        3 => any::<u32>().prop_map(On::InsertA),
        3 => any::<u32>().prop_map(On::InsertB),
        1 => Just(On::RemoveA),
        1 => Just(On::RemoveB),
        2 => any::<(u32, u32)>().prop_map(|(b, a)| On::InsertBundle(b, a)),
        1 => Just(On::Despawn),
    ]
}

fn cmd() -> impl Strategy<Value = Cmd> {
    prop_oneof![
        1 => proptest::collection::vec(on(), 0..4).prop_map(Cmd::Spawn),
        1 => (any::<(u32, u32)>(), proptest::collection::vec(on(), 0..4))
            .prop_map(|((a, b), ons)| Cmd::SpawnBundle(a, b, ons)),
        3 => (any::<usize>(), on()).prop_map(|(i, o)| Cmd::Existing(i, o)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Invariant: commands applied at the sync point equal the same operations applied directly
    /// in system registration order, entity ids included, on 1 and 4 workers, frame after frame.
    #[test]
    fn commands_equal_direct_operations(
        entities in proptest::collection::vec(any::<(Option<u32>, Option<u32>)>(), 1..30),
        dead in 0..5_usize,
        systems in proptest::collection::vec(proptest::collection::vec(cmd(), 0..12), 1..5),
        frames in 1..3_usize,
    ) {
        check(&Case { entities, dead, systems, frames });
    }

    /// The fuzz target's decoder: arbitrary bytes are a valid case.
    #[test]
    fn decoded_bytes_equal_direct_operations(
        data in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        check(&decode(&data));
    }
}

// Behaviour of single schedules.

struct Log(Vec<u32>);

fn spawn_three(mut commands: Commands<'_>) -> Result<(), EcsError> {
    for i in 0..3 {
        commands.spawn()?.insert(A(i))?;
    }
    Ok(())
}

fn spawn_then_fail(mut commands: Commands<'_>) -> Result<(), String> {
    commands
        .spawn()
        .map_err(|e| e.to_string())?
        .insert(A(100))
        .map_err(|e| e.to_string())?;
    Err("refused".to_owned())
}

fn spawn_then_panic(mut commands: Commands<'_>) -> Result<(), EcsError> {
    commands.spawn()?.insert(A(200))?;
    panic!("crashed");
}

fn count(mut rows: View<&A>, mut log: ResMut<Log>) -> Result<(), EcsError> {
    log.0.push(u32::try_from(rows.iter()?.count()).unwrap());
    Ok(())
}

#[test]
fn commands_take_effect_at_the_end_of_the_frame() {
    let mut world = World::new();
    world.insert_resource(Log(Vec::new()));
    let mut builder = Schedule::builder();
    builder.add_system("spawn", spawn_three).unwrap();
    // Ordered after the spawner, it still sees the world as it was before the frame.
    builder
        .add_system_with("count", count, |c| c.after("spawn"))
        .unwrap();
    let mut schedule = builder.build().unwrap();
    let pool = &pools()[1];
    schedule.run(&mut world, pool).unwrap();
    schedule.run(&mut world, pool).unwrap();
    assert_eq!(world.resource::<Log>().unwrap().0, [0, 3]);
    assert_eq!(world.len(), 6);
}

#[test]
fn commands_of_failed_and_panicked_systems_are_discarded() {
    let mut world = World::new();
    let mut builder = Schedule::builder();
    builder.add_system("ok", spawn_three).unwrap();
    builder.add_system("fails", spawn_then_fail).unwrap();
    let mut schedule = builder.build().unwrap();
    let pool = &pools()[1];
    match schedule.run(&mut world, pool) {
        Err(EcsError::SystemFailed { system, .. }) => assert_eq!(system, "fails"),
        other => panic!("unexpected {other:?}"),
    }
    let values: Vec<u32> = Query::<&A>::new()
        .unwrap()
        .iter(&world)
        .unwrap()
        .map(|a| a.0)
        .collect();
    assert_eq!(values, [0, 1, 2]);

    let mut builder = Schedule::builder();
    builder.add_system("panics", spawn_then_panic).unwrap();
    builder.add_system("ok", spawn_three).unwrap();
    let mut schedule = builder.build().unwrap();
    assert!(matches!(
        schedule.run(&mut world, pool),
        Err(EcsError::SystemPanicked { .. })
    ));
    assert_eq!(world.len(), 6);
    assert!(
        Query::<&A>::new()
            .unwrap()
            .iter(&world)
            .unwrap()
            .all(|a| a.0 < 3)
    );
    // The next frame does not apply leftovers of the failed frame.
    let mut builder = Schedule::builder();
    builder.add_system("ok", spawn_three).unwrap();
    builder.build().unwrap().run(&mut world, pool).unwrap();
    assert_eq!(world.len(), 9);
}

#[test]
fn two_systems_despawning_the_same_entity_is_not_an_error() {
    let mut world = World::new();
    let victim = world.spawn().unwrap();
    world.insert(victim, A(1)).unwrap();
    let mut builder = Schedule::builder();
    for name in ["first", "second"] {
        builder
            .add_system(
                name,
                move |mut commands: Commands<'_>| -> Result<(), EcsError> {
                    commands.insert(victim, B(2))?;
                    commands.despawn(victim)
                },
            )
            .unwrap();
    }
    let mut schedule = builder.build().unwrap();
    schedule.run(&mut world, &pools()[0]).unwrap();
    assert!(world.is_empty());
    assert!(!world.contains(victim));
}
