//! What a query yields ([`QueryData`]) and which tables it visits ([`QueryFilter`]).
//!
//! Borrowing the columns of one table without `unsafe` (DECISIONS #13): the table's columns are
//! walked once with `iter_mut` and offered, as [`ColumnRef`]s, to every element of the query. A
//! write takes the exclusive reference, a read turns it into a shared one that further reads
//! copy. Each element then downcasts its column into a slice, and the rows of a table are the
//! element slices iterated in lockstep.

use std::any::{Any, TypeId, type_name};
use std::fmt;
use std::iter::Copied;
use std::marker::PhantomData;
use std::{mem, slice};

use pragma_core::graph::{self, TaskContext};

use crate::access::{Access, AccessKind};
use crate::lend::Columns;
use crate::{Component, EcsError, Entity};

mod sealed {
    /// Keeps [`QueryData`](super::QueryData) and [`QueryFilter`](super::QueryFilter) closed:
    /// their machinery is internal and may change (DECISIONS #12).
    pub trait Sealed {}
}

use sealed::Sealed;

/// A column of a table offered to the elements of a query: still exclusive, or already shared
/// by a reading element.
#[doc(hidden)]
#[derive(Debug)]
pub enum ColumnRef<'w> {
    /// Nobody has taken the column yet.
    Unique(&'w mut dyn Any),
    /// A reading element has taken it; other reads copy the reference.
    Shared(&'w dyn Any),
}

/// Takes `column` for reading, leaving a shared reference for other readers.
fn take_shared<'w, T: Component>(column: &mut Option<ColumnRef<'w>>) -> Option<&'w [T]> {
    let shared: &'w dyn Any = match column.take()? {
        ColumnRef::Unique(c) => c,
        ColumnRef::Shared(c) => c,
    };
    *column = Some(ColumnRef::Shared(shared));
    shared.downcast_ref::<Vec<T>>().map(Vec::as_slice)
}

/// Takes `column` for writing; `None` (the column left as is) if a reader already shares it.
fn take_unique<'w, T: Component>(column: &mut Option<ColumnRef<'w>>) -> Option<&'w mut [T]> {
    match column.take()? {
        ColumnRef::Unique(c) => c.downcast_mut::<Vec<T>>().map(Vec::as_mut_slice),
        shared @ ColumnRef::Shared(_) => {
            *column = Some(shared);
            None
        }
    }
}

/// What a [`Query`](crate::Query) yields for every matching entity.
///
/// Implemented for:
///
/// | `Q` | yields | access | matches tables |
/// |---|---|---|---|
/// | `&T` | `&T` | read `T` | with `T` |
/// | `&mut T` | `&mut T` | write `T` | with `T` |
/// | `Option<&T>` | `Option<&T>` | read `T` | all |
/// | [`Entity`] | the entity id | none | all |
/// | `(Q1, Q2, …)`, up to 8 | a tuple of the items | the union | all of them |
///
/// The trait is sealed: its methods are internal (DECISIONS #12).
///
/// ```
/// use pragma_ecs::{Entity, Query, World};
///
/// struct Position(i32);
/// struct Name(&'static str);
///
/// let mut world = World::new();
/// let a = world.spawn()?;
/// world.insert(a, Position(1))?;
/// world.insert(a, Name("a"))?;
/// let b = world.spawn()?;
/// world.insert(b, Position(2))?;
///
/// let mut query = Query::<(Entity, &mut Position, Option<&Name>)>::new()?;
/// for (entity, position, name) in query.iter_mut(&mut world)? {
///     position.0 *= 10;
///     assert_eq!(name.is_some(), entity == a);
/// }
/// assert_eq!(world.get::<Position>(b).map(|p| p.0), Some(20));
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub trait QueryData: Sealed {
    /// The value yielded per entity; `'w` is the borrow of the world.
    type Item<'w>;

    /// Columns offered so far for one table.
    #[doc(hidden)]
    type State<'w>: Default;

    /// The rows of one table (or of a chunk of it): one slice per element.
    #[doc(hidden)]
    type Slice<'w>: Send;

    /// Iterates the rows of a [`Slice`](Self::Slice).
    #[doc(hidden)]
    type Iter<'w>: Iterator<Item = Self::Item<'w>>;

    /// Adds the element's component access to `access`.
    #[doc(hidden)]
    fn declare(access: &mut Access) -> Result<(), EcsError>;

    /// Whether a table with the component types `types` (sorted) has what the element needs.
    #[doc(hidden)]
    fn matches(types: &[TypeId]) -> bool;

    /// Offers the column of type `ty`; the element takes it if it needs it.
    #[doc(hidden)]
    fn offer<'w>(state: &mut Self::State<'w>, ty: TypeId, column: &mut Option<ColumnRef<'w>>);

    /// The rows of the table once every column was offered; `None` if a needed column is missing.
    #[doc(hidden)]
    fn finish<'w>(state: Self::State<'w>, entities: &'w [Entity]) -> Option<Self::Slice<'w>>;

    /// Splits the rows at `mid` (clamped to the row count).
    #[doc(hidden)]
    fn split_at<'w>(slice: Self::Slice<'w>, mid: usize) -> (Self::Slice<'w>, Self::Slice<'w>);

    /// Iterates the rows.
    #[doc(hidden)]
    fn iter<'w>(slice: Self::Slice<'w>) -> Self::Iter<'w>;

    /// The borrows of the lent columns a system holds during a frame (DECISIONS #18).
    #[doc(hidden)]
    type Guard<'w>;

    /// Walks the lent columns table by table, in ascending table order.
    #[doc(hidden)]
    type Cursor<'a>;

    /// Borrows the element's lent columns from the frame graph.
    #[doc(hidden)]
    fn acquire<'w>(ctx: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError>;

    /// Starts walking the borrowed columns.
    #[doc(hidden)]
    fn cursor<'a>(guard: &'a mut Self::Guard<'_>) -> Self::Cursor<'a>;

    /// The rows of table `id` (ids ascending between calls), whose row entities are `entities`;
    /// `None` if a needed column is missing.
    #[doc(hidden)]
    fn lent<'a>(
        cursor: &mut Self::Cursor<'a>,
        id: usize,
        entities: &'a [Entity],
    ) -> Option<Self::Slice<'a>>;
}

/// The slot of table `id` in lent columns, if it holds the table's rows.
fn lent_shared<T>(columns: &[Vec<T>], id: usize, rows: usize) -> Option<&[T]> {
    columns
        .get(id)
        .map(Vec::as_slice)
        .filter(|column| column.len() == rows)
}

/// Walks lent columns for writing: the slots are split off one table at a time.
#[doc(hidden)]
#[derive(Debug)]
pub struct MutCursor<'a, T> {
    rest: &'a mut [Vec<T>],
    /// The table id of `rest[0]`.
    offset: usize,
}

/// A [`QueryData`] that only reads, so it can run on a shared `&World`
/// ([`Query::iter`](crate::Query::iter), [`Query::par_for_each`](crate::Query::par_for_each)).
///
/// Implemented for `&T`, `Option<&T>`, [`Entity`] and tuples of them.
///
/// ```
/// use pragma_ecs::{Entity, Query, ReadOnlyQueryData, World};
///
/// fn count<Q: ReadOnlyQueryData>(world: &World) -> Result<usize, pragma_ecs::EcsError> {
///     Ok(Query::<Q>::new()?.iter(world)?.count())
/// }
///
/// let mut world = World::new();
/// let entity = world.spawn()?;
/// world.insert(entity, 1_u32)?;
/// assert_eq!(count::<(Entity, &u32)>(&world)?, 1);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub trait ReadOnlyQueryData: QueryData {}

/// Restricts the tables a [`Query`](crate::Query) visits without reading their data.
///
/// Implemented for `()` (no restriction), [`With<T>`], [`Without<T>`] and tuples of filters (up
/// to 8; all must hold). The trait is sealed (DECISIONS #12).
///
/// ```
/// use pragma_ecs::{Entity, Query, With, Without, World};
///
/// struct Enemy;
/// struct Dead;
///
/// let mut world = World::new();
/// let alive = world.spawn()?;
/// world.insert(alive, Enemy)?;
/// let dead = world.spawn()?;
/// world.insert(dead, Enemy)?;
/// world.insert(dead, Dead)?;
///
/// let mut living = Query::<Entity, (With<Enemy>, Without<Dead>)>::new()?;
/// assert_eq!(living.iter(&world)?.collect::<Vec<_>>(), [alive]);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub trait QueryFilter: Sealed {
    /// Whether a table with the component types `types` (sorted) passes the filter.
    #[doc(hidden)]
    fn matches(types: &[TypeId]) -> bool;
}

fn has<T: Component>(types: &[TypeId]) -> bool {
    types.binary_search(&TypeId::of::<T>()).is_ok()
}

impl<T: Component> Sealed for &T {}

impl<T: Component> QueryData for &T {
    type Item<'w> = &'w T;
    type State<'w> = Option<&'w [T]>;
    type Slice<'w> = &'w [T];
    type Iter<'w> = slice::Iter<'w, T>;

    fn declare(access: &mut Access) -> Result<(), EcsError> {
        access.add_component::<T>(AccessKind::Read)
    }

    fn matches(types: &[TypeId]) -> bool {
        has::<T>(types)
    }

    fn offer<'w>(state: &mut Self::State<'w>, ty: TypeId, column: &mut Option<ColumnRef<'w>>) {
        if ty == TypeId::of::<T>() && state.is_none() {
            *state = take_shared(column);
        }
    }

    fn finish<'w>(state: Self::State<'w>, entities: &'w [Entity]) -> Option<Self::Slice<'w>> {
        debug_assert!(state.is_none_or(|s| s.len() == entities.len()));
        state
    }

    fn split_at<'w>(slice: Self::Slice<'w>, mid: usize) -> (Self::Slice<'w>, Self::Slice<'w>) {
        slice.split_at(mid.min(slice.len()))
    }

    fn iter<'w>(slice: Self::Slice<'w>) -> Self::Iter<'w> {
        slice.iter()
    }

    type Guard<'w> = graph::Res<'w, Columns<T>>;
    type Cursor<'a> = &'a [Vec<T>];

    fn acquire<'w>(ctx: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError> {
        ctx.read::<Columns<T>>().map_err(EcsError::graph)
    }

    fn cursor<'a>(guard: &'a mut Self::Guard<'_>) -> Self::Cursor<'a> {
        guard.tables()
    }

    fn lent<'a>(
        cursor: &mut Self::Cursor<'a>,
        id: usize,
        entities: &'a [Entity],
    ) -> Option<Self::Slice<'a>> {
        lent_shared(cursor, id, entities.len())
    }
}

impl<T: Component> ReadOnlyQueryData for &T {}

impl<T: Component> Sealed for &mut T {}

impl<T: Component> QueryData for &mut T {
    type Item<'w> = &'w mut T;
    type State<'w> = Option<&'w mut [T]>;
    type Slice<'w> = &'w mut [T];
    type Iter<'w> = slice::IterMut<'w, T>;

    fn declare(access: &mut Access) -> Result<(), EcsError> {
        access.add_component::<T>(AccessKind::Write)
    }

    fn matches(types: &[TypeId]) -> bool {
        has::<T>(types)
    }

    fn offer<'w>(state: &mut Self::State<'w>, ty: TypeId, column: &mut Option<ColumnRef<'w>>) {
        if ty == TypeId::of::<T>() && state.is_none() {
            *state = take_unique(column);
        }
    }

    fn finish<'w>(state: Self::State<'w>, entities: &'w [Entity]) -> Option<Self::Slice<'w>> {
        debug_assert!(state.as_ref().is_none_or(|s| s.len() == entities.len()));
        state
    }

    fn split_at<'w>(slice: Self::Slice<'w>, mid: usize) -> (Self::Slice<'w>, Self::Slice<'w>) {
        let mid = mid.min(slice.len());
        slice.split_at_mut(mid)
    }

    fn iter<'w>(slice: Self::Slice<'w>) -> Self::Iter<'w> {
        slice.iter_mut()
    }

    type Guard<'w> = graph::ResMut<'w, Columns<T>>;
    type Cursor<'a> = MutCursor<'a, T>;

    fn acquire<'w>(ctx: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError> {
        ctx.write::<Columns<T>>().map_err(EcsError::graph)
    }

    fn cursor<'a>(guard: &'a mut Self::Guard<'_>) -> Self::Cursor<'a> {
        MutCursor {
            rest: guard.tables_mut(),
            offset: 0,
        }
    }

    fn lent<'a>(
        cursor: &mut Self::Cursor<'a>,
        id: usize,
        entities: &'a [Entity],
    ) -> Option<Self::Slice<'a>> {
        let rest = mem::take(&mut cursor.rest);
        let skip = id.checked_sub(cursor.offset)?.min(rest.len());
        let (column, rest) = rest.split_at_mut(skip).1.split_first_mut()?;
        cursor.rest = rest;
        cursor.offset = id.saturating_add(1);
        let column = column.as_mut_slice();
        (column.len() == entities.len()).then_some(column)
    }
}

/// The rows of an `Option<&T>` element: the column, if the table has one, and the row count.
#[doc(hidden)]
#[derive(Debug)]
pub struct OptionSlice<'w, T> {
    values: Option<&'w [T]>,
    len: usize,
}

/// Iterates an [`OptionSlice`].
#[doc(hidden)]
#[derive(Debug)]
pub struct OptionIter<'w, T> {
    values: Option<slice::Iter<'w, T>>,
    remaining: usize,
}

impl<'w, T> Iterator for OptionIter<'w, T> {
    type Item = Option<&'w T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.remaining = self.remaining.checked_sub(1)?;
        Some(self.values.as_mut().and_then(Iterator::next))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<T: Component> Sealed for Option<&T> {}

impl<T: Component> QueryData for Option<&T> {
    type Item<'w> = Option<&'w T>;
    type State<'w> = Option<&'w [T]>;
    type Slice<'w> = OptionSlice<'w, T>;
    type Iter<'w> = OptionIter<'w, T>;

    fn declare(access: &mut Access) -> Result<(), EcsError> {
        access.add_component::<T>(AccessKind::Read)
    }

    fn matches(_: &[TypeId]) -> bool {
        true
    }

    fn offer<'w>(state: &mut Self::State<'w>, ty: TypeId, column: &mut Option<ColumnRef<'w>>) {
        <&T>::offer(state, ty, column);
    }

    fn finish<'w>(state: Self::State<'w>, entities: &'w [Entity]) -> Option<Self::Slice<'w>> {
        debug_assert!(state.is_none_or(|s| s.len() == entities.len()));
        Some(OptionSlice {
            values: state,
            len: entities.len(),
        })
    }

    fn split_at<'w>(slice: Self::Slice<'w>, mid: usize) -> (Self::Slice<'w>, Self::Slice<'w>) {
        let mid = mid.min(slice.len);
        let rest = slice.len.saturating_sub(mid);
        let (left, right) = match slice.values {
            Some(values) => {
                let (left, right) = values.split_at(mid.min(values.len()));
                (Some(left), Some(right))
            }
            None => (None, None),
        };
        (
            OptionSlice {
                values: left,
                len: mid,
            },
            OptionSlice {
                values: right,
                len: rest,
            },
        )
    }

    fn iter<'w>(slice: Self::Slice<'w>) -> Self::Iter<'w> {
        OptionIter {
            values: slice.values.map(<[T]>::iter),
            remaining: slice.len,
        }
    }

    type Guard<'w> = graph::Res<'w, Columns<T>>;
    type Cursor<'a> = &'a [Vec<T>];

    fn acquire<'w>(ctx: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError> {
        <&T>::acquire(ctx)
    }

    fn cursor<'a>(guard: &'a mut Self::Guard<'_>) -> Self::Cursor<'a> {
        guard.tables()
    }

    fn lent<'a>(
        cursor: &mut Self::Cursor<'a>,
        id: usize,
        entities: &'a [Entity],
    ) -> Option<Self::Slice<'a>> {
        // A table without `T` has an empty slot; with rows, its length tells them apart.
        Some(OptionSlice {
            values: lent_shared(cursor, id, entities.len()),
            len: entities.len(),
        })
    }
}

impl<T: Component> ReadOnlyQueryData for Option<&T> {}

impl Sealed for Entity {}

impl QueryData for Entity {
    type Item<'w> = Entity;
    type State<'w> = ();
    type Slice<'w> = &'w [Entity];
    type Iter<'w> = Copied<slice::Iter<'w, Entity>>;

    fn declare(_: &mut Access) -> Result<(), EcsError> {
        Ok(())
    }

    fn matches(_: &[TypeId]) -> bool {
        true
    }

    fn offer<'w>(_: &mut Self::State<'w>, _: TypeId, _: &mut Option<ColumnRef<'w>>) {}

    fn finish<'w>((): Self::State<'w>, entities: &'w [Entity]) -> Option<Self::Slice<'w>> {
        Some(entities)
    }

    fn split_at<'w>(slice: Self::Slice<'w>, mid: usize) -> (Self::Slice<'w>, Self::Slice<'w>) {
        slice.split_at(mid.min(slice.len()))
    }

    fn iter<'w>(slice: Self::Slice<'w>) -> Self::Iter<'w> {
        slice.iter().copied()
    }

    type Guard<'w> = ();
    type Cursor<'a> = ();

    fn acquire<'w>(_: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError> {
        Ok(())
    }

    fn cursor<'a>((): &'a mut Self::Guard<'_>) -> Self::Cursor<'a> {}

    fn lent<'a>(
        (): &mut Self::Cursor<'a>,
        _: usize,
        entities: &'a [Entity],
    ) -> Option<Self::Slice<'a>> {
        Some(entities)
    }
}

impl ReadOnlyQueryData for Entity {}

/// Iterates the element iterators of a tuple query in lockstep.
#[doc(hidden)]
#[derive(Debug)]
pub struct TupleIter<I>(I);

macro_rules! tuple_query {
    ($($name:ident $index:tt),+) => {
        // Both data and filter tuples.
        impl<$($name),+> Sealed for ($($name,)+) {}

        impl<$($name: QueryData),+> QueryData for ($($name,)+) {
            type Item<'w> = ($($name::Item<'w>,)+);
            type State<'w> = ($($name::State<'w>,)+);
            type Slice<'w> = ($($name::Slice<'w>,)+);
            type Iter<'w> = TupleIter<($($name::Iter<'w>,)+)>;

            fn declare(access: &mut Access) -> Result<(), EcsError> {
                $($name::declare(access)?;)+
                Ok(())
            }

            fn matches(types: &[TypeId]) -> bool {
                $($name::matches(types))&&+
            }

            fn offer<'w>(
                state: &mut Self::State<'w>,
                ty: TypeId,
                column: &mut Option<ColumnRef<'w>>,
            ) {
                $($name::offer(&mut state.$index, ty, column);)+
            }

            fn finish<'w>(
                state: Self::State<'w>,
                entities: &'w [Entity],
            ) -> Option<Self::Slice<'w>> {
                Some(($($name::finish(state.$index, entities)?,)+))
            }

            fn split_at<'w>(
                slice: Self::Slice<'w>,
                mid: usize,
            ) -> (Self::Slice<'w>, Self::Slice<'w>) {
                let halves = ($($name::split_at(slice.$index, mid),)+);
                (($(halves.$index.0,)+), ($(halves.$index.1,)+))
            }

            fn iter<'w>(slice: Self::Slice<'w>) -> Self::Iter<'w> {
                TupleIter(($($name::iter(slice.$index),)+))
            }

            type Guard<'w> = ($($name::Guard<'w>,)+);
            type Cursor<'a> = ($($name::Cursor<'a>,)+);

            fn acquire<'w>(ctx: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError> {
                Ok(($($name::acquire(ctx)?,)+))
            }

            fn cursor<'a>(guard: &'a mut Self::Guard<'_>) -> Self::Cursor<'a> {
                ($($name::cursor(&mut guard.$index),)+)
            }

            fn lent<'a>(
                cursor: &mut Self::Cursor<'a>,
                id: usize,
                entities: &'a [Entity],
            ) -> Option<Self::Slice<'a>> {
                Some(($($name::lent(&mut cursor.$index, id, entities)?,)+))
            }
        }

        impl<$($name: ReadOnlyQueryData),+> ReadOnlyQueryData for ($($name,)+) {}

        impl<$($name: Iterator),+> Iterator for TupleIter<($($name,)+)> {
            type Item = ($($name::Item,)+);

            fn next(&mut self) -> Option<Self::Item> {
                Some(($(self.0.$index.next()?,)+))
            }
        }

        impl<$($name: QueryFilter),+> QueryFilter for ($($name,)+) {
            fn matches(types: &[TypeId]) -> bool {
                $($name::matches(types))&&+
            }
        }
    };
}

tuple_query!(A 0);
tuple_query!(A 0, B 1);
tuple_query!(A 0, B 1, C 2);
tuple_query!(A 0, B 1, C 2, D 3);
tuple_query!(A 0, B 1, C 2, D 3, E 4);
tuple_query!(A 0, B 1, C 2, D 3, E 4, F 5);
tuple_query!(A 0, B 1, C 2, D 3, E 4, F 5, G 6);
tuple_query!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);

impl Sealed for () {}

impl QueryFilter for () {
    fn matches(_: &[TypeId]) -> bool {
        true
    }
}

/// A [`QueryFilter`]: only entities that have a `T` component, whose value is not read.
///
/// ```
/// use pragma_ecs::{Query, With, World};
///
/// struct Health(u32);
/// struct Player;
///
/// let mut world = World::new();
/// let player = world.spawn()?;
/// world.insert(player, Health(10))?;
/// world.insert(player, Player)?;
/// let monster = world.spawn()?;
/// world.insert(monster, Health(50))?;
///
/// let mut players = Query::<&Health, With<Player>>::new()?;
/// let total: u32 = players.iter(&world)?.map(|h| h.0).sum();
/// assert_eq!(total, 10);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct With<T: Component>(PhantomData<fn() -> T>);

impl<T: Component> fmt::Debug for With<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "With<{}>", type_name::<T>())
    }
}

impl<T: Component> Sealed for With<T> {}

impl<T: Component> QueryFilter for With<T> {
    fn matches(types: &[TypeId]) -> bool {
        has::<T>(types)
    }
}

/// A [`QueryFilter`]: only entities that do not have a `T` component.
///
/// ```
/// use pragma_ecs::{Query, Without, World};
///
/// struct Health(u32);
/// struct Dead;
///
/// let mut world = World::new();
/// let alive = world.spawn()?;
/// world.insert(alive, Health(10))?;
/// let dead = world.spawn()?;
/// world.insert(dead, Health(0))?;
/// world.insert(dead, Dead)?;
///
/// let mut living = Query::<&mut Health, Without<Dead>>::new()?;
/// for health in living.iter_mut(&mut world)? {
///     health.0 += 1;
/// }
/// assert_eq!(world.get::<Health>(alive).map(|h| h.0), Some(11));
/// assert_eq!(world.get::<Health>(dead).map(|h| h.0), Some(0));
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct Without<T: Component>(PhantomData<fn() -> T>);

impl<T: Component> fmt::Debug for Without<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Without<{}>", type_name::<T>())
    }
}

impl<T: Component> Sealed for Without<T> {}

impl<T: Component> QueryFilter for Without<T> {
    fn matches(types: &[TypeId]) -> bool {
        !has::<T>(types)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shared_column_is_not_handed_out_for_writing() {
        let mut values = vec![1_u32, 2];
        let mut column = Some(ColumnRef::Unique(&mut values));
        assert_eq!(take_shared::<u32>(&mut column), Some(&[1, 2][..]));
        assert_eq!(take_shared::<u32>(&mut column), Some(&[1, 2][..]));
        assert!(take_unique::<u32>(&mut column).is_none());
        assert!(matches!(column, Some(ColumnRef::Shared(_))));
        assert!(take_shared::<u64>(&mut column).is_none());
    }

    #[test]
    fn option_slices_split_with_and_without_a_column() {
        let values = [1, 2, 3];
        let present = OptionSlice {
            values: Some(&values[..]),
            len: 3,
        };
        let (left, right) = <Option<&i32> as QueryData>::split_at(present, 2);
        let left: Vec<_> = <Option<&i32> as QueryData>::iter(left).collect();
        let right: Vec<_> = <Option<&i32> as QueryData>::iter(right).collect();
        assert_eq!(left, [Some(&1), Some(&2)]);
        assert_eq!(right, [Some(&3)]);

        let absent = OptionSlice::<i32> {
            values: None,
            len: 3,
        };
        let (left, right) = <Option<&i32> as QueryData>::split_at(absent, 5);
        assert_eq!(<Option<&i32> as QueryData>::iter(left).count(), 3);
        assert_eq!(
            <Option<&i32> as QueryData>::iter(right).size_hint(),
            (0, Some(0))
        );
    }
}
