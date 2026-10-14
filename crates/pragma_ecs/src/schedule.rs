//! Schedules: systems as tasks of the `pragma_core` frame graph (DECISIONS #17–#21).

use std::fmt;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pragma_core::graph::{FrameGraph, FrameGraphBuilder, GraphError, TaskContext};
use pragma_core::jobs::ThreadPool;

use crate::access::Access;
use crate::event::EventType;
use crate::lend::{Lender, WorldSlot, data_name};
use crate::system::{BoxError, Env, IntoSystem, Prepared, RunSystem, SystemTicks};
use crate::{EcsError, World};

/// The explicit order of a system relative to others, given to
/// [`ScheduleBuilder::add_system_with`].
///
/// Order matters only for systems whose accesses conflict (one writes what the other uses); it
/// can also order systems that share nothing.
///
/// ```
/// use pragma_ecs::{Schedule, View};
///
/// struct Position(f32);
///
/// fn input() {}
/// fn movement(_: View<&mut Position>) {}
/// fn render(_: View<&Position>) {}
///
/// let mut builder = Schedule::builder();
/// builder.add_system("input", input)?;
/// builder.add_system_with("movement", movement, |c| c.after("input"))?;
/// builder.add_system_with("render", render, |c| c.after("movement"))?;
/// let schedule = builder.build()?;
/// assert_eq!(schedule.order().collect::<Vec<_>>(), ["input", "movement", "render"]);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
#[derive(Debug, Default)]
#[must_use]
pub struct SystemConfig {
    after: Vec<String>,
    before: Vec<String>,
}

impl SystemConfig {
    /// The system runs after system `system`.
    pub fn after(mut self, system: impl Into<String>) -> Self {
        self.after.push(system.into());
        self
    }

    /// The system runs before system `system`.
    pub fn before(mut self, system: impl Into<String>) -> Self {
        self.before.push(system.into());
        self
    }
}

/// The error a system returned: its message, and the error itself if it was an [`EcsError`].
type Failure = (String, Option<EcsError>);

/// A system with what the schedule tracks about it between frames.
struct Cell {
    system: Box<dyn RunSystem>,
    /// The system's position in the schedule's stable topological order: its tick in a frame
    /// is the frame's base tick plus this (DECISIONS #27).
    position: u64,
    /// The tick of its last run, and the world it ran over.
    last_run: u64,
    world: Option<u64>,
    /// Whether it completed without an error in the current frame; only then are its deferred
    /// commands and events applied (DECISIONS #25).
    ok: bool,
}

/// The system, shared by its frame-graph task and the schedule's sync point.
type Shared = Arc<Mutex<Cell>>;

fn lock(cell: &Shared) -> MutexGuard<'_, Cell> {
    cell.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A registered system: its name, access, the error it returned in the last frame and the
/// system itself.
struct SystemInfo {
    name: String,
    access: Access,
    failure: Arc<Mutex<Option<Failure>>>,
    cell: Shared,
}

/// The work of one frame-graph task.
struct Task {
    cell: Shared,
    failure: Arc<Mutex<Option<Failure>>>,
}

impl Task {
    fn run(&self, ctx: &TaskContext<'_>) -> Result<(), BoxError> {
        let world = ctx.read::<WorldSlot>().map_err(EcsError::graph)?;
        let mut cell = lock(&self.cell);
        let id = world.0.id();
        if cell.world != Some(id) {
            cell.world = Some(id);
            cell.last_run = 0;
        }
        let ticks = SystemTicks {
            last_run: cell.last_run,
            this_run: world.0.frame_base().saturating_add(cell.position),
        };
        cell.ok = false;
        let result = cell.system.run(Env::new(ctx, &world.0, ticks));
        cell.last_run = ticks.this_run;
        cell.ok = result.is_ok();
        if let Err(error) = &result {
            let typed = error.downcast_ref::<EcsError>().cloned();
            *self.failure.lock().unwrap_or_else(PoisonError::into_inner) =
                Some((error.to_string(), typed));
        }
        result
    }
}

/// Builds a [`Schedule`] (see [`Schedule::builder`]).
///
/// ```
/// use pragma_ecs::{EcsError, ScheduleBuilder, View};
///
/// struct Health(u32);
///
/// fn regenerate(mut healths: View<&mut Health>) -> Result<(), EcsError> {
///     healths.iter_mut()?.for_each(|mut h| h.0 += 1);
///     Ok(())
/// }
///
/// fn register_gameplay(builder: &mut ScheduleBuilder) -> Result<(), EcsError> {
///     builder.add_system("regenerate", regenerate)?;
///     Ok(())
/// }
///
/// let mut builder = pragma_ecs::Schedule::builder();
/// register_gameplay(&mut builder)?;
/// assert_eq!(builder.build()?.len(), 1);
/// # Ok::<(), EcsError>(())
/// ```
#[must_use]
pub struct ScheduleBuilder {
    graph: FrameGraphBuilder,
    systems: Vec<SystemInfo>,
    /// One per component type some system uses, in registration order.
    lenders: Vec<Lender>,
    /// One per event type some system writes or reads, in registration order.
    events: Vec<EventType>,
}

impl fmt::Debug for ScheduleBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScheduleBuilder")
            .field(
                "systems",
                &self.systems.iter().map(|s| &s.name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl ScheduleBuilder {
    /// The `ordered` mode: systems whose accesses conflict without an explicit order run in
    /// registration order instead of failing the build with [`EcsError::AmbiguousOrder`]
    /// (pragma.core's `ordered` mode, DECISIONS #19). Explicit `after`/`before` take precedence;
    /// independent systems still run in parallel.
    ///
    /// ```
    /// use pragma_ecs::{ResMut, Schedule};
    ///
    /// struct Score(u32);
    ///
    /// fn add(mut score: ResMut<Score>) { score.0 += 1; }
    /// fn double(mut score: ResMut<Score>) { score.0 *= 2; }
    ///
    /// let mut builder = Schedule::builder().ordered();
    /// builder.add_system("add", add)?.add_system("double", double)?; // no `after` needed
    /// let schedule = builder.build()?;
    /// assert_eq!(schedule.order().collect::<Vec<_>>(), ["add", "double"]);
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    pub fn ordered(self) -> Self {
        Self {
            graph: self.graph.ordered(),
            ..self
        }
    }

    /// Adds `system` under the name `name`, without an explicit order.
    ///
    /// The system's access is the union of its parameters' accesses (DECISIONS #17).
    ///
    /// # Errors
    ///
    /// [`EcsError::ConflictingAccess`] / [`EcsError::ConflictingResourceAccess`] if the system
    /// writes a type it also uses elsewhere in its parameters; [`EcsError::OutOfMemory`] if the
    /// system list cannot grow. A name used twice is reported by [`build`](Self::build).
    pub fn add_system<M>(
        &mut self,
        name: impl Into<String>,
        system: impl IntoSystem<M>,
    ) -> Result<&mut Self, EcsError> {
        self.add_system_with(name, system, |config| config)
    }

    /// Adds `system` under the name `name`, ordered by `configure` (see [`SystemConfig`]).
    ///
    /// # Errors
    ///
    /// As [`add_system`](Self::add_system).
    pub fn add_system_with<M>(
        &mut self,
        name: impl Into<String>,
        system: impl IntoSystem<M>,
        configure: impl FnOnce(SystemConfig) -> SystemConfig,
    ) -> Result<&mut Self, EcsError> {
        let oom = |_| EcsError::OutOfMemory { entities: 0 };
        let name = name.into();
        let Prepared { access, system } = system.into_system()?;
        let config = configure(SystemConfig::default());
        self.systems.try_reserve(1).map_err(oom)?;
        for lender in access.lenders() {
            if !self.lenders.iter().any(|known| known.ty == lender.ty) {
                self.lenders.try_reserve(1).map_err(oom)?;
                self.lenders.push(lender);
            }
        }
        for event in access.events() {
            if !self.events.contains(&event) {
                self.events.try_reserve(1).map_err(oom)?;
                self.events.push(event);
            }
        }

        let failure = Arc::new(Mutex::new(None));
        let cell = Arc::new(Mutex::new(Cell {
            system,
            position: 0,
            last_run: 0,
            world: None,
            ok: false,
        }));
        let task = Task {
            cell: Arc::clone(&cell),
            failure: Arc::clone(&failure),
        };
        self.graph.task(
            name.as_str(),
            |task| {
                let task = access.configure(task.reads::<WorldSlot>());
                let task = config.after.into_iter().fold(task, |t, s| t.after(s));
                config.before.into_iter().fold(task, |t, s| t.before(s))
            },
            move |ctx: &TaskContext<'_>| task.run(ctx),
        );
        self.systems.push(SystemInfo {
            name,
            access,
            failure,
            cell,
        });
        Ok(self)
    }

    /// Validates the systems and builds the schedule. Build once at startup, or when the set
    /// of systems changes, not every frame.
    ///
    /// # Errors
    ///
    /// [`EcsError::AmbiguousOrder`] if two systems conflict without an explicit order (unless
    /// the builder is [`ordered`](Self::ordered)); [`EcsError::Schedule`] for a dependency
    /// cycle, an unknown system in `after`/`before`, a name used twice or a schedule too large.
    pub fn build(self) -> Result<Schedule, EcsError> {
        let Self {
            graph,
            systems,
            lenders,
            events,
        } = self;
        let graph = graph.build().map_err(|error| map_error(error, &systems))?;
        // A system's tick in a frame follows the stable topological order, which every pair of
        // conflicting systems runs in (DECISIONS #27).
        for (position, name) in (0_u64..).zip(graph.order()) {
            if let Some(system) = systems.iter().find(|s| s.name == name) {
                lock(&system.cell).position = position;
            }
        }
        Ok(Schedule {
            graph,
            systems,
            lenders,
            events,
        })
    }
}

/// Systems on the `pragma_core` frame graph: built once ([`Schedule::builder`]), run every frame
/// ([`Schedule::run`]).
///
/// Every system is a graph task whose declared access is the system's [`Access`]: systems with
/// disjoint access run in parallel on the pool, conflicting ones in the order given by
/// [`SystemConfig`], and conflicting systems without an order are a build error
/// ([`EcsError::AmbiguousOrder`], DECISIONS #19).
///
/// ```
/// use pragma_core::prelude::{PoolConfig, ThreadPool};
/// use pragma_ecs::{EcsError, Res, ResMut, Schedule, View, World};
///
/// struct Position(f32);
/// struct Velocity(f32);
/// struct Dt(f32);
/// struct Energy(f32);
///
/// fn movement(mut bodies: View<(&mut Position, &Velocity)>, dt: Res<Dt>) -> Result<(), EcsError> {
///     for (mut position, velocity) in bodies.iter_mut()? {
///         position.0 += velocity.0 * dt.0;
///     }
///     Ok(())
/// }
///
/// fn energy(mut velocities: View<&Velocity>, mut energy: ResMut<Energy>) -> Result<(), EcsError> {
///     energy.0 = velocities.iter()?.map(|v| v.0 * v.0 / 2.0).sum();
///     Ok(())
/// }
///
/// let mut world = World::new();
/// for i in 0..100 {
///     let entity = world.spawn()?;
///     world.insert(entity, Position(0.0))?;
///     world.insert(entity, Velocity(if i % 2 == 0 { 1.0 } else { 2.0 }))?;
/// }
/// world.insert_resource(Dt(0.5));
/// world.insert_resource(Energy(0.0));
///
/// let mut builder = Schedule::builder();
/// builder.add_system("movement", movement)?;
/// builder.add_system("energy", energy)?; // only reads `Velocity`: runs in parallel
/// let mut schedule = builder.build()?;
///
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
/// for _ in 0..4 {
///     schedule.run(&mut world, &pool)?;
/// }
/// assert_eq!(world.resource::<Energy>().map(|e| e.0), Some(50.0 * 0.5 + 50.0 * 2.0));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Schedule {
    graph: FrameGraph,
    systems: Vec<SystemInfo>,
    lenders: Vec<Lender>,
    events: Vec<EventType>,
}

impl fmt::Debug for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Schedule")
            .field("order", &self.order().collect::<Vec<_>>())
            .field("edges", &self.edges().count())
            .finish()
    }
}

impl Schedule {
    /// A new builder.
    pub fn builder() -> ScheduleBuilder {
        ScheduleBuilder {
            graph: FrameGraph::builder(),
            systems: Vec::new(),
            lenders: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Number of systems.
    #[must_use]
    pub fn len(&self) -> usize {
        self.systems.len()
    }

    /// Whether the schedule has no systems.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.systems.is_empty()
    }

    /// Every system's name and access, in registration order.
    ///
    /// ```
    /// use pragma_ecs::{AccessKind, Schedule, View};
    ///
    /// fn movement(_: View<(&mut f32, &u8)>) {}
    ///
    /// let mut builder = Schedule::builder();
    /// builder.add_system("movement", movement)?;
    /// let schedule = builder.build()?;
    /// let systems: Vec<_> = schedule.systems().collect();
    /// assert_eq!(systems[0].0, "movement");
    /// assert_eq!(systems[0].1.kind_of::<f32>(), Some(AccessKind::Write));
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    pub fn systems(&self) -> impl Iterator<Item = (&str, &Access)> {
        self.systems.iter().map(|s| (s.name.as_str(), &s.access))
    }

    /// System names in a stable topological order: among ready systems, the one registered
    /// earlier comes first. A frame on one worker does not necessarily run them in this order,
    /// but any actual order respects the same edges. See the [`SystemConfig`] example.
    pub fn order(&self) -> impl Iterator<Item = &str> {
        self.graph.order()
    }

    /// "Earlier → later" edges between systems after transitive reduction.
    ///
    /// ```
    /// use pragma_ecs::{Schedule, View};
    ///
    /// fn write(_: View<&mut u32>) {}
    /// fn read(_: View<&u32>) {}
    ///
    /// let mut builder = Schedule::builder();
    /// builder.add_system("write", write)?;
    /// builder.add_system_with("read", read, |c| c.after("write"))?;
    /// let schedule = builder.build()?;
    /// assert_eq!(schedule.edges().collect::<Vec<_>>(), [("write", "read")]);
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    pub fn edges(&self) -> impl Iterator<Item = (&str, &str)> {
        self.graph.edges()
    }

    /// Runs a frame: every system once, after the systems ordered before it; independent
    /// systems in parallel on `pool`; then the sync point.
    ///
    /// The columns of every component type a system uses are lent to the frame graph and
    /// returned afterwards, also after a failed frame (DECISIONS #18). At the sync point the
    /// event types the systems use are updated, then the commands and events every system
    /// deferred are applied, system after system in registration order (DECISIONS #24). Call it
    /// from a thread outside the pool (the main loop): that thread runs systems too.
    ///
    /// # Errors
    ///
    /// [`EcsError::MissingResource`] before any system runs if the world lacks a resource a
    /// system takes (no sync point then). [`EcsError::SystemFailed`] /
    /// [`EcsError::SystemPanicked`] if a system returned an error or panicked: independent
    /// systems still ran, the systems ordered after it did not, the error of the system
    /// registered first is returned, and only that system's deferred commands are discarded.
    /// [`EcsError::CommandFailed`] if applying deferred commands or events failed (the rest is
    /// discarded, DECISIONS #25). A pool failure is [`EcsError::Job`];
    /// [`EcsError::OutOfMemory`] if the lending slots cannot grow (nothing ran). The schedule and
    /// the world stay usable after any error.
    pub fn run(&mut self, world: &mut World, pool: &ThreadPool) -> Result<(), EcsError> {
        for system in &self.systems {
            *system
                .failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
            lock(&system.cell).ok = false;
        }
        // Everything that can fail happens before the first column is lent.
        if !world.resources_mut().contains::<WorldSlot>() {
            world.resources_mut().insert(WorldSlot(World::new()));
        }
        for event in &self.events {
            event.ensure(world);
        }
        for lender in &self.lenders {
            lender.prepare(world)?;
        }
        world.begin_frame(self.systems.len());

        for lender in &self.lenders {
            lender.swap(world);
        }
        let mut resources = mem::take(world.resources_mut());
        if let Some(slot) = resources.get_mut::<WorldSlot>() {
            mem::swap(&mut slot.0, world);
        }
        let result = self.graph.run(pool, &mut resources);
        if let Some(slot) = resources.get_mut::<WorldSlot>() {
            mem::swap(&mut slot.0, world);
        }
        *world.resources_mut() = resources;
        for lender in &self.lenders {
            lender.swap(world);
        }

        let started = !matches!(result, Err(GraphError::MissingResource { .. }));
        let applied = if started { self.sync(world) } else { Ok(()) };
        result.map_err(|error| map_error(error, &self.systems))?;
        applied
    }

    /// The sync point (DECISIONS #24, #25): updates the event types, then applies the deferred
    /// buffers of the systems that completed, in registration order. After a failure the rest
    /// is discarded.
    fn sync(&self, world: &mut World) -> Result<(), EcsError> {
        for event in &self.events {
            event.update(world);
        }
        let mut applied = Ok(());
        for system in &self.systems {
            let mut cell = lock(&system.cell);
            if !cell.ok || applied.is_err() {
                cell.system.discard();
                continue;
            }
            if let Err(source) = cell.system.apply(world) {
                cell.system.discard();
                applied = Err(EcsError::CommandFailed {
                    system: system.name.clone(),
                    source: Box::new(source),
                });
            }
        }
        applied
    }
}

/// The ECS error for a frame-graph error (DECISIONS #21).
fn map_error(error: GraphError, systems: &[SystemInfo]) -> EcsError {
    match error {
        GraphError::AmbiguousOrder { a, b, resource, .. } => EcsError::AmbiguousOrder {
            first: a,
            second: b,
            data: data_name(resource),
        },
        GraphError::MissingResource { task, resource, .. } => EcsError::MissingResource {
            system: task,
            resource,
        },
        GraphError::TaskPanicked { task, message, .. } => EcsError::SystemPanicked {
            system: task,
            message,
        },
        GraphError::Job(source) => EcsError::Job { source },
        GraphError::TaskFailed { ref task, .. } => {
            let returned = systems.iter().find(|s| s.name == *task).and_then(|s| {
                s.failure
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
            });
            match returned {
                Some((message, source)) => EcsError::SystemFailed {
                    system: task.clone(),
                    message,
                    source: source.map(Box::new),
                },
                None => EcsError::graph(error),
            }
        }
        other => EcsError::graph(other),
    }
}
