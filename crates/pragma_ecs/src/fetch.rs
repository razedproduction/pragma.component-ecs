//! What a query yields ([`QueryData`]) and which entities it visits ([`QueryFilter`]).
//!
//! Borrowing the columns of one table without `unsafe` (DECISIONS #13): the table's columns are
//! walked once with `iter_mut` and offered, as [`ColumnRef`]s, to every element of the query. A
//! write takes the exclusive reference, a read turns it into a shared one that further reads
//! copy. Each element then downcasts its column into a slice, and the rows of a table are the
//! element slices iterated in lockstep.
//!
//! Row filters ([`Added`], [`Changed`]) do not take part in that pass: they fill a mask of the
//! passing rows from the tick columns before the data columns are borrowed (DECISIONS #28).

use std::any::{Any, TypeId, type_name};
use std::fmt;
use std::iter::Copied;
use std::marker::PhantomData;
use std::{mem, slice};

use pragma_core::graph::{self, TaskContext};

use crate::access::{Access, AccessKind};
use crate::change::{Mut, MutIter, MutSlice};
use crate::component::{ColumnData, Ticks};
use crate::lend::Columns;
use crate::{Component, EcsError, Entity};

mod sealed {
    /// Keeps [`QueryData`](super::QueryData) and [`QueryFilter`](super::QueryFilter) closed:
    /// their machinery is internal and may change (DECISIONS #12).
    pub trait Sealed {}
}

pub(crate) use sealed::Sealed;

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
    shared
        .downcast_ref::<ColumnData<T>>()
        .map(ColumnData::values)
}

/// Takes `column` for writing; `None` (the column left as is) if a reader already shares it.
fn take_unique<'w, T: Component>(
    column: &mut Option<ColumnRef<'w>>,
) -> Option<(&'w mut [T], &'w mut [u64])> {
    match column.take()? {
        ColumnRef::Unique(c) => c.downcast_mut::<ColumnData<T>>().map(ColumnData::parts_mut),
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
/// | `&mut T` | [`Mut<T>`] | write `T` | with `T` |
/// | `Option<&T>` | `Option<&T>` | read `T` | all |
/// | [`Entity`] | the entity id | none | all |
/// | `(Q1, Q2, …)`, up to 8 | a tuple of the items | the union | all of them |
///
/// `&mut T` yields a [`Mut<T>`], which dereferences to `T` and records a change when it is
/// written (DECISIONS #28). The trait is sealed: its methods are internal (DECISIONS #12).
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
/// for (entity, mut position, name) in query.iter_mut(&mut world)? {
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

    /// Whether the element uses the component type `ty`.
    #[doc(hidden)]
    fn holds(ty: TypeId) -> bool;

    /// Whether a table with the component types `types` (sorted) has what the element needs.
    #[doc(hidden)]
    fn matches(types: &[TypeId]) -> bool;

    /// Offers the column of type `ty`; the element takes it if it needs it.
    #[doc(hidden)]
    fn offer<'w>(state: &mut Self::State<'w>, ty: TypeId, column: &mut Option<ColumnRef<'w>>);

    /// The rows of the table once every column was offered; `None` if a needed column is
    /// missing. A write is stamped with the change tick `tick`.
    #[doc(hidden)]
    fn finish<'w>(
        state: Self::State<'w>,
        entities: &'w [Entity],
        tick: u64,
    ) -> Option<Self::Slice<'w>>;

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

    /// The lent change ticks of component type `ty` in table `table`, if the element holds them.
    #[doc(hidden)]
    fn lent_ticks<'a>(guard: &'a Self::Guard<'_>, ty: TypeId, table: usize) -> Option<Ticks<'a>>;

    /// Starts walking the borrowed columns.
    #[doc(hidden)]
    fn cursor<'a>(guard: &'a mut Self::Guard<'_>) -> Self::Cursor<'a>;

    /// The rows of table `id` (ids ascending between calls), whose row entities are `entities`;
    /// `None` if a needed column is missing. A write is stamped with `tick`.
    #[doc(hidden)]
    fn lent<'a>(
        cursor: &mut Self::Cursor<'a>,
        id: usize,
        entities: &'a [Entity],
        tick: u64,
    ) -> Option<Self::Slice<'a>>;
}

/// The slot of table `id` in lent columns, if it holds the table's rows.
fn lent_shared<T>(columns: &[ColumnData<T>], id: usize, rows: usize) -> Option<&[T]> {
    columns
        .get(id)
        .map(ColumnData::values)
        .filter(|column| column.len() == rows)
}

/// The lent ticks of `T` in table `table`, if `ty` is `T`.
fn lent_ticks_of<T: Component>(
    columns: &Columns<T>,
    ty: TypeId,
    table: usize,
) -> Option<Ticks<'_>> {
    if ty == TypeId::of::<T>() {
        columns.tables().get(table).map(ColumnData::ticks)
    } else {
        None
    }
}

/// Walks lent columns for writing: the slots are split off one table at a time.
#[doc(hidden)]
#[derive(Debug)]
pub struct MutCursor<'a, T> {
    rest: &'a mut [ColumnData<T>],
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

/// Restricts the entities a [`Query`](crate::Query) visits.
///
/// Implemented for `()` (no restriction), [`With<T>`] and [`Without<T>`] (whole tables, without
/// reading data), [`Added<T>`] and [`Changed<T>`] (rows, by their change ticks, DECISIONS #28),
/// and tuples of filters (up to 8; all must hold). The trait is sealed (DECISIONS #12).
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
    /// Whether the filter keeps every row of a matching table (no row filter inside).
    #[doc(hidden)]
    const DENSE: bool;

    /// Whether a table with the component types `types` (sorted) passes the filter.
    #[doc(hidden)]
    fn matches(types: &[TypeId]) -> bool;

    /// Adds the filter's reads to `access`; a type the query's data already uses is merged.
    #[doc(hidden)]
    fn declare(access: &mut Access) -> Result<(), EcsError>;

    /// Clears the entries of `mask` (one per row of a matching table) whose row fails the
    /// filter; `ticks` looks up the change ticks of the table's column of a type.
    #[doc(hidden)]
    fn mask<'a>(ticks: &dyn Fn(TypeId) -> Option<Ticks<'a>>, last_run: u64, mask: &mut [bool]);

    /// The borrows of lent tick columns a system holds during a frame.
    #[doc(hidden)]
    type Guard<'w>;

    /// Borrows the lent columns the filter reads that the data `Q` does not already hold.
    #[doc(hidden)]
    fn acquire<'w, Q: QueryData>(ctx: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError>;

    /// The lent change ticks of component type `ty` in table `table`, if the filter holds them.
    #[doc(hidden)]
    fn lent_ticks<'a>(guard: &'a Self::Guard<'_>, ty: TypeId, table: usize) -> Option<Ticks<'a>>;
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

    fn holds(ty: TypeId) -> bool {
        ty == TypeId::of::<T>()
    }

    fn matches(types: &[TypeId]) -> bool {
        has::<T>(types)
    }

    fn offer<'w>(state: &mut Self::State<'w>, ty: TypeId, column: &mut Option<ColumnRef<'w>>) {
        if ty == TypeId::of::<T>() && state.is_none() {
            *state = take_shared(column);
        }
    }

    fn finish<'w>(
        state: Self::State<'w>,
        entities: &'w [Entity],
        _: u64,
    ) -> Option<Self::Slice<'w>> {
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
    type Cursor<'a> = &'a [ColumnData<T>];

    fn acquire<'w>(ctx: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError> {
        ctx.read::<Columns<T>>().map_err(EcsError::graph)
    }

    fn lent_ticks<'a>(guard: &'a Self::Guard<'_>, ty: TypeId, table: usize) -> Option<Ticks<'a>> {
        lent_ticks_of(guard, ty, table)
    }

    fn cursor<'a>(guard: &'a mut Self::Guard<'_>) -> Self::Cursor<'a> {
        guard.tables()
    }

    fn lent<'a>(
        cursor: &mut Self::Cursor<'a>,
        id: usize,
        entities: &'a [Entity],
        _: u64,
    ) -> Option<Self::Slice<'a>> {
        lent_shared(cursor, id, entities.len())
    }
}

impl<T: Component> ReadOnlyQueryData for &T {}

impl<T: Component> Sealed for &mut T {}

impl<T: Component> QueryData for &mut T {
    type Item<'w> = Mut<'w, T>;
    type State<'w> = Option<(&'w mut [T], &'w mut [u64])>;
    type Slice<'w> = MutSlice<'w, T>;
    type Iter<'w> = MutIter<'w, T>;

    fn declare(access: &mut Access) -> Result<(), EcsError> {
        access.add_component::<T>(AccessKind::Write)
    }

    fn holds(ty: TypeId) -> bool {
        ty == TypeId::of::<T>()
    }

    fn matches(types: &[TypeId]) -> bool {
        has::<T>(types)
    }

    fn offer<'w>(state: &mut Self::State<'w>, ty: TypeId, column: &mut Option<ColumnRef<'w>>) {
        if ty == TypeId::of::<T>() && state.is_none() {
            *state = take_unique(column);
        }
    }

    fn finish<'w>(
        state: Self::State<'w>,
        entities: &'w [Entity],
        tick: u64,
    ) -> Option<Self::Slice<'w>> {
        let (values, ticks) = state?;
        debug_assert_eq!(values.len(), entities.len());
        MutSlice::new(values, ticks, tick)
    }

    fn split_at<'w>(slice: Self::Slice<'w>, mid: usize) -> (Self::Slice<'w>, Self::Slice<'w>) {
        slice.split_at(mid)
    }

    fn iter<'w>(slice: Self::Slice<'w>) -> Self::Iter<'w> {
        slice.into_iter()
    }

    type Guard<'w> = graph::ResMut<'w, Columns<T>>;
    type Cursor<'a> = MutCursor<'a, T>;

    fn acquire<'w>(ctx: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError> {
        ctx.write::<Columns<T>>().map_err(EcsError::graph)
    }

    fn lent_ticks<'a>(guard: &'a Self::Guard<'_>, ty: TypeId, table: usize) -> Option<Ticks<'a>> {
        lent_ticks_of(guard, ty, table)
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
        tick: u64,
    ) -> Option<Self::Slice<'a>> {
        let rest = mem::take(&mut cursor.rest);
        let skip = id.checked_sub(cursor.offset)?.min(rest.len());
        let (column, rest) = rest.split_at_mut(skip).1.split_first_mut()?;
        cursor.rest = rest;
        cursor.offset = id.saturating_add(1);
        if column.len() != entities.len() {
            return None;
        }
        let (values, ticks) = column.parts_mut();
        MutSlice::new(values, ticks, tick)
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

    fn holds(ty: TypeId) -> bool {
        ty == TypeId::of::<T>()
    }

    fn matches(_: &[TypeId]) -> bool {
        true
    }

    fn offer<'w>(state: &mut Self::State<'w>, ty: TypeId, column: &mut Option<ColumnRef<'w>>) {
        <&T>::offer(state, ty, column);
    }

    fn finish<'w>(
        state: Self::State<'w>,
        entities: &'w [Entity],
        _: u64,
    ) -> Option<Self::Slice<'w>> {
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
    type Cursor<'a> = &'a [ColumnData<T>];

    fn acquire<'w>(ctx: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError> {
        <&T>::acquire(ctx)
    }

    fn lent_ticks<'a>(guard: &'a Self::Guard<'_>, ty: TypeId, table: usize) -> Option<Ticks<'a>> {
        lent_ticks_of(guard, ty, table)
    }

    fn cursor<'a>(guard: &'a mut Self::Guard<'_>) -> Self::Cursor<'a> {
        guard.tables()
    }

    fn lent<'a>(
        cursor: &mut Self::Cursor<'a>,
        id: usize,
        entities: &'a [Entity],
        _: u64,
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

    fn holds(_: TypeId) -> bool {
        false
    }

    fn matches(_: &[TypeId]) -> bool {
        true
    }

    fn offer<'w>(_: &mut Self::State<'w>, _: TypeId, _: &mut Option<ColumnRef<'w>>) {}

    fn finish<'w>((): Self::State<'w>, entities: &'w [Entity], _: u64) -> Option<Self::Slice<'w>> {
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

    fn lent_ticks<'a>((): &'a Self::Guard<'_>, _: TypeId, _: usize) -> Option<Ticks<'a>> {
        None
    }

    fn cursor<'a>((): &'a mut Self::Guard<'_>) -> Self::Cursor<'a> {}

    fn lent<'a>(
        (): &mut Self::Cursor<'a>,
        _: usize,
        entities: &'a [Entity],
        _: u64,
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

            fn holds(ty: TypeId) -> bool {
                $($name::holds(ty))||+
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
                tick: u64,
            ) -> Option<Self::Slice<'w>> {
                Some(($($name::finish(state.$index, entities, tick)?,)+))
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

            fn lent_ticks<'a>(
                guard: &'a Self::Guard<'_>,
                ty: TypeId,
                table: usize,
            ) -> Option<Ticks<'a>> {
                None$(.or_else(|| $name::lent_ticks(&guard.$index, ty, table)))+
            }

            fn cursor<'a>(guard: &'a mut Self::Guard<'_>) -> Self::Cursor<'a> {
                ($($name::cursor(&mut guard.$index),)+)
            }

            fn lent<'a>(
                cursor: &mut Self::Cursor<'a>,
                id: usize,
                entities: &'a [Entity],
                tick: u64,
            ) -> Option<Self::Slice<'a>> {
                Some(($($name::lent(&mut cursor.$index, id, entities, tick)?,)+))
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
            const DENSE: bool = $($name::DENSE)&&+;

            fn matches(types: &[TypeId]) -> bool {
                $($name::matches(types))&&+
            }

            fn declare(access: &mut Access) -> Result<(), EcsError> {
                $($name::declare(access)?;)+
                Ok(())
            }

            fn mask<'a>(
                ticks: &dyn Fn(TypeId) -> Option<Ticks<'a>>,
                last_run: u64,
                mask: &mut [bool],
            ) {
                $($name::mask(ticks, last_run, mask);)+
            }

            type Guard<'w> = ($($name::Guard<'w>,)+);

            fn acquire<'w, Q: QueryData>(
                ctx: &TaskContext<'w>,
            ) -> Result<Self::Guard<'w>, EcsError> {
                Ok(($($name::acquire::<Q>(ctx)?,)+))
            }

            fn lent_ticks<'a>(
                guard: &'a Self::Guard<'_>,
                ty: TypeId,
                table: usize,
            ) -> Option<Ticks<'a>> {
                None$(.or_else(|| $name::lent_ticks(&guard.$index, ty, table)))+
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

/// The filter methods of filters that look at whole tables only.
macro_rules! table_filter {
    () => {
        const DENSE: bool = true;

        fn declare(_: &mut Access) -> Result<(), EcsError> {
            Ok(())
        }

        fn mask<'a>(_: &dyn Fn(TypeId) -> Option<Ticks<'a>>, _: u64, _: &mut [bool]) {}

        type Guard<'w> = ();

        fn acquire<'w, Q: QueryData>(_: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError> {
            Ok(())
        }

        fn lent_ticks<'a>((): &'a Self::Guard<'_>, _: TypeId, _: usize) -> Option<Ticks<'a>> {
            None
        }
    };
}

impl QueryFilter for () {
    table_filter!();

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
    table_filter!();

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
/// for mut health in living.iter_mut(&mut world)? {
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
    table_filter!();

    fn matches(types: &[TypeId]) -> bool {
        !has::<T>(types)
    }
}

/// The filter methods of [`Added`] and [`Changed`]: a row passes if its `$which` tick is newer
/// than the tick of the query's last run.
macro_rules! tick_filter {
    ($which:ident) => {
        const DENSE: bool = false;

        fn matches(types: &[TypeId]) -> bool {
            has::<T>(types)
        }

        fn declare(access: &mut Access) -> Result<(), EcsError> {
            access.add_filter_read::<T>()
        }

        fn mask<'a>(ticks: &dyn Fn(TypeId) -> Option<Ticks<'a>>, last_run: u64, mask: &mut [bool]) {
            match ticks(TypeId::of::<T>()) {
                Some(ticks) if ticks.$which.len() == mask.len() => {
                    for (keep, tick) in mask.iter_mut().zip(ticks.$which) {
                        // `&=`, not `&&`: no branch, so the loop vectorizes.
                        *keep &= *tick > last_run;
                    }
                }
                // Unreachable: a matching table has the column.
                _ => mask.fill(false),
            }
        }

        type Guard<'w> = Option<graph::Res<'w, Columns<T>>>;

        fn acquire<'w, Q: QueryData>(ctx: &TaskContext<'w>) -> Result<Self::Guard<'w>, EcsError> {
            if Q::holds(TypeId::of::<T>()) {
                // The data's own guard has the ticks; a second borrow would conflict with it.
                Ok(None)
            } else {
                ctx.read::<Columns<T>>().map(Some).map_err(EcsError::graph)
            }
        }

        fn lent_ticks<'a>(
            guard: &'a Self::Guard<'_>,
            ty: TypeId,
            table: usize,
        ) -> Option<Ticks<'a>> {
            lent_ticks_of(guard.as_ref()?, ty, table)
        }
    };
}

/// A [`QueryFilter`]: only entities whose `T` component was added since the query (or the
/// system) last ran (DECISIONS #27, #28). Reads the change ticks of `T`.
///
/// A component counts as added when it is inserted into an entity that did not have one,
/// directly or by a command; replacing it does not count. On its first run a query sees every
/// existing `T` as added.
///
/// ```
/// use pragma_ecs::{Added, Entity, Query, World};
///
/// struct Player;
///
/// let mut world = World::new();
/// let first = world.spawn()?;
/// world.insert(first, Player)?;
///
/// let mut joined = Query::<Entity, Added<Player>>::new()?;
/// assert_eq!(joined.iter(&world)?.collect::<Vec<_>>(), [first]);
/// assert_eq!(joined.iter(&world)?.count(), 0); // nothing new since
///
/// let second = world.spawn()?;
/// world.insert(second, Player)?;
/// world.insert(first, Player)?; // a replacement, not an addition
/// assert_eq!(joined.iter(&world)?.collect::<Vec<_>>(), [second]);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct Added<T: Component>(PhantomData<fn() -> T>);

impl<T: Component> fmt::Debug for Added<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Added<{}>", type_name::<T>())
    }
}

impl<T: Component> Sealed for Added<T> {}

impl<T: Component> QueryFilter for Added<T> {
    tick_filter!(added);
}

/// A [`QueryFilter`]: only entities whose `T` component was added or changed since the query
/// (or the system) last ran (DECISIONS #27, #28). Reads the change ticks of `T`.
///
/// A component counts as changed when it is written through a [`Mut<T>`], borrowed with
/// [`World::get_mut`](crate::World::get_mut), replaced with
/// [`World::insert`](crate::World::insert), or added. A query does not see its own changes on
/// its next run. `Query<&mut T, Changed<T>>` is allowed: the filter is evaluated before the rows
/// are borrowed.
///
/// ```
/// use pragma_ecs::{Changed, Entity, Query, World};
///
/// struct Health(u32);
///
/// let mut world = World::new();
/// let a = world.spawn()?;
/// world.insert(a, Health(10))?;
/// let b = world.spawn()?;
/// world.insert(b, Health(10))?;
///
/// let mut hurt = Query::<Entity, Changed<Health>>::new()?;
/// assert_eq!(hurt.iter(&world)?.count(), 2); // first run: everything is new
///
/// if let Some(health) = world.get_mut::<Health>(b) {
///     health.0 -= 3;
/// }
/// assert_eq!(hurt.iter(&world)?.collect::<Vec<_>>(), [b]);
/// assert_eq!(hurt.iter(&world)?.count(), 0);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct Changed<T: Component>(PhantomData<fn() -> T>);

impl<T: Component> fmt::Debug for Changed<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Changed<{}>", type_name::<T>())
    }
}

impl<T: Component> Sealed for Changed<T> {}

impl<T: Component> QueryFilter for Changed<T> {
    tick_filter!(changed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(values: &[u32]) -> ColumnData<u32> {
        let mut column = ColumnData::default();
        for (tick, value) in (1..).zip(values) {
            column.push(*value, tick, tick);
        }
        column
    }

    #[test]
    fn a_shared_column_is_not_handed_out_for_writing() {
        let mut values = column(&[1, 2]);
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

    #[test]
    fn tick_filters_clear_failing_rows() {
        let ticks = column(&[7, 8, 9]);
        let lookup = |ty: TypeId| (ty == TypeId::of::<u32>()).then(|| ticks.ticks());
        let mut mask = [true, true, false];
        <Changed<u32> as QueryFilter>::mask(&lookup, 1, &mut mask);
        assert_eq!(mask, [false, true, false]);
        let mut mask = [true; 3];
        <(Added<u32>, With<u8>) as QueryFilter>::mask(&lookup, 2, &mut mask);
        assert_eq!(mask, [false, false, true]);
        // A missing column clears everything.
        let mut mask = [true; 3];
        <Changed<u64> as QueryFilter>::mask(&lookup, 0, &mut mask);
        assert_eq!(mask, [false; 3]);
        const { assert!(<(With<u8>, Without<u16>) as QueryFilter>::DENSE) };
        const { assert!(!<(With<u8>, Changed<u16>) as QueryFilter>::DENSE) };
        assert_eq!(format!("{:?}", Added::<u8>(PhantomData)), "Added<u8>");
    }
}
