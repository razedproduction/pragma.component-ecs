//! An Entity Component System for game engines and frameworks built on
//! [`pragma_core`](https://docs.rs/pragma_core).
//!
//! The crate is at milestone M2: entities, component storage and queries. A [`World`] holds
//! entities ([`Entity`], generational ids) and their components (any `Send + Sync + 'static`
//! type, see [`Component`]) in archetype tables. A [`Query`] iterates the entities that have the
//! components it asks for, sequentially or in parallel on the `pragma_core` thread pool, and
//! declares which component types it reads and writes ([`Access`]). Systems come next; see
//! `docs/roadmap.md` and `docs/architecture.md`.
//!
//! ```
//! use pragma_ecs::{Query, World};
//!
//! struct Position(f32, f32);
//! struct Velocity(f32, f32);
//!
//! let mut world = World::new();
//! let ball = world.spawn()?;
//! world.insert(ball, Position(0.0, 0.0))?;
//! world.insert(ball, Velocity(1.0, 2.0))?;
//!
//! let mut movement = Query::<(&mut Position, &Velocity)>::new()?;
//! for (position, velocity) in movement.iter_mut(&mut world)? {
//!     position.0 += velocity.0;
//!     position.1 += velocity.1;
//! }
//! assert_eq!(world.get::<Position>(ball).map(|p| (p.0, p.1)), Some((1.0, 2.0)));
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
mod query;
mod world;

pub use access::{Access, AccessKind};
pub use component::Component;
pub use entity::Entity;
pub use error::EcsError;
pub use fetch::{QueryData, QueryFilter, ReadOnlyQueryData, With, Without};
pub use query::{Query, QueryIter, QueryIterMut};
pub use world::World;
