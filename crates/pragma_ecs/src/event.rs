//! Events: double-buffered messages between systems, published at the sync point (DECISIONS
//! #24, #26).

use std::any::TypeId;
use std::fmt;
use std::iter::Chain;
use std::marker::PhantomData;
use std::mem;
use std::slice;

use pragma_core::graph;

use crate::access::{Access, AccessKind};
use crate::system::{Env, Sealed, SystemParam};
use crate::{EcsError, World};

/// A message sent from one system to others: [`EventWriter<T>`] sends, [`EventReader<T>`]
/// reads, [`Events<T>`] stores.
///
/// Implemented for every `Send + Sync + 'static` type, like [`Component`](crate::Component).
///
/// ```
/// use pragma_ecs::{Event, Events};
///
/// struct Collision { a: u32, b: u32 }
///
/// fn assert_event<T: Event>() {}
/// assert_event::<Collision>();
/// let events = Events::<Collision>::new();
/// assert!(events.is_empty());
/// ```
pub trait Event: Send + Sync + 'static {}

impl<T: Send + Sync + 'static> Event for T {}

/// The events of type `T`: a resource of the [`World`] with two buffers, the events of the last
/// two updates (DECISIONS #26).
///
/// A [`Schedule`](crate::Schedule) updates it at its sync point, at the end of every
/// [`run`](crate::Schedule::run): the older buffer is dropped, then the events the systems' writers
/// sent during the frame are appended in system registration order. Every event has a running
/// id, so a reader that keeps an [`EventCursor`] sees every event once, as long as it reads at
/// least once every two updates. The schedule creates the resource for every event type its
/// systems use; [`World::send_event`] creates it on the first event.
///
/// ```
/// use pragma_ecs::{EventCursor, Events};
///
/// let mut events = Events::<&str>::new();
/// let mut cursor = EventCursor::default();
/// events.send("hello")?;
/// assert_eq!(cursor.read(&events).copied().collect::<Vec<_>>(), ["hello"]);
///
/// events.update(); // "hello" is still kept for one more update
/// events.send("world")?;
/// assert_eq!(events.len(), 2);
/// assert_eq!(cursor.read(&events).copied().collect::<Vec<_>>(), ["world"]);
///
/// events.update();
/// events.update(); // both are gone
/// assert!(events.is_empty());
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct Events<T> {
    /// The events of the previous update; the first has id `start`.
    previous: Vec<T>,
    /// The events since the last update; ids follow `previous`.
    current: Vec<T>,
    start: u64,
}

impl<T> Default for Events<T> {
    fn default() -> Self {
        Self {
            previous: Vec::new(),
            current: Vec::new(),
            start: 0,
        }
    }
}

impl<T> fmt::Debug for Events<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Events")
            .field("previous", &self.previous.len())
            .field("current", &self.current.len())
            .field("next_id", &self.end())
            .finish()
    }
}

impl<T> Events<T> {
    /// No events. Does not allocate.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            previous: Vec::new(),
            current: Vec::new(),
            start: 0,
        }
    }

    /// Appends `event` to the current buffer.
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the buffer cannot grow; the event is dropped.
    pub fn send(&mut self, event: T) -> Result<(), EcsError> {
        self.current
            .try_reserve(1)
            .map_err(|_| EcsError::OutOfMemory {
                entities: self.len().saturating_add(1),
            })?;
        self.current.push(event);
        Ok(())
    }

    /// Drops the events of the previous update and keeps the current ones as the previous. Does
    /// not allocate: the two buffers trade places.
    pub fn update(&mut self) {
        let dropped = u64::try_from(self.previous.len()).unwrap_or(u64::MAX);
        self.start = self.start.saturating_add(dropped);
        self.previous.clear();
        mem::swap(&mut self.previous, &mut self.current);
    }

    /// Number of events kept (both buffers).
    #[must_use]
    pub fn len(&self) -> usize {
        self.previous.len().saturating_add(self.current.len())
    }

    /// Whether no event is kept.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.previous.is_empty() && self.current.is_empty()
    }

    /// Every event kept, oldest first.
    ///
    /// ```
    /// use pragma_ecs::Events;
    ///
    /// let mut events = Events::new();
    /// events.send(1)?;
    /// events.update();
    /// events.send(2)?;
    /// assert_eq!(events.iter().copied().collect::<Vec<_>>(), [1, 2]);
    /// # Ok::<(), pragma_ecs::EcsError>(())
    /// ```
    pub fn iter(&self) -> EventIter<'_, T> {
        EventIter {
            inner: self.previous.iter().chain(self.current.iter()),
        }
    }

    /// The id the next event will get.
    fn end(&self) -> u64 {
        let kept = u64::try_from(self.len()).unwrap_or(u64::MAX);
        self.start.saturating_add(kept)
    }

    /// The events with an id from `from` on, oldest first.
    fn since(&self, from: u64) -> EventIter<'_, T> {
        let skip = usize::try_from(from.saturating_sub(self.start)).unwrap_or(usize::MAX);
        let previous = self.previous.get(skip..).unwrap_or_default();
        let current = self
            .current
            .get(skip.saturating_sub(self.previous.len())..)
            .unwrap_or_default();
        EventIter {
            inner: previous.iter().chain(current.iter()),
        }
    }

    /// Appends every event of `sent`, which is left empty (its capacity kept).
    fn publish(&mut self, sent: &mut Vec<T>) -> Result<(), EcsError> {
        self.current
            .try_reserve(sent.len())
            .map_err(|_| EcsError::OutOfMemory {
                entities: self.len().saturating_add(sent.len()),
            })?;
        self.current.append(sent);
        Ok(())
    }
}

/// The iterator of [`Events::iter`] and [`EventCursor::read`].
///
/// ```
/// use pragma_ecs::{EventIter, Events};
///
/// let mut events = Events::new();
/// events.send('a')?;
/// let all: EventIter<'_, char> = events.iter();
/// assert_eq!(all.count(), 1);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct EventIter<'a, T> {
    inner: Chain<slice::Iter<'a, T>, slice::Iter<'a, T>>,
}

impl<T> fmt::Debug for EventIter<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventIter").finish_non_exhaustive()
    }
}

impl<'a, T> Iterator for EventIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// A reading position in an [`Events<T>`]: the id of the next event to read. An
/// [`EventReader`] keeps one per system; outside systems, keep your own.
///
/// ```
/// use pragma_ecs::{EventCursor, Events};
///
/// let mut events = Events::new();
/// let mut cursor = EventCursor::default();
/// events.send(1)?;
/// events.send(2)?;
/// assert_eq!(cursor.unread(&events), 2);
/// assert_eq!(cursor.read(&events).sum::<i32>(), 3);
/// assert_eq!(cursor.read(&events).count(), 0);
/// # Ok::<(), pragma_ecs::EcsError>(())
/// ```
pub struct EventCursor<T> {
    next: u64,
    marker: PhantomData<fn() -> T>,
}

impl<T> Default for EventCursor<T> {
    fn default() -> Self {
        Self {
            next: 0,
            marker: PhantomData,
        }
    }
}

impl<T> fmt::Debug for EventCursor<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventCursor")
            .field("next", &self.next)
            .finish()
    }
}

impl<T> EventCursor<T> {
    /// The events not read yet through this cursor, oldest first; the cursor moves past all of
    /// them. Events dropped before they were read are skipped.
    pub fn read<'a>(&mut self, events: &'a Events<T>) -> EventIter<'a, T> {
        let unread = events.since(self.next);
        self.next = events.end();
        unread
    }

    /// Number of events not read yet through this cursor.
    #[must_use]
    pub fn unread(&self, events: &Events<T>) -> usize {
        events.since(self.next).len_hint()
    }
}

impl<T> EventIter<'_, T> {
    fn len_hint(&self) -> usize {
        self.inner.size_hint().0
    }
}

/// How the schedule creates and updates the [`Events<T>`] of one event type (DECISIONS #26).
#[derive(Clone, Copy)]
pub(crate) struct EventType {
    pub(crate) ty: TypeId,
    ensure: fn(&mut World),
    update: fn(&mut World),
}

impl PartialEq for EventType {
    fn eq(&self, other: &Self) -> bool {
        self.ty == other.ty
    }
}

impl Eq for EventType {}

impl EventType {
    pub(crate) fn of<T: Event>() -> Self {
        Self {
            ty: TypeId::of::<T>(),
            ensure: ensure::<T>,
            update: update::<T>,
        }
    }

    /// Inserts an empty `Events<T>` if the world has none.
    pub(crate) fn ensure(&self, world: &mut World) {
        (self.ensure)(world);
    }

    /// Updates the world's `Events<T>`.
    pub(crate) fn update(&self, world: &mut World) {
        (self.update)(world);
    }
}

fn ensure<T: Event>(world: &mut World) {
    if !world.contains_resource::<Events<T>>() {
        world.insert_resource(Events::<T>::new());
    }
}

fn update<T: Event>(world: &mut World) {
    if let Some(events) = world.resource_mut::<Events<T>>() {
        events.update();
    }
}

/// A system parameter that sends events of type `T`. The events are kept in the system's own
/// buffer and published at the sync point, at the end of the frame, in system registration order
/// (DECISIONS #26): a writer declares no access, so it never orders systems, and readers see the
/// events from the next frame on.
///
/// ```
/// use pragma_core::prelude::{PoolConfig, ThreadPool};
/// use pragma_ecs::{EcsError, EventReader, EventWriter, ResMut, Schedule, View, World};
///
/// struct Health(i32);
/// struct Died(u32);
/// struct Graveyard(Vec<u32>);
///
/// fn check(mut healths: View<&Health>, mut died: EventWriter<Died>) -> Result<(), EcsError> {
///     for (i, health) in (0..).zip(healths.iter()?) {
///         if health.0 <= 0 {
///             died.send(Died(i))?;
///         }
///     }
///     Ok(())
/// }
///
/// fn bury(mut died: EventReader<Died>, mut graveyard: ResMut<Graveyard>) {
///     graveyard.0.extend(died.read().map(|d| d.0));
/// }
///
/// let mut world = World::new();
/// for hp in [10, 0, -5] {
///     let entity = world.spawn()?;
///     world.insert(entity, Health(hp))?;
/// }
/// world.insert_resource(Graveyard(Vec::new()));
///
/// let mut builder = Schedule::builder();
/// builder.add_system("check", check)?.add_system("bury", bury)?; // no order needed
/// let mut schedule = builder.build()?;
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
///
/// schedule.run(&mut world, &pool)?; // the events are published at the end of the frame
/// assert!(world.resource::<Graveyard>().is_some_and(|g| g.0.is_empty()));
/// schedule.run(&mut world, &pool)?;
/// assert_eq!(world.resource::<Graveyard>().map(|g| g.0.len()), Some(2));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct EventWriter<'s, T: Event> {
    sent: &'s mut Vec<T>,
}

impl<T: Event> fmt::Debug for EventWriter<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventWriter")
            .field("sent", &self.sent.len())
            .finish()
    }
}

impl<T: Event> EventWriter<'_, T> {
    /// Sends `event`; it is published at the end of the frame.
    ///
    /// # Errors
    ///
    /// [`EcsError::OutOfMemory`] if the system's buffer cannot grow; the event is dropped.
    pub fn send(&mut self, event: T) -> Result<(), EcsError> {
        self.sent
            .try_reserve(1)
            .map_err(|_| EcsError::OutOfMemory {
                entities: self.sent.len().saturating_add(1),
            })?;
        self.sent.push(event);
        Ok(())
    }

    /// Number of events sent by this system in this frame.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sent.len()
    }

    /// Whether this system sent no event in this frame.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sent.is_empty()
    }
}

impl<T: Event> Sealed for EventWriter<'_, T> {}

impl<T: Event> SystemParam for EventWriter<'_, T> {
    type State = Vec<T>;
    type Item<'w, 's> = EventWriter<'s, T>;

    fn init() -> Result<Self::State, EcsError> {
        Ok(Vec::new())
    }

    fn declare(access: &mut Access) -> Result<(), EcsError> {
        access.add_event::<T>()
    }

    fn fetch<'w, 's>(
        state: &'s mut Self::State,
        _: Env<'w>,
    ) -> Result<Self::Item<'w, 's>, EcsError> {
        Ok(EventWriter { sent: state })
    }

    fn apply(state: &mut Self::State, world: &mut World) -> Result<(), EcsError> {
        ensure::<T>(world);
        let result = match world.resource_mut::<Events<T>>() {
            Some(events) => events.publish(state),
            None => Ok(()),
        };
        state.clear();
        result
    }

    fn discard(state: &mut Self::State) {
        state.clear();
    }
}

/// The per-system state of an [`EventReader`].
#[doc(hidden)]
pub struct ReaderState<T> {
    cursor: EventCursor<T>,
    /// The world the cursor reads.
    world: Option<u64>,
}

impl<T> fmt::Debug for ReaderState<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReaderState")
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

/// A system parameter that reads the events of type `T` the system has not read yet, from the
/// [`Events<T>`] resource (a resource read: it conflicts only with a system that takes
/// `ResMut<Events<T>>`). The reading position is kept per system and reset for another world.
/// See the [`EventWriter`] example.
///
/// ```
/// use pragma_ecs::{EventReader, ResMut};
///
/// struct Scored(u32);
/// struct Total(u32);
///
/// fn tally(mut scored: EventReader<Scored>, mut total: ResMut<Total>) {
///     total.0 += scored.read().map(|s| s.0).sum::<u32>();
/// }
/// # let _ = tally;
/// ```
pub struct EventReader<'w, 's, T: Event> {
    events: graph::Res<'w, Events<T>>,
    cursor: &'s mut EventCursor<T>,
}

impl<T: Event> fmt::Debug for EventReader<'_, '_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventReader")
            .field("unread", &self.len())
            .finish()
    }
}

impl<T: Event> EventReader<'_, '_, T> {
    /// The events this system has not read yet, oldest first; they count as read.
    pub fn read(&mut self) -> EventIter<'_, T> {
        self.cursor.read(&self.events)
    }

    /// Number of events this system has not read yet.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cursor.unread(&self.events)
    }

    /// Whether this system has read every event.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T: Event> Sealed for EventReader<'_, '_, T> {}

impl<T: Event> SystemParam for EventReader<'_, '_, T> {
    type State = ReaderState<T>;
    type Item<'w, 's> = EventReader<'w, 's, T>;

    fn init() -> Result<Self::State, EcsError> {
        Ok(ReaderState {
            cursor: EventCursor::default(),
            world: None,
        })
    }

    fn declare(access: &mut Access) -> Result<(), EcsError> {
        access.add_resource::<Events<T>>(AccessKind::Read)?;
        access.add_event::<T>()
    }

    fn fetch<'w, 's>(
        state: &'s mut Self::State,
        env: Env<'w>,
    ) -> Result<Self::Item<'w, 's>, EcsError> {
        let world = env.world().id();
        if state.world != Some(world) {
            state.world = Some(world);
            state.cursor = EventCursor::default();
        }
        Ok(EventReader {
            events: env.ctx().read::<Events<T>>().map_err(EcsError::graph)?,
            cursor: &mut state.cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_skips_dropped_events_and_never_rereads() {
        let mut events = Events::new();
        let mut early = EventCursor::default();
        let mut late = EventCursor::default();
        for i in 0..3 {
            events.send(i).unwrap();
        }
        assert_eq!(early.read(&events).copied().collect::<Vec<_>>(), [0, 1, 2]);
        events.update();
        events.send(3).unwrap();
        events.update();
        events.send(4).unwrap();
        // 0..3 were dropped by the second update.
        assert_eq!(late.unread(&events), 2);
        assert_eq!(late.read(&events).copied().collect::<Vec<_>>(), [3, 4]);
        assert_eq!(early.read(&events).copied().collect::<Vec<_>>(), [3, 4]);
        assert_eq!(early.unread(&events), 0);

        let mut sent = vec![5, 6];
        events.publish(&mut sent).unwrap();
        assert!(sent.is_empty());
        assert_eq!(late.read(&events).copied().collect::<Vec<_>>(), [5, 6]);
        assert!(format!("{events:?}").contains("next_id: 7"));
        assert!(format!("{late:?}").contains("next: 7"));
        assert!(EventType::of::<u8>() == EventType::of::<u8>());
        assert!(EventType::of::<u8>() != EventType::of::<u16>());
    }
}
