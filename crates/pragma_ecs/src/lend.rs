//! Lending the world's columns to the frame graph for one frame (DECISIONS #18).
//!
//! Before a frame, the `Vec<T>` of every table that has a component type `T` some system uses is
//! swapped with an empty `Vec` kept in the graph resource [`Columns<T>`] (one slot per table).
//! Systems borrow `Columns<T>` through the graph's per-resource lock, so systems with disjoint
//! component access run in parallel without `unsafe`. After the frame the same swap puts every
//! column back. Swapping moves three words per column and never allocates; the slots grow
//! (`try_reserve`) only when new tables appear, before anything is swapped.

use std::any::TypeId;
use std::fmt;
use std::mem;

use crate::{Component, EcsError, World};

/// The columns of the component type `T` lent to the frame graph for one frame: the slot of a
/// table is indexed by its archetype id, and tables without `T` keep an empty `Vec`.
///
/// Internal to the schedule; visible only because query machinery names it.
#[doc(hidden)]
pub struct Columns<T> {
    tables: Vec<Vec<T>>,
}

impl<T> fmt::Debug for Columns<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Columns")
            .field("tables", &self.tables.len())
            .finish()
    }
}

impl<T> Columns<T> {
    pub(crate) fn tables(&self) -> &[Vec<T>] {
        &self.tables
    }

    pub(crate) fn tables_mut(&mut self) -> &mut [Vec<T>] {
        &mut self.tables
    }
}

/// The world itself during a frame: its tables' types and entities, the lent columns empty.
/// Every system reads it, so it never orders systems.
pub(crate) struct WorldSlot(pub(crate) World);

/// The name the frame graph reports for the lent columns of a component type: the component's
/// own name, if `resource` names a [`Columns`] slot.
pub(crate) fn data_name(resource: String) -> String {
    let wrapper = std::any::type_name::<Columns<()>>();
    let prefix = wrapper.strip_suffix("()>").unwrap_or(wrapper);
    match resource
        .strip_prefix(prefix)
        .and_then(|inner| inner.strip_suffix('>'))
    {
        Some(component) => component.to_owned(),
        None => resource,
    }
}

/// Lends and returns the columns of one component type.
#[derive(Clone, Copy)]
pub(crate) struct Lender {
    pub(crate) ty: TypeId,
    prepare: fn(&mut World) -> Result<(), EcsError>,
    swap: fn(&mut World),
}

impl Lender {
    pub(crate) fn of<T: Component>() -> Self {
        Self {
            ty: TypeId::of::<T>(),
            prepare: prepare::<T>,
            swap: swap::<T>,
        }
    }

    /// Makes sure `world` has a slot for every table. Nothing is lent yet; on an error nothing
    /// changed but a possibly created empty [`Columns`] resource.
    pub(crate) fn prepare(&self, world: &mut World) -> Result<(), EcsError> {
        (self.prepare)(world)
    }

    /// Lends the columns, or returns them if they are lent. Does not allocate.
    pub(crate) fn swap(&self, world: &mut World) {
        (self.swap)(world);
    }
}

fn prepare<T: Component>(world: &mut World) -> Result<(), EcsError> {
    let entities = world.len();
    let (tables, resources) = world.lend_parts();
    if !resources.contains::<Columns<T>>() {
        resources.insert(Columns::<T> { tables: Vec::new() });
    }
    if let Some(columns) = resources.get_mut::<Columns<T>>() {
        let missing = tables.len().saturating_sub(columns.tables.len());
        columns
            .tables
            .try_reserve(missing)
            .map_err(|_| EcsError::OutOfMemory { entities })?;
        columns.tables.resize_with(tables.len(), Vec::new);
    }
    Ok(())
}

fn swap<T: Component>(world: &mut World) {
    let (tables, resources) = world.lend_parts();
    let Some(columns) = resources.get_mut::<Columns<T>>() else {
        return;
    };
    for (table, slot) in tables.iter_mut().zip(&mut columns.tables) {
        if let Some(column) = table.column_mut::<T>() {
            mem::swap(column, slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Position(u32);

    #[test]
    fn lending_twice_returns_the_columns() {
        let mut world = World::new();
        let e = world.spawn().unwrap();
        world.insert(e, Position(7)).unwrap();
        let lender = Lender::of::<Position>();
        lender.prepare(&mut world).unwrap();

        lender.swap(&mut world);
        assert!(world.get::<Position>(e).is_none(), "the column is lent");
        let lent = world.lend_parts().1.get_mut::<Columns<Position>>().unwrap();
        assert_eq!(lent.tables().iter().map(Vec::len).sum::<usize>(), 1);
        assert!(format!("{lent:?}").starts_with("Columns"));

        lender.swap(&mut world);
        assert_eq!(world.get::<Position>(e).map(|p| p.0), Some(7));
    }

    #[test]
    fn graph_names_of_lent_columns_are_component_names() {
        let name = std::any::type_name::<Columns<Position>>().to_owned();
        assert_eq!(data_name(name), std::any::type_name::<Position>());
        assert_eq!(data_name("game::Score".to_owned()), "game::Score");
    }
}
