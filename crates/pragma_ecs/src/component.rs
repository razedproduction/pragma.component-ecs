//! Components and their type-erased columns (DECISIONS #8, #9).

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

/// One column of an archetype table: a `Vec<T>` behind a trait object, so that a table can hold
/// columns of different types without `unsafe` (DECISIONS #9). Typed access is a single
/// `downcast` per column; the rows are a plain `Vec<T>`.
pub(crate) trait Column: Send + Sync {
    fn as_any(&self) -> &dyn Any;

    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// An empty column of the same type. Allocates only the (fixed-size) box.
    fn empty(&self) -> Box<dyn Column>;

    /// Reserves room for `additional` more rows (`Vec::try_reserve`).
    fn reserve_rows(&mut self, additional: usize) -> Result<(), TryReserveError>;

    /// Drops the value at `row`, moving the last row into its place. `false` if out of range.
    fn swap_remove_drop(&mut self, row: usize) -> bool;

    /// Moves the value at `row` to the end of `dst`, moving the last row into its place.
    /// `false` (nothing changed) if `row` is out of range or `dst` holds another type.
    ///
    /// Does not allocate if `dst` has room reserved.
    fn swap_remove_into(&mut self, row: usize, dst: &mut dyn Column) -> bool;
}

impl<T: Component> Column for Vec<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn empty(&self) -> Box<dyn Column> {
        Box::new(Vec::<T>::new())
    }

    fn reserve_rows(&mut self, additional: usize) -> Result<(), TryReserveError> {
        Vec::try_reserve(self, additional)
    }

    fn swap_remove_drop(&mut self, row: usize) -> bool {
        if row >= self.len() {
            return false;
        }
        drop(self.swap_remove(row));
        true
    }

    fn swap_remove_into(&mut self, row: usize, dst: &mut dyn Column) -> bool {
        let Some(dst) = dst.as_any_mut().downcast_mut::<Self>() else {
            return false;
        };
        if row >= self.len() {
            return false;
        }
        dst.push(self.swap_remove(row));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_between_columns_of_the_same_type_only() {
        let mut src: Vec<u32> = vec![1, 2, 3];
        let mut dst = src.empty();
        let mut wrong: Box<dyn Column> = Box::new(Vec::<u64>::new());

        assert!(!src.swap_remove_into(0, wrong.as_mut()));
        assert!(!src.swap_remove_into(3, dst.as_mut()));
        assert!(src.swap_remove_into(0, dst.as_mut()));
        assert_eq!(src, [3, 2]);
        assert_eq!(dst.as_any().downcast_ref::<Vec<u32>>(), Some(&vec![1]));

        assert!(src.swap_remove_drop(1));
        assert!(!src.swap_remove_drop(1));
        assert_eq!(src, [3]);
    }
}
