//! # pragma_ecs
//!
//! An Entity Component System in pure Rust for game engines and frameworks built on
//! [`pragma_core`](https://docs.rs/pragma_core). It owns no threads: systems run as tasks of the
//! `pragma_core` frame graph on the engine's thread pool, and conflicting data access between
//! systems is a build error of the schedule, not a data race in a frame.
//!
//! | Concept | Items |
//! |---|---|
//! | Entities and components | [`World`], [`Entity`] (generational ids), [`Component`] (any `Send + Sync + 'static` type), [`Bundle`] (a tuple of components spawned or inserted at once) |
//! | Resources (singletons) | [`World::insert_resource`], [`Resource`], [`Res`], [`ResMut`] |
//! | Queries | [`Query`] over [`QueryData`] (`&T`, `&mut T` as [`Mut<T>`], `Option<&T>`, [`Entity`], tuples) and [`QueryFilter`] ([`With`], [`Without`], [`Added`], [`Changed`]); declared [`Access`] |
//! | Systems | plain functions of [`View`], [`Res`], [`ResMut`], [`Commands`], [`EventWriter`], [`EventReader`]; a [`Schedule`] built by a [`ScheduleBuilder`] |
//! | Deferred changes | [`Commands`], [`EntityCommands`], [`Events`], [`EventCursor`], applied at the sync point at the end of [`Schedule::run`] |
//! | Errors | [`EcsError`] (`#[non_exhaustive]`) |
//!
//! Components live in archetype tables: entities with the same component set share a table, one
//! column per type, so a query iterates dense slices. A [`Schedule`] runs systems with disjoint
//! access in parallel, orders conflicting ones as told (or fails to build), and applies the
//! commands and events systems deferred at the end of every frame in a fixed order, so the world
//! after a frame is the same on any number of workers. The design is in
//! [`docs/architecture.md`](https://github.com/razedproduction/pragma.component-ecs/blob/main/docs/architecture.md),
//! the reasons in
//! [`DECISIONS.md`](https://github.com/razedproduction/pragma.component-ecs/blob/main/DECISIONS.md).
//!
//! ```
//! use pragma_ecs::pragma_core::prelude::{PoolConfig, ThreadPool};
//! use pragma_ecs::{EcsError, Res, Schedule, View, World};
//!
//! struct Position(f32, f32);
//! struct Velocity(f32, f32);
//! struct Dt(f32);
//!
//! fn movement(mut bodies: View<(&mut Position, &Velocity)>, dt: Res<Dt>) -> Result<(), EcsError> {
//!     for (mut position, velocity) in bodies.iter_mut()? {
//!         position.0 += velocity.0 * dt.0;
//!         position.1 += velocity.1 * dt.0;
//!     }
//!     Ok(())
//! }
//!
//! let mut world = World::new();
//! let ball = world.spawn_bundle((Position(0.0, 0.0), Velocity(1.0, 2.0)))?;
//! world.insert_resource(Dt(0.5));
//!
//! let mut builder = Schedule::builder();
//! builder.add_system("movement", movement)?;
//! let mut schedule = builder.build()?;
//! let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
//! schedule.run(&mut world, &pool)?;
//! assert_eq!(world.get::<Position>(ball).map(|p| (p.0, p.1)), Some((0.5, 1.0)));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Every operation that grows storage reserves memory with `try_reserve` and reports a failure
//! as [`EcsError::OutOfMemory`] instead of aborting; the library contains no `unsafe` code.
//!
//! ## Semver guarantees
//!
//! The public API follows semver, except for items hidden from the documentation
//! (`#[doc(hidden)]`): the machinery types that the sealed traits ([`QueryData`],
//! [`QueryFilter`], [`SystemParam`], [`SystemOutput`], [`IntoSystem`], [`Bundle`]) name in their
//! associated types and hidden methods, and those methods themselves. They are internal and may
//! change in any release (DECISIONS #36). The sealed traits cannot be implemented outside the
//! crate. `pragma_core` types appear in the API ([`ThreadPool`](pragma_core::jobs::ThreadPool),
//! [`Grain`](pragma_core::jobs::Grain), the sources of [`EcsError::Job`] and
//! [`EcsError::Schedule`]); the crate re-exports the [`pragma_core`] it is built with, and a new
//! major version of `pragma_core` means a new major version of `pragma_ecs`. The minimum
//! supported Rust version (1.88) is raised only in a minor release.
//!
//! ## Integrating into an engine
//!
//! The ECS is a library, not a framework: it has no game loop, no App or plugin system and no
//! global state. The engine owns the pieces and decides when they run. These are the decisions
//! to make, with the defaults the crate is designed and tested for;
//! [`examples/ecs_sim`](https://github.com/razedproduction/pragma.component-ecs/tree/main/examples/ecs_sim)
//! is a complete headless loop built this way.
//!
//! 1. **One pool, owned by the engine.** Create one `pragma_core`
//!    [`ThreadPool`](pragma_core::jobs::ThreadPool) per process (through the re-exported
//!    [`pragma_core`], so the versions match) and pass `&ThreadPool` to
//!    [`Schedule::run`] and to parallel queries. Call `run` from a thread outside the pool, the
//!    main loop: that thread runs systems too while it waits. The rest of pragma.core's advice
//!    (worker count, panic hook, shutdown order) is in its own "Integrating into an engine".
//! 2. **The engine owns the [`World`].** Between frames it changes the world directly: spawning
//!    ([`spawn_bundle`](World::spawn_bundle) puts an entity straight into its final table),
//!    inserting resources, loading a level. During a frame the world belongs to the schedule:
//!    systems see it only through their parameters, which is what lets them run in parallel.
//! 3. **Build schedules once.** [`ScheduleBuilder::build`] validates every system at startup:
//!    two systems that conflict without an order are [`EcsError::AmbiguousOrder`] naming both and
//!    the data. Stages (input, update, late update) are several schedules run one after another
//!    over the same world; a sequential loop of systems ports as one
//!    [`ordered`](ScheduleBuilder::ordered) schedule. Rebuild only when the set of systems changes.
//! 4. **Resources are the boundary.** Feed the frame through resources set before `run` (the
//!    time step, input, handles of loaded assets) and read results after it (resources, or
//!    read-only queries over `&World` for rendering, also in parallel with
//!    [`Query::par_for_each`]). A resource a system needs but the world lacks is
//!    [`EcsError::MissingResource`] before any system runs.
//! 5. **Structural changes and events take effect at the sync point.** [`Commands`] and
//!    [`EventWriter`] record into the system's own buffers; at the end of [`Schedule::run`] the
//!    events are published and the commands applied, system after system in registration order.
//!    A spawned entity gets its id only then, and readers see an event from the next frame on.
//!    Code outside systems sends events with [`World::send_event`].
//! 6. **Errors to react to.** A system that failed or panicked
//!    ([`EcsError::SystemFailed`], [`EcsError::SystemPanicked`]) stops the systems ordered after
//!    it in that frame and loses its deferred commands; [`EcsError::CommandFailed`] means memory
//!    ran out while applying commands. In all three the world and the schedule stay consistent and
//!    usable: report and run the next frame. [`EcsError::OutOfMemory`] from a world operation
//!    leaves the world unchanged.
//! 7. **Determinism.** A frame gives the same world on any number of workers as long as the
//!    systems do: no results that depend on completion order or on wall-clock time, parallel
//!    iteration ([`View::par_for_each_mut`]) writing only its own rows, `Entity::to_bits` rather
//!    than addresses for anything hashed or sent over the network. Check it the way
//!    `ecs_sim --verify` does: the same scenario on 1 and N workers must give the same state
//!    hash.
//! 8. **Shutdown.** Drop the schedules and the world before the pool (struct fields drop in
//!    declaration order); a schedule holds no thread and no reference into the world.
//!
//! The skeleton of such an engine:
//!
//! ```
//! use pragma_ecs::pragma_core::prelude::{PoolConfig, ThreadPool};
//! use pragma_ecs::{Commands, EcsError, Query, Res, ResMut, Schedule, View, World};
//!
//! struct Position(f32, f32);
//! struct Velocity(f32, f32);
//! struct Dt(f32);
//! #[derive(Default)]
//! struct Input {
//!     fire: bool,
//! }
//!
//! fn shoot(input: Res<Input>, mut commands: Commands) -> Result<(), EcsError> {
//!     if input.fire {
//!         commands.spawn_bundle((Position(0.0, 0.0), Velocity(10.0, 0.0)))?;
//!     }
//!     Ok(())
//! }
//!
//! fn movement(mut bodies: View<(&mut Position, &Velocity)>, dt: Res<Dt>) -> Result<(), EcsError> {
//!     for (mut position, velocity) in bodies.iter_mut()? {
//!         position.0 += velocity.0 * dt.0;
//!         position.1 += velocity.1 * dt.0;
//!     }
//!     Ok(())
//! }
//!
//! struct Engine {
//!     update: Schedule,
//!     world: World,
//!     /// Built once: its archetype cache follows the world between frames.
//!     render: Query<&'static Position>,
//!     pool: ThreadPool, // dropped last
//! }
//!
//! impl Engine {
//!     fn new() -> Result<Self, Box<dyn std::error::Error>> {
//!         let mut world = World::new();
//!         world.insert_resource(Dt(1.0 / 60.0));
//!         world.insert_resource(Input::default());
//!         for i in 0..100 {
//!             world.spawn_bundle((Position(i as f32, 0.0), Velocity(0.0, 1.0)))?;
//!         }
//!
//!         let mut builder = Schedule::builder();
//!         builder.add_system("shoot", shoot)?;
//!         builder.add_system("movement", movement)?;
//!         let update = builder.build()?; // ordering mistakes fail here, at startup
//!
//!         let pool = ThreadPool::new(PoolConfig::default())?;
//!         Ok(Self { update, world, render: Query::new()?, pool })
//!     }
//!
//!     /// One frame; returns the number of bodies drawn.
//!     fn frame(&mut self, dt: f32, fire: bool) -> Result<usize, EcsError> {
//!         // 1. Input and time go in as resources.
//!         if let Some(time) = self.world.resource_mut::<Dt>() {
//!             time.0 = dt;
//!         }
//!         if let Some(input) = self.world.resource_mut::<Input>() {
//!             input.fire = fire;
//!         }
//!         // 2. The frame: systems in parallel, then the sync point.
//!         match self.update.run(&mut self.world, &self.pool) {
//!             Ok(()) => {}
//!             Err(
//!                 err @ (EcsError::SystemFailed { .. }
//!                 | EcsError::SystemPanicked { .. }
//!                 | EcsError::CommandFailed { .. }),
//!             ) => eprintln!("frame: {err}"), // the world stays consistent
//!             Err(other) => return Err(other),
//!         }
//!         // 3. Rendering reads the world after the frame.
//!         Ok(self.render.iter(&self.world)?.count())
//!     }
//! }
//!
//! let mut engine = Engine::new()?;
//! assert_eq!(engine.frame(1.0 / 60.0, false)?, 100);
//! // The bullet is spawned at the sync point, so rendering after the frame already sees it.
//! assert_eq!(engine.frame(1.0 / 60.0, true)?, 101);
//! assert_eq!(engine.frame(1.0 / 60.0, false)?, 101);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![forbid(unsafe_code)]

mod access;
mod archetype;
mod bundle;
mod change;
mod command;
mod component;
mod entity;
mod error;
mod event;
mod fetch;
mod hash;
mod lend;
mod query;
mod resource;
mod schedule;
mod system;
mod world;

/// The `pragma_core` this crate is built with: name `ThreadPool`, `Grain` and `PoolConfig`
/// through it to use the same version (DECISIONS #36).
pub use pragma_core;

pub use access::{Access, AccessKind};
pub use bundle::Bundle;
pub use change::Mut;
pub use command::{Commands, EntityCommands};
pub use component::Component;
pub use entity::Entity;
pub use error::EcsError;
pub use event::{Event, EventCursor, EventIter, EventReader, EventWriter, Events};
pub use fetch::{Added, Changed, QueryData, QueryFilter, ReadOnlyQueryData, With, Without};
pub use query::{Query, QueryIter, QueryIterMut};
pub use resource::{Res, ResMut, Resource};
pub use schedule::{Schedule, ScheduleBuilder, SystemConfig};
pub use system::{IntoSystem, SystemOutput, SystemParam, View, ViewIter};
pub use world::World;
