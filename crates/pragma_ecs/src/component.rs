//! Components and their type-erased columns (DECISIONS #8, #9, #27).

use std::any::Any;
use std::collections::TryReserveError;

/// Data attached to an entity.
///
/// Implemented for every `Send + Sync + 'static` type: any plain struct or enum is a component,
/// no derive is needed. An entity holds at most one component of each type.
///
/// ```
/// use pragma_ecs::{Component, World};
///
/// #[derive(Debug, PartialEq)]
/// struct Position { x: f32, y: f32 }
///
/// fn assert_component<T: Component>() {}
/// assert_component::<Position>();
///
/// let mut world = World::new();
/// let entity = world.spawn()?;
/// world.insert(entity, Position { x: 1.0, y: 2.0 })?;
/// assert_eq!(world.get::<Position>(entity), Some(&Position { x: 1.0, y: 2.0 }));
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub trait Component: Send + Sync + 'static {}

impl<T: Send + Sync + 'static> Component for T {}

/// The change ticks of the rows of one column: when each component was added to its entity and
/// when it was last changed (DECISIONS #27). Internal; visible only because query machinery
/// names it.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct Ticks<'a> {
    pub(crate) added: &'a [u64],
    pub(crate) changed: &'a [u64],
}

/// The rows of one component type in one table: the values and, parallel to them, their added
/// and changed ticks in two separate vectors, so that a write touches only the changed ticks
/// (DECISIONS #27). Internal; visible only because query machinery names it.
#[doc(hidden)]
#[derive(Debug)]
pub struct ColumnData<T> {
    values: Vec<T>,
    added: Vec<u64>,
    changed: Vec<u64>,
}

impl<T> Default for ColumnData<T> {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            added: Vec::new(),
            changed: Vec::new(),
        }
    }
}

impl<T> ColumnData<T> {
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    pub(crate) fn values(&self) -> &[T] {
        &self.values
    }

    pub(crate) fn ticks(&self) -> Ticks<'_> {
        Ticks {
            added: &self.added,
            changed: &self.changed,
        }
    }

    /// The values and their changed ticks at once, both mutably.
    pub(crate) fn parts_mut(&mut self) -> (&mut [T], &mut [u64]) {
        (&mut self.values, &mut self.changed)
    }

    /// The value of `row` and its changed tick.
    pub(crate) fn get_mut(&mut self, row: usize) -> Option<(&mut T, &mut u64)> {
        Some((self.values.get_mut(row)?, self.changed.get_mut(row)?))
    }

    /// Appends a row added at `added` and changed at `changed`. Callers reserve the room first
    /// (`Column::reserve_rows`).
    pub(crate) fn push(&mut self, value: T, added: u64, changed: u64) {
        self.values.push(value);
        self.added.push(added);
        self.changed.push(changed);
    }

    /// Removes `row` and returns its value and ticks (the last row takes its place).
    pub(crate) fn swap_remove(&mut self, row: usize) -> Option<(T, u64, u64)> {
        if row >= self.values.len() || row >= self.added.len() || row >= self.changed.len() {
            return None;
        }
        let added = self.added.swap_remove(row);
        let changed = self.changed.swap_remove(row);
        Some((self.values.swap_remove(row), added, changed))
    }
}

/// One column of an archetype table: a [`ColumnData<T>`] behind a trait object, so that a table
/// can hold columns of different types without `unsafe` (DECISIONS #9). Typed access is a
/// single `downcast` per column; the rows are a plain `Vec<T>` plus their ticks.
pub(crate) trait Column: Send + Sync {
    fn as_any(&self) -> &dyn Any;

    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// An empty column of the same type. Allocates only the (fixed-size) box.
    fn empty(&self) -> Box<dyn Column>;

    /// Reserves room for `additional` more rows (`Vec::try_reserve`).
    fn reserve_rows(&mut self, additional: usize) -> Result<(), TryReserveError>;

    /// The change ticks of every row.
    fn ticks(&self) -> Ticks<'_>;

    /// Drops the value at `row`, moving the last row into its place. `false` if out of range.
    fn swap_remove_drop(&mut self, row: usize) -> bool;

    /// Moves the value at `row` and its ticks to the end of `dst`, moving the last row into its
    /// place. `false` (nothing changed) if `row` is out of range or `dst` holds another type.
    ///
    /// Does not allocate if `dst` has room reserved.
    fn swap_remove_into(&mut self, row: usize, dst: &mut dyn Column) -> bool;
}

impl<T: Component> Column for ColumnData<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn empty(&self) -> Box<dyn Column> {
        Box::new(Self::default())
    }

    fn reserve_rows(&mut self, additional: usize) -> Result<(), TryReserveError> {
        self.values.try_reserve(additional)?;
        self.added.try_reserve(additional)?;
        self.changed.try_reserve(additional)
    }

    fn ticks(&self) -> Ticks<'_> {
        ColumnData::ticks(self)
    }

    fn swap_remove_drop(&mut self, row: usize) -> bool {
        self.swap_remove(row).is_some()
    }

    fn swap_remove_into(&mut self, row: usize, dst: &mut dyn Column) -> bool {
        let Some(dst) = dst.as_any_mut().downcast_mut::<Self>() else {
            return false;
        };
        match self.swap_remove(row) {
            Some((value, added, changed)) => {
                dst.push(value, added, changed);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(values: &[u32]) -> ColumnData<u32> {
        let mut column = ColumnData::default();
        for (tick, value) in (1..).zip(values) {
            column.push(*value, tick, tick.saturating_mul(10));
        }
        column
    }

    #[test]
    fn moves_between_columns_of_the_same_type_only() {
        let mut src = column(&[1, 2, 3]);
        let mut dst = src.empty();
        let mut wrong: Box<dyn Column> = Box::new(ColumnData::<u64>::default());

        assert!(!src.swap_remove_into(0, wrong.as_mut()));
        assert!(!src.swap_remove_into(3, dst.as_mut()));
        assert!(src.swap_remove_into(0, dst.as_mut()));
        assert_eq!(src.values(), [3, 2]);
        assert_eq!(
            (src.ticks().added, src.ticks().changed),
            (&[3, 2][..], &[30, 20][..])
        );
        let moved = dst.as_any().downcast_ref::<ColumnData<u32>>().unwrap();
        assert_eq!(moved.values(), [1]);
        assert_eq!(
            (dst.ticks().added, dst.ticks().changed),
            (&[1][..], &[10][..])
        );

        assert!(src.swap_remove_drop(1));
        assert!(!src.swap_remove_drop(1));
        assert_eq!(src.values(), [3]);
        assert_eq!(src.len(), 1);
    }
}
