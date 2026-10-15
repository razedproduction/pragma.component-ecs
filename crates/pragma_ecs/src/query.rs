//! Queries: typed iteration over the entities whose components match (DECISIONS #12–#15, #28).

use std::any::TypeId;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::slice;

use pragma_core::jobs::{Grain, ThreadPool};

use crate::access::Access;
use crate::archetype::Archetype;
use crate::component::Ticks;
use crate::fetch::{ColumnRef, QueryData, QueryFilter, ReadOnlyQueryData};
use crate::{EcsError, Entity, World};

/// A typed query: the entities that have the components `Q` asks for and pass the filter `F`.
///
/// `Q` is what is yielded per entity ([`QueryData`]: `&T`, `&mut T`, `Option<&T>`, [`Entity`]
/// or a tuple of them); `F` restricts the entities visited ([`QueryFilter`]: [`With`],
/// [`Without`], [`Added`], [`Changed`] or a tuple of them; `()` by default). `&mut T` yields a
/// [`Mut<T>`](crate::Mut), which records the change when it is written.
///
/// A `Query` is built once and reused: it does not borrow the world. [`new`](Self::new) checks
/// the declared [`Access`] and rejects aliasing (`(&A, &mut A)`) as
/// [`EcsError::ConflictingAccess`]. Every call remembers which archetype tables match, so the
/// next call only checks the tables created since (DECISIONS #12), and the world's change tick,
/// so that [`Added`] and [`Changed`] mean "since this query's previous call" (DECISIONS #27).
/// Using a query with another world is allowed; the cache and the tick are then reset.
///
/// Rows are visited table by table, in table creation order, and within a table in row order.
/// That order is deterministic for the same sequence of world operations, but it is not the
/// order of spawning.
///
/// [`With`]: crate::With
/// [`Without`]: crate::Without
/// [`Added`]: crate::Added
/// [`Changed`]: crate::Changed
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
/// for (mut position, velocity) in movement.iter_mut(&mut world)? {
///     position.0 += velocity.0;
/// }
///
/// // The same query, its rows split into chunks over the pragma.core thread pool.
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
/// movement.par_for_each_mut(&mut world, &pool, Grain::Auto, |(mut position, velocity)| {
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
    /// The world's change tick at the previous call (DECISIONS #27).
    last_run: u64,
    /// For a row filter: whether each row of the matching tables passes, table after table.
    mask: Vec<bool>,
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
    /// use pragma_ecs::{Changed, EcsError, Query};
    ///
    /// struct A;
    ///
    /// assert!(Query::<(&A, &A)>::new().is_ok()); // repeated reads are fine
    /// assert!(Query::<&mut A, Changed<A>>::new().is_ok()); // a filter reads ticks first
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
        F::declare(&mut access)?;
        Ok(Self {
            access,
            world: None,
            seen: 0,
            matched: Vec::new(),
            last_run: 0,
            mask: Vec::new(),
            marker: PhantomData,
        })
    }

    /// The component types the query reads and writes.
    ///
    /// ```
    /// use pragma_ecs::{AccessKind, Changed, Entity, Query};
    ///
    /// let query = Query::<(Entity, &mut u32, Option<&u8>), Changed<u16>>::new()?;
    /// assert_eq!(query.access().kind_of::<u32>(), Some(AccessKind::Write));
    /// assert_eq!(query.access().kind_of::<u8>(), Some(AccessKind::Read));
    /// assert_eq!(query.access().kind_of::<u16>(), Some(AccessKind::Read)); // its ticks
    /// assert_eq!(query.access().len(), 3);
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
    /// [`EcsError::OutOfMemory`] if the cache of matching tables or the row mask of a row
    /// filter cannot grow.
    pub fn iter<'w, 's>(&'s mut self, world: &'w World) -> Result<QueryIter<'w, 's, Q, F>, EcsError>
    where
        Q: ReadOnlyQueryData,
    {
        self.prepare(world, world.change_tick())?;
        Ok(QueryIter {
            rows: Rows::new(self.shared(world), &self.mask),
        })
    }

    /// Iterates the matching entities of `world`, with mutable access to the components `Q`
    /// writes (as [`Mut<T>`](crate::Mut)).
    ///
    /// ```
    /// use pragma_ecs::{Query, World};
    ///
    /// let mut world = World::new();
    /// let entity = world.spawn()?;
    /// world.insert(entity, 1_u32)?;
    ///
    /// let mut query = Query::<&mut u32>::new()?;
    /// for mut value in query.iter_mut(&mut world)? {
    ///     *value += 1;
    /// }
    /// assert_eq!(world.get::<u32>(entity), Some(&2));
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the cache of matching tables or the row mask of a row
    /// filter cannot grow.
    pub fn iter_mut<'w, 's>(
        &'s mut self,
        world: &'w mut World,
    ) -> Result<QueryIterMut<'w, 's, Q, F>, EcsError> {
        let tick = world.next_tick();
        self.prepare(world, tick)?;
        Ok(QueryIterMut {
            rows: Rows::new(Exclusive::new(world, &self.matched, tick), &self.mask),
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
    /// [`EcsError::OutOfMemory`] if the cache, the row mask or the chunk list cannot be
    /// allocated.
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
        self.prepare(world, world.change_tick())?;
        let plan = Plan::new(world.archetypes(), &self.matched, grain);
        run::<Q, F, G>(self.shared(world), &self.mask, plan, pool, &f, world.len())
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
    /// movement.par_for_each_mut(&mut world, &pool, Grain::Auto, |(mut position, velocity)| {
    ///     position.0 += velocity.0;
    /// })?;
    /// assert!(Query::<&Position>::new()?.iter(&world)?.all(|p| p.0 == 0.5));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::Job`] if `f` panicked (the other chunks still run to completion);
    /// [`EcsError::OutOfMemory`] if the cache, the row mask or the chunk list cannot be
    /// allocated.
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
        let tick = world.next_tick();
        self.prepare(world, tick)?;
        let entities = world.len();
        let plan = Plan::new(world.archetypes(), &self.matched, grain);
        let tables = Exclusive::new(world, &self.matched, tick);
        run::<Q, F, G>(tables, &self.mask, plan, pool, &f, entities)
    }

    /// Brings the cache up to date with `world`, records `now` as the tick of this call and
    /// fills the row mask with the changes since the previous call, from the tick columns of
    /// the world's tables.
    fn prepare(&mut self, world: &World, now: u64) -> Result<(), EcsError> {
        self.update(world)?;
        let last_run = mem::replace(&mut self.last_run, now);
        let tables = world.archetypes();
        self.fill_mask(tables, last_run, world.len(), |id, ty| {
            tables.get(id)?.ticks(ty)
        })
    }

    /// The matching tables of a shared `world`.
    fn shared<'w, 's>(&'s self, world: &'w World) -> Shared<'w, 's> {
        Shared {
            tables: world.archetypes(),
            ids: self.matched.iter(),
        }
    }

    /// Brings the cache of matching tables up to date with `world`.
    pub(crate) fn update(&mut self, world: &World) -> Result<(), EcsError> {
        if self.world != Some(world.id()) {
            self.world = Some(world.id());
            self.seen = 0;
            self.matched.clear();
            self.last_run = 0;
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

    /// Fills the row mask of a row filter for the matching tables of `tables` (nothing for a
    /// table filter); `ticks(table id, type)` looks up a tick column.
    pub(crate) fn fill_mask<'a>(
        &mut self,
        tables: &[Archetype],
        last_run: u64,
        entities: usize,
        ticks: impl Fn(usize, TypeId) -> Option<Ticks<'a>>,
    ) -> Result<(), EcsError> {
        self.mask.clear();
        if F::DENSE {
            return Ok(());
        }
        let rows = |id: &usize| tables.get(*id).map_or(0, Archetype::len);
        let total = self.matched.iter().map(rows).fold(0, usize::saturating_add);
        self.mask
            .try_reserve(total)
            .map_err(|_| EcsError::OutOfMemory { entities })?;
        for id in &self.matched {
            let start = self.mask.len();
            self.mask.resize(start.saturating_add(rows(id)), true);
            if let Some(segment) = self.mask.get_mut(start..) {
                F::mask(&|ty| ticks(*id, ty), last_run, segment);
            }
        }
        Ok(())
    }

    /// Ids of the matching tables, ascending, as of the last [`update`](Self::update).
    pub(crate) fn matched(&self) -> &[usize] {
        &self.matched
    }

    /// The row mask of the last [`fill_mask`](Self::fill_mask).
    pub(crate) fn mask(&self) -> &[bool] {
        &self.mask
    }
}

/// The rows of one table for `Q`, given the table's columns as `columns` (parallel to `types`).
fn fetch<'w, Q: QueryData>(
    types: &[TypeId],
    columns: impl Iterator<Item = ColumnRef<'w>>,
    entities: &'w [Entity],
    tick: u64,
) -> Option<Q::Slice<'w>> {
    let mut state = Q::State::default();
    for (ty, column) in types.iter().zip(columns) {
        Q::offer(&mut state, *ty, &mut Some(column));
    }
    Q::finish(state, entities, tick)
}

fn fetch_shared<Q: QueryData>(table: &Archetype) -> Option<Q::Slice<'_>> {
    let columns = table
        .columns()
        .iter()
        .map(|c| ColumnRef::Shared(c.as_any()));
    fetch::<Q>(table.types(), columns, table.entities(), 0)
}

fn fetch_unique<Q: QueryData>(table: &mut Archetype, tick: u64) -> Option<Q::Slice<'_>> {
    let (types, columns, entities) = table.parts_mut();
    let columns = columns
        .iter_mut()
        .map(|c| ColumnRef::Unique(c.as_any_mut()));
    fetch::<Q>(types, columns, entities, tick)
}

/// Where the rows of a query come from, table by table: the tables of a shared world
/// ([`Shared`]), of an exclusively borrowed world ([`Exclusive`]) or the columns lent to a
/// system (`Lent`, in `system.rs`). [`Rows`] and [`run`] walk any of them, so `Query` and
/// `View` share one sequential and one parallel path.
pub(crate) trait Tables<'w, Q: QueryData> {
    /// The row count and the rows of the next matching table (`None` for the rows if a column
    /// is missing); `None` after the last table.
    fn next_table(&mut self) -> Option<(usize, Option<Q::Slice<'w>>)>;

    /// The number of matching tables not visited yet.
    fn left(&self) -> usize;
}

/// The matching tables of a shared world.
pub(crate) struct Shared<'w, 's> {
    tables: &'w [Archetype],
    ids: slice::Iter<'s, usize>,
}

impl<'w, Q: QueryData> Tables<'w, Q> for Shared<'w, '_> {
    #[inline(always)]
    fn next_table(&mut self) -> Option<(usize, Option<Q::Slice<'w>>)> {
        let &id = self.ids.next()?;
        let table = self.tables.get(id)?;
        Some((table.len(), fetch_shared::<Q>(table)))
    }

    fn left(&self) -> usize {
        self.ids.len()
    }
}

/// The matching tables of an exclusively borrowed world, without `unsafe`: the slice of tables
/// is split past every table handed out.
pub(crate) struct Exclusive<'w, 's> {
    rest: &'w mut [Archetype],
    /// The id of `rest[0]`.
    offset: usize,
    ids: slice::Iter<'s, usize>,
    /// The change tick a write stamps.
    tick: u64,
}

impl<'w, 's> Exclusive<'w, 's> {
    fn new(world: &'w mut World, ids: &'s [usize], tick: u64) -> Self {
        Self {
            rest: world.archetypes_mut(),
            offset: 0,
            ids: ids.iter(),
            tick,
        }
    }
}

impl<'w, Q: QueryData> Tables<'w, Q> for Exclusive<'w, '_> {
    #[inline(always)]
    fn next_table(&mut self) -> Option<(usize, Option<Q::Slice<'w>>)> {
        let &id = self.ids.next()?;
        let rest = mem::take(&mut self.rest);
        let skip = id.checked_sub(self.offset)?.min(rest.len());
        let (table, rest) = rest.split_at_mut(skip).1.split_first_mut()?;
        self.rest = rest;
        self.offset = id.saturating_add(1);
        Some((table.len(), fetch_unique::<Q>(table, self.tick)))
    }

    fn left(&self) -> usize {
        self.ids.len()
    }
}

/// Hands out the row mask of a filter table by table; empty for a table filter.
struct Masks<'m, F> {
    rest: &'m [bool],
    marker: PhantomData<fn() -> F>,
}

impl<'m, F: QueryFilter> Masks<'m, F> {
    const fn new(mask: &'m [bool]) -> Self {
        Self {
            rest: mask,
            marker: PhantomData,
        }
    }

    /// The mask of the next table, of `rows` rows.
    #[inline(always)]
    fn take(&mut self, rows: usize) -> &'m [bool] {
        if F::DENSE {
            return &[];
        }
        let (mask, rest) = self.rest.split_at(rows.min(self.rest.len()));
        self.rest = rest;
        mask
    }
}

/// Folds the rows of one table that pass a row filter's `mask` (every row for a table filter).
#[inline(always)]
fn fold_rows<'w, Q: QueryData, F: QueryFilter, B>(
    items: Q::Iter<'w>,
    mask: &[bool],
    init: B,
    mut g: impl FnMut(B, Q::Item<'w>) -> B,
) -> B {
    if F::DENSE {
        items.fold(init, g)
    } else {
        items.zip(mask).fold(
            init,
            |acc, (item, keep)| {
                if *keep { g(acc, item) } else { acc }
            },
        )
    }
}

/// The rest of one table: its rows and, for a row filter, the rest of its row mask.
struct Masked<'m, I> {
    items: I,
    mask: Option<slice::Iter<'m, bool>>,
}

impl<I: Iterator> Masked<'_, I> {
    /// The whole per-row path (this, the element iterators, the table switch of [`Rows`]) is
    /// inlined into the caller's loop, and the table-filter case is decided once per table
    /// rather than per row (DECISIONS #37).
    #[inline(always)]
    fn next(&mut self) -> Option<I::Item> {
        match &mut self.mask {
            None => self.items.next(),
            Some(mask) => loop {
                let item = self.items.next()?;
                if mask.next().copied().unwrap_or(false) {
                    return Some(item);
                }
            },
        }
    }
}

/// The sequential iterator of every query and view: the rows of the matching tables of `T`
/// that pass the filter `F`, table by table (DECISIONS #41).
pub(crate) struct Rows<'w, 'm, Q: QueryData, F, T> {
    /// The rest of the current table; `None` before the first one.
    rows: Option<Masked<'m, Q::Iter<'w>>>,
    /// The row mask of the tables not visited yet.
    masks: Masks<'m, F>,
    tables: T,
}

impl<'w, 'm, Q: QueryData, F: QueryFilter, T: Tables<'w, Q>> Rows<'w, 'm, Q, F, T> {
    pub(crate) const fn new(tables: T, mask: &'m [bool]) -> Self {
        Self {
            rows: None,
            masks: Masks::new(mask),
            tables,
        }
    }

    /// The rows of the next matching table (`None` inside if a column is missing); `None`
    /// after the last table.
    #[inline(always)]
    fn table(&mut self) -> Option<Option<Masked<'m, Q::Iter<'w>>>> {
        let (len, rows) = self.tables.next_table()?;
        let mask = self.masks.take(len);
        Some(rows.map(|rows| Masked {
            items: Q::iter(rows),
            mask: (!F::DENSE).then_some(mask.iter()),
        }))
    }

    pub(crate) fn left(&self) -> usize {
        self.tables.left()
    }
}

impl<'w, Q: QueryData, F: QueryFilter, T: Tables<'w, Q>> Iterator for Rows<'w, '_, Q, F, T> {
    type Item = Q::Item<'w>;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.rows.as_mut().and_then(Masked::next) {
                return Some(item);
            }
            self.rows = self.table()?;
        }
    }

    fn fold<B, G: FnMut(B, Self::Item) -> B>(self, init: B, mut g: G) -> B {
        let Self {
            rows,
            mut masks,
            mut tables,
        } = self;
        let mut acc = match rows {
            Some(Masked { items, mask: None }) => fold_rows::<Q, (), B>(items, &[], init, &mut g),
            Some(Masked {
                items,
                mask: Some(mask),
            }) => fold_rows::<Q, F, B>(items, mask.as_slice(), init, &mut g),
            None => init,
        };
        while let Some((len, rows)) = tables.next_table() {
            let mask = masks.take(len);
            if let Some(rows) = rows {
                acc = fold_rows::<Q, F, B>(Q::iter(rows), mask, acc, &mut g);
            }
        }
        acc
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
    /// The plan for the tables of `tables` with the ids `matched`.
    pub(crate) fn new(tables: &[Archetype], matched: &[usize], grain: Grain) -> Self {
        let lens = matched
            .iter()
            .filter_map(|&id| tables.get(id))
            .map(Archetype::len);
        Self::from_lens(lens, grain)
    }

    fn from_lens(lens: impl Iterator<Item = usize> + Clone, grain: Grain) -> Self {
        let total = lens.clone().fold(0, usize::saturating_add);
        let size = grain.chunk_size(total).max(1);
        let chunks = lens
            .map(|len| len.div_ceil(size))
            .fold(0, usize::saturating_add);
        Self { size, chunks }
    }
}

/// The rows of a chunk and their part of the row mask (empty for a table filter).
type Chunk<'w, 'm, Q> = (<Q as QueryData>::Slice<'w>, &'m [bool]);

/// Splits the rows of every table of `tables` (with its part of the row mask `mask`) into
/// chunks by `plan` and runs `f` over the rows that pass on `pool`.
pub(crate) fn run<'w, Q, F, G>(
    mut tables: impl Tables<'w, Q>,
    mask: &[bool],
    plan: Plan,
    pool: &ThreadPool,
    f: &G,
    entities: usize,
) -> Result<(), EcsError>
where
    Q: QueryData,
    F: QueryFilter,
    G: Fn(Q::Item<'w>) + Sync,
{
    let mut chunks: Vec<Option<Chunk<'w, '_, Q>>> = Vec::new();
    chunks
        .try_reserve_exact(plan.chunks)
        .map_err(|_| EcsError::OutOfMemory { entities })?;
    let mut masks = Masks::<F>::new(mask);
    while let Some((mut len, rows)) = tables.next_table() {
        let mut mask = masks.take(len);
        let Some(mut rows) = rows else { continue };
        while len > plan.size {
            let (head, tail) = Q::split_at(rows, plan.size);
            let (head_mask, tail_mask) = mask.split_at(plan.size.min(mask.len()));
            chunks.push(Some((head, head_mask)));
            rows = tail;
            mask = tail_mask;
            len = len.saturating_sub(plan.size);
        }
        if len > 0 {
            chunks.push(Some((rows, mask)));
        }
    }
    debug_assert_eq!(chunks.len(), plan.chunks);
    pool.parallel_for(&mut chunks, Grain::Fixed(1), |chunk| {
        if let Some((rows, mask)) = chunk.take() {
            fold_rows::<Q, F, ()>(Q::iter(rows), mask, (), |(), item| f(item));
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
pub struct QueryIter<'w, 's, Q: QueryData, F: QueryFilter = ()> {
    rows: Rows<'w, 's, Q, F, Shared<'w, 's>>,
}

impl<Q: QueryData, F: QueryFilter> fmt::Debug for QueryIter<'_, '_, Q, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryIter")
            .field("tables_left", &self.rows.left())
            .finish_non_exhaustive()
    }
}

impl<'w, Q: QueryData, F: QueryFilter> Iterator for QueryIter<'w, '_, Q, F> {
    type Item = Q::Item<'w>;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        self.rows.next()
    }

    fn fold<B, G: FnMut(B, Self::Item) -> B>(self, init: B, g: G) -> B {
        self.rows.fold(init, g)
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
/// iter.for_each(|mut value| *value = -*value);
/// assert_eq!(world.get::<i32>(entity), Some(&-5));
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct QueryIterMut<'w, 's, Q: QueryData, F: QueryFilter = ()> {
    rows: Rows<'w, 's, Q, F, Exclusive<'w, 's>>,
}

impl<Q: QueryData, F: QueryFilter> fmt::Debug for QueryIterMut<'_, '_, Q, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryIterMut")
            .field("tables_left", &self.rows.left())
            .finish_non_exhaustive()
    }
}

impl<'w, Q: QueryData, F: QueryFilter> Iterator for QueryIterMut<'w, '_, Q, F> {
    type Item = Q::Item<'w>;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        self.rows.next()
    }

    fn fold<B, G: FnMut(B, Self::Item) -> B>(self, init: B, g: G) -> B {
        self.rows.fold(init, g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Changed, With, Without};

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
        assert!(Query::<&mut A, (Changed<A>, Changed<B>)>::new().is_ok());
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
        for (e, mut a, b) in &mut iter {
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
    fn a_query_sees_changes_since_its_previous_call_only() {
        let mut world = World::new();
        let ids: Vec<Entity> = (0..6).map(|_| world.spawn().unwrap()).collect();
        for (i, e) in (0_u32..).zip(&ids) {
            world.insert(*e, A(i)).unwrap();
            if i % 2 == 0 {
                world.insert(*e, B(i)).unwrap();
            }
        }
        let mut changed = Query::<Entity, Changed<A>>::new().unwrap();
        let mut double = Query::<(Entity, &mut A), Changed<A>>::new().unwrap();
        assert_eq!(changed.iter(&world).unwrap().count(), 6);

        // Writes through `Mut` only for odd values; reading is not a change.
        let mut writer = Query::<&mut A>::new().unwrap();
        for mut a in writer.iter_mut(&mut world).unwrap() {
            if a.0 % 2 == 1 {
                a.0 += 10;
            }
        }
        let mut seen: Vec<Entity> = changed.iter(&world).unwrap().collect();
        seen.sort();
        assert_eq!(seen, [ids[1], ids[3], ids[5]]);

        // A query with `&mut A, Changed<A>` sees everything on its first call, then what changed
        // since, but not its own writes.
        for (_, mut a) in double.iter_mut(&mut world).unwrap() {
            a.0 += 1;
        }
        world.insert(ids[4], A(0)).unwrap();
        let rows: Vec<Entity> = double
            .iter_mut(&mut world)
            .unwrap()
            .map(|(e, _)| e)
            .collect();
        assert_eq!(rows, [ids[4]]);
        assert_eq!(changed.iter(&world).unwrap().count(), 6);
        assert_eq!(changed.iter(&world).unwrap().count(), 0, "no writes since");
    }

    #[test]
    fn masks_split_with_the_chunks() {
        let pool =
            ThreadPool::new(pragma_core::jobs::PoolConfig::default().with_workers(2)).unwrap();
        let mut world = World::new();
        let ids: Vec<Entity> = (0..50).map(|_| world.spawn().unwrap()).collect();
        for (i, e) in (0_u32..).zip(&ids) {
            world.insert(*e, A(i)).unwrap();
            if i % 5 == 0 {
                world.insert(*e, Marker).unwrap();
            }
        }
        let mut changed = Query::<&mut A, Changed<A>>::new().unwrap();
        changed
            .par_for_each_mut(&mut world, &pool, Grain::Fixed(4), |_| {})
            .unwrap();
        for e in ids.iter().step_by(7) {
            world.get_mut::<A>(*e).unwrap().0 += 1000;
        }
        let hits = std::sync::atomic::AtomicU32::new(0);
        let mut reader = Query::<&A, Changed<A>>::new().unwrap();
        reader.iter(&world).unwrap().for_each(drop);
        changed
            .par_for_each_mut(&mut world, &pool, Grain::Fixed(4), |a| {
                assert!(a.0 >= 1000);
                hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
            .unwrap();
        assert_eq!(hits.into_inner(), 8);
        reader
            .par_for_each(&world, &pool, Grain::Fixed(3), |_| panic!("no change"))
            .unwrap();
    }

    #[test]
    fn chunks_never_span_tables() {
        let plan = Plan::from_lens([5_usize, 0, 3].into_iter(), Grain::Fixed(2));
        assert_eq!((plan.size, plan.chunks), (2, 3 + 2));
        let empty = Plan::from_lens(std::iter::empty(), Grain::Auto);
        assert_eq!(empty.chunks, 0);
    }
}
