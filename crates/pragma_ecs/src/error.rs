//! The crate's error type.

/// An error of an ECS operation.
///
/// New variants are added as the API grows (`#[non_exhaustive]`), so match with a `_` arm.
///
/// ```
/// use pragma_ecs::EcsError;
///
/// fn describe(err: &EcsError) -> String {
///     match err {
///         EcsError::OutOfMemory { entities, .. } => format!("storage full at {entities}"),
///         _ => err.to_string(),
///     }
/// }
/// # let _ = describe;
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EcsError {
    /// An allocation whose size the input sets failed (`try_reserve`).
    #[error("out of memory while growing storage to {entities} entities")]
    #[non_exhaustive]
    OutOfMemory {
        /// The entity count the storage tried to grow to.
        entities: usize,
    },
}
