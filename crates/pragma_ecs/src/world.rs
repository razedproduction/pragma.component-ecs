//! The world: entities and their components in archetype tables.

use crate::hash::TypeIdMap;
use std::any::TypeId;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use pragma_core::graph::Resources;

use crate::archetype::{Archetype, two_mut};
use crate::bundle::{Bundle, BundleInfo, BundleRow, new_column};
use crate::component::{Column, ColumnData, Component};
use crate::entity::{Entities, Location};
use crate::{EcsError, Entity, Event, Events, Res, Resource};

/// The archetype of entities without components; it is always the first one created.
const EMPTY: usize = 0;

/// The id of the next world created. 2^64 worlds will not be created in one process.
static NEXT_WORLD_ID: AtomicU64 = AtomicU64::new(0);

/// A container of entities, their components and resources.
///
/// Entities with the same set of component types share an archetype table, one column per type
/// (`docs/architecture.md` §3). Adding or removing a component moves the entity to another table.
/// Resources are singletons, one value per type ([`insert_resource`](Self::insert_resource)).
/// A [`Schedule`](crate::Schedule) runs systems over the world.
///
/// Every operation that grows component storage reserves memory first (`try_reserve`): running
/// out of memory is [`EcsError::OutOfMemory`] and leaves the world unchanged. Despawning and
/// lookups never allocate.
///
/// ```
/// use pragma_ecs::{EcsError, World};
///
/// #[derive(Debug, PartialEq)]
/// struct Health(u32);
/// struct Player;
///
/// let mut world = World::new();
/// let hero = world.spawn()?;
/// world.insert(hero, Health(100))?;
/// world.insert(hero, Player)?;
///
/// if let Some(health) = world.get_mut::<Health>(hero) {
///     health.0 -= 30;
/// }
/// assert_eq!(world.get::<Health>(hero), Some(&Health(70)));
///
/// assert_eq!(world.remove::<Health>(hero)?, Some(Health(70)));
/// assert!(world.get::<Health>(hero).is_none());
///
/// world.despawn(hero)?;
/// assert!(world.is_empty());
/// assert!(matches!(world.despawn(hero), Err(EcsError::NoSuchEntity { .. })));
/// # Ok::<(), EcsError>(())
/// ```
pub struct World {
    /// Unique per world in the process; ties a query's archetype cache to its world.
    id: u64,
    entities: Entities,
    archetypes: Vec<Archetype>,
    /// Archetype lookup by its sorted component type list.
    by_types: TypeIdMap<Vec<TypeId>, usize>,
    /// The resources, and the slots a schedule lends the columns through (DECISIONS #18, #20).
    resources: Resources,
    /// The last change tick taken (DECISIONS #27).
    tick: u64,
    /// The tick of the first system of the running frame.
    frame_base: u64,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for World {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("World")
            .field("entities", &self.len())
            .field("archetypes", &self.archetypes.len())
            .finish_non_exhaustive()
    }
}

impl World {
    /// An empty world. Does not allocate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: NEXT_WORLD_ID.fetch_add(1, Ordering::Relaxed),
            entities: Entities::default(),
            archetypes: Vec::new(),
            by_types: TypeIdMap::default(),
            resources: Resources::new(),
            tick: 0,
            frame_base: 0,
        }
    }

    /// The world's change tick: the tick of the most recent change (DECISIONS #27).
    ///
    /// Every mutating operation (`insert`, `get_mut`, a mutable query, a frame of a
    /// [`Schedule`](crate::Schedule)) takes newer ticks; a component's added and changed ticks
    /// are compared with the tick at which a query or a system last ran to answer
    /// [`Added<T>`](crate::Added) and [`Changed<T>`](crate::Changed).
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// let mut world = World::new();
    /// assert_eq!(world.change_tick(), 0);
    /// let entity = world.spawn()?;
    /// world.insert(entity, 1_u8)?;
    /// let before = world.change_tick();
    /// let _ = world.get_mut::<u8>(entity);
    /// assert!(world.change_tick() > before);
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    #[must_use]
    pub const fn change_tick(&self) -> u64 {
        self.tick
    }

    /// Sends `event` to the [`Events<T>`] resource, creating it if the world has none. Readers
    /// see it from the next frame on (DECISIONS #26).
    ///
    /// The first event of a type reserves its buffer before the resource is created, which then
    /// allocates one box, as [`insert_resource`](Self::insert_resource) does (DECISIONS #20, #40).
    ///
    /// ```
    /// use pragma_ecs::{EventCursor, Events, World};
    ///
    /// struct Explosion(u32);
    ///
    /// let mut world = World::new();
    /// world.send_event(Explosion(3))?;
    /// let mut cursor = EventCursor::<Explosion>::default();
    /// let events = world.resource::<Events<Explosion>>().ok_or("no events")?;
    /// assert_eq!(cursor.read(&events).map(|e| e.0).collect::<Vec<_>>(), [3]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the event buffer cannot grow; the event is dropped and the
    /// world is unchanged (no empty [`Events<T>`] is left behind).
    pub fn send_event<T: Event>(&mut self, event: T) -> Result<(), EcsError> {
        if let Some(events) = self.resources.get_mut::<Events<T>>() {
            return events.send(event);
        }
        let mut events = Events::new();
        events.send(event)?;
        self.resources.insert(events);
        Ok(())
    }

    /// Takes a new change tick for one mutating operation.
    pub(crate) fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.saturating_add(1);
        self.tick
    }

    /// Takes `count` consecutive ticks for the systems of a frame and returns the first one.
    pub(crate) fn begin_frame(&mut self, count: usize) -> u64 {
        let base = self.tick.saturating_add(1);
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        self.tick = base.saturating_add(count);
        self.frame_base = base;
        base
    }

    /// The tick of the first system of the frame running over this world.
    pub(crate) const fn frame_base(&self) -> u64 {
        self.frame_base
    }

    /// Number of live entities.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether the world has no live entities.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `entity` is alive in this world.
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// let mut world = World::new();
    /// let entity = world.spawn()?;
    /// assert!(world.contains(entity));
    /// world.despawn(entity)?;
    /// assert!(!world.contains(entity));
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    #[must_use]
    pub fn contains(&self, entity: Entity) -> bool {
        self.entities.location(entity).is_some()
    }

    /// Creates an entity without components.
    ///
    /// Freed slots are reused with a bumped generation (see [`Entity`]). An entity that gets
    /// several components at once is cheaper to create with [`spawn_bundle`](Self::spawn_bundle).
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// let mut world = World::new();
    /// let a = world.spawn()?;
    /// let b = world.spawn()?;
    /// assert_ne!(a, b);
    /// assert_eq!(world.len(), 2);
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if storage cannot grow, [`EcsError::CapacityExhausted`] if the
    /// entity index space is used up. The world is unchanged in both cases.
    pub fn spawn(&mut self) -> Result<Entity, EcsError> {
        self.ensure_empty_table()?;
        let table = self
            .archetypes
            .get_mut(EMPTY)
            .ok_or(EcsError::CapacityExhausted)?;
        table.reserve_row()?;
        let row = table.entities().len();
        let entity = self.entities.alloc(Location {
            archetype: EMPTY,
            row,
        })?;
        table.push_entity(entity);
        Ok(entity)
    }

    /// Creates an entity with the components of `bundle`, directly in the table of that
    /// component set (DECISIONS #35): one table write instead of one move per
    /// [`insert`](Self::insert). The components count as added and changed.
    ///
    /// ```
    /// use pragma_ecs::{Query, World};
    ///
    /// #[derive(Debug, PartialEq)]
    /// struct Position(f32, f32);
    /// #[derive(Debug, PartialEq)]
    /// struct Velocity(f32, f32);
    ///
    /// let mut world = World::new();
    /// for i in 0..3 {
    ///     world.spawn_bundle((Position(i as f32, 0.0), Velocity(1.0, 0.0)))?;
    /// }
    /// let tagged = world.spawn_bundle(("a name",))?; // a one-component bundle
    /// assert_eq!(world.get::<&str>(tagged), Some(&"a name"));
    ///
    /// let mut moving = Query::<(&Position, &Velocity)>::new()?;
    /// assert_eq!(moving.iter(&world)?.count(), 3);
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::DuplicateComponent`] if the bundle holds a component type twice,
    /// [`EcsError::OutOfMemory`] if storage cannot grow, [`EcsError::CapacityExhausted`] if the
    /// entity index space is used up. The world is unchanged (except for a possibly created
    /// empty table) and `bundle` is dropped.
    pub fn spawn_bundle<B: Bundle>(&mut self, bundle: B) -> Result<Entity, EcsError> {
        self.ensure_empty_table()?;
        let to = self.bundle_target::<B>(EMPTY)?;
        let tick = self.next_tick();
        let table = self
            .archetypes
            .get_mut(to)
            .ok_or(EcsError::CapacityExhausted)?;
        table.reserve_row()?;
        let row = table.len();
        let entity = self.entities.alloc(Location { archetype: to, row })?;
        // Nothing can fail from here on: the room is reserved.
        bundle.write(&mut BundleRow::new(table, row, tick));
        table.push_entity(entity);
        debug_assert!(table.is_aligned(), "the bundle filled every column");
        Ok(entity)
    }

    /// Attaches the components of `bundle` to `entity`, replacing those of the same types it
    /// already has, in one move to the table of the resulting component set (DECISIONS #35).
    ///
    /// New components count as added and changed, replaced ones as changed; the replaced
    /// values are dropped.
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// #[derive(Debug, PartialEq)]
    /// struct Health(u32);
    /// struct Armor(u32);
    ///
    /// let mut world = World::new();
    /// let knight = world.spawn_bundle((Health(10),))?;
    /// world.insert_bundle(knight, (Health(100), Armor(5)))?;
    /// assert_eq!(world.get::<Health>(knight), Some(&Health(100)));
    /// assert!(world.get::<Armor>(knight).is_some());
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::NoSuchEntity`] if `entity` is not alive, [`EcsError::DuplicateComponent`] if
    /// the bundle holds a component type twice, [`EcsError::OutOfMemory`] if storage cannot
    /// grow. On an error the world is unchanged and `bundle` is dropped.
    pub fn insert_bundle<B: Bundle>(&mut self, entity: Entity, bundle: B) -> Result<(), EcsError> {
        let missing = || EcsError::NoSuchEntity { entity };
        let from = self.entities.location(entity).ok_or_else(missing)?;
        let to = self.bundle_target::<B>(from.archetype)?;
        let tick = self.next_tick();
        if to == from.archetype {
            // Every type is already there: replace in place.
            let table = self.archetypes.get_mut(to).ok_or_else(missing)?;
            bundle.write(&mut BundleRow::new(table, from.row, tick));
            return Ok(());
        }

        let (source, dest) =
            two_mut(&mut self.archetypes, from.archetype, to).ok_or_else(missing)?;
        dest.reserve_row()?;
        // Nothing can fail from here on: the room is reserved.
        let row = dest.len();
        let mut left_behind = 0_usize;
        source.move_row(from.row, dest, |_, _| {
            left_behind = left_behind.saturating_add(1);
        });
        debug_assert_eq!(left_behind, 0, "the target has every source column");
        bundle.write(&mut BundleRow::new(dest, row, tick));
        let row = dest.push_entity(entity);
        debug_assert!(dest.is_aligned(), "the bundle filled every new column");
        let swapped = source.swap_remove_entity(from.row);
        self.settle(entity, Location { archetype: to, row }, swapped, from);
        Ok(())
    }

    /// Destroys `entity` and drops its components. Does not allocate.
    ///
    /// ```
    /// use pragma_ecs::{EcsError, World};
    ///
    /// let mut world = World::new();
    /// let rock = world.spawn_bundle(("rock", 3_u32))?;
    /// world.despawn(rock)?;
    /// assert!(!world.contains(rock));
    /// assert!(matches!(world.despawn(rock), Err(EcsError::NoSuchEntity { entity, .. }) if entity == rock));
    /// # Ok::<(), EcsError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::NoSuchEntity`] if `entity` is not alive.
    pub fn despawn(&mut self, entity: Entity) -> Result<(), EcsError> {
        let missing = || EcsError::NoSuchEntity { entity };
        let location = self.entities.location(entity).ok_or_else(missing)?;
        let table = self
            .archetypes
            .get_mut(location.archetype)
            .ok_or_else(missing)?;
        table.drop_row(location.row);
        let swapped = table.swap_remove_entity(location.row);
        self.entities.free(entity);
        if let Some(moved) = swapped {
            self.entities.relocate(moved, location);
        }
        Ok(())
    }

    /// Attaches `value` to `entity`, or replaces the component of that type it already has.
    ///
    /// Returns the replaced component, if any. Adding a new component type moves the entity to
    /// another archetype table. A new component counts as added and changed, a replaced one as
    /// changed (DECISIONS #28).
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// let mut world = World::new();
    /// let entity = world.spawn()?;
    /// assert_eq!(world.insert(entity, 1_u8)?, None);
    /// assert_eq!(world.insert(entity, 2_u8)?, Some(1));
    /// assert_eq!(world.get::<u8>(entity), Some(&2));
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::NoSuchEntity`] if `entity` is not alive, [`EcsError::OutOfMemory`] if storage
    /// cannot grow. On an error the world is unchanged and `value` is dropped.
    pub fn insert<T: Component>(
        &mut self,
        entity: Entity,
        value: T,
    ) -> Result<Option<T>, EcsError> {
        let missing = || EcsError::NoSuchEntity { entity };
        let from = self.entities.location(entity).ok_or_else(missing)?;
        let tick = self.next_tick();
        let source = self
            .archetypes
            .get_mut(from.archetype)
            .ok_or_else(missing)?;
        if let Some(slot) = source.get_mut::<T>(from.row, tick) {
            return Ok(Some(std::mem::replace(slot, value)));
        }

        let to = self.target(from.archetype, TypeId::of::<T>(), Some(new_column::<T>))?;
        let (source, dest) =
            two_mut(&mut self.archetypes, from.archetype, to).ok_or_else(missing)?;
        dest.reserve_row()?;
        // Nothing can fail from here on: the room is reserved.
        let mut left_behind = 0_usize;
        source.move_row(from.row, dest, |_, _| {
            left_behind = left_behind.saturating_add(1);
        });
        debug_assert_eq!(left_behind, 0, "the target has every source column");
        let pushed = dest.push_component(value, tick);
        debug_assert!(pushed, "the target has a column of the inserted type");
        let row = dest.push_entity(entity);
        let swapped = source.swap_remove_entity(from.row);
        self.settle(entity, Location { archetype: to, row }, swapped, from);
        Ok(None)
    }

    /// Detaches the component of type `T` from `entity` and returns it; `Ok(None)` if the entity
    /// has no such component.
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// let mut world = World::new();
    /// let entity = world.spawn()?;
    /// world.insert(entity, "name")?;
    /// assert_eq!(world.remove::<&str>(entity)?, Some("name"));
    /// assert_eq!(world.remove::<&str>(entity)?, None);
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::NoSuchEntity`] if `entity` is not alive, [`EcsError::OutOfMemory`] if storage
    /// cannot grow. On an error the world is unchanged.
    pub fn remove<T: Component>(&mut self, entity: Entity) -> Result<Option<T>, EcsError> {
        let missing = || EcsError::NoSuchEntity { entity };
        let from = self.entities.location(entity).ok_or_else(missing)?;
        let ty = TypeId::of::<T>();
        if !self
            .archetypes
            .get(from.archetype)
            .ok_or_else(missing)?
            .has(ty)
        {
            return Ok(None);
        }

        let to = self.target(from.archetype, ty, None)?;
        let (source, dest) =
            two_mut(&mut self.archetypes, from.archetype, to).ok_or_else(missing)?;
        dest.reserve_row()?;
        // Nothing can fail from here on: the room is reserved.
        let row = from.row;
        let mut taken = None;
        source.move_row(row, dest, |_, column| {
            let removed_type = column.as_any().is::<ColumnData<T>>();
            debug_assert!(
                removed_type,
                "only the column of the removed type is left behind"
            );
            match column.as_any_mut().downcast_mut::<ColumnData<T>>() {
                Some(values) if row < values.len() => {
                    taken = values.swap_remove(row).map(|(value, _, _)| value);
                }
                // Unreachable; keeps the columns aligned anyway.
                _ => {
                    column.swap_remove_drop(row);
                }
            }
        });
        let new_row = dest.push_entity(entity);
        let swapped = source.swap_remove_entity(row);
        self.settle(
            entity,
            Location {
                archetype: to,
                row: new_row,
            },
            swapped,
            from,
        );
        Ok(taken)
    }

    /// The component of type `T` of `entity`; `None` if the entity is not alive or has no such
    /// component.
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// let mut world = World::new();
    /// let entity = world.spawn()?;
    /// assert_eq!(world.get::<u32>(entity), None);
    /// world.insert(entity, 7_u32)?;
    /// assert_eq!(world.get::<u32>(entity), Some(&7));
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    #[must_use]
    pub fn get<T: Component>(&self, entity: Entity) -> Option<&T> {
        let location = self.entities.location(entity)?;
        self.archetypes
            .get(location.archetype)?
            .get::<T>(location.row)
    }

    /// A mutable reference to the component of type `T` of `entity`; `None` if the entity is
    /// not alive or has no such component.
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// let mut world = World::new();
    /// let entity = world.spawn()?;
    /// world.insert(entity, 7_u32)?;
    /// if let Some(value) = world.get_mut::<u32>(entity) {
    ///     *value += 1;
    /// }
    /// assert_eq!(world.get::<u32>(entity), Some(&8));
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    ///
    /// The component counts as changed (DECISIONS #28): [`Changed<T>`](crate::Changed) sees it
    /// whether or not it is actually written.
    #[must_use]
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<&mut T> {
        let location = self.entities.location(entity)?;
        let tick = self.next_tick();
        self.archetypes
            .get_mut(location.archetype)?
            .get_mut::<T>(location.row, tick)
    }

    /// Stores the resource `value`; returns the previous resource of this type, if any.
    ///
    /// Allocates one box for the value (fixed size, infallible, DECISIONS #20).
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// struct Score(u32);
    ///
    /// let mut world = World::new();
    /// assert!(world.insert_resource(Score(1)).is_none());
    /// assert_eq!(world.insert_resource(Score(2)).map(|s| s.0), Some(1));
    /// assert_eq!(world.resource::<Score>().map(|s| s.0), Some(2));
    /// ```
    pub fn insert_resource<T: Resource>(&mut self, value: T) -> Option<T> {
        self.resources.insert(value)
    }

    /// The resource of type `T`; `None` if the world has none.
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// let mut world = World::new();
    /// assert!(world.resource::<u64>().is_none());
    /// world.insert_resource(5_u64);
    /// assert_eq!(world.resource::<u64>().as_deref(), Some(&5));
    /// ```
    #[must_use]
    pub fn resource<T: Resource>(&self) -> Option<Res<'_, T>> {
        self.resources.get::<T>().map(Res)
    }

    /// A mutable reference to the resource of type `T`; `None` if the world has none.
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// let mut world = World::new();
    /// world.insert_resource(vec![1, 2]);
    /// if let Some(list) = world.resource_mut::<Vec<i32>>() {
    ///     list.push(3);
    /// }
    /// assert_eq!(world.resource::<Vec<i32>>().map(|l| l.len()), Some(3));
    /// ```
    #[must_use]
    pub fn resource_mut<T: Resource>(&mut self) -> Option<&mut T> {
        self.resources.get_mut::<T>()
    }

    /// Removes the resource of type `T` and returns it.
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// let mut world = World::new();
    /// world.insert_resource('x');
    /// assert_eq!(world.remove_resource::<char>(), Some('x'));
    /// assert!(!world.contains_resource::<char>());
    /// ```
    pub fn remove_resource<T: Resource>(&mut self) -> Option<T> {
        self.resources.remove::<T>()
    }

    /// Whether the world has a resource of type `T`.
    ///
    /// ```
    /// use pragma_ecs::World;
    ///
    /// let mut world = World::new();
    /// world.insert_resource(1.5_f32);
    /// assert!(world.contains_resource::<f32>());
    /// assert!(!world.contains_resource::<f64>());
    /// ```
    #[must_use]
    pub fn contains_resource<T: Resource>(&self) -> bool {
        self.resources.contains::<T>()
    }

    /// The process-unique id of this world.
    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    /// The resources, including the slots a schedule lends columns through.
    pub(crate) fn resources_mut(&mut self) -> &mut Resources {
        &mut self.resources
    }

    /// The tables and the resources at once, to lend columns (DECISIONS #18).
    pub(crate) fn lend_parts(&mut self) -> (&mut [Archetype], &mut Resources) {
        (&mut self.archetypes, &mut self.resources)
    }

    /// The archetype tables, indexed by archetype id. Tables are only ever appended.
    pub(crate) fn archetypes(&self) -> &[Archetype] {
        &self.archetypes
    }

    /// The archetype tables, mutably (rows only: the set of tables does not change).
    pub(crate) fn archetypes_mut(&mut self) -> &mut [Archetype] {
        &mut self.archetypes
    }

    /// Records the new location of a moved entity and of the entity that took its old row.
    fn settle(&mut self, entity: Entity, to: Location, swapped: Option<Entity>, from: Location) {
        let relocated = self.entities.relocate(entity, to);
        debug_assert!(relocated, "{entity} is alive");
        if let Some(moved) = swapped {
            let relocated = self.entities.relocate(moved, from);
            debug_assert!(relocated, "{moved} is alive");
        }
    }

    /// The archetype reached from `source` by adding (`add` is the new column's constructor)
    /// or removing the component type `ty`; created if it does not exist yet.
    fn target(
        &mut self,
        source: usize,
        ty: TypeId,
        add: Option<fn() -> Box<dyn Column>>,
    ) -> Result<usize, EcsError> {
        let oom = |_| EcsError::OutOfMemory {
            entities: self.entities.len(),
        };
        let table = self
            .archetypes
            .get(source)
            .ok_or(EcsError::CapacityExhausted)?;
        let cached = match add {
            Some(_) => table.add_edge(ty),
            None => table.remove_edge(ty),
        };
        if let Some(target) = cached {
            return Ok(target);
        }

        let mut types = Vec::new();
        types
            .try_reserve_exact(table.types().len().saturating_add(1))
            .map_err(oom)?;
        types.extend(table.types().iter().copied().filter(|t| *t != ty));
        let position = if add.is_some() {
            let position = types.partition_point(|t| *t < ty);
            types.insert(position, ty);
            Some(position)
        } else {
            None
        };

        let target = match self.by_types.get(types.as_slice()) {
            Some(&target) => target,
            None => {
                let mut columns: Vec<Box<dyn Column>> = Vec::new();
                columns.try_reserve_exact(types.len()).map_err(oom)?;
                columns.extend(
                    table
                        .types()
                        .iter()
                        .zip(table.columns())
                        .filter(|(t, _)| **t != ty)
                        .map(|(_, column)| column.empty()),
                );
                if let (Some(position), Some(new)) = (position, add) {
                    columns.insert(position, new());
                }
                self.push_archetype(types, columns)?
            }
        };
        if let Some(table) = self.archetypes.get_mut(source) {
            table.cache_edge(add.is_some(), ty, target);
        }
        Ok(target)
    }

    /// Creates the archetype of entities without components if the world has none yet.
    fn ensure_empty_table(&mut self) -> Result<(), EcsError> {
        if self.archetypes.is_empty() {
            let created = self.push_archetype(Vec::new(), Vec::new())?;
            debug_assert_eq!(created, EMPTY);
        }
        Ok(())
    }

    /// The archetype reached from `source` by inserting a bundle `B` (`source` itself if it
    /// already has every type); created if it does not exist yet (DECISIONS #35).
    fn bundle_target<B: Bundle>(&mut self, source: usize) -> Result<usize, EcsError> {
        let key = TypeId::of::<B>();
        let entities = self.entities.len();
        let oom = |_| EcsError::OutOfMemory { entities };
        let table = self
            .archetypes
            .get(source)
            .ok_or(EcsError::CapacityExhausted)?;
        if let Some(target) = table.bundle_edge(key) {
            return Ok(target);
        }

        let info = BundleInfo::of::<B>(entities)?;
        // The union of two sorted, unique lists.
        let mut types = Vec::new();
        types
            .try_reserve_exact(table.types().len().saturating_add(B::COUNT))
            .map_err(oom)?;
        types.extend_from_slice(table.types());
        for (ty, _) in info.components() {
            if let Err(position) = types.binary_search(&ty) {
                types.insert(position, ty);
            }
        }

        let target = if types.as_slice() == table.types() {
            source
        } else if let Some(&target) = self.by_types.get(types.as_slice()) {
            target
        } else {
            let mut columns: Vec<Box<dyn Column>> = Vec::new();
            columns.try_reserve_exact(types.len()).map_err(oom)?;
            for ty in &types {
                let column = match table.types().binary_search(ty) {
                    Ok(i) => table.columns().get(i).map(|column| column.empty()),
                    Err(_) => info
                        .components()
                        .find(|(t, _)| t == ty)
                        .map(|(_, new)| new()),
                };
                columns.push(column.ok_or(EcsError::CapacityExhausted)?);
            }
            self.push_archetype(types, columns)?
        };
        if let Some(table) = self.archetypes.get_mut(source) {
            table.cache_bundle_edge(key, target);
        }
        Ok(target)
    }

    /// Adds an archetype table for `types` (sorted, unique) and returns its id.
    fn push_archetype(
        &mut self,
        types: Vec<TypeId>,
        columns: Vec<Box<dyn Column>>,
    ) -> Result<usize, EcsError> {
        let oom = |_| EcsError::OutOfMemory {
            entities: self.entities.len(),
        };
        let id = self.archetypes.len();
        self.archetypes.try_reserve(1).map_err(oom)?;
        self.by_types.try_reserve(1).map_err(oom)?;
        let mut key = Vec::new();
        key.try_reserve_exact(types.len()).map_err(oom)?;
        key.extend_from_slice(&types);
        self.by_types.insert(key, id);
        self.archetypes.push(Archetype::new(types, columns));
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct A(u32);
    #[derive(Debug, PartialEq)]
    struct B(&'static str);
    #[derive(Debug, PartialEq)]
    struct Marker;

    #[test]
    fn components_survive_table_moves() {
        let mut world = World::new();
        let e0 = world.spawn().unwrap();
        let e1 = world.spawn().unwrap();
        let e2 = world.spawn().unwrap();
        for (i, e) in [e0, e1, e2].into_iter().enumerate() {
            world.insert(e, A(u32::try_from(i).unwrap())).unwrap();
            world.insert(e, B("b")).unwrap();
        }
        // Moving e0 out of the {A, B} table swaps e2 into its row.
        world.insert(e0, Marker).unwrap();
        assert_eq!(world.get::<A>(e2), Some(&A(2)));
        assert_eq!(world.get::<A>(e0), Some(&A(0)));
        assert_eq!(world.get::<Marker>(e0), Some(&Marker));
        assert_eq!(world.get::<Marker>(e1), None);

        assert_eq!(world.remove::<A>(e1).unwrap(), Some(A(1)));
        assert_eq!(world.get::<B>(e1), Some(&B("b")));
        assert_eq!(world.get::<A>(e2), Some(&A(2)));

        world.despawn(e2).unwrap();
        assert_eq!(world.get::<A>(e2), None);
        assert_eq!(world.len(), 2);
    }

    #[test]
    fn transitions_reuse_tables() {
        let mut world = World::new();
        let a = world.spawn().unwrap();
        let b = world.spawn().unwrap();
        world.insert(a, A(1)).unwrap();
        world.insert(a, B("x")).unwrap();
        world.insert(b, B("y")).unwrap();
        world.insert(b, A(2)).unwrap();
        // {}, {A}, {A, B}, {B}: the order of insertion does not create a second {A, B}.
        assert_eq!(world.archetypes.len(), 4);
        world.remove::<B>(a).unwrap();
        world.remove::<A>(b).unwrap();
        assert_eq!(world.archetypes.len(), 4);
    }

    #[test]
    fn bundles_go_straight_to_their_final_table() {
        let mut world = World::new();
        let a = world.spawn_bundle((A(1), B("a"))).unwrap();
        // {} and {A, B} only: no intermediate tables.
        assert_eq!(world.archetypes.len(), 2);
        let b = world.spawn_bundle((B("b"), A(2))).unwrap();
        assert_eq!(
            world.archetypes.len(),
            2,
            "the order in the tuple does not matter"
        );
        let empty = world.spawn_bundle(()).unwrap();
        assert_eq!(world.entities.location(empty).unwrap().archetype, EMPTY);

        // Replace one, add one: a single move to {A, B, Marker}.
        world.insert_bundle(a, (A(10), Marker)).unwrap();
        assert_eq!(world.archetypes.len(), 3);
        assert_eq!(
            (world.get::<A>(a), world.get::<B>(a), world.get::<Marker>(a)),
            (Some(&A(10)), Some(&B("a")), Some(&Marker))
        );
        // `b` took `a`'s old row.
        assert_eq!(world.get::<A>(b), Some(&A(2)));
        // Every type present: replaced in place, no move.
        world.insert_bundle(b, (A(20), B("bb"))).unwrap();
        assert_eq!(world.get::<B>(b), Some(&B("bb")));
        world.insert_bundle(empty, (Marker,)).unwrap();
        assert_eq!(world.archetypes.len(), 4);

        let dup = Err(EcsError::DuplicateComponent {
            component: std::any::type_name::<A>(),
        });
        assert_eq!(world.spawn_bundle((A(0), A(1))).map(drop), dup.clone());
        assert_eq!(world.insert_bundle(a, (A(0), Marker, A(1))), dup);
        assert_eq!(world.len(), 3);
        world.despawn(a).unwrap();
        assert_eq!(
            world.insert_bundle(a, (Marker,)),
            Err(EcsError::NoSuchEntity { entity: a })
        );
        assert!(world.archetypes.iter().all(Archetype::is_aligned));
    }

    #[test]
    fn stale_entities_are_rejected() {
        let mut world = World::new();
        let old = world.spawn().unwrap();
        world.insert(old, A(1)).unwrap();
        world.despawn(old).unwrap();
        let new = world.spawn().unwrap();
        assert_eq!(old.index(), new.index());

        let gone = Err(EcsError::NoSuchEntity { entity: old });
        assert_eq!(world.insert(old, A(2)), gone);
        assert_eq!(
            world.remove::<A>(old),
            Err(EcsError::NoSuchEntity { entity: old })
        );
        assert_eq!(
            world.despawn(old),
            Err(EcsError::NoSuchEntity { entity: old })
        );
        assert_eq!(world.get::<A>(old), None);
        assert_eq!(world.get::<A>(new), None);
        assert!(format!("{world:?}").starts_with("World { entities: 1"));
    }
}
