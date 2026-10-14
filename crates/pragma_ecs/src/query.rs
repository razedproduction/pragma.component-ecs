//! Queries: typed iteration over the entities whose components match (DECISIONS #12–#15).

use std::any::TypeId;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::slice;

use pragma_core::jobs::{Grain, ThreadPool};

use crate::access::Access;
use crate::archetype::Archetype;
use crate::fetch::{ColumnRef, QueryData, QueryFilter, ReadOnlyQueryData};
use crate::{EcsError, Entity, World};

/// A typed query: the entities that have the components `Q` asks for and pass the filter `F`.
///
/// `Q` is what is yielded per entity ([`QueryData`]: `&T`, `&mut T`, `Option<&T>`, [`Entity`]
/// or a tuple of them); `F` restricts the tables visited ([`QueryFilter`]: [`With`],
/// [`Without`] or a tuple of them; `()` by default).
///
/// A `Query` is built once and reused: it does not borrow the world. [`new`](Self::new) checks
/// the declared [`Access`] and rejects aliasing (`(&A, &mut A)`) as
/// [`EcsError::ConflictingAccess`]. Every call remembers which archetype tables match, so the
/// next call only checks the tables created since (DECISIONS #12). Using a query with another
/// world is allowed; the cache is then rebuilt.
///
/// Rows are visited table by table, in table creation order, and within a table in row order.
/// That order is deterministic for the same sequence of world operations, but it is not the
/// order of spawning.
///
/// [`With`]: crate::With
/// [`Without`]: crate::Without
///
/// ```
/// use pragma_ecs::{Query, Without, World};
/// use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
///
/// #[derive(Debug, PartialEq)]
/// struct Position(f32);
/// struct Velocity(f32);
/// struct Frozen;
///
/// let mut world = World::new();
/// for i in 0..100 {
///     let entity = world.spawn()?;
///     world.insert(entity, Position(0.0))?;
///     world.insert(entity, Velocity(1.0))?;
///     if i % 2 == 0 {
///         world.insert(entity, Frozen)?;
///     }
/// }
///
/// let mut movement = Query::<(&mut Position, &Velocity), Without<Frozen>>::new()?;
/// for (position, velocity) in movement.iter_mut(&mut world)? {
///     position.0 += velocity.0;
/// }
///
/// // The same query, its rows split into chunks over the pragma.core thread pool.
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
/// movement.par_for_each_mut(&mut world, &pool, Grain::Auto, |(position, velocity)| {
///     position.0 += velocity.0;
/// })?;
///
/// let mut positions = Query::<&Position>::new()?;
/// let moved = positions.iter(&world)?.filter(|p| **p == Position(2.0)).count();
/// assert_eq!(moved, 50);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Query<Q: QueryData, F: QueryFilter = ()> {
    access: Access,
    /// The world the cache describes.
    world: Option<u64>,
    /// Archetype tables already checked: ids `0..seen`.
    seen: usize,
    /// Ids of the matching tables, ascending.
    matched: Vec<usize>,
    marker: PhantomData<fn() -> (Q, F)>,
}

impl<Q: QueryData, F: QueryFilter> fmt::Debug for Query<Q, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Query")
            .field("access", &self.access)
            .field("matched_tables", &self.matched.len())
            .finish_non_exhaustive()
    }
}

impl<Q: QueryData, F: QueryFilter> Query<Q, F> {
    /// A query for `Q` filtered by `F`, with its declared access checked.
    ///
    /// ```
    /// use pragma_ecs::{EcsError, Query};
    ///
    /// struct A;
    ///
    /// assert!(Query::<(&A, &A)>::new().is_ok()); // repeated reads are fine
    /// assert!(matches!(
    ///     Query::<(&A, Option<&A>, &mut A)>::new(),
    ///     Err(EcsError::ConflictingAccess { .. })
    /// ));
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::ConflictingAccess`] if a component type is written and also accessed
    /// elsewhere in `Q`; [`EcsError::OutOfMemory`] if the access list cannot be allocated.
    pub fn new() -> Result<Self, EcsError> {
        let mut access = Access::new();
        Q::declare(&mut access)?;
        Ok(Self {
            access,
            world: None,
            seen: 0,
            matched: Vec::new(),
            marker: PhantomData,
        })
    }

    /// The component types the query reads and writes.
    ///
    /// ```
    /// use pragma_ecs::{AccessKind, Entity, Query};
    ///
    /// let query = Query::<(Entity, &mut u32, Option<&u8>)>::new()?;
    /// assert_eq!(query.access().kind_of::<u32>(), Some(AccessKind::Write));
    /// assert_eq!(query.access().kind_of::<u8>(), Some(AccessKind::Read));
    /// assert_eq!(query.access().len(), 2);
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    #[must_use]
    pub const fn access(&self) -> &Access {
        &self.access
    }

    /// Iterates the matching entities of `world` without changing them.
    ///
    /// ```
    /// use pragma_ecs::{Entity, Query, World};
    ///
    /// let mut world = World::new();
    /// let a = world.spawn()?;
    /// world.insert(a, 1_u32)?;
    /// let b = world.spawn()?;
    /// world.insert(b, 2_u32)?;
    /// world.insert(b, 'b')?;
    ///
    /// let mut query = Query::<(Entity, &u32, Option<&char>)>::new()?;
    /// let mut rows: Vec<_> = query.iter(&world)?.collect();
    /// rows.sort();
    /// assert_eq!(rows, [(a, &1, None), (b, &2, Some(&'b'))]);
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the cache of matching tables cannot grow.
    pub fn iter<'w, 's>(&'s mut self, world: &'w World) -> Result<QueryIter<'w, 's, Q>, EcsError>
    where
        Q: ReadOnlyQueryData,
    {
        self.update(world)?;
        Ok(QueryIter {
            tables: world.archetypes(),
            ids: self.matched.iter(),
            rows: None,
        })
    }

    /// Iterates the matching entities of `world`, with mutable access to the components `Q`
    /// writes.
    ///
    /// ```
    /// use pragma_ecs::{Query, World};
    ///
    /// let mut world = World::new();
    /// let entity = world.spawn()?;
    /// world.insert(entity, 1_u32)?;
    ///
    /// let mut query = Query::<&mut u32>::new()?;
    /// for value in query.iter_mut(&mut world)? {
    ///     *value += 1;
    /// }
    /// assert_eq!(world.get::<u32>(entity), Some(&2));
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the cache of matching tables cannot grow.
    pub fn iter_mut<'w, 's>(
        &'s mut self,
        world: &'w mut World,
    ) -> Result<QueryIterMut<'w, 's, Q>, EcsError> {
        self.update(world)?;
        Ok(QueryIterMut {
            tables: TablesMut::new(world.archetypes_mut(), &self.matched),
            rows: None,
        })
    }

    /// Calls `f` for every matching entity of `world`, in parallel on `pool`, without changing
    /// the world.
    ///
    /// The rows are split into chunks of `grain.chunk_size(matching rows)` rows, never spanning
    /// two tables, and the chunks go to [`ThreadPool::parallel_for`]. The split depends only on
    /// the data, not on the number of workers (DECISIONS #15).
    ///
    /// ```
    /// use std::sync::atomic::{AtomicU64, Ordering};
    ///
    /// use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
    /// use pragma_ecs::{Query, World};
    ///
    /// let mut world = World::new();
    /// for i in 1..=1000_u64 {
    ///     let entity = world.spawn()?;
    ///     world.insert(entity, i)?;
    /// }
    ///
    /// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
    /// let sum = AtomicU64::new(0);
    /// Query::<&u64>::new()?.par_for_each(&world, &pool, Grain::Fixed(100), |value| {
    ///     sum.fetch_add(*value, Ordering::Relaxed);
    /// })?;
    /// assert_eq!(sum.into_inner(), 500_500);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::Job`] if `f` panicked (the other chunks still run to completion);
    /// [`EcsError::OutOfMemory`] if the cache or the chunk list cannot be allocated.
    pub fn par_for_each<'w, G>(
        &mut self,
        world: &'w World,
        pool: &ThreadPool,
        grain: Grain,
        f: G,
    ) -> Result<(), EcsError>
    where
        Q: ReadOnlyQueryData,
        G: Fn(Q::Item<'w>) + Sync,
    {
        self.update(world)?;
        let tables = world.archetypes();
        let matched = || self.matched.iter().filter_map(|&id| tables.get(id));
        let plan = Plan::new(matched().map(Archetype::len), grain);
        let rows = matched().filter_map(|table| Some((table.len(), fetch_shared::<Q>(table)?)));
        run::<Q, G>(rows, plan, pool, &f, world.len())
    }

    /// Calls `f` for every matching entity of `world`, in parallel on `pool`, with mutable
    /// access to the components `Q` writes. Chunks as in [`par_for_each`](Self::par_for_each).
    ///
    /// ```
    /// use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
    /// use pragma_ecs::{Query, World};
    ///
    /// struct Position(f32);
    /// struct Velocity(f32);
    ///
    /// let mut world = World::new();
    /// for _ in 0..1000 {
    ///     let entity = world.spawn()?;
    ///     world.insert(entity, Position(0.0))?;
    ///     world.insert(entity, Velocity(0.5))?;
    /// }
    ///
    /// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
    /// let mut movement = Query::<(&mut Position, &Velocity)>::new()?;
    /// movement.par_for_each_mut(&mut world, &pool, Grain::Auto, |(position, velocity)| {
    ///     position.0 += velocity.0;
    /// })?;
    /// assert!(Query::<&Position>::new()?.iter(&world)?.all(|p| p.0 == 0.5));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::Job`] if `f` panicked (the other chunks still run to completion);
    /// [`EcsError::OutOfMemory`] if the cache or the chunk list cannot be allocated.
    pub fn par_for_each_mut<'w, G>(
        &mut self,
        world: &'w mut World,
        pool: &ThreadPool,
        grain: Grain,
        f: G,
    ) -> Result<(), EcsError>
    where
        G: Fn(Q::Item<'w>) + Sync,
    {
        self.update(world)?;
        let entities = world.len();
        let tables = world.archetypes();
        let plan = Plan::new(
            self.matched
                .iter()
                .filter_map(|&id| tables.get(id))
                .map(Archetype::len),
            grain,
        );
        let rows = TablesMut::new(world.archetypes_mut(), &self.matched)
            .filter_map(|table| Some((table.len(), fetch_unique::<Q>(table)?)));
        run::<Q, G>(rows, plan, pool, &f, entities)
    }

    /// Brings the cache of matching tables up to date with `world`.
    pub(crate) fn update(&mut self, world: &World) -> Result<(), EcsError> {
        if self.world != Some(world.id()) {
            self.world = Some(world.id());
            self.seen = 0;
            self.matched.clear();
        }
        let tables = world.archetypes();
        for (id, table) in tables.iter().enumerate().skip(self.seen) {
            let types: &[TypeId] = table.types();
            if Q::matches(types) && F::matches(types) {
                self.matched
                    .try_reserve(1)
                    .map_err(|_| EcsError::OutOfMemory {
                        entities: world.len(),
                    })?;
                self.matched.push(id);
            }
            self.seen = id.saturating_add(1);
        }
        Ok(())
    }

    /// Ids of the matching tables, ascending, as of the last [`update`](Self::update).
    pub(crate) fn matched(&self) -> &[usize] {
        &self.matched
    }
}

/// The rows of one table for `Q`, given the table's columns as `columns` (parallel to `types`).
fn fetch<'w, Q: QueryData>(
    types: &[TypeId],
    columns: impl Iterator<Item = ColumnRef<'w>>,
    entities: &'w [Entity],
) -> Option<Q::Slice<'w>> {
    let mut state = Q::State::default();
    for (ty, column) in types.iter().zip(columns) {
        Q::offer(&mut state, *ty, &mut Some(column));
    }
    Q::finish(state, entities)
}

fn fetch_shared<Q: QueryData>(table: &Archetype) -> Option<Q::Slice<'_>> {
    let columns = table
        .columns()
        .iter()
        .map(|c| ColumnRef::Shared(c.as_any()));
    fetch::<Q>(table.types(), columns, table.entities())
}

fn fetch_unique<Q: QueryData>(table: &mut Archetype) -> Option<Q::Slice<'_>> {
    let (types, columns, entities) = table.parts_mut();
    let columns = columns
        .iter_mut()
        .map(|c| ColumnRef::Unique(c.as_any_mut()));
    fetch::<Q>(types, columns, entities)
}

/// The tables with the given ascending ids, mutably, without `unsafe`: the slice of tables is
/// split past every table handed out.
struct TablesMut<'w, 's> {
    rest: &'w mut [Archetype],
    /// The id of `rest[0]`.
    offset: usize,
    ids: slice::Iter<'s, usize>,
}

impl<'w, 's> TablesMut<'w, 's> {
    fn new(tables: &'w mut [Archetype], ids: &'s [usize]) -> Self {
        Self {
            rest: tables,
            offset: 0,
            ids: ids.iter(),
        }
    }
}

impl<'w> Iterator for TablesMut<'w, '_> {
    type Item = &'w mut Archetype;

    fn next(&mut self) -> Option<Self::Item> {
        let &id = self.ids.next()?;
        let rest = mem::take(&mut self.rest);
        let skip = id.checked_sub(self.offset)?.min(rest.len());
        let (table, rest) = rest.split_at_mut(skip).1.split_first_mut()?;
        self.rest = rest;
        self.offset = id.saturating_add(1);
        Some(table)
    }
}

/// How a parallel query splits its rows.
#[derive(Clone, Copy)]
pub(crate) struct Plan {
    /// Rows per chunk, at least 1.
    size: usize,
    /// Number of chunks: a chunk never spans two tables.
    chunks: usize,
}

impl Plan {
    pub(crate) fn new(lens: impl Iterator<Item = usize> + Clone, grain: Grain) -> Self {
        let total = lens.clone().fold(0, usize::saturating_add);
        let size = grain.chunk_size(total).max(1);
        let chunks = lens
            .map(|len| len.div_ceil(size))
            .fold(0, usize::saturating_add);
        Self { size, chunks }
    }
}

/// Splits the rows of every table into chunks by `plan` and runs `f` over them on `pool`.
pub(crate) fn run<'w, Q, G>(
    tables: impl Iterator<Item = (usize, Q::Slice<'w>)>,
    plan: Plan,
    pool: &ThreadPool,
    f: &G,
    entities: usize,
) -> Result<(), EcsError>
where
    Q: QueryData,
    G: Fn(Q::Item<'w>) + Sync,
{
    let mut chunks: Vec<Option<Q::Slice<'w>>> = Vec::new();
    chunks
        .try_reserve_exact(plan.chunks)
        .map_err(|_| EcsError::OutOfMemory { entities })?;
    for (mut len, mut rows) in tables {
        while len > plan.size {
            let (head, tail) = Q::split_at(rows, plan.size);
            chunks.push(Some(head));
            rows = tail;
            len = len.saturating_sub(plan.size);
        }
        if len > 0 {
            chunks.push(Some(rows));
        }
    }
    debug_assert_eq!(chunks.len(), plan.chunks);
    pool.parallel_for(&mut chunks, Grain::Fixed(1), |chunk| {
        if let Some(rows) = chunk.take() {
            Q::iter(rows).for_each(f);
        }
    })?;
    Ok(())
}

/// The iterator of [`Query::iter`]: the matching entities of a shared world.
///
/// ```
/// use pragma_ecs::{Query, QueryIter, World};
///
/// let mut world = World::new();
/// let entity = world.spawn()?;
/// world.insert(entity, 5_i32)?;
///
/// let mut query = Query::<&i32>::new()?;
/// let iter: QueryIter<'_, '_, &i32> = query.iter(&world)?;
/// assert_eq!(iter.copied().sum::<i32>(), 5);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct QueryIter<'w, 's, Q: QueryData> {
    tables: &'w [Archetype],
    ids: slice::Iter<'s, usize>,
    /// The rest of the current table.
    rows: Option<Q::Iter<'w>>,
}

impl<Q: QueryData> fmt::Debug for QueryIter<'_, '_, Q> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryIter")
            .field("tables_left", &self.ids.len())
            .finish_non_exhaustive()
    }
}

impl<'w, Q: QueryData> Iterator for QueryIter<'w, '_, Q> {
    type Item = Q::Item<'w>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.rows.as_mut().and_then(Iterator::next) {
                return Some(item);
            }
            let &id = self.ids.next()?;
            let tables = self.tables;
            self.rows = tables.get(id).and_then(fetch_shared::<Q>).map(Q::iter);
        }
    }
}

/// The iterator of [`Query::iter_mut`]: the matching entities of an exclusively borrowed world.
///
/// ```
/// use pragma_ecs::{Query, QueryIterMut, World};
///
/// let mut world = World::new();
/// let entity = world.spawn()?;
/// world.insert(entity, 5_i32)?;
///
/// let mut query = Query::<&mut i32>::new()?;
/// let iter: QueryIterMut<'_, '_, &mut i32> = query.iter_mut(&mut world)?;
/// iter.for_each(|value| *value = -*value);
/// assert_eq!(world.get::<i32>(entity), Some(&-5));
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct QueryIterMut<'w, 's, Q: QueryData> {
    tables: TablesMut<'w, 's>,
    /// The rest of the current table.
    rows: Option<Q::Iter<'w>>,
}

impl<Q: QueryData> fmt::Debug for QueryIterMut<'_, '_, Q> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryIterMut")
            .field("tables_left", &self.tables.ids.len())
            .finish_non_exhaustive()
    }
}

impl<'w, Q: QueryData> Iterator for QueryIterMut<'w, '_, Q> {
    type Item = Q::Item<'w>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.rows.as_mut().and_then(Iterator::next) {
                return Some(item);
            }
            let table = self.tables.next()?;
            self.rows = fetch_unique::<Q>(table).map(Q::iter);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{With, Without};

    #[derive(Debug, PartialEq)]
    struct A(u32);
    #[derive(Debug, PartialEq)]
    struct B(u32);
    struct Marker;

    #[test]
    fn aliasing_is_rejected_at_construction() {
        let name = std::any::type_name::<A>();
        let conflict = Err(EcsError::ConflictingAccess { component: name });
        assert_eq!(Query::<(&A, &mut A)>::new().map(|_| ()), conflict);
        assert_eq!(Query::<(&mut A, &mut A)>::new().map(|_| ()), conflict);
        assert_eq!(Query::<(Option<&A>, &mut A)>::new().map(|_| ()), conflict);
        assert_eq!(Query::<(&mut A, (&B, &A))>::new().map(|_| ()), conflict);
        assert!(Query::<(&A, &A, Option<&A>)>::new().is_ok());
        assert!(Query::<(&mut A, &mut B), With<A>>::new().is_ok());
    }

    #[test]
    fn the_cache_follows_new_tables_and_other_worlds() {
        let mut world = World::new();
        let e = world.spawn().unwrap();
        world.insert(e, A(1)).unwrap();
        let mut query = Query::<&A, Without<Marker>>::new().unwrap();
        assert_eq!(query.iter(&world).unwrap().count(), 1);

        // A new table {A, B} appears after the first call.
        let f = world.spawn().unwrap();
        world.insert(f, B(2)).unwrap();
        world.insert(f, A(2)).unwrap();
        // {A, Marker} does not match.
        let g = world.spawn().unwrap();
        world.insert(g, A(3)).unwrap();
        world.insert(g, Marker).unwrap();
        let mut seen: Vec<u32> = query.iter(&world).unwrap().map(|a| a.0).collect();
        seen.sort_unstable();
        assert_eq!(seen, [1, 2]);

        // Another world with different table ids: the cache is rebuilt.
        let mut other = World::new();
        let h = other.spawn().unwrap();
        other.insert(h, Marker).unwrap();
        other.insert(h, B(0)).unwrap();
        other.insert(h, A(4)).unwrap();
        assert_eq!(query.iter(&other).unwrap().count(), 0);
        let i = other.spawn().unwrap();
        other.insert(i, A(5)).unwrap();
        let only: Vec<u32> = query.iter(&other).unwrap().map(|a| a.0).collect();
        assert_eq!(only, [5]);
        assert!(format!("{query:?}").starts_with("Query { access: {"));
    }

    #[test]
    fn mutable_iteration_skips_tables_and_writes_every_row() {
        let mut world = World::new();
        let mut expected = Vec::new();
        for i in 0..20_u32 {
            let e = world.spawn().unwrap();
            world.insert(e, A(i)).unwrap();
            if i % 3 == 0 {
                world.insert(e, B(i)).unwrap();
            }
            if i % 4 == 0 {
                world.insert(e, Marker).unwrap();
            }
            if i % 3 == 0 && i % 4 != 0 {
                expected.push(i);
            }
        }
        let mut query = Query::<(Entity, &mut A, &B), Without<Marker>>::new().unwrap();
        let mut iter = query.iter_mut(&mut world).unwrap();
        assert!(format!("{iter:?}").starts_with("QueryIterMut"));
        let mut visited = Vec::new();
        for (e, a, b) in &mut iter {
            a.0 += 100;
            visited.push((e, b.0));
        }
        visited.sort_by_key(|(_, b)| *b);
        assert_eq!(
            visited.iter().map(|(_, b)| *b).collect::<Vec<_>>(),
            expected
        );
        for (e, b) in visited {
            assert_eq!(world.get::<A>(e), Some(&A(b + 100)));
        }
    }

    #[test]
    fn chunks_never_span_tables() {
        let plan = Plan::new([5_usize, 0, 3].into_iter(), Grain::Fixed(2));
        assert_eq!((plan.size, plan.chunks), (2, 3 + 2));
        let empty = Plan::new(std::iter::empty(), Grain::Auto);
        assert_eq!(empty.chunks, 0);
    }
}
