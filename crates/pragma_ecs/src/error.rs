//! The crate's error type.

use pragma_core::graph::GraphError;
use pragma_core::jobs::JobError;

use crate::Entity;

/// An error of an ECS operation.
///
/// New variants are added as the API grows (`#[non_exhaustive]`), so match with a `_` arm.
///
/// ```
/// use pragma_ecs::{EcsError, World};
///
/// let mut world = World::new();
/// let entity = world.spawn()?;
/// world.despawn(entity)?;
///
/// match world.insert(entity, 1.0_f32) {
///     Err(EcsError::NoSuchEntity { entity: gone, .. }) => assert_eq!(gone, entity),
///     other => panic!("unexpected: {other:?}"),
/// }
/// # Ok::<(), EcsError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EcsError {
    /// An allocation whose size the input sets failed (`try_reserve`). The world is unchanged.
    #[error("out of memory while growing storage to {entities} entities")]
    #[non_exhaustive]
    OutOfMemory {
        /// The entity count (or table row count) the storage tried to grow to; the world's entity
        /// count for a query's cache or a schedule's lending slots, 0 when no world is involved
        /// (building a query's access, adding a system).
        entities: usize,
    },

    /// The entity was despawned (its id is stale) or was never spawned by this world.
    #[error("entity {entity} does not exist (despawned or never spawned)")]
    #[non_exhaustive]
    NoSuchEntity {
        /// The id that was passed in.
        entity: Entity,
    },

    /// The world has used up its entity index space (`u32::MAX` slots).
    #[error("entity index space exhausted")]
    CapacityExhausted,

    /// A query writes a component type that it also reads or writes elsewhere, e.g.
    /// `(&A, &mut A)`: the two references would alias (DECISIONS #14).
    ///
    /// ```
    /// use pragma_ecs::{EcsError, Query};
    ///
    /// struct Health(u32);
    ///
    /// let err = Query::<(&Health, &mut Health)>::new().err();
    /// assert!(matches!(
    ///     err,
    ///     Some(EcsError::ConflictingAccess { component, .. }) if component.ends_with("Health")
    /// ));
    /// ```
    #[error(
        "conflicting access to component `{component}`: it is written and also accessed elsewhere"
    )]
    #[non_exhaustive]
    ConflictingAccess {
        /// The type name of the component (`std::any::type_name`).
        component: &'static str,
    },

    /// A [`Bundle`](crate::Bundle) holds the same component type twice, e.g. `(A, A)`: an
    /// entity has at most one component of each type (DECISIONS #35). Nothing was changed.
    ///
    /// ```
    /// use pragma_ecs::{EcsError, World};
    ///
    /// struct Speed(f32);
    ///
    /// let mut world = World::new();
    /// let err = world.spawn_bundle((Speed(1.0), Speed(2.0))).err();
    /// assert!(matches!(
    ///     err,
    ///     Some(EcsError::DuplicateComponent { component, .. }) if component.ends_with("Speed")
    /// ));
    /// assert!(world.is_empty());
    /// ```
    #[error("a bundle holds the component type `{component}` more than once")]
    #[non_exhaustive]
    DuplicateComponent {
        /// The type name of the component (`std::any::type_name`).
        component: &'static str,
    },

    /// A system uses a resource type twice and at least one use writes it, e.g.
    /// `(Res<Score>, ResMut<Score>)`: the two borrows would alias (DECISIONS #17).
    ///
    /// ```
    /// use pragma_ecs::{EcsError, Res, ResMut, Schedule};
    ///
    /// struct Score(u32);
    ///
    /// fn cheat(_: Res<Score>, _: ResMut<Score>) {}
    ///
    /// let err = Schedule::builder().add_system("cheat", cheat).err();
    /// assert!(matches!(
    ///     err,
    ///     Some(EcsError::ConflictingResourceAccess { resource, .. }) if resource.ends_with("Score")
    /// ));
    /// ```
    #[error(
        "conflicting access to resource `{resource}`: it is written and also accessed elsewhere"
    )]
    #[non_exhaustive]
    ConflictingResourceAccess {
        /// The type name of the resource (`std::any::type_name`).
        resource: &'static str,
    },

    /// A pool task failed: the closure of a parallel query panicked (the panic was caught) or
    /// the pool cancelled the task. Rows of the other chunks were still processed.
    #[error("parallel query iteration failed: {source}")]
    #[non_exhaustive]
    Job {
        /// The error reported by the `pragma_core` thread pool.
        #[from]
        source: JobError,
    },

    /// Two systems use the same data, at least one of them writes it, and neither is ordered
    /// before the other (DECISIONS #19). Returned by
    /// [`ScheduleBuilder::build`](crate::ScheduleBuilder::build).
    ///
    /// ```
    /// use pragma_ecs::{EcsError, Schedule, View};
    ///
    /// struct Position(f32);
    ///
    /// fn movement(_: View<&mut Position>) {}
    /// fn render(_: View<&Position>) {}
    ///
    /// let mut builder = Schedule::builder();
    /// builder.add_system("movement", movement)?.add_system("render", render)?;
    /// match builder.build() {
    ///     Err(EcsError::AmbiguousOrder { first, second, data, .. }) => {
    ///         assert_eq!((first.as_str(), second.as_str()), ("movement", "render"));
    ///         assert!(data.ends_with("Position"));
    ///     }
    ///     other => panic!("unexpected {other:?}"),
    /// }
    /// # Ok::<(), EcsError>(())
    /// ```
    #[error(
        "systems `{first}` and `{second}` conflict on `{data}` without an explicit order \
         (add `after`/`before`, or build an `ordered()` schedule)"
    )]
    #[non_exhaustive]
    AmbiguousOrder {
        /// The system registered first.
        first: String,
        /// The system registered second.
        second: String,
        /// The type name of the component or resource both use.
        data: String,
    },

    /// A system takes a resource the world does not have. Checked before any system of the
    /// frame runs, so the frame did not start.
    ///
    /// ```
    /// use pragma_core::prelude::{PoolConfig, ThreadPool};
    /// use pragma_ecs::{EcsError, Res, Schedule, World};
    ///
    /// struct Gravity(f32);
    ///
    /// fn fall(_: Res<Gravity>) {}
    ///
    /// let mut builder = Schedule::builder();
    /// builder.add_system("fall", fall)?;
    /// let mut schedule = builder.build()?;
    /// let pool = ThreadPool::new(PoolConfig::default().with_workers(1))?;
    /// let err = schedule.run(&mut World::new(), &pool).err();
    /// assert!(matches!(
    ///     err,
    ///     Some(EcsError::MissingResource { system, resource, .. })
    ///         if system == "fall" && resource.ends_with("Gravity")
    /// ));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[error("system `{system}` needs resource `{resource}`, which the world does not have")]
    #[non_exhaustive]
    MissingResource {
        /// The first system that takes the resource.
        system: String,
        /// The type name of the resource.
        resource: String,
    },

    /// A system returned an error (see [`SystemOutput`](crate::SystemOutput)). Independent
    /// systems still ran; the systems ordered after it did not run in this frame.
    #[error("system `{system}` failed: {message}")]
    #[non_exhaustive]
    SystemFailed {
        /// The system's name.
        system: String,
        /// The message of the error the system returned (its `Display`).
        message: String,
        /// The error itself, if it was an `EcsError`.
        source: Option<Box<EcsError>>,
    },

    /// A system panicked; the panic was caught. Independent systems still ran; the systems
    /// ordered after it did not run in this frame.
    #[error("system `{system}` panicked: {message}")]
    #[non_exhaustive]
    SystemPanicked {
        /// The system's name.
        system: String,
        /// The panic message, if it was a string.
        message: String,
    },

    /// Applying the commands or events a system deferred to the sync point failed, e.g. out of
    /// memory while spawning (DECISIONS #25). The commands applied before stay applied; the
    /// failing one and everything deferred after it, by this and later systems, were discarded.
    /// A command on an entity that is gone is not an error: it does nothing.
    ///
    /// ```
    /// use pragma_core::prelude::ThreadPool;
    /// use pragma_ecs::{EcsError, Schedule, World};
    ///
    /// fn frame(schedule: &mut Schedule, world: &mut World, pool: &ThreadPool) -> Result<(), EcsError> {
    ///     match schedule.run(world, pool) {
    ///         Err(EcsError::CommandFailed { system, source, .. }) => {
    ///             // The world is consistent: report, free memory, run the next frame.
    ///             eprintln!("commands of `{system}` were not applied: {source}");
    ///             Ok(())
    ///         }
    ///         other => other,
    ///     }
    /// }
    /// # let _ = frame;
    /// ```
    #[error("applying the commands of system `{system}` failed: {source}")]
    #[non_exhaustive]
    CommandFailed {
        /// The system whose commands or events could not be applied.
        system: String,
        /// Why: [`EcsError::OutOfMemory`] or [`EcsError::CapacityExhausted`].
        source: Box<EcsError>,
    },

    /// Any other error of the `pragma_core` frame graph a schedule is built on: a dependency
    /// cycle, an `after`/`before` naming an unknown system, a name used twice, a schedule too
    /// large (DECISIONS #21).
    ///
    /// ```
    /// use pragma_ecs::{EcsError, Schedule};
    ///
    /// fn tick() {}
    ///
    /// let mut builder = Schedule::builder();
    /// builder.add_system_with("tick", tick, |c| c.after("input"))?;
    /// assert!(matches!(builder.build(), Err(EcsError::Schedule { .. })));
    /// # Ok::<(), EcsError>(())
    /// ```
    #[error("schedule error: {source}")]
    #[non_exhaustive]
    Schedule {
        /// The error reported by the frame graph.
        source: GraphError,
    },
}

impl EcsError {
    /// Wraps a frame-graph error that has no variant of its own.
    pub(crate) const fn graph(source: GraphError) -> Self {
        Self::Schedule { source }
    }
}
