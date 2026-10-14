//! Deferred commands against direct world operations (DECISIONS #23–#25).
//!
//! Several systems get command lists (spawning with commands on the new entity, despawning,
//! inserting and removing on live, stale and same-frame-despawned entities) and run through a
//! schedule on 1 and 4 workers; the world, entity ids included, must equal the same operations
//! applied directly, system after system in registration order, with operations on dead entities
//! ignored. Used by `tests/commands.rs` and the `commands` fuzz target.

use std::sync::OnceLock;

use pragma_core::prelude::{PoolConfig, ThreadPool};
use pragma_ecs::{Commands, EcsError, Entity, EntityCommands, Query, Schedule, World};

use super::bytes::{Bytes, pick};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct A(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct B(pub u32);

pub fn pools() -> &'static [ThreadPool; 2] {
    static POOLS: OnceLock<[ThreadPool; 2]> = OnceLock::new();
    POOLS.get_or_init(|| {
        [1, 4].map(|workers| ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap())
    })
}

/// A command on one entity.
#[derive(Clone, Copy, Debug)]
pub enum On {
    InsertA(u32),
    InsertB(u32),
    RemoveA,
    RemoveB,
    Despawn,
}

/// A command of a system.
#[derive(Clone, Debug)]
pub enum Cmd {
    /// Spawns an entity and records the commands on it.
    Spawn(Vec<On>),
    /// Commands on one of the entities known before the frame (alive or not), by raw index.
    Existing(usize, On),
}

/// One case: the initial world, the systems' command lists and the number of frames.
#[derive(Clone, Debug)]
pub struct Case {
    /// The initial entities (A, B or both); at least one.
    pub entities: Vec<(Option<u32>, Option<u32>)>,
    /// How many of the last initial entities are despawned before the first frame.
    pub dead: usize,
    /// The command list of every system, in registration order.
    pub systems: Vec<Vec<Cmd>>,
    /// Frames to run (every frame repeats the same commands).
    pub frames: usize,
}

fn decode_on(bytes: &mut Bytes<'_>) -> On {
    match bytes.below(9) {
        0..=2 => On::InsertA(bytes.u32()),
        3..=5 => On::InsertB(bytes.u32()),
        6 => On::RemoveA,
        7 => On::RemoveB,
        _ => On::Despawn,
    }
}

fn decode_option(bytes: &mut Bytes<'_>) -> Option<u32> {
    bytes.bool().then(|| bytes.u32())
}

/// Decodes fuzzer input into a case, within the bounds of the property test's strategy.
pub fn decode(data: &[u8]) -> Case {
    let mut bytes = Bytes::new(data);
    let entities = (0..1 + usize::from(bytes.below(29)))
        .map(|_| (decode_option(&mut bytes), decode_option(&mut bytes)))
        .collect();
    let dead = usize::from(bytes.below(5));
    let frames = 1 + usize::from(bytes.below(2));
    let mut systems = Vec::new();
    for _ in 0..1 + usize::from(bytes.below(4)) {
        let mut cmds = Vec::new();
        for _ in 0..bytes.below(12) {
            cmds.push(if bytes.below(4) == 0 {
                Cmd::Spawn((0..bytes.below(4)).map(|_| decode_on(&mut bytes)).collect())
            } else {
                let i = bytes.index();
                Cmd::Existing(i, decode_on(&mut bytes))
            });
        }
        systems.push(cmds);
    }
    Case {
        entities,
        dead,
        systems,
        frames,
    }
}

/// Records `on` for an entity through `EntityCommands`.
fn record(entity: &mut EntityCommands<'_>, on: On) -> Result<(), EcsError> {
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
                let e = pick(known, *i).unwrap();
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
            Cmd::Existing(i, on) => apply_on(world, pick(known, *i).unwrap(), *on),
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

/// Invariant: commands applied at the sync point equal the same operations applied directly in
/// system registration order, entity ids included, on 1 and 4 workers, frame after frame.
pub fn check(case: &Case) {
    assert!(!case.entities.is_empty(), "a case has at least one entity");
    let dead = case.dead.min(case.entities.len() - 1);
    let (mut expected, known) = world(&case.entities, dead);
    for _ in 0..case.frames {
        for cmds in &case.systems {
            apply_directly(&mut expected, cmds, &known);
        }
    }
    let expected = state(&expected);

    for pool in pools() {
        let (mut actual, same) = world(&case.entities, dead);
        assert_eq!(same, known);
        let mut schedule = schedule(&case.systems, &known);
        for _ in 0..case.frames {
            schedule.run(&mut actual, pool).unwrap();
        }
        assert_eq!(state(&actual), expected);
    }
}
