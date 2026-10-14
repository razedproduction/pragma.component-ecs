//! Declared component access of a query (DECISIONS #14).

use std::any::TypeId;
use std::fmt;

use crate::{Component, EcsError};

/// How a query uses a component type.
///
/// ```
/// use pragma_ecs::{AccessKind, Query};
///
/// struct Position(f32);
/// struct Velocity(f32);
///
/// let query = Query::<(&mut Position, &Velocity)>::new()?;
/// assert_eq!(query.access().kind_of::<Position>(), Some(AccessKind::Write));
/// assert_eq!(query.access().kind_of::<Velocity>(), Some(AccessKind::Read));
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccessKind {
    /// Shared access: `&T`, `Option<&T>`.
    Read,
    /// Exclusive access: `&mut T`.
    Write,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Entry {
    ty: TypeId,
    name: &'static str,
    kind: AccessKind,
}

/// The component types a query reads and writes.
///
/// Built by [`Query::new`](crate::Query::new), which rejects a type that is written and also
/// accessed elsewhere in the same query ([`EcsError::ConflictingAccess`]). Two accesses that do
/// not [`conflict`](Self::conflict) may run at the same time; systems (roadmap M3) declare
/// theirs to the frame graph from it.
///
/// ```
/// use pragma_ecs::{Query, With};
///
/// struct Position(f32);
/// struct Velocity(f32);
/// struct Player;
///
/// let movement = Query::<(&mut Position, &Velocity)>::new()?;
/// let physics = Query::<&mut Velocity>::new()?;
/// let players = Query::<&Position, With<Player>>::new()?;
///
/// assert_eq!(movement.access().len(), 2);
/// assert!(players.access().kind_of::<Player>().is_none()); // filters read no data
/// assert_eq!(movement.access().conflict(physics.access()), Some(std::any::type_name::<Velocity>()));
/// assert_eq!(physics.access().conflict(players.access()), None);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Access {
    /// Sorted by type id, one entry per type.
    entries: Vec<Entry>,
}

impl fmt::Debug for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.entries.iter().map(|e| (e.name, e.kind)))
            .finish()
    }
}

impl Access {
    /// An empty access: no component is used. Does not allocate.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Number of distinct component types used.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no component type is used.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How the component type with id `ty` is used; `None` if it is not.
    #[must_use]
    pub fn kind(&self, ty: TypeId) -> Option<AccessKind> {
        let i = self.entries.binary_search_by(|e| e.ty.cmp(&ty)).ok()?;
        self.entries.get(i).map(|e| e.kind)
    }

    /// How the component type `T` is used; `None` if it is not.
    #[must_use]
    pub fn kind_of<T: Component>(&self) -> Option<AccessKind> {
        self.kind(TypeId::of::<T>())
    }

    /// Every used component type with its access, in an unspecified but fixed order.
    pub fn iter(&self) -> impl Iterator<Item = (TypeId, AccessKind)> + '_ {
        self.entries.iter().map(|e| (e.ty, e.kind))
    }

    /// The name of the first component type that one of the two accesses writes and the other
    /// uses; `None` if they may run at the same time.
    #[must_use]
    pub fn conflict(&self, other: &Self) -> Option<&'static str> {
        self.entries.iter().find_map(|e| {
            let theirs = other.kind(e.ty)?;
            (e.kind == AccessKind::Write || theirs == AccessKind::Write).then_some(e.name)
        })
    }

    /// Records that `ty` (named `name`) is used with `kind`.
    ///
    /// Repeated reads merge; a write together with any other use of the same type is
    /// [`EcsError::ConflictingAccess`]. Grows through `try_reserve`.
    pub(crate) fn add(
        &mut self,
        ty: TypeId,
        name: &'static str,
        kind: AccessKind,
    ) -> Result<(), EcsError> {
        match self.entries.binary_search_by(|e| e.ty.cmp(&ty)) {
            Ok(i) => match self.entries.get(i) {
                Some(e) if e.kind == AccessKind::Read && kind == AccessKind::Read => Ok(()),
                _ => Err(EcsError::ConflictingAccess { component: name }),
            },
            Err(i) => {
                self.entries
                    .try_reserve(1)
                    .map_err(|_| EcsError::OutOfMemory { entities: 0 })?;
                self.entries.insert(i, Entry { ty, name, kind });
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add<T: 'static>(access: &mut Access, kind: AccessKind) -> Result<(), EcsError> {
        access.add(TypeId::of::<T>(), std::any::type_name::<T>(), kind)
    }

    #[test]
    fn reads_merge_and_writes_conflict() {
        let mut access = Access::new();
        add::<u8>(&mut access, AccessKind::Read).unwrap();
        add::<u8>(&mut access, AccessKind::Read).unwrap();
        add::<u16>(&mut access, AccessKind::Write).unwrap();
        assert_eq!(access.len(), 2);
        assert_eq!(
            add::<u8>(&mut access, AccessKind::Write),
            Err(EcsError::ConflictingAccess { component: "u8" })
        );
        assert_eq!(
            add::<u16>(&mut access, AccessKind::Read),
            Err(EcsError::ConflictingAccess { component: "u16" })
        );
        assert_eq!(access.kind_of::<u8>(), Some(AccessKind::Read));
        assert_eq!(access.kind_of::<u32>(), None);
        assert_eq!(
            format!("{access:?}").len(),
            "{\"u8\": Read, \"u16\": Write}".len()
        );
    }

    #[test]
    fn conflict_needs_a_write_on_either_side() {
        let mut reads = Access::new();
        add::<u8>(&mut reads, AccessKind::Read).unwrap();
        let mut other_reads = Access::new();
        add::<u8>(&mut other_reads, AccessKind::Read).unwrap();
        let mut writes = Access::new();
        add::<u8>(&mut writes, AccessKind::Write).unwrap();

        assert_eq!(reads.conflict(&other_reads), None);
        assert_eq!(reads.conflict(&writes), Some("u8"));
        assert_eq!(writes.conflict(&reads), Some("u8"));
        assert_eq!(writes.conflict(&Access::new()), None);
    }
}
