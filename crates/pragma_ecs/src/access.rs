//! Declared component and resource access of queries and systems (DECISIONS #14, #17).

use std::any::{TypeId, type_name};
use std::fmt;

use pragma_core::graph::TaskConfig;

use crate::lend::{Columns, Lender};
use crate::{Component, EcsError, Resource};

/// How a query or a system uses a component or resource type.
///
/// ```
/// use pragma_ecs::{AccessKind, Query};
///
/// struct Position(f32);
/// struct Velocity(f32);
///
/// let query = Query::<(&mut Position, &Velocity)>::new()?;
/// assert_eq!(query.access().kind_of::<Position>(), Some(AccessKind::Write));
/// assert_eq!(query.access().kind_of::<Velocity>(), Some(AccessKind::Read));
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccessKind {
    /// Shared access: `&T`, `Option<&T>`, `Res<T>`.
    Read,
    /// Exclusive access: `&mut T`, `ResMut<T>`.
    Write,
}

#[derive(Clone, Copy)]
struct Entry {
    ty: TypeId,
    name: &'static str,
    kind: AccessKind,
    /// Declares this access to the frame graph (DECISIONS #18, #20).
    declare: fn(TaskConfig, AccessKind) -> TaskConfig,
    /// For a component: lends its columns to the frame graph.
    lender: Option<Lender>,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        (self.ty, self.kind) == (other.ty, other.kind)
    }
}

impl Eq for Entry {}

/// The component and resource types a query or a system reads and writes.
///
/// Built by [`Query::new`](crate::Query::new), which rejects a component type that is written
/// and also accessed elsewhere in the same query ([`EcsError::ConflictingAccess`]), and for a
/// system as the union of its parameters' accesses (DECISIONS #17). Components and resources are
/// kept apart: [`kind_of`](Self::kind_of) and [`iter`](Self::iter) describe components,
/// [`resource_kind_of`](Self::resource_kind_of) and [`resources`](Self::resources) resources. Two
/// accesses that do not [`conflict`](Self::conflict) may run at the same time; a schedule
/// declares them to the `pragma_core` frame graph.
///
/// ```
/// use pragma_ecs::{Query, With};
///
/// struct Position(f32);
/// struct Velocity(f32);
/// struct Player;
///
/// let movement = Query::<(&mut Position, &Velocity)>::new()?;
/// let physics = Query::<&mut Velocity>::new()?;
/// let players = Query::<&Position, With<Player>>::new()?;
///
/// assert_eq!(movement.access().len(), 2);
/// assert!(players.access().kind_of::<Player>().is_none()); // filters read no data
/// assert_eq!(movement.access().conflict(physics.access()), Some(std::any::type_name::<Velocity>()));
/// assert_eq!(physics.access().conflict(players.access()), None);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Access {
    /// Sorted by type id, one entry per type.
    components: Vec<Entry>,
    /// Sorted by type id, one entry per type.
    resources: Vec<Entry>,
}

impl fmt::Debug for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        map.entries(self.components.iter().map(|e| (e.name, e.kind)));
        if !self.resources.is_empty() {
            let resources = self.resources.iter().map(|e| (e.name, e.kind));
            map.entry(&"resources", &Resources(resources.collect()));
        }
        map.finish()
    }
}

/// Formats the resource part of an [`Access`].
struct Resources(Vec<(&'static str, AccessKind)>);

impl fmt::Debug for Resources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.0.iter().copied()).finish()
    }
}

impl Access {
    /// An empty access: nothing is used. Does not allocate.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            components: Vec::new(),
            resources: Vec::new(),
        }
    }

    /// Number of distinct component types used.
    #[must_use]
    pub fn len(&self) -> usize {
        self.components.len()
    }

    /// Whether no component type is used.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    /// How the component type with id `ty` is used; `None` if it is not.
    #[must_use]
    pub fn kind(&self, ty: TypeId) -> Option<AccessKind> {
        find(&self.components, ty).map(|e| e.kind)
    }

    /// How the component type `T` is used; `None` if it is not.
    #[must_use]
    pub fn kind_of<T: Component>(&self) -> Option<AccessKind> {
        self.kind(TypeId::of::<T>())
    }

    /// Every used component type with its access, in an unspecified but fixed order.
    pub fn iter(&self) -> impl Iterator<Item = (TypeId, AccessKind)> + '_ {
        self.components.iter().map(|e| (e.ty, e.kind))
    }

    /// How the resource type `T` is used; `None` if it is not.
    ///
    /// ```
    /// use pragma_ecs::{AccessKind, Res, ResMut, Schedule, View};
    ///
    /// struct Gravity(f32);
    /// struct Steps(u32);
    /// struct Velocity(f32);
    ///
    /// fn fall(_: View<&mut Velocity>, _: Res<Gravity>, _: ResMut<Steps>) {}
    ///
    /// let mut builder = Schedule::builder();
    /// builder.add_system("fall", fall)?;
    /// let schedule = builder.build()?;
    /// let (_, access) = schedule.systems().next().ok_or("no system")?;
    /// assert_eq!(access.resource_kind_of::<Gravity>(), Some(AccessKind::Read));
    /// assert_eq!(access.resource_kind_of::<Steps>(), Some(AccessKind::Write));
    /// assert_eq!(access.resource_kind_of::<Velocity>(), None); // a component here
    /// assert_eq!(access.resources().count(), 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn resource_kind_of<T: Resource>(&self) -> Option<AccessKind> {
        find(&self.resources, TypeId::of::<T>()).map(|e| e.kind)
    }

    /// Every used resource type with its access, in an unspecified but fixed order.
    pub fn resources(&self) -> impl Iterator<Item = (TypeId, AccessKind)> + '_ {
        self.resources.iter().map(|e| (e.ty, e.kind))
    }

    /// The name of the first component or resource type that one of the two accesses writes and
    /// the other uses; `None` if they may run at the same time.
    #[must_use]
    pub fn conflict(&self, other: &Self) -> Option<&'static str> {
        conflict(&self.components, &other.components)
            .or_else(|| conflict(&self.resources, &other.resources))
    }

    /// Records that the component type `T` is used with `kind`.
    ///
    /// Repeated reads merge; a write together with any other use of the same type is
    /// [`EcsError::ConflictingAccess`]. Grows through `try_reserve`.
    pub(crate) fn add_component<T: Component>(&mut self, kind: AccessKind) -> Result<(), EcsError> {
        let entry = Entry {
            ty: TypeId::of::<T>(),
            name: type_name::<T>(),
            kind,
            declare: declare::<Columns<T>>,
            lender: Some(Lender::of::<T>()),
        };
        match add(&mut self.components, entry)? {
            Some(component) => Err(EcsError::ConflictingAccess { component }),
            None => Ok(()),
        }
    }

    /// Records that the resource type `T` is used with `kind`; a write together with any other
    /// use is [`EcsError::ConflictingResourceAccess`].
    pub(crate) fn add_resource<T: Resource>(&mut self, kind: AccessKind) -> Result<(), EcsError> {
        let entry = Entry {
            ty: TypeId::of::<T>(),
            name: type_name::<T>(),
            kind,
            declare: declare::<T>,
            lender: None,
        };
        match add(&mut self.resources, entry)? {
            Some(resource) => Err(EcsError::ConflictingResourceAccess { resource }),
            None => Ok(()),
        }
    }

    /// Declares every access to a frame-graph task.
    pub(crate) fn configure(&self, task: TaskConfig) -> TaskConfig {
        self.components
            .iter()
            .chain(&self.resources)
            .fold(task, |task, e| (e.declare)(task, e.kind))
    }

    /// The lenders of the component columns used.
    pub(crate) fn lenders(&self) -> impl Iterator<Item = Lender> + '_ {
        self.components.iter().filter_map(|e| e.lender)
    }
}

fn find(entries: &[Entry], ty: TypeId) -> Option<&Entry> {
    let i = entries.binary_search_by(|e| e.ty.cmp(&ty)).ok()?;
    entries.get(i)
}

fn conflict(ours: &[Entry], theirs: &[Entry]) -> Option<&'static str> {
    ours.iter().find_map(|e| {
        let theirs = find(theirs, e.ty)?.kind;
        (e.kind == AccessKind::Write || theirs == AccessKind::Write).then_some(e.name)
    })
}

/// Adds `entry`; `Ok(Some(name))` if it conflicts with an entry of the same type.
fn add(entries: &mut Vec<Entry>, entry: Entry) -> Result<Option<&'static str>, EcsError> {
    match entries.binary_search_by(|e| e.ty.cmp(&entry.ty)) {
        Ok(i) => match entries.get(i) {
            Some(e) if e.kind == AccessKind::Read && entry.kind == AccessKind::Read => Ok(None),
            _ => Ok(Some(entry.name)),
        },
        Err(i) => {
            entries
                .try_reserve(1)
                .map_err(|_| EcsError::OutOfMemory { entities: 0 })?;
            entries.insert(i, entry);
            Ok(None)
        }
    }
}

/// Declares `kind` access to the graph resource `R`.
fn declare<R: Resource>(task: TaskConfig, kind: AccessKind) -> TaskConfig {
    match kind {
        AccessKind::Read => task.reads::<R>(),
        AccessKind::Write => task.writes::<R>(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_merge_and_writes_conflict() {
        let mut access = Access::new();
        access.add_component::<u8>(AccessKind::Read).unwrap();
        access.add_component::<u8>(AccessKind::Read).unwrap();
        access.add_component::<u16>(AccessKind::Write).unwrap();
        assert_eq!(access.len(), 2);
        assert_eq!(
            access.add_component::<u8>(AccessKind::Write),
            Err(EcsError::ConflictingAccess { component: "u8" })
        );
        assert_eq!(
            access.add_component::<u16>(AccessKind::Read),
            Err(EcsError::ConflictingAccess { component: "u16" })
        );
        assert_eq!(access.kind_of::<u8>(), Some(AccessKind::Read));
        assert_eq!(access.kind_of::<u32>(), None);
        assert_eq!(
            format!("{access:?}").len(),
            "{\"u8\": Read, \"u16\": Write}".len()
        );
    }

    #[test]
    fn components_and_resources_are_apart() {
        let mut access = Access::new();
        access.add_component::<u8>(AccessKind::Write).unwrap();
        access.add_resource::<u8>(AccessKind::Read).unwrap();
        access.add_resource::<u8>(AccessKind::Read).unwrap();
        assert_eq!(access.kind_of::<u8>(), Some(AccessKind::Write));
        assert_eq!(access.resource_kind_of::<u8>(), Some(AccessKind::Read));
        assert_eq!(
            access.add_resource::<u8>(AccessKind::Write),
            Err(EcsError::ConflictingResourceAccess { resource: "u8" })
        );
        assert_eq!(access.lenders().count(), 1);
        assert!(format!("{access:?}").contains("\"resources\": {\"u8\": Read}"));

        let mut writer = Access::new();
        writer.add_resource::<u8>(AccessKind::Write).unwrap();
        assert_eq!(access.conflict(&writer), Some("u8"));
        let mut reader = Access::new();
        reader.add_component::<u16>(AccessKind::Read).unwrap();
        assert_eq!(access.conflict(&reader), None);
    }

    #[test]
    fn conflict_needs_a_write_on_either_side() {
        let mut reads = Access::new();
        reads.add_component::<u8>(AccessKind::Read).unwrap();
        let mut other_reads = Access::new();
        other_reads.add_component::<u8>(AccessKind::Read).unwrap();
        let mut writes = Access::new();
        writes.add_component::<u8>(AccessKind::Write).unwrap();

        assert_eq!(reads.conflict(&other_reads), None);
        assert_eq!(reads.conflict(&writes), Some("u8"));
        assert_eq!(writes.conflict(&reads), Some("u8"));
        assert_eq!(writes.conflict(&Access::new()), None);
    }
}
