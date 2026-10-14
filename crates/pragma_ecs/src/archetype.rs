//! Archetype tables (DECISIONS #8): entities with the same component set share a table whose
//! columns are parallel to its `entities` row list.

use std::any::TypeId;
use std::collections::HashMap;

use crate::component::{Column, ColumnData, Component, Ticks};
use crate::{EcsError, Entity};

pub(crate) struct Archetype {
    /// Component types, sorted and unique.
    types: Vec<TypeId>,
    /// One column per entry of `types`, in the same order.
    columns: Vec<Box<dyn Column>>,
    /// The entity of every row.
    entities: Vec<Entity>,
    /// Cached transitions: the archetype reached by adding / removing one component type.
    add_edges: HashMap<TypeId, usize>,
    remove_edges: HashMap<TypeId, usize>,
    /// Cached transitions by bundle type: the archetype reached by inserting the bundle
    /// (DECISIONS #35). From the empty archetype, the table a bundle spawns into.
    bundle_edges: HashMap<TypeId, usize>,
}

impl Archetype {
    /// A table for `types` (sorted, unique) with `columns` parallel to them.
    pub(crate) fn new(types: Vec<TypeId>, columns: Vec<Box<dyn Column>>) -> Self {
        debug_assert!(types.windows(2).all(|w| matches!(w, [a, b] if a < b)));
        debug_assert_eq!(types.len(), columns.len());
        Self {
            types,
            columns,
            entities: Vec::new(),
            add_edges: HashMap::new(),
            remove_edges: HashMap::new(),
            bundle_edges: HashMap::new(),
        }
    }

    pub(crate) fn types(&self) -> &[TypeId] {
        &self.types
    }

    pub(crate) fn columns(&self) -> &[Box<dyn Column>] {
        &self.columns
    }

    pub(crate) fn entities(&self) -> &[Entity] {
        &self.entities
    }

    /// Number of rows.
    pub(crate) fn len(&self) -> usize {
        self.entities.len()
    }

    /// The component types, the columns (mutably) and the row entities at once, so that a query
    /// can borrow several columns of the table (DECISIONS #13).
    pub(crate) fn parts_mut(&mut self) -> (&[TypeId], &mut [Box<dyn Column>], &[Entity]) {
        (&self.types, &mut self.columns, &self.entities)
    }

    pub(crate) fn has(&self, ty: TypeId) -> bool {
        self.types.binary_search(&ty).is_ok()
    }

    pub(crate) fn get<T: Component>(&self, row: usize) -> Option<&T> {
        let column = self.types.binary_search(&TypeId::of::<T>()).ok()?;
        self.columns
            .get(column)?
            .as_any()
            .downcast_ref::<ColumnData<T>>()?
            .values()
            .get(row)
    }

    /// The component of `T` at `row`, its changed tick stamped with `tick` (DECISIONS #28).
    pub(crate) fn get_mut<T: Component>(&mut self, row: usize, tick: u64) -> Option<&mut T> {
        let (value, changed) = self.column_mut::<T>()?.get_mut(row)?;
        *changed = tick;
        Some(value)
    }

    /// The whole column of `T`, to lend it to a frame (DECISIONS #18). Callers must not change
    /// its length.
    pub(crate) fn column_mut<T: Component>(&mut self) -> Option<&mut ColumnData<T>> {
        let column = self.types.binary_search(&TypeId::of::<T>()).ok()?;
        self.columns
            .get_mut(column)?
            .as_any_mut()
            .downcast_mut::<ColumnData<T>>()
    }

    /// The change ticks of the column of type `ty`, if the table has one.
    pub(crate) fn ticks(&self, ty: TypeId) -> Option<Ticks<'_>> {
        let column = self.types.binary_search(&ty).ok()?;
        Some(self.columns.get(column)?.ticks())
    }

    pub(crate) fn add_edge(&self, ty: TypeId) -> Option<usize> {
        self.add_edges.get(&ty).copied()
    }

    pub(crate) fn remove_edge(&self, ty: TypeId) -> Option<usize> {
        self.remove_edges.get(&ty).copied()
    }

    pub(crate) fn bundle_edge(&self, bundle: TypeId) -> Option<usize> {
        self.bundle_edges.get(&bundle).copied()
    }

    /// Caches the transition by a bundle; skipped if out of memory, like
    /// [`cache_edge`](Self::cache_edge).
    pub(crate) fn cache_bundle_edge(&mut self, bundle: TypeId, target: usize) {
        if self.bundle_edges.try_reserve(1).is_ok() {
            self.bundle_edges.insert(bundle, target);
        }
    }

    /// Caches a transition. A cache is an optimization: running out of memory just skips it.
    pub(crate) fn cache_edge(&mut self, add: bool, ty: TypeId, target: usize) {
        let edges = if add {
            &mut self.add_edges
        } else {
            &mut self.remove_edges
        };
        if edges.try_reserve(1).is_ok() {
            edges.insert(ty, target);
        }
    }

    /// Reserves room for one more row in every column, so that the next push does not allocate.
    pub(crate) fn reserve_row(&mut self) -> Result<(), EcsError> {
        let entities = self.entities.len().saturating_add(1);
        let oom = |_| EcsError::OutOfMemory { entities };
        self.entities.try_reserve(1).map_err(oom)?;
        for column in &mut self.columns {
            column.reserve_rows(1).map_err(oom)?;
        }
        Ok(())
    }

    /// Appends a row for `entity` whose columns are already filled; returns its row.
    ///
    /// Callers reserve the room first ([`reserve_row`](Self::reserve_row)).
    pub(crate) fn push_entity(&mut self, entity: Entity) -> usize {
        let row = self.entities.len();
        self.entities.push(entity);
        row
    }

    /// Pushes `value`, added at `tick`, into the column of `T`; `false` if the table has no such
    /// column.
    pub(crate) fn push_component<T: Component>(&mut self, value: T, tick: u64) -> bool {
        match self.column_mut::<T>() {
            Some(column) => {
                column.push(value, tick, tick);
                true
            }
            None => false,
        }
    }

    /// Writes `value` at `row` of the column of `T`: replaces the component there (stamped as
    /// changed at `tick`) or, if the column does not reach `row` yet, appends it (added and
    /// changed at `tick`; callers reserve the room first). `false` if the table has no such
    /// column.
    pub(crate) fn put_component<T: Component>(&mut self, row: usize, value: T, tick: u64) -> bool {
        let Some(column) = self.column_mut::<T>() else {
            return false;
        };
        match column.get_mut(row) {
            Some((slot, changed)) => {
                *slot = value;
                *changed = tick;
            }
            None => {
                debug_assert_eq!(column.len(), row, "a bundle writes the next row");
                column.push(value, tick, tick);
            }
        }
        true
    }

    /// Whether every column has exactly one row per entity (a debug check).
    pub(crate) fn is_aligned(&self) -> bool {
        let rows = self.entities.len();
        self.columns.iter().all(|column| {
            let ticks = column.ticks();
            ticks.added.len() == rows && ticks.changed.len() == rows
        })
    }

    /// Removes `row` from the entity list (the last row takes its place) and returns the entity
    /// now at `row`, if any, so the caller can update its location. Columns are handled separately.
    pub(crate) fn swap_remove_entity(&mut self, row: usize) -> Option<Entity> {
        if row >= self.entities.len() {
            return None;
        }
        self.entities.swap_remove(row);
        self.entities.get(row).copied()
    }

    /// Drops every component of `row` (the last row takes its place).
    pub(crate) fn drop_row(&mut self, row: usize) {
        for column in &mut self.columns {
            let dropped = column.swap_remove_drop(row);
            debug_assert!(dropped, "row {row} out of range");
        }
    }

    /// Moves the components of `row` into `dst` (room reserved): columns present in both tables
    /// are moved, the others are handed to `leftover` with their type. The last row of `self`
    /// takes the place of `row`. The entity list is left to the caller.
    pub(crate) fn move_row(
        &mut self,
        row: usize,
        dst: &mut Self,
        mut leftover: impl FnMut(TypeId, &mut dyn Column),
    ) {
        for (ty, column) in self.types.iter().zip(&mut self.columns) {
            match dst
                .types
                .binary_search(ty)
                .ok()
                .and_then(|i| dst.columns.get_mut(i))
            {
                Some(target) => {
                    let moved = column.swap_remove_into(row, target.as_mut());
                    debug_assert!(moved, "row {row} out of range or column type mismatch");
                }
                None => leftover(*ty, column.as_mut()),
            }
        }
    }
}

/// Mutable references to two different elements of a slice; `None` if `a == b` or out of range.
pub(crate) fn two_mut<T>(slice: &mut [T], a: usize, b: usize) -> Option<(&mut T, &mut T)> {
    if a < b {
        let (left, right) = slice.split_at_mut_checked(b)?;
        Some((left.get_mut(a)?, right.first_mut()?))
    } else if b < a {
        let (left, right) = slice.split_at_mut_checked(a)?;
        let (eb, ea) = (left.get_mut(b)?, right.first_mut()?);
        Some((ea, eb))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_mut_rejects_aliasing_and_out_of_range() {
        let mut v = [1, 2, 3];
        assert!(two_mut(&mut v, 1, 1).is_none());
        assert!(two_mut(&mut v, 0, 3).is_none());
        let (a, b) = two_mut(&mut v, 2, 0).unwrap();
        std::mem::swap(a, b);
        assert_eq!(v, [3, 2, 1]);
    }
}
