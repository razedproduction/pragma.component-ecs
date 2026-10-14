//! The crate's error type.

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
        /// count for a query's cache, 0 when no world is involved (building a query's access).
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

    /// A task of a parallel query failed: the closure panicked (the panic was caught) or the
    /// pool cancelled the task. Rows of the other chunks were still processed.
    #[error("parallel query iteration failed: {source}")]
    #[non_exhaustive]
    Job {
        /// The error reported by the `pragma_core` thread pool.
        #[from]
        source: JobError,
    },
}
