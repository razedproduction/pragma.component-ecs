//! Resources: singletons stored in the world (DECISIONS #20).

use std::fmt;
use std::ops::{Deref, DerefMut};

use pragma_core::graph;

/// A singleton stored in the [`World`](crate::World), one value per type.
///
/// Implemented for every `Send + Sync + 'static` type, like [`Component`](crate::Component). A
/// type may be a component and a resource at once; the two are separate data.
///
/// ```
/// use pragma_ecs::{Resource, World};
///
/// struct Gravity(f32);
///
/// fn assert_resource<T: Resource>() {}
/// assert_resource::<Gravity>();
///
/// let mut world = World::new();
/// world.insert_resource(Gravity(9.8));
/// assert_eq!(world.resource::<Gravity>().map(|g| g.0), Some(9.8));
/// ```
pub trait Resource: Send + Sync + 'static {}

impl<T: Send + Sync + 'static> Resource for T {}

/// A resource borrowed for reading: [`World::resource`](crate::World::resource), or a system
/// parameter that reads resource `T`.
///
/// ```
/// use pragma_core::prelude::{PoolConfig, ThreadPool};
/// use pragma_ecs::{Res, ResMut, Schedule, World};
///
/// struct Gravity(f32);
/// struct Speed(f32);
///
/// fn fall(gravity: Res<Gravity>, mut speed: ResMut<Speed>) {
///     speed.0 += gravity.0;
/// }
///
/// let mut world = World::new();
/// world.insert_resource(Gravity(2.0));
/// world.insert_resource(Speed(0.0));
/// let mut builder = Schedule::builder();
/// builder.add_system("fall", fall)?;
/// let mut schedule = builder.build()?;
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(1))?;
/// schedule.run(&mut world, &pool)?;
/// schedule.run(&mut world, &pool)?;
/// assert_eq!(world.resource::<Speed>().map(|s| s.0), Some(4.0));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Res<'w, T: Resource>(pub(crate) graph::Res<'w, T>);

impl<T: Resource> Deref for Res<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Resource + fmt::Debug> fmt::Debug for Res<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// A resource borrowed for writing: a system parameter that writes resource `T`. See the
/// [`Res`] example.
///
/// ```
/// use pragma_ecs::{ResMut, Schedule};
///
/// struct Frame(u64);
///
/// fn count(mut frame: ResMut<Frame>) {
///     frame.0 += 1;
/// }
///
/// let mut builder = Schedule::builder();
/// builder.add_system("count", count)?;
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct ResMut<'w, T: Resource>(pub(crate) graph::ResMut<'w, T>);

impl<T: Resource> Deref for ResMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Resource> DerefMut for ResMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: Resource + fmt::Debug> fmt::Debug for ResMut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}
