//! Events against a naive model (DECISIONS #24, #26, #40).
//!
//! Writer systems send their own lists of events every frame, reader systems read on the frames
//! their pattern says (skipping others), and between frames the world sends events
//! (`World::send_event`, also before the resource exists), updates `Events<T>` by hand and reads
//! through an outside `EventCursor`. The model keeps every event ever sent with the number of
//! updates before it was sent: an event is kept until two more updates, a cursor yields the kept
//! events after it. Every read, every reader's `len()` and the kept events after every step must
//! match, on 1 and 4 workers. Used by `tests/events.rs` and the `events` fuzz target.

use std::sync::{Arc, Mutex};

use pragma_ecs::{EcsError, EventCursor, EventReader, EventWriter, Events, Schedule, World};

use super::bytes::Bytes;
use super::commands::pools;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ev(pub u32);

/// A step between frames, or a frame.
#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// One run of the schedule.
    Frame,
    /// `World::send_event`.
    Send(u32),
    /// `Events::update` on the world's resource (nothing without one).
    Update,
    /// A read through an outside `EventCursor` (nothing without the resource).
    Read,
}

/// One case: the systems and the steps.
#[derive(Clone, Debug)]
pub struct Case {
    /// Per writer system: the events it sends on its k-th run, cycled; at least one list each.
    pub writers: Vec<Vec<Vec<u32>>>,
    /// Per reader system: whether it reads on its k-th run, cycled; at least one entry each.
    pub readers: Vec<Vec<bool>>,
    pub steps: Vec<Step>,
}

/// Decodes fuzzer input into a case, within the bounds of the property test's strategy.
pub fn decode(data: &[u8]) -> Case {
    let mut bytes = Bytes::new(data);
    let mut writers = Vec::new();
    for _ in 0..bytes.below(4) {
        let mut lists = Vec::new();
        for _ in 0..1 + bytes.below(3) {
            lists.push((0..bytes.below(4)).map(|_| bytes.u32()).collect());
        }
        writers.push(lists);
    }
    let mut readers = Vec::new();
    for _ in 0..bytes.below(4) {
        readers.push((0..1 + bytes.below(4)).map(|_| bytes.bool()).collect());
    }
    let mut steps = Vec::new();
    while !bytes.is_empty() && steps.len() < 64 {
        steps.push(match bytes.below(6) {
            0..=2 => Step::Frame,
            3 => Step::Send(bytes.u32()),
            4 => Step::Update,
            _ => Step::Read,
        });
    }
    Case {
        writers,
        readers,
        steps,
    }
}

/// What a reader saw: (frame, reader, `len()` before reading, the events read).
type Seen = (usize, usize, usize, Vec<u32>);

/// The two-buffer rule written out naively.
#[derive(Debug, Default)]
struct Model {
    /// Whether the world has an `Events<Ev>`.
    exists: bool,
    /// Every event ever sent (its id is its index) and the number of updates before it was sent.
    all: Vec<(u32, u64)>,
    updates: u64,
}

impl Model {
    fn send(&mut self, value: u32) {
        self.exists = true;
        self.all.push((value, self.updates));
    }

    fn update(&mut self) {
        if self.exists {
            self.updates += 1;
        }
    }

    /// An event lives through the update after it was sent and is dropped by the next one.
    fn kept(&self, sent_after: u64) -> bool {
        sent_after + 2 > self.updates
    }

    fn kept_from(&self, cursor: usize) -> Vec<u32> {
        self.all[cursor.min(self.all.len())..]
            .iter()
            .filter(|(_, sent_after)| self.kept(*sent_after))
            .map(|(value, _)| *value)
            .collect()
    }

    /// Reads from `cursor` (an event id) and moves it past every event sent.
    fn read(&self, cursor: &mut usize) -> Vec<u32> {
        let read = self.kept_from(*cursor);
        *cursor = self.all.len();
        read
    }
}

/// The schedule of the case's systems, logging what the readers see into `log`.
fn schedule(case: &Case, log: &Arc<Mutex<Vec<Seen>>>) -> Schedule {
    let mut builder = Schedule::builder();
    for (i, lists) in case.writers.iter().enumerate() {
        let lists = lists.clone();
        let mut run = 0;
        builder
            .add_system(
                format!("w{i}"),
                move |mut writer: EventWriter<'_, Ev>| -> Result<(), EcsError> {
                    let list = &lists[run % lists.len()];
                    run += 1;
                    for &value in list {
                        writer.send(Ev(value))?;
                    }
                    Ok(())
                },
            )
            .unwrap();
    }
    for (i, pattern) in case.readers.iter().enumerate() {
        let (pattern, log) = (pattern.clone(), Arc::clone(log));
        let mut run = 0;
        builder
            .add_system(
                format!("r{i}"),
                move |mut reader: EventReader<'_, '_, Ev>| {
                    let frame = run;
                    run += 1;
                    if pattern[frame % pattern.len()] {
                        let unread = reader.len();
                        let read = reader.read().map(|event| event.0).collect();
                        log.lock().unwrap().push((frame, i, unread, read));
                    }
                },
            )
            .unwrap();
    }
    builder.build().unwrap()
}

/// The events the world keeps, or `None` without the resource.
fn kept(world: &World) -> Option<Vec<u32>> {
    let events = world.resource::<Events<Ev>>()?;
    let kept: Vec<u32> = events.iter().map(|event| event.0).collect();
    assert_eq!(events.len(), kept.len());
    Some(kept)
}

/// Invariant: every reader, inside or outside systems, reads exactly the kept events it has not
/// read yet, in sending order (system writers in registration order at the sync point), and the
/// world keeps exactly the events of the last two updates, on 1 and 4 workers.
pub fn check(case: &Case) {
    let uses_events = !case.writers.is_empty() || !case.readers.is_empty();
    let mut model = Model::default();
    let mut expected_seen = Vec::new();
    let mut expected_outside = Vec::new();
    let mut readers = vec![0; case.readers.len()];
    let mut outside = 0;
    let mut expected_kept = Vec::new();
    let mut frame = 0;
    for step in &case.steps {
        match *step {
            Step::Frame => {
                if uses_events {
                    model.exists = true;
                    for (i, pattern) in case.readers.iter().enumerate() {
                        if pattern[frame % pattern.len()] {
                            let read = model.read(&mut readers[i]);
                            expected_seen.push((frame, i, read.len(), read));
                        }
                    }
                    model.update();
                    for lists in &case.writers {
                        for &value in &lists[frame % lists.len()] {
                            model.send(value);
                        }
                    }
                }
                frame += 1;
            }
            Step::Send(value) => model.send(value),
            Step::Update => model.update(),
            Step::Read => expected_outside.push(
                model
                    .exists
                    .then(|| (model.kept_from(outside).len(), model.read(&mut outside))),
            ),
        }
        expected_kept.push(model.exists.then(|| model.kept_from(0)));
    }

    for pool in pools() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut schedule = schedule(case, &log);
        let mut world = World::new();
        let mut cursor = EventCursor::<Ev>::default();
        let mut seen_outside = Vec::new();
        let mut actual_kept = Vec::new();
        for step in &case.steps {
            match *step {
                Step::Frame => schedule.run(&mut world, pool).unwrap(),
                Step::Send(value) => world.send_event(Ev(value)).unwrap(),
                Step::Update => {
                    if let Some(events) = world.resource_mut::<Events<Ev>>() {
                        events.update();
                    }
                }
                Step::Read => seen_outside.push(world.resource::<Events<Ev>>().map(|events| {
                    let unread = cursor.unread(&events);
                    (unread, cursor.read(&events).map(|event| event.0).collect())
                })),
            }
            actual_kept.push(kept(&world));
        }
        let mut seen = log.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, expected_seen, "readers in systems");
        assert_eq!(seen_outside, expected_outside, "the outside cursor");
        assert_eq!(
            actual_kept, expected_kept,
            "the kept events after each step"
        );
    }
}
