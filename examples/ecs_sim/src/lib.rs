//! `ecs_sim` is the vertical slice of `pragma.component-ecs` (milestone M3, DECISIONS #22): a
//! headless fixed-step simulation through the whole stack.
//!
//! - **World**: N entities with a position, a velocity, health and age, in archetype tables;
//!   entities whose health runs out get a `Dead` marker between ticks, so they move to another
//!   table while the simulation runs.
//! - **Systems** on one [`Schedule`] over the `pragma_core` frame graph: `clock`, `steer`,
//!   `movement`, `hazard`, `aging` and `hash`, with access derived from their parameters. Data
//!   dependencies are ordered explicitly; `aging` shares nothing with the others and runs in
//!   parallel with them. The per-entity systems split their rows over the pool.
//! - **State hash** of every tick: an FNV-1a hash of every entity's id and components, computed
//!   by the last system in table and row order.
//!
//! ## Determinism
//!
//! The hash of every tick depends only on the configuration, not on the number of workers: the
//! per-entity updates are element-wise (the row split depends only on the data), the hash is a
//! sequential fold in a fixed order, and every conflicting pair of systems is ordered.
//!
//! ```
//! use ecs_sim::{SimConfig, run};
//!
//! let config = SimConfig { entities: 2_000, ticks: 12, ..SimConfig::default() };
//! let one = run(&SimConfig { workers: 1, ..config.clone() })?;
//! let many = run(&SimConfig { workers: 3, ..config })?;
//! assert_eq!(one.hashes, many.hashes);
//! # Ok::<(), ecs_sim::SimError>(())
//! ```
#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use pragma_core::jobs::{Grain, JobsError, PoolConfig, ThreadPool};
use pragma_ecs::{EcsError, Entity, Query, Res, ResMut, Schedule, View, With, Without, World};

/// The side of the square arena.
const ARENA: f32 = 1024.0;
const CENTER: f32 = ARENA / 2.0;
/// The fixed simulation step.
const DT: f32 = 1.0 / 60.0;
const PULL: f32 = 0.6;
const SWIRL: f32 = 40.0;
const DRAG: f32 = 0.995;
/// The radius of the hazard around the center and its damage per second.
const HAZARD_RADIUS: f32 = 160.0;
const HAZARD_DPS: f32 = 30.0;
const REGEN: f32 = 2.0;
const MAX_HEALTH: f32 = 100.0;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Simulation errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SimError {
    /// Failed to create the pool.
    #[error("thread pool: {0}")]
    Pool(#[from] JobsError),
    /// A world, schedule or system error.
    #[error("ecs: {0}")]
    Ecs(#[from] EcsError),
    /// Invalid configuration.
    #[error("invalid config: {0}")]
    Config(&'static str),
}

/// Run parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimConfig {
    /// Number of entities.
    pub entities: usize,
    /// Number of ticks.
    pub ticks: u64,
    /// Pool workers.
    pub workers: usize,
    /// Seed of the initial state.
    pub seed: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            entities: 100_000,
            ticks: 300,
            workers: default_workers(),
            seed: 0x5EED_CAFE,
        }
    }
}

/// The default number of pool workers: the cores minus the main thread, which runs systems
/// too while it waits for a frame.
#[must_use]
pub fn default_workers() -> usize {
    PoolConfig::default().workers()
}

/// The outcome of a run.
#[derive(Debug, Clone)]
pub struct SimReport {
    /// The state hash after every tick.
    pub hashes: Vec<u64>,
    /// Entities marked dead by the end of the run.
    pub dead: usize,
    /// Run time.
    pub elapsed: Duration,
}

/// Position in the arena.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Position {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate.
    pub y: f32,
}

/// Velocity per second.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Velocity {
    /// Horizontal component.
    pub x: f32,
    /// Vertical component.
    pub y: f32,
}

/// Health; at zero or below the entity is marked [`Dead`] between ticks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Health(pub f32);

/// Ticks lived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Age(pub u32);

/// A marker: the entity no longer moves or takes damage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dead;

/// The current tick (a resource).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tick(pub u64);

/// The state hash of the last tick (a resource).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateHash(pub u64);

/// A deterministic generator for the initial state (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `[0, 1)`.
    #[allow(clippy::cast_precision_loss)]
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1_u64 << 24) as f32
    }
}

fn fnv(hash: u64, word: u64) -> u64 {
    word.to_le_bytes()
        .iter()
        .fold(hash, |h, b| (h ^ u64::from(*b)).wrapping_mul(FNV_PRIME))
}

/// Advances the tick.
fn clock(mut tick: ResMut<Tick>) {
    tick.0 = tick.0.wrapping_add(1);
}

/// Pulls every living entity towards the center with a swirl that changes direction every
/// 100 ticks.
fn steer(
    mut bodies: View<(&Position, &mut Velocity), Without<Dead>>,
    tick: Res<Tick>,
) -> Result<(), EcsError> {
    let swirl = if (tick.0 / 100).is_multiple_of(2) {
        SWIRL
    } else {
        -SWIRL
    };
    bodies.par_for_each_mut(Grain::Auto, |(position, velocity)| {
        let (dx, dy) = (CENTER - position.x, CENTER - position.y);
        let distance = (dx * dx + dy * dy).sqrt().max(1.0);
        velocity.x = (velocity.x + (dx * PULL - dy / distance * swirl) * DT) * DRAG;
        velocity.y = (velocity.y + (dy * PULL + dx / distance * swirl) * DT) * DRAG;
    })
}

/// Moves every living entity, clamped to the arena.
fn movement(mut bodies: View<(&mut Position, &Velocity), Without<Dead>>) -> Result<(), EcsError> {
    bodies.par_for_each_mut(Grain::Auto, |(position, velocity)| {
        position.x = (position.x + velocity.x * DT).clamp(0.0, ARENA);
        position.y = (position.y + velocity.y * DT).clamp(0.0, ARENA);
    })
}

/// Damages living entities inside the hazard, heals the others.
fn hazard(mut bodies: View<(&Position, &mut Health), Without<Dead>>) -> Result<(), EcsError> {
    bodies.par_for_each_mut(Grain::Auto, |(position, health)| {
        let (dx, dy) = (position.x - CENTER, position.y - CENTER);
        let change = if dx * dx + dy * dy < HAZARD_RADIUS * HAZARD_RADIUS {
            -HAZARD_DPS
        } else {
            REGEN
        };
        health.0 = (health.0 + change * DT).min(MAX_HEALTH);
    })
}

/// Ages every entity, dead or alive. Shares no data with the other systems but `hash`.
fn aging(mut ages: View<&mut Age>) -> Result<(), EcsError> {
    ages.par_for_each_mut(Grain::Auto, |age| age.0 = age.0.wrapping_add(1))
}

/// Everything the state hash covers, per entity.
type Snapshot = (
    Entity,
    &'static Position,
    &'static Velocity,
    &'static Health,
    &'static Age,
    Option<&'static Dead>,
);

/// The state hash: every entity's id and components, in table and row order.
fn hash(
    mut rows: View<Snapshot>,
    tick: Res<Tick>,
    mut state: ResMut<StateHash>,
) -> Result<(), EcsError> {
    let mut h = fnv(FNV_OFFSET, tick.0);
    for (entity, position, velocity, health, age, dead) in rows.iter()? {
        h = fnv(h, entity.to_bits());
        h = fnv(
            h,
            u64::from(position.x.to_bits()) << 32 | u64::from(position.y.to_bits()),
        );
        h = fnv(
            h,
            u64::from(velocity.x.to_bits()) << 32 | u64::from(velocity.y.to_bits()),
        );
        h = fnv(h, u64::from(health.0.to_bits()) << 32 | u64::from(age.0));
        h = fnv(h, u64::from(dead.is_some()));
    }
    state.0 = h;
    Ok(())
}

/// The systems of one tick. Conflicting pairs are ordered explicitly; `aging` runs in
/// parallel with everything before `hash`.
///
/// # Errors
///
/// [`EcsError::AmbiguousOrder`] if two systems conflict without an order (a bug in the
/// example), or another schedule error.
pub fn schedule() -> Result<Schedule, EcsError> {
    let mut builder = Schedule::builder();
    builder.add_system("clock", clock)?;
    builder.add_system_with("steer", steer, |c| c.after("clock"))?;
    builder.add_system_with("movement", movement, |c| c.after("steer"))?;
    builder.add_system_with("hazard", hazard, |c| c.after("movement"))?;
    builder.add_system("aging", aging)?;
    builder.add_system_with("hash", hash, |c| c.after("hazard").after("aging"))?;
    builder.build()
}

/// The initial world: `config.entities` entities scattered over the arena.
///
/// # Errors
///
/// [`EcsError::OutOfMemory`] if the world cannot grow.
pub fn populate(config: &SimConfig) -> Result<World, EcsError> {
    let mut rng = Rng(config.seed);
    let mut world = World::new();
    for _ in 0..config.entities {
        let entity = world.spawn()?;
        world.insert(
            entity,
            Position {
                x: rng.unit() * ARENA,
                y: rng.unit() * ARENA,
            },
        )?;
        world.insert(
            entity,
            Velocity {
                x: (rng.unit() - 0.5) * 100.0,
                y: (rng.unit() - 0.5) * 100.0,
            },
        )?;
        world.insert(entity, Health(MAX_HEALTH * (0.2 + 0.8 * rng.unit())))?;
        world.insert(entity, Age(0))?;
    }
    world.insert_resource(Tick(0));
    world.insert_resource(StateHash(0));
    Ok(world)
}

/// Marks the entities whose health ran out as [`Dead`]: a structural change between ticks,
/// which moves them to another table.
fn bury(
    world: &mut World,
    dying: &mut Query<(Entity, &Health), Without<Dead>>,
) -> Result<(), EcsError> {
    let dead: Vec<Entity> = dying
        .iter(world)?
        .filter(|(_, health)| health.0 <= 0.0)
        .map(|(entity, _)| entity)
        .collect();
    for entity in dead {
        world.insert(entity, Dead)?;
    }
    Ok(())
}

/// Runs the simulation and returns the state hash of every tick.
///
/// # Errors
///
/// [`SimError::Config`] for zero ticks or workers, [`SimError::Pool`] if the pool cannot be
/// created, [`SimError::Ecs`] if a frame fails.
pub fn run(config: &SimConfig) -> Result<SimReport, SimError> {
    if config.ticks == 0 {
        return Err(SimError::Config("ticks must be positive"));
    }
    if config.workers == 0 {
        return Err(SimError::Config("workers must be positive"));
    }
    let pool = ThreadPool::new(PoolConfig::default().with_workers(config.workers))?;
    let mut schedule = schedule()?;
    let mut world = populate(config)?;
    let mut dying = Query::<(Entity, &Health), Without<Dead>>::new()?;
    let ticks = usize::try_from(config.ticks).map_err(|_| SimError::Config("too many ticks"))?;
    let mut hashes = Vec::new();
    hashes
        .try_reserve_exact(ticks)
        .map_err(|_| SimError::Config("too many ticks"))?;

    let start = Instant::now();
    for _ in 0..config.ticks {
        schedule.run(&mut world, &pool)?;
        hashes.push(world.resource::<StateHash>().map_or(0, |h| h.0));
        bury(&mut world, &mut dying)?;
    }
    let elapsed = start.elapsed();
    let dead = Query::<Entity, With<Dead>>::new()?.iter(&world)?.count();
    Ok(SimReport {
        hashes,
        dead,
        elapsed,
    })
}

/// The result of comparing runs on several worker counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every run gave the same hashes.
    Same,
    /// The run on `workers` workers first differed from the first run at tick `tick`.
    Differs {
        /// The worker count of the differing run.
        workers: usize,
        /// The first differing tick (zero-based).
        tick: usize,
    },
}

/// Runs `config` on every worker count of `workers` and compares the hashes of all ticks.
///
/// # Errors
///
/// The first error of a run.
pub fn compare(config: &SimConfig, workers: &[usize]) -> Result<Verdict, SimError> {
    let mut reference: Option<Vec<u64>> = None;
    for &count in workers {
        let report = run(&SimConfig {
            workers: count,
            ..config.clone()
        })?;
        match &reference {
            None => reference = Some(report.hashes),
            Some(expected) => {
                if let Some(tick) = expected
                    .iter()
                    .zip(&report.hashes)
                    .position(|(a, b)| a != b)
                {
                    return Ok(Verdict::Differs {
                        workers: count,
                        tick,
                    });
                }
            }
        }
    }
    Ok(Verdict::Same)
}
