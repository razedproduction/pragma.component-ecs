//! Systems: functions whose parameters declare what they access (DECISIONS #17).

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::slice;

use pragma_core::graph::TaskContext;
use pragma_core::jobs::{Grain, ThreadPool};

use crate::access::{Access, AccessKind};
use crate::archetype::Archetype;
use crate::query::{Masked, Masks, Plan, run};
use crate::{
    EcsError, Query, QueryData, QueryFilter, ReadOnlyQueryData, Res, ResMut, Resource, World,
};

mod sealed {
    /// Keeps [`SystemParam`](super::SystemParam), [`SystemOutput`](super::SystemOutput) and
    /// [`IntoSystem`](super::IntoSystem) closed: their machinery is internal (DECISIONS #17).
    pub trait Sealed {}

    /// Keeps [`IntoSystem`](super::IntoSystem) closed.
    pub trait SealedSystem<Marker> {}
}

pub(crate) use sealed::Sealed;
use sealed::SealedSystem;

/// The change ticks of one run of a system (DECISIONS #27).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SystemTicks {
    /// The tick of the system's previous run; changes after it are new to the system.
    pub(crate) last_run: u64,
    /// The tick of this run, stamped on the system's writes.
    pub(crate) this_run: u64,
}

/// What a system sees while it runs: the frame's task context, the world's tables and the
/// system's change ticks.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct Env<'w> {
    ctx: &'w TaskContext<'w>,
    world: &'w World,
    ticks: SystemTicks,
}

impl fmt::Debug for Env<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("system", &self.ctx.name())
            .finish_non_exhaustive()
    }
}

impl<'w> Env<'w> {
    pub(crate) const fn new(
        ctx: &'w TaskContext<'w>,
        world: &'w World,
        ticks: SystemTicks,
    ) -> Self {
        Self { ctx, world, ticks }
    }

    pub(crate) const fn ctx(&self) -> &'w TaskContext<'w> {
        self.ctx
    }

    pub(crate) const fn world(&self) -> &'w World {
        self.world
    }
}

/// A parameter of a system function: [`View<Q, F>`](View), [`Res<T>`](Res),
/// [`ResMut<T>`](ResMut), [`Commands`](crate::Commands), [`EventWriter<T>`](crate::EventWriter)
/// or [`EventReader<T>`](crate::EventReader).
///
/// A system's access is the union of its parameters' accesses; using a type for writing and
/// also elsewhere in the same system is rejected when the system is added
/// ([`EcsError::ConflictingAccess`], [`EcsError::ConflictingResourceAccess`]). The trait is
/// sealed: its methods are internal (DECISIONS #17).
///
/// ```
/// use pragma_ecs::{Res, Schedule, SystemParam, View};
///
/// struct Position(f32);
/// struct Gravity(f32);
///
/// fn assert_param<P: SystemParam>() {}
/// assert_param::<View<&mut Position>>();
/// assert_param::<Res<Gravity>>();
///
/// fn aliasing(_: View<&mut Position>, _: View<&Position>) {}
/// assert!(Schedule::builder().add_system("aliasing", aliasing).is_err());
/// ```
pub trait SystemParam: Sealed {
    /// The per-system state kept between frames (a query's cache).
    #[doc(hidden)]
    type State: Send + 'static;

    /// The parameter as the system receives it in a frame.
    type Item<'w, 's>;

    /// The state of a new system.
    #[doc(hidden)]
    fn init() -> Result<Self::State, EcsError>;

    /// Adds the parameter's access to the system's `access`.
    #[doc(hidden)]
    fn declare(access: &mut Access) -> Result<(), EcsError>;

    /// The parameter for one run of the system.
    #[doc(hidden)]
    fn fetch<'w, 's>(
        state: &'s mut Self::State,
        env: Env<'w>,
    ) -> Result<Self::Item<'w, 's>, EcsError>;

    /// Applies what the parameter deferred to the sync point (commands, events) and empties
    /// its buffers (DECISIONS #24).
    #[doc(hidden)]
    fn apply(_state: &mut Self::State, _world: &mut World) -> Result<(), EcsError> {
        Ok(())
    }

    /// Drops what the parameter deferred, without applying it (DECISIONS #25).
    #[doc(hidden)]
    fn discard(_state: &mut Self::State) {}
}

/// A system parameter: the entities that have the components `Q` asks for and pass the filter
/// `F`, in the frame's world.
///
/// The system-side counterpart of [`Query<Q, F>`](Query), with the same `Q` and `F`, the same
/// row order and the same archetype cache kept between frames. Its components are the columns
/// the schedule lent to the frame (DECISIONS #18): the system holds them for its whole run, so
/// systems with disjoint access run in parallel.
///
/// ```
/// use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
/// use pragma_ecs::{EcsError, Schedule, View, Without, World};
///
/// struct Position(f32);
/// struct Velocity(f32);
/// struct Frozen;
///
/// fn movement(mut bodies: View<(&mut Position, &Velocity), Without<Frozen>>) -> Result<(), EcsError> {
///     for (mut position, velocity) in bodies.iter_mut()? {
///         position.0 += velocity.0;
///     }
///     Ok(())
/// }
///
/// fn drift(mut positions: View<&mut Position>) -> Result<(), EcsError> {
///     positions.par_for_each_mut(Grain::Auto, |mut position| position.0 += 0.5)
/// }
///
/// let mut world = World::new();
/// for i in 0..10 {
///     let entity = world.spawn()?;
///     world.insert(entity, Position(0.0))?;
///     world.insert(entity, Velocity(1.0))?;
///     if i % 2 == 0 {
///         world.insert(entity, Frozen)?;
///     }
/// }
///
/// let mut builder = Schedule::builder();
/// builder.add_system("movement", movement)?;
/// builder.add_system_with("drift", drift, |c| c.after("movement"))?;
/// let mut schedule = builder.build()?;
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
/// schedule.run(&mut world, &pool)?;
///
/// let mut positions = pragma_ecs::Query::<&Position>::new()?;
/// let sum: f32 = positions.iter(&world)?.map(|p| p.0).sum();
/// assert_eq!(sum, 5.0 * 1.0 + 10.0 * 0.5);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct View<'w, 's, Q: QueryData, F: QueryFilter = ()> {
    world: &'w World,
    pool: &'w ThreadPool,
    guard: Q::Guard<'w>,
    /// The tick columns a row filter reads that `guard` does not hold.
    filter: F::Guard<'w>,
    ticks: SystemTicks,
    query: &'s mut Query<Q, F>,
}

impl<Q: QueryData, F: QueryFilter> fmt::Debug for View<'_, '_, Q, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("View")
            .field("query", &self.query)
            .finish_non_exhaustive()
    }
}

impl<'w, Q: QueryData, F: QueryFilter> View<'w, '_, Q, F> {
    /// The component types the view reads and writes.
    ///
    /// ```
    /// use pragma_ecs::{AccessKind, View};
    ///
    /// fn system(view: View<(&mut u32, &u8)>) {
    ///     assert_eq!(view.access().kind_of::<u32>(), Some(AccessKind::Write));
    /// }
    /// # let _ = system;
    /// ```
    #[must_use]
    pub fn access(&self) -> &Access {
        self.query.access()
    }

    /// Iterates the matching entities without changing them.
    ///
    /// ```
    /// use pragma_ecs::{EcsError, ResMut, View};
    ///
    /// struct Health(u32);
    /// struct Total(u32);
    ///
    /// fn total(mut healths: View<&Health>, mut total: ResMut<Total>) -> Result<(), EcsError> {
    ///     total.0 = healths.iter()?.map(|h| h.0).sum();
    ///     Ok(())
    /// }
    /// # let _ = total;
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the cache of matching tables cannot grow.
    pub fn iter(&mut self) -> Result<ViewIter<'_, Q, F>, EcsError>
    where
        Q: ReadOnlyQueryData,
    {
        self.rows()
    }

    /// Iterates the matching entities, with mutable access to the components `Q` writes.
    ///
    /// ```
    /// use pragma_ecs::{EcsError, View};
    ///
    /// struct Health(u32);
    ///
    /// fn heal(mut healths: View<&mut Health>) -> Result<(), EcsError> {
    ///     for mut health in healths.iter_mut()? {
    ///         health.0 += 1;
    ///     }
    ///     Ok(())
    /// }
    /// # let _ = heal;
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the cache of matching tables cannot grow.
    pub fn iter_mut(&mut self) -> Result<ViewIter<'_, Q, F>, EcsError> {
        self.rows()
    }

    /// Calls `f` for every matching entity, in parallel on the frame's pool, without changing
    /// them. Chunks as in [`Query::par_for_each`]: the split depends only on the data.
    ///
    /// ```
    /// use std::sync::atomic::{AtomicU64, Ordering};
    ///
    /// use pragma_core::prelude::Grain;
    /// use pragma_ecs::{EcsError, ResMut, View};
    ///
    /// struct Mass(u64);
    /// struct Total(u64);
    ///
    /// fn weigh(mut masses: View<&Mass>, mut total: ResMut<Total>) -> Result<(), EcsError> {
    ///     let sum = AtomicU64::new(0);
    ///     masses.par_for_each(Grain::Auto, |m| {
    ///         sum.fetch_add(m.0, Ordering::Relaxed);
    ///     })?;
    ///     total.0 = sum.into_inner();
    ///     Ok(())
    /// }
    /// # let _ = weigh;
    /// ```
    ///
    /// # Errors
    ///
    /// [`EcsError::Job`] if `f` panicked (the other chunks still run to completion);
    /// [`EcsError::OutOfMemory`] if the cache or the chunk list cannot be allocated.
    pub fn par_for_each<'a, G>(&'a mut self, grain: Grain, f: G) -> Result<(), EcsError>
    where
        Q: ReadOnlyQueryData,
        G: Fn(Q::Item<'a>) + Sync,
    {
        self.par_rows(grain, f)
    }

    /// Calls `f` for every matching entity, in parallel on the frame's pool, with mutable access
    /// to the components `Q` writes. See the [`View`] example.
    ///
    /// # Errors
    ///
    /// [`EcsError::Job`] if `f` panicked (the other chunks still run to completion);
    /// [`EcsError::OutOfMemory`] if the cache or the chunk list cannot be allocated.
    pub fn par_for_each_mut<'a, G>(&'a mut self, grain: Grain, f: G) -> Result<(), EcsError>
    where
        G: Fn(Q::Item<'a>) + Sync,
    {
        self.par_rows(grain, f)
    }

    fn rows(&mut self) -> Result<ViewIter<'_, Q, F>, EcsError> {
        self.prepare()?;
        Ok(ViewIter {
            tables: self.world.archetypes(),
            ids: self.query.matched().iter(),
            masks: Masks::new(self.query.mask()),
            cursor: Q::cursor(&mut self.guard),
            tick: self.ticks.this_run,
            rows: None,
        })
    }

    fn par_rows<'a, G>(&'a mut self, grain: Grain, f: G) -> Result<(), EcsError>
    where
        G: Fn(Q::Item<'a>) + Sync,
    {
        self.prepare()?;
        let tables: &'a [Archetype] = self.world.archetypes();
        let matched = self.query.matched();
        let plan = Plan::new(
            matched
                .iter()
                .filter_map(|&id| tables.get(id))
                .map(Archetype::len),
            grain,
        );
        let tick = self.ticks.this_run;
        let mut masks = Masks::<F>::new(self.query.mask());
        let mut cursor = Q::cursor(&mut self.guard);
        let rows = matched.iter().filter_map(|&id| {
            let entities = tables.get(id)?.entities();
            let mask = masks.take(entities.len());
            Some((
                entities.len(),
                Q::lent(&mut cursor, id, entities, tick)?,
                mask,
            ))
        });
        run::<Q, G>(rows, plan, self.pool, &f, self.world.len())
    }

    /// Brings the archetype cache up to date and fills the row mask of a row filter from the
    /// lent tick columns: the filter's own guards, or the data's for a type it holds.
    fn prepare(&mut self) -> Result<(), EcsError> {
        self.query.update(self.world)?;
        let (filter, guard) = (&self.filter, &self.guard);
        self.query.fill_mask(
            self.world.archetypes(),
            self.ticks.last_run,
            self.world.len(),
            |id, ty| F::lent_ticks(filter, ty, id).or_else(|| Q::lent_ticks(guard, ty, id)),
        )
    }
}

impl<Q: QueryData, F: QueryFilter> Sealed for View<'_, '_, Q, F> {}

impl<Q, F> SystemParam for View<'_, '_, Q, F>
where
    Q: QueryData + 'static,
    F: QueryFilter + 'static,
{
    type State = Query<Q, F>;
    type Item<'w, 's> = View<'w, 's, Q, F>;

    fn init() -> Result<Self::State, EcsError> {
        Query::new()
    }

    fn declare(access: &mut Access) -> Result<(), EcsError> {
        // A filter's read merges into the same view's data access, not into other parameters'
        // (DECISIONS #28).
        let mut view = Access::new();
        Q::declare(&mut view)?;
        F::declare(&mut view)?;
        access.merge(&view)
    }

    fn fetch<'w, 's>(
        state: &'s mut Self::State,
        env: Env<'w>,
    ) -> Result<Self::Item<'w, 's>, EcsError> {
        Ok(View {
            world: env.world,
            pool: env.ctx.pool(),
            guard: Q::acquire(env.ctx)?,
            filter: F::acquire::<Q>(env.ctx)?,
            ticks: env.ticks,
            query: state,
        })
    }
}

/// The iterator of [`View::iter`] and [`View::iter_mut`].
///
/// ```
/// use pragma_ecs::{EcsError, View, ViewIter};
///
/// fn count(mut values: View<&u32>) -> Result<(), EcsError> {
///     let rows: ViewIter<'_, &u32> = values.iter()?;
///     let _ = rows.count();
///     Ok(())
/// }
/// # let _ = count;
/// ```
pub struct ViewIter<'a, Q: QueryData, F: QueryFilter = ()> {
    tables: &'a [Archetype],
    ids: slice::Iter<'a, usize>,
    /// The row mask of the tables not visited yet.
    masks: Masks<'a, F>,
    cursor: Q::Cursor<'a>,
    /// The change tick a write stamps.
    tick: u64,
    /// The rest of the current table.
    rows: Option<Masked<'a, Q::Iter<'a>>>,
}

impl<Q: QueryData, F: QueryFilter> fmt::Debug for ViewIter<'_, Q, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewIter")
            .field("tables_left", &self.ids.len())
            .finish_non_exhaustive()
    }
}

impl<'a, Q: QueryData, F: QueryFilter> ViewIter<'a, Q, F> {
    /// The rows of the next matching table; `None` when every table was visited.
    #[inline(always)]
    fn table(&mut self) -> Option<Option<Masked<'a, Q::Iter<'a>>>> {
        let &id = self.ids.next()?;
        let entities = self.tables.get(id)?.entities();
        let mask = self.masks.take(entities.len());
        Some(
            Q::lent(&mut self.cursor, id, entities, self.tick)
                .map(|rows| Masked::new(Q::iter(rows), mask)),
        )
    }
}

impl<'a, Q: QueryData, F: QueryFilter> Iterator for ViewIter<'a, Q, F> {
    type Item = Q::Item<'a>;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.rows.as_mut().and_then(Iterator::next) {
                return Some(item);
            }
            self.rows = self.table()?;
        }
    }

    fn fold<B, G: FnMut(B, Self::Item) -> B>(mut self, init: B, mut g: G) -> B {
        let mut acc = self
            .rows
            .take()
            .into_iter()
            .fold(init, |acc, rows| rows.fold(acc, &mut g));
        while let Some(rows) = self.table() {
            acc = rows
                .into_iter()
                .fold(acc, |acc, rows| rows.fold(acc, &mut g));
        }
        acc
    }
}

impl<T: Resource> Sealed for Res<'_, T> {}

impl<T: Resource> SystemParam for Res<'_, T> {
    type State = ();
    type Item<'w, 's> = Res<'w, T>;

    fn init() -> Result<Self::State, EcsError> {
        Ok(())
    }

    fn declare(access: &mut Access) -> Result<(), EcsError> {
        access.add_resource::<T>(AccessKind::Read)
    }

    fn fetch<'w, 's>((): &'s mut (), env: Env<'w>) -> Result<Self::Item<'w, 's>, EcsError> {
        env.ctx.read::<T>().map(Res).map_err(EcsError::graph)
    }
}

impl<T: Resource> Sealed for ResMut<'_, T> {}

impl<T: Resource> SystemParam for ResMut<'_, T> {
    type State = ();
    type Item<'w, 's> = ResMut<'w, T>;

    fn init() -> Result<Self::State, EcsError> {
        Ok(())
    }

    fn declare(access: &mut Access) -> Result<(), EcsError> {
        access.add_resource::<T>(AccessKind::Write)
    }

    fn fetch<'w, 's>((): &'s mut (), env: Env<'w>) -> Result<Self::Item<'w, 's>, EcsError> {
        env.ctx.write::<T>().map(ResMut).map_err(EcsError::graph)
    }
}

/// An error a system returned, type-erased.
pub(crate) type BoxError = Box<dyn Error + Send + Sync>;

/// What a system function may return: `()` or `Result<(), E>` for any error type `E` (an
/// [`EcsError`], a `Box<dyn Error + Send + Sync>`, a game's own error type).
///
/// An error stops the systems ordered after this one in the frame and is returned from
/// [`Schedule::run`](crate::Schedule::run) as [`EcsError::SystemFailed`], with the error's
/// message and, if it was an [`EcsError`], the error itself. The trait is sealed.
///
/// ```
/// use pragma_core::prelude::{PoolConfig, ThreadPool};
/// use pragma_ecs::{EcsError, Res, Schedule, View, World};
///
/// struct Level(u32);
///
/// fn infallible(_: View<&u8>) {}
/// fn checked(mut v: View<&u16>) -> Result<(), EcsError> {
///     v.iter()?.for_each(drop);
///     Ok(())
/// }
/// fn scripted(level: Res<Level>) -> Result<(), String> {
///     if level.0 > 9 { Err(format!("no level {}", level.0)) } else { Ok(()) }
/// }
///
/// let mut builder = Schedule::builder();
/// builder.add_system("infallible", infallible)?.add_system("checked", checked)?;
/// builder.add_system("scripted", scripted)?;
/// let mut schedule = builder.build()?;
///
/// let mut world = World::new();
/// world.insert_resource(Level(10));
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(1))?;
/// match schedule.run(&mut world, &pool) {
///     Err(EcsError::SystemFailed { system, message, source, .. }) => {
///         assert_eq!((system.as_str(), message.as_str()), ("scripted", "no level 10"));
///         assert!(source.is_none()); // not an `EcsError`
///     }
///     other => panic!("unexpected {other:?}"),
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub trait SystemOutput: Sealed {
    /// The result as the schedule sees it.
    #[doc(hidden)]
    fn into_result(self) -> Result<(), Box<dyn Error + Send + Sync>>;
}

impl SystemOutput for () {
    fn into_result(self) -> Result<(), BoxError> {
        Ok(())
    }
}

impl<E> Sealed for Result<(), E> {}

impl<E: Into<BoxError>> SystemOutput for Result<(), E> {
    fn into_result(self) -> Result<(), BoxError> {
        self.map_err(Into::into)
    }
}

/// A system ready to run, with its access.
#[doc(hidden)]
pub struct Prepared {
    pub(crate) access: Access,
    pub(crate) system: Box<dyn RunSystem>,
}

impl fmt::Debug for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prepared")
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

/// A type-erased system.
#[doc(hidden)]
pub trait RunSystem: Send {
    /// Runs the system once.
    fn run(&mut self, env: Env<'_>) -> Result<(), BoxError>;

    /// Applies what the system's parameters deferred to the sync point.
    fn apply(&mut self, world: &mut World) -> Result<(), EcsError>;

    /// Drops what the system's parameters deferred.
    fn discard(&mut self);
}

/// A function that can be added to a [`Schedule`](crate::Schedule) as a system.
///
/// Implemented for functions (and closures) of up to 8 [`SystemParam`]s returning a
/// [`SystemOutput`]; `Marker` only tells the implementations apart. The trait is sealed.
///
/// ```
/// use pragma_ecs::{IntoSystem, Res, View};
///
/// struct Speed(f32);
///
/// fn movement(_: View<&mut f32>, _: Res<Speed>) {}
///
/// fn assert_system<M>(_: impl IntoSystem<M>) {}
/// assert_system(movement);
/// ```
pub trait IntoSystem<Marker>: SealedSystem<Marker> {
    /// The system with its state and access.
    #[doc(hidden)]
    fn into_system(self) -> Result<Prepared, EcsError>;
}

/// A function with its parameters' state.
struct FunctionSystem<Func, P: SystemParam, Out> {
    func: Func,
    state: P::State,
    marker: PhantomData<fn() -> Out>,
}

/// The parameter type of a system function in a frame.
type ParamItem<'w, 's, P> = <P as SystemParam>::Item<'w, 's>;

macro_rules! function_system {
    ($($param:ident $value:ident),*) => {
        impl<$($param: SystemParam),*> Sealed for ($($param,)*) {}

        #[allow(unused_variables)]
        impl<$($param: SystemParam),*> SystemParam for ($($param,)*) {
            type State = ($($param::State,)*);
            type Item<'w, 's> = ($($param::Item<'w, 's>,)*);

            fn init() -> Result<Self::State, EcsError> {
                Ok(($($param::init()?,)*))
            }

            fn declare(access: &mut Access) -> Result<(), EcsError> {
                $($param::declare(access)?;)*
                Ok(())
            }

            fn fetch<'w, 's>(
                state: &'s mut Self::State,
                env: Env<'w>,
            ) -> Result<Self::Item<'w, 's>, EcsError> {
                let ($($value,)*) = state;
                Ok(($($param::fetch($value, env)?,)*))
            }

            fn apply(state: &mut Self::State, world: &mut World) -> Result<(), EcsError> {
                let ($($value,)*) = state;
                $($param::apply($value, world)?;)*
                Ok(())
            }

            fn discard(state: &mut Self::State) {
                let ($($value,)*) = state;
                $($param::discard($value);)*
            }
        }

        impl<Func, Out, $($param),*> SealedSystem<fn($($param,)*) -> Out> for Func
        where
            Func: Send + 'static,
            for<'a> &'a mut Func:
                FnMut($($param),*) -> Out + FnMut($(ParamItem<'_, '_, $param>),*) -> Out,
            Out: SystemOutput + 'static,
            $($param: SystemParam + 'static,)*
        {
        }

        impl<Func, Out, $($param),*> IntoSystem<fn($($param,)*) -> Out> for Func
        where
            Func: Send + 'static,
            for<'a> &'a mut Func:
                FnMut($($param),*) -> Out + FnMut($(ParamItem<'_, '_, $param>),*) -> Out,
            Out: SystemOutput + 'static,
            $($param: SystemParam + 'static,)*
        {
            fn into_system(self) -> Result<Prepared, EcsError> {
                let mut access = Access::new();
                <($($param,)*) as SystemParam>::declare(&mut access)?;
                let system = FunctionSystem::<Func, ($($param,)*), Out> {
                    func: self,
                    state: <($($param,)*) as SystemParam>::init()?,
                    marker: PhantomData,
                };
                Ok(Prepared {
                    access,
                    system: Box::new(system),
                })
            }
        }

        impl<Func, Out, $($param),*> RunSystem for FunctionSystem<Func, ($($param,)*), Out>
        where
            Func: Send + 'static,
            for<'a> &'a mut Func:
                FnMut($($param),*) -> Out + FnMut($(ParamItem<'_, '_, $param>),*) -> Out,
            Out: SystemOutput + 'static,
            $($param: SystemParam + 'static,)*
        {
            fn run(&mut self, env: Env<'_>) -> Result<(), BoxError> {
                // Calling through a generic function lets the compiler pick the
                // higher-ranked `FnMut` bound.
                #[allow(clippy::too_many_arguments)]
                fn call<Out, $($param),*>(
                    mut f: impl FnMut($($param),*) -> Out,
                    $($value: $param,)*
                ) -> Out {
                    f($($value),*)
                }
                let ($($value,)*) = <($($param,)*) as SystemParam>::fetch(&mut self.state, env)?;
                call(&mut self.func, $($value),*).into_result()
            }

            fn apply(&mut self, world: &mut World) -> Result<(), EcsError> {
                <($($param,)*) as SystemParam>::apply(&mut self.state, world)
            }

            fn discard(&mut self) {
                <($($param,)*) as SystemParam>::discard(&mut self.state);
            }
        }
    };
}

function_system!();
function_system!(P0 p0);
function_system!(P0 p0, P1 p1);
function_system!(P0 p0, P1 p1, P2 p2);
function_system!(P0 p0, P1 p1, P2 p2, P3 p3);
function_system!(P0 p0, P1 p1, P2 p2, P3 p3, P4 p4);
function_system!(P0 p0, P1 p1, P2 p2, P3 p3, P4 p4, P5 p5);
function_system!(P0 p0, P1 p1, P2 p2, P3 p3, P4 p4, P5 p5, P6 p6);
function_system!(P0 p0, P1 p1, P2 p2, P3 p3, P4 p4, P5 p5, P6 p6, P7 p7);
