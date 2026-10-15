//! Change detection: [`Mut<T>`] records writes in the change ticks of its row (DECISIONS #27,
//! #28); the [`Added`](crate::Added) and [`Changed`](crate::Changed) filters read them.

use std::fmt;
use std::iter::Zip;
use std::ops::{Deref, DerefMut};
use std::slice;

/// A component borrowed for writing by a query or a view (`&mut T` in the query): dereferences
/// to `T`, and a mutable dereference marks the component as changed, so that
/// [`Changed<T>`](crate::Changed) sees exactly the rows that were written (DECISIONS #28).
///
/// Field access and method calls work through auto-deref; to pass the component to a function
/// that takes `&mut T`, write `&mut *item` (or [`into_inner`](Self::into_inner)).
///
/// ```
/// use pragma_ecs::{Changed, Entity, Mut, Query, World};
///
/// struct Health(u32);
///
/// fn heal(health: &mut Health) {
///     health.0 += 5;
/// }
///
/// let mut world = World::new();
/// let entities = [world.spawn()?, world.spawn()?];
/// for entity in entities {
///     world.insert(entity, Health(50))?;
/// }
/// let mut changed = Query::<Entity, Changed<Health>>::new()?;
/// assert_eq!(changed.iter(&world)?.count(), 2); // first run: everything is new
///
/// let mut healths = Query::<(Entity, &mut Health)>::new()?;
/// for (entity, mut health) in healths.iter_mut(&mut world)? {
///     if entity == entities[1] {
///         heal(&mut health); // `&mut Mut<Health>` coerces to `&mut Health`
///     }
///     let _read_only = health.0; // reading is not a change
/// }
/// assert_eq!(changed.iter(&world)?.collect::<Vec<_>>(), [entities[1]]);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct Mut<'w, T> {
    value: &'w mut T,
    /// The changed tick of the row.
    changed: &'w mut u64,
    tick: u64,
}

impl<'w, T> Mut<'w, T> {
    /// The mutable reference itself; the component counts as changed.
    ///
    /// ```
    /// use pragma_ecs::{Query, World};
    ///
    /// let mut world = World::new();
    /// let entity = world.spawn()?;
    /// world.insert(entity, 1_u32)?;
    /// let mut values = Query::<&mut u32>::new()?;
    /// let refs: Vec<&mut u32> = values.iter_mut(&mut world)?.map(|v| v.into_inner()).collect();
    /// assert_eq!(refs, [&mut 1]);
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    #[must_use]
    pub fn into_inner(self) -> &'w mut T {
        *self.changed = self.tick;
        self.value
    }
}

impl<T> Deref for Mut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> DerefMut for Mut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        *self.changed = self.tick;
        self.value
    }
}

impl<T> AsRef<T> for Mut<'_, T> {
    fn as_ref(&self) -> &T {
        self.value
    }
}

impl<T> AsMut<T> for Mut<'_, T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

impl<T: fmt::Debug> fmt::Debug for Mut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// The rows of a `&mut T` query element: values, their changed ticks and the tick a write
/// stamps.
#[doc(hidden)]
#[derive(Debug)]
pub struct MutSlice<'w, T> {
    values: &'w mut [T],
    ticks: &'w mut [u64],
    tick: u64,
}

impl<'w, T> MutSlice<'w, T> {
    /// `None` if the two columns do not have the same length.
    pub(crate) fn new(values: &'w mut [T], ticks: &'w mut [u64], tick: u64) -> Option<Self> {
        (values.len() == ticks.len()).then_some(Self {
            values,
            ticks,
            tick,
        })
    }

    /// Splits the rows at `mid` (clamped to the row count).
    pub(crate) fn split_at(self, mid: usize) -> (Self, Self) {
        let mid = mid.min(self.values.len()).min(self.ticks.len());
        let (values, other_values) = self.values.split_at_mut(mid);
        let (ticks, other_ticks) = self.ticks.split_at_mut(mid);
        (
            Self {
                values,
                ticks,
                tick: self.tick,
            },
            Self {
                values: other_values,
                ticks: other_ticks,
                tick: self.tick,
            },
        )
    }
}

impl<'w, T> IntoIterator for MutSlice<'w, T> {
    type Item = Mut<'w, T>;
    type IntoIter = MutIter<'w, T>;

    fn into_iter(self) -> MutIter<'w, T> {
        MutIter {
            rows: self.values.iter_mut().zip(self.ticks.iter_mut()),
            tick: self.tick,
        }
    }
}

/// Iterates a [`MutSlice`]. The two slices are zipped, so the rows are walked with one index.
#[doc(hidden)]
#[derive(Debug)]
pub struct MutIter<'w, T> {
    rows: Zip<slice::IterMut<'w, T>, slice::IterMut<'w, u64>>,
    tick: u64,
}

impl<'w, T> Iterator for MutIter<'w, T> {
    type Item = Mut<'w, T>;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        let (value, changed) = self.rows.next()?;
        Some(Mut {
            value,
            changed,
            tick: self.tick,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rows.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mutable_access_stamps_the_tick() {
        let mut values = [1_u32, 2, 3];
        let mut ticks = [1_u64; 3];
        let slice = MutSlice::new(&mut values, &mut ticks, 9).unwrap();
        let (left, right) = slice.split_at(1);
        for mut item in left {
            *item += 10;
            assert_eq!(format!("{item:?}"), "11");
        }
        for (i, item) in right.into_iter().enumerate() {
            if i == 1 {
                let _ = item.into_inner();
            } else {
                assert_eq!(*item.as_ref(), 2);
            }
        }
        assert_eq!(values, [11, 2, 3]);
        assert_eq!(ticks, [9, 1, 9]);
        assert!(MutSlice::new(&mut [1_u8][..], &mut [], 0).is_none());
    }
}
