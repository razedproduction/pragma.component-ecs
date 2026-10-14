//! Deferred commands (DECISIONS #23–#25).
//!
//! The property test gives several systems random command lists (spawning with commands on the
//! new entity, despawning, inserting and removing on live, stale and same-frame-despawned
//! entities), runs them through a schedule on 1 and 4 workers, and checks that the world (entity
//! ids included) equals the same operations applied directly to the world, system after system
//! in registration order, with operations on dead entities ignored.
// Test code: a panic is a test failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value,
    clippy::unreachable
)]

use std::sync::OnceLock;

use pragma_core::prelude::{PoolConfig, ThreadPool};
use pragma_ecs::{Commands, EcsError, Entity, Query, ResMut, Schedule, View, World};
use proptest::prelude::*;
use proptest::sample::Index;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct A(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct B(u32);

fn pools() -> &'static [ThreadPool; 2] {
    static POOLS: OnceLock<[ThreadPool; 2]> = OnceLock::new();
    POOLS.get_or_init(|| {
        [1, 4].map(|workers| ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap())
    })
}

/// A command on one entity.
#[derive(Clone, Copy, Debug)]
enum On {
    InsertA(u32),
    InsertB(u32),
    RemoveA,
    RemoveB,
    Despawn,
}

/// A command of a system.
#[derive(Clone, Debug)]
enum Cmd {
    /// Spawns an entity and records the commands on it.
    Spawn(Vec<On>),
    /// Commands on one of the entities known before the frame (alive or not).
    Existing(Index, On),
}

fn on() -> impl Strategy<Value = On> {
    prop_oneof![
        3 => any::<u32>().prop_map(On::InsertA),
        3 => any::<u32>().prop_map(On::InsertB),
        1 => Just(On::RemoveA),
        1 => Just(On::RemoveB),
        1 => Just(On::Despawn),
    ]
}

fn cmd() -> impl Strategy<Value = Cmd> {
    prop_oneof![
        1 => proptest::collection::vec(on(), 0..4).prop_map(Cmd::Spawn),
        3 => (any::<Index>(), on()).prop_map(|(i, o)| Cmd::Existing(i, o)),
    ]
}

/// Records `on` for an entity through `EntityCommands`.
fn record(entity: &mut pragma_ecs::EntityCommands<'_>, on: On) -> Result<(), EcsError> {
    match on {
        On::InsertA(v) => drop(entity.insert(A(v))?),
        On::InsertB(v) => drop(entity.insert(B(v))?),
        On::RemoveA => drop(entity.remove::<A>()?),
        On::RemoveB => drop(entity.remove::<B>()?),
        On::Despawn => unreachable!("despawn consumes the builder"),
    }
    Ok(())
}

/// Records the commands of one system.
fn issue(commands: &mut Commands<'_>, cmds: &[Cmd], known: &[Entity]) -> Result<(), EcsError> {
    for cmd in cmds {
        match cmd {
            Cmd::Spawn(ons) => {
                let mut entity = commands.spawn()?;
                for &on in ons {
                    if matches!(on, On::Despawn) {
                        entity.despawn()?;
                        break;
                    }
                    record(&mut entity, on)?;
                }
            }
            Cmd::Existing(i, on) => {
                let e = *i.get(known);
                match on {
                    On::InsertA(v) => commands.insert(e, A(*v))?,
                    On::InsertB(v) => commands.insert(e, B(*v))?,
                    On::RemoveA => commands.remove::<A>(e)?,
                    On::RemoveB => commands.remove::<B>(e)?,
                    On::Despawn => commands.entity(e).despawn()?,
                }
            }
        }
    }
    Ok(())
}

/// Ignores an operation on a dead entity, as commands do.
fn lenient<T>(result: Result<T, EcsError>) {
    match result {
        Ok(_) | Err(EcsError::NoSuchEntity { .. }) => {}
        Err(other) => panic!("unexpected {other}"),
    }
}

fn apply_on(world: &mut World, e: Entity, on: On) {
    match on {
        On::InsertA(v) => lenient(world.insert(e, A(v))),
        On::InsertB(v) => lenient(world.insert(e, B(v))),
        On::RemoveA => lenient(world.remove::<A>(e)),
        On::RemoveB => lenient(world.remove::<B>(e)),
        On::Despawn => lenient(world.despawn(e)),
    }
}

/// The same commands applied directly to the world.
fn apply_directly(world: &mut World, cmds: &[Cmd], known: &[Entity]) {
    for cmd in cmds {
        match cmd {
            Cmd::Spawn(ons) => {
                let e = world.spawn().unwrap();
                for &on in ons {
                    apply_on(world, e, on);
                    if matches!(on, On::Despawn) {
                        break;
                    }
                }
            }
            Cmd::Existing(i, on) => apply_on(world, *i.get(known), *on),
        }
    }
}

/// A world of `entities` (A, B or both), the last `dead` of them despawned; returns the ids.
fn world(entities: &[(Option<u32>, Option<u32>)], dead: usize) -> (World, Vec<Entity>) {
    let mut world = World::new();
    let ids: Vec<Entity> = entities
        .iter()
        .map(|(a, b)| {
            let e = world.spawn().unwrap();
            if let Some(a) = a {
                world.insert(e, A(*a)).unwrap();
            }
            if let Some(b) = b {
                world.insert(e, B(*b)).unwrap();
            }
            e
        })
        .collect();
    for e in ids.iter().rev().take(dead) {
        world.despawn(*e).unwrap();
    }
    (world, ids)
}

type State = (usize, Vec<(Entity, Option<A>, Option<B>)>);

fn state(world: &World) -> State {
    let mut rows: Vec<_> = Query::<(Entity, Option<&A>, Option<&B>)>::new()
        .unwrap()
        .iter(world)
        .unwrap()
        .map(|(e, a, b)| (e, a.copied(), b.copied()))
        .collect();
    rows.sort();
    (world.len(), rows)
}

fn schedule(systems: &[Vec<Cmd>], known: &[Entity]) -> Schedule {
    let mut builder = Schedule::builder();
    for (i, cmds) in systems.iter().enumerate() {
        let (cmds, known) = (cmds.clone(), known.to_vec());
        builder
            .add_system(
                format!("s{i}"),
                move |mut commands: Commands<'_>| -> Result<(), EcsError> {
                    issue(&mut commands, &cmds, &known)
                },
            )
            .unwrap();
    }
    builder.build().unwrap()
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
        let (mut expected, known) = world(&entities, dead.min(entities.len() - 1));
        for _ in 0..frames {
            for cmds in &systems {
                apply_directly(&mut expected, cmds, &known);
            }
        }
        let expected = state(&expected);

        for pool in pools() {
            let (mut actual, same) = world(&entities, dead.min(entities.len() - 1));
            prop_assert_eq!(&same, &known);
            let mut schedule = schedule(&systems, &known);
            for _ in 0..frames {
                schedule.run(&mut actual, pool).unwrap();
            }
            prop_assert_eq!(&state(&actual), &expected);
        }
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
