//! An Entity Component System for game engines and frameworks built on
//! [`pragma_core`](https://docs.rs/pragma_core).
//!
//! The crate is at milestone M1: entities and component storage. A [`World`] holds entities
//! ([`Entity`], generational ids) and their components (any `Send + Sync + 'static` type, see
//! [`Component`]) in archetype tables. Queries and systems come in later milestones; see
//! `docs/roadmap.md` and `docs/architecture.md`.
//!
//! ```
//! use pragma_ecs::World;
//!
//! struct Position(f32, f32);
//! struct Velocity(f32, f32);
//!
//! let mut world = World::new();
//! let ball = world.spawn()?;
//! world.insert(ball, Position(0.0, 0.0))?;
//! world.insert(ball, Velocity(1.0, 2.0))?;
//!
//! let &Velocity(dx, dy) = world.get::<Velocity>(ball).ok_or("no velocity")?;
//! if let Some(position) = world.get_mut::<Position>(ball) {
//!     position.0 += dx;
//!     position.1 += dy;
//! }
//! assert_eq!(world.get::<Position>(ball).map(|p| (p.0, p.1)), Some((1.0, 2.0)));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Every operation that grows storage reserves memory with `try_reserve` and reports a failure
//! as [`EcsError::OutOfMemory`] instead of aborting; the library contains no `unsafe` code.
#![forbid(unsafe_code)]

mod archetype;
mod component;
mod entity;
mod error;
mod world;

pub use component::Component;
pub use entity::Entity;
pub use error::EcsError;
pub use world::World;
