//! Deferred commands: structural changes recorded by systems and applied at the sync point
//! (DECISIONS #23–#25).

use std::any::{Any, TypeId};
use std::fmt;

use crate::access::Access;
use crate::system::{Env, Sealed, SystemParam};
use crate::{Component, EcsError, Entity, World};

/// The entity a command applies to.
#[derive(Clone, Copy, Debug)]
enum Target {
    /// An entity that existed when the command was recorded.
    Existing(Entity),
    /// The entity created by the `n`-th spawn of the same queue.
    Spawned(usize),
}

/// One recorded command.
enum Op {
    Spawn,
    Despawn(Target),
    /// Inserts the next value of lane `lane`.
    Insert {
        target: Target,
        lane: usize,
    },
    Remove {
        target: Target,
        remove: fn(&mut World, Entity) -> Result<(), EcsError>,
    },
}

/// The values to insert of one component type, in recording order (reversed while applying).
trait Lane: Send {
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Reverses the values, so that popping yields them in recording order.
    fn reverse(&mut self);

    /// Inserts the next value into `entity` (drops it if the entity is gone).
    fn insert_next(&mut self, world: &mut World, entity: Option<Entity>) -> Result<(), EcsError>;

    /// Drops every value, keeping the capacity.
    fn clear(&mut self);
}

impl<T: Component> Lane for Vec<T> {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn reverse(&mut self) {
        <[T]>::reverse(self);
    }

    fn insert_next(&mut self, world: &mut World, entity: Option<Entity>) -> Result<(), EcsError> {
        match (self.pop(), entity) {
            (Some(value), Some(entity)) => lenient(world.insert(entity, value).map(drop)),
            _ => Ok(()),
        }
    }

    fn clear(&mut self) {
        Vec::clear(self);
    }
}

/// A command on an entity that is gone is a no-op (DECISIONS #25).
fn lenient(result: Result<(), EcsError>) -> Result<(), EcsError> {
    match result {
        Err(EcsError::NoSuchEntity { .. }) => Ok(()),
        other => other,
    }
}

fn remove<T: Component>(world: &mut World, entity: Entity) -> Result<(), EcsError> {
    world.remove::<T>(entity).map(drop)
}

/// The commands one system recorded in one frame, kept in the system's state. Buffers keep
/// their capacity between frames, so a warm queue does not allocate.
#[doc(hidden)]
#[derive(Default)]
pub struct CommandQueue {
    ops: Vec<Op>,
    /// One lane per component type ever inserted through this queue.
    lanes: Vec<(TypeId, Box<dyn Lane>)>,
    /// Number of `Spawn` ops in `ops`.
    spawns: usize,
    /// The entities created while applying, by spawn number.
    spawned: Vec<Entity>,
}

impl fmt::Debug for CommandQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandQueue")
            .field("commands", &self.ops.len())
            .finish_non_exhaustive()
    }
}

impl CommandQueue {
    fn oom(&self) -> EcsError {
        EcsError::OutOfMemory {
            entities: self.ops.len().saturating_add(1),
        }
    }

    fn push(&mut self, op: Op) -> Result<(), EcsError> {
        self.ops.try_reserve(1).map_err(|_| self.oom())?;
        self.ops.push(op);
        Ok(())
    }

    fn spawn(&mut self) -> Result<Target, EcsError> {
        self.push(Op::Spawn)?;
        let target = Target::Spawned(self.spawns);
        self.spawns = self.spawns.saturating_add(1);
        Ok(target)
    }

    fn insert<T: Component>(&mut self, target: Target, value: T) -> Result<(), EcsError> {
        let ty = TypeId::of::<T>();
        let lane = match self.lanes.iter().position(|(t, _)| *t == ty) {
            Some(lane) => lane,
            None => {
                self.lanes.try_reserve(1).map_err(|_| self.oom())?;
                // One fixed-size box per component type, kept between frames (DECISIONS #23).
                self.lanes.push((ty, Box::new(Vec::<T>::new())));
                self.lanes.len().saturating_sub(1)
            }
        };
        self.ops.try_reserve(1).map_err(|_| self.oom())?;
        let oom = self.oom();
        let values = self
            .lanes
            .get_mut(lane)
            .and_then(|(_, lane)| lane.as_any_mut().downcast_mut::<Vec<T>>())
            .ok_or(EcsError::CapacityExhausted)?;
        values.try_reserve(1).map_err(|_| oom)?;
        values.push(value);
        self.ops.push(Op::Insert { target, lane });
        Ok(())
    }

    fn remove<T: Component>(&mut self, target: Target) -> Result<(), EcsError> {
        self.push(Op::Remove {
            target,
            remove: remove::<T>,
        })
    }

    /// Number of commands recorded.
    fn len(&self) -> usize {
        self.ops.len()
    }

    /// Applies every command in recording order, then empties the queue (also on an error).
    pub(crate) fn apply(&mut self, world: &mut World) -> Result<(), EcsError> {
        let result = self.apply_all(world);
        self.clear();
        result
    }

    fn apply_all(&mut self, world: &mut World) -> Result<(), EcsError> {
        let Self {
            ops,
            lanes,
            spawns,
            spawned,
        } = self;
        spawned.clear();
        spawned
            .try_reserve(*spawns)
            .map_err(|_| EcsError::OutOfMemory {
                entities: world.len().saturating_add(*spawns),
            })?;
        for (_, lane) in lanes.iter_mut() {
            lane.reverse();
        }
        let resolve = |spawned: &[Entity], target: Target| match target {
            Target::Existing(entity) => Some(entity),
            Target::Spawned(n) => spawned.get(n).copied(),
        };
        for op in ops.drain(..) {
            match op {
                Op::Spawn => spawned.push(world.spawn()?),
                Op::Despawn(target) => {
                    if let Some(entity) = resolve(spawned, target) {
                        lenient(world.despawn(entity))?;
                    }
                }
                Op::Insert { target, lane } => {
                    if let Some((_, lane)) = lanes.get_mut(lane) {
                        lane.insert_next(world, resolve(spawned, target))?;
                    }
                }
                Op::Remove { target, remove } => {
                    if let Some(entity) = resolve(spawned, target) {
                        lenient(remove(world, entity))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Drops every command, keeping the buffers.
    pub(crate) fn clear(&mut self) {
        self.ops.clear();
        for (_, lane) in &mut self.lanes {
            lane.clear();
        }
        self.spawns = 0;
        self.spawned.clear();
    }
}

/// A system parameter that records structural changes: spawning and despawning entities,
/// inserting and removing components (DECISIONS #23).
///
/// The commands are kept in the system's own queue, so `Commands` declares no access and never
/// orders systems. They are applied at the sync point at the end of
/// [`Schedule::run`](crate::Schedule::run), system after system in registration order, each
/// queue in recording order, so the world after a frame (entity ids included) is the same on any
/// number of workers (DECISIONS #24). A new entity gets its id only then: [`spawn`](Self::spawn)
/// returns an [`EntityCommands`] for further commands on it.
///
/// A command on an entity that is no longer alive is a no-op. If applying fails otherwise (out
/// of memory), the rest is discarded and `run` returns [`EcsError::CommandFailed`]; the commands
/// of a system that failed or panicked are discarded (DECISIONS #25).
///
/// ```
/// use pragma_core::prelude::{PoolConfig, ThreadPool};
/// use pragma_ecs::{Commands, EcsError, Entity, Query, Schedule, View, World};
///
/// struct Health(i32);
/// struct Corpse;
///
/// fn reap(mut bodies: View<(Entity, &Health)>, mut commands: Commands) -> Result<(), EcsError> {
///     for (entity, health) in bodies.iter()? {
///         if health.0 <= 0 {
///             commands.despawn(entity)?;
///             commands.spawn()?.insert(Corpse)?;
///         }
///     }
///     Ok(())
/// }
///
/// let mut world = World::new();
/// for hp in [5, 0, 7] {
///     let entity = world.spawn()?;
///     world.insert(entity, Health(hp))?;
/// }
/// let mut builder = Schedule::builder();
/// builder.add_system("reap", reap)?;
/// let mut schedule = builder.build()?;
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
/// schedule.run(&mut world, &pool)?;
///
/// assert_eq!(world.len(), 3);
/// assert_eq!(Query::<&Health>::new()?.iter(&world)?.count(), 2);
/// assert_eq!(Query::<&Corpse>::new()?.iter(&world)?.count(), 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Commands<'s> {
    queue: &'s mut CommandQueue,
}

impl fmt::Debug for Commands<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Commands")
            .field("commands", &self.queue.len())
            .finish()
    }
}

impl Commands<'_> {
    /// Records the creation of an entity. Its id is allocated when the commands are applied;
    /// the returned [`EntityCommands`] records further commands on it.
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the queue cannot grow.
    pub fn spawn(&mut self) -> Result<EntityCommands<'_>, EcsError> {
        let target = self.queue.spawn()?;
        Ok(EntityCommands {
            queue: self.queue,
            target,
        })
    }

    /// Commands on the existing entity `entity`.
    ///
    /// ```
    /// use pragma_ecs::{Commands, EcsError, Entity, View};
    ///
    /// struct Burning;
    /// struct Wet;
    ///
    /// fn douse(mut burning: View<Entity>, mut commands: Commands) -> Result<(), EcsError> {
    ///     for entity in burning.iter()? {
    ///         commands.entity(entity).remove::<Burning>()?.insert(Wet)?;
    ///     }
    ///     Ok(())
    /// }
    /// # let _ = douse;
    /// ```
    pub fn entity(&mut self, entity: Entity) -> EntityCommands<'_> {
        EntityCommands {
            queue: self.queue,
            target: Target::Existing(entity),
        }
    }

    /// Records despawning `entity`.
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the queue cannot grow.
    pub fn despawn(&mut self, entity: Entity) -> Result<(), EcsError> {
        self.queue.push(Op::Despawn(Target::Existing(entity)))
    }

    /// Records inserting `value` into `entity` (or replacing its component of that type).
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the queue cannot grow; `value` is dropped.
    pub fn insert<T: Component>(&mut self, entity: Entity, value: T) -> Result<(), EcsError> {
        self.queue.insert(Target::Existing(entity), value)
    }

    /// Records removing the component of type `T` from `entity`.
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the queue cannot grow.
    pub fn remove<T: Component>(&mut self, entity: Entity) -> Result<(), EcsError> {
        self.queue.remove::<T>(Target::Existing(entity))
    }

    /// Number of commands this system recorded in this frame.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether this system recorded no command in this frame.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.len() == 0
    }
}

/// Commands on one entity: an existing one ([`Commands::entity`]) or one being spawned
/// ([`Commands::spawn`]). The methods chain: `commands.spawn()?.insert(a)?.insert(b)?`.
///
/// ```
/// use pragma_ecs::{Commands, EcsError};
///
/// struct Position(f32, f32);
/// struct Bullet { damage: u32 }
///
/// fn fire(mut commands: Commands) -> Result<(), EcsError> {
///     commands
///         .spawn()?
///         .insert(Position(0.0, 0.0))?
///         .insert(Bullet { damage: 3 })?;
///     Ok(())
/// }
/// # let _ = fire;
/// ```
pub struct EntityCommands<'a> {
    queue: &'a mut CommandQueue,
    target: Target,
}

impl fmt::Debug for EntityCommands<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntityCommands")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl EntityCommands<'_> {
    /// The entity, if it existed when the commands were recorded; `None` for a spawned entity,
    /// whose id is allocated when the commands are applied.
    #[must_use]
    pub const fn id(&self) -> Option<Entity> {
        match self.target {
            Target::Existing(entity) => Some(entity),
            Target::Spawned(_) => None,
        }
    }

    /// Records inserting `value` (or replacing the component of that type).
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the queue cannot grow; `value` is dropped.
    pub fn insert<T: Component>(&mut self, value: T) -> Result<&mut Self, EcsError> {
        self.queue.insert(self.target, value)?;
        Ok(self)
    }

    /// Records removing the component of type `T`.
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the queue cannot grow.
    pub fn remove<T: Component>(&mut self) -> Result<&mut Self, EcsError> {
        self.queue.remove::<T>(self.target)?;
        Ok(self)
    }

    /// Records despawning the entity.
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the queue cannot grow.
    pub fn despawn(self) -> Result<(), EcsError> {
        self.queue.push(Op::Despawn(self.target))
    }
}

impl Sealed for Commands<'_> {}

impl SystemParam for Commands<'_> {
    type State = CommandQueue;
    type Item<'w, 's> = Commands<'s>;

    fn init() -> Result<Self::State, EcsError> {
        Ok(CommandQueue::default())
    }

    fn declare(_: &mut Access) -> Result<(), EcsError> {
        Ok(())
    }

    fn fetch<'w, 's>(
        state: &'s mut Self::State,
        _: Env<'w>,
    ) -> Result<Self::Item<'w, 's>, EcsError> {
        Ok(Commands { queue: state })
    }

    fn apply(state: &mut Self::State, world: &mut World) -> Result<(), EcsError> {
        state.apply(world)
    }

    fn discard(state: &mut Self::State) {
        state.clear();
    }
}

impl fmt::Debug for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn => f.write_str("Spawn"),
            Self::Despawn(target) => write!(f, "Despawn({target:?})"),
            Self::Insert { target, .. } => write!(f, "Insert({target:?})"),
            Self::Remove { target, .. } => write!(f, "Remove({target:?})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct A(u32);
    #[derive(Debug, PartialEq)]
    struct B(u32);

    #[test]
    fn commands_apply_in_order_and_skip_dead_entities() {
        let mut world = World::new();
        let old = world.spawn().unwrap();
        let gone = world.spawn().unwrap();
        world.despawn(gone).unwrap();

        let mut queue = CommandQueue::default();
        let mut commands = Commands { queue: &mut queue };
        commands
            .spawn()
            .unwrap()
            .insert(A(1))
            .unwrap()
            .insert(B(2))
            .unwrap();
        commands.insert(old, A(3)).unwrap();
        commands.insert(gone, A(4)).unwrap();
        commands.despawn(gone).unwrap();
        commands.remove::<B>(gone).unwrap();
        let mut second = commands.spawn().unwrap();
        assert_eq!(second.id(), None);
        second.insert(A(5)).unwrap().remove::<A>().unwrap();
        second.despawn().unwrap();
        commands.entity(old).insert(A(6)).unwrap();
        assert_eq!(commands.entity(old).id(), Some(old));
        assert_eq!(commands.len(), 12);
        assert!(format!("{commands:?}").contains("12"));

        queue.apply(&mut world).unwrap();
        assert_eq!(queue.len(), 0);
        assert_eq!(world.len(), 2);
        assert_eq!(world.get::<A>(old), Some(&A(6)));
        let mut query = crate::Query::<(Entity, &A, &B)>::new().unwrap();
        let rows: Vec<_> = query
            .iter(&world)
            .unwrap()
            .map(|(_, a, b)| (a.0, b.0))
            .collect();
        assert_eq!(rows, [(1, 2)]);

        // A warm queue keeps its lanes; a cleared queue applies nothing.
        Commands { queue: &mut queue }.insert(old, A(7)).unwrap();
        queue.clear();
        queue.apply(&mut world).unwrap();
        assert_eq!(world.get::<A>(old), Some(&A(6)));
        assert!(format!("{queue:?}").starts_with("CommandQueue"));
        assert_eq!(format!("{:?}", Op::Spawn), "Spawn");
    }
}
