//! Entity ids and their allocator (DECISIONS #10).

use std::fmt;

use crate::EcsError;

/// A generational entity id: (index, generation).
///
/// Despawning an entity bumps its slot's generation, so an old `Entity` never addresses a new
/// entity that reuses the slot: the [`World`](crate::World) reports it as gone instead.
///
/// An `Entity` is `Copy` and does not borrow the world: store it in components, send it between
/// threads, or round-trip it through [`to_bits`](Self::to_bits) / [`from_bits`](Self::from_bits)
/// for serialization. The world decides on every access whether it is still alive.
///
/// ```
/// use pragma_ecs::World;
///
/// let mut world = World::new();
/// let old = world.spawn()?;
/// world.despawn(old)?;
/// let new = world.spawn()?; // reuses the slot
///
/// assert_eq!(old.index(), new.index());
/// assert_ne!(old.generation(), new.generation());
/// assert!(!world.contains(old));
/// assert!(world.contains(new));
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Entity {
    index: u32,
    generation: u32,
}

impl Entity {
    /// Slot index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// Slot generation at the time the entity was spawned.
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation
    }

    /// The id packed into 64 bits: the generation in the high half, the index in the low half.
    ///
    /// ```
    /// use pragma_ecs::{Entity, World};
    ///
    /// let mut world = World::new();
    /// let entity = world.spawn()?;
    /// assert_eq!(Entity::from_bits(entity.to_bits()), entity);
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        (self.generation as u64).wrapping_shl(32) | self.index as u64
    }

    /// The inverse of [`to_bits`](Self::to_bits). Every `u64` is a well-formed id; whether it
    /// names a live entity is up to the world it is used with.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self {
            // Truncation is the point: the low and the high half.
            index: bits as u32,
            generation: bits.wrapping_shr(32) as u32,
        }
    }
}

impl fmt::Debug for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Entity({self})")
    }
}

impl fmt::Display for Entity {
    /// `<index>v<generation>`, e.g. `3v1`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}v{}", self.index, self.generation)
    }
}

/// Where a live entity's components are: an archetype and a row in its table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Location {
    pub(crate) archetype: usize,
    pub(crate) row: usize,
}

#[derive(Debug)]
struct Meta {
    generation: u32,
    /// `None` for a free (or retired) slot.
    location: Option<Location>,
}

/// The slot table of entity ids, modeled on `pragma_core`'s `HandlePool`: freed slots are reused
/// with a bumped generation, a slot whose generation is exhausted is retired forever.
#[derive(Debug, Default)]
pub(crate) struct Entities {
    meta: Vec<Meta>,
    free: Vec<u32>,
    len: usize,
}

impl Entities {
    /// Number of live entities.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Issues an id for a new entity placed at `location`.
    ///
    /// Grows via `try_reserve`; room in the free list is reserved together with the slot, so
    /// [`free`](Self::free) never allocates.
    pub(crate) fn alloc(&mut self, location: Location) -> Result<Entity, EcsError> {
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                let slots = self.meta.len();
                let index = u32::try_from(slots).map_err(|_| EcsError::CapacityExhausted)?;
                let oom = |_| EcsError::OutOfMemory {
                    entities: slots.saturating_add(1),
                };
                self.meta.try_reserve(1).map_err(oom)?;
                // `free.len() ≤ slots`, so the subtraction cannot overflow.
                let free_room = slots.saturating_add(1).saturating_sub(self.free.len());
                self.free.try_reserve(free_room).map_err(oom)?;
                self.meta.push(Meta {
                    generation: 0,
                    location: None,
                });
                index
            }
        };
        let meta = self.meta_mut(index).ok_or(EcsError::CapacityExhausted)?;
        meta.location = Some(location);
        let generation = meta.generation;
        self.len = self.len.saturating_add(1);
        Ok(Entity { index, generation })
    }

    /// The location of a live entity, `None` if it is stale.
    pub(crate) fn location(&self, entity: Entity) -> Option<Location> {
        self.meta
            .get(usize::try_from(entity.index).ok()?)
            .filter(|m| m.generation == entity.generation)
            .and_then(|m| m.location)
    }

    /// Moves a live entity to a new location; `false` if it is stale.
    pub(crate) fn relocate(&mut self, entity: Entity, location: Location) -> bool {
        match self
            .meta_mut(entity.index)
            .filter(|m| m.generation == entity.generation)
            .and_then(|m| m.location.as_mut())
        {
            Some(slot) => {
                *slot = location;
                true
            }
            None => false,
        }
    }

    /// Frees a live entity's id and returns its last location; `None` if it is stale.
    pub(crate) fn free(&mut self, entity: Entity) -> Option<Location> {
        let meta = self
            .meta_mut(entity.index)
            .filter(|m| m.generation == entity.generation)?;
        let location = meta.location.take()?;
        match meta.generation.checked_add(1) {
            Some(next) => {
                meta.generation = next;
                self.free.push(entity.index);
            }
            // Retired: a generation never repeats.
            None => meta.generation = u32::MAX,
        }
        self.len = self.len.saturating_sub(1);
        Some(location)
    }

    fn meta_mut(&mut self, index: u32) -> Option<&mut Meta> {
        self.meta.get_mut(usize::try_from(index).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: Location = Location {
        archetype: 0,
        row: 0,
    };

    #[test]
    fn bits_round_trip() {
        let e = Entity {
            index: 7,
            generation: u32::MAX,
        };
        assert_eq!(e.to_bits(), 0xFFFF_FFFF_0000_0007);
        assert_eq!(Entity::from_bits(e.to_bits()), e);
        assert_eq!(format!("{e:?}"), "Entity(7v4294967295)");
    }

    #[test]
    fn reuse_bumps_generation() {
        let mut entities = Entities::default();
        let a = entities.alloc(AT).unwrap();
        assert_eq!(entities.free(a), Some(AT));
        let b = entities.alloc(AT).unwrap();
        assert_eq!((a.index(), a.generation()), (b.index(), 0));
        assert_eq!(b.generation(), 1);
        assert_eq!(entities.location(a), None);
        assert_eq!(entities.free(a), None);
        assert!(!entities.relocate(a, AT));
        assert_eq!(entities.len(), 1);
    }

    #[test]
    fn exhausted_generation_retires_the_slot() {
        let mut entities = Entities::default();
        let a = entities.alloc(AT).unwrap();
        entities.meta[0].generation = u32::MAX;
        let last = Entity {
            index: a.index(),
            generation: u32::MAX,
        };
        assert_eq!(entities.free(last), Some(AT));
        // The slot is not reused and the last id stays dead.
        let b = entities.alloc(AT).unwrap();
        assert_ne!(b.index(), a.index());
        assert_eq!(entities.location(last), None);
        assert_eq!(entities.free(last), None);
        assert_eq!(entities.len(), 1);
    }
}
