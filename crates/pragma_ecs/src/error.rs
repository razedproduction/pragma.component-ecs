//! The crate's error type.

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
        /// The entity count (or table row count) the storage tried to grow to.
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
}
