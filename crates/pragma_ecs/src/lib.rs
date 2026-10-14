//! An Entity Component System for game engines and frameworks built on
//! [`pragma_core`](https://docs.rs/pragma_core).
//!
//! The crate is at milestone M3: entities, component storage, queries and systems. A [`World`]
//! holds entities ([`Entity`], generational ids), their components (any `Send + Sync + 'static`
//! type, see [`Component`]) in archetype tables, and resources (singletons, see [`Resource`]).
//! A [`Query`] iterates the entities that have the components it asks for, sequentially or in
//! parallel on the `pragma_core` thread pool, and declares which component types it reads and
//! writes ([`Access`]). Systems are plain functions of [`View`], [`Res`] and [`ResMut`]
//! parameters; a [`Schedule`] runs them as tasks of the `pragma_core` frame graph, in parallel
//! where their accesses allow, and rejects conflicting systems without an explicit order when it
//! is built. See `docs/roadmap.md` and `docs/architecture.md`.
//!
//! ```
//! use pragma_core::prelude::{PoolConfig, ThreadPool};
//! use pragma_ecs::{EcsError, Res, Schedule, View, World};
//!
//! struct Position(f32, f32);
//! struct Velocity(f32, f32);
//! struct Dt(f32);
//!
//! fn movement(mut bodies: View<(&mut Position, &Velocity)>, dt: Res<Dt>) -> Result<(), EcsError> {
//!     for (position, velocity) in bodies.iter_mut()? {
//!         position.0 += velocity.0 * dt.0;
//!         position.1 += velocity.1 * dt.0;
//!     }
//!     Ok(())
//! }
//!
//! let mut world = World::new();
//! let ball = world.spawn()?;
//! world.insert(ball, Position(0.0, 0.0))?;
//! world.insert(ball, Velocity(1.0, 2.0))?;
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
#![forbid(unsafe_code)]

mod access;
mod archetype;
mod component;
mod entity;
mod error;
mod fetch;
mod lend;
mod query;
mod resource;
mod schedule;
mod system;
mod world;

pub use access::{Access, AccessKind};
pub use component::Component;
pub use entity::Entity;
pub use error::EcsError;
pub use fetch::{QueryData, QueryFilter, ReadOnlyQueryData, With, Without};
pub use query::{Query, QueryIter, QueryIterMut};
pub use resource::{Res, ResMut, Resource};
pub use schedule::{Schedule, ScheduleBuilder, SystemConfig};
pub use system::{IntoSystem, SystemOutput, SystemParam, View, ViewIter};
pub use world::World;
