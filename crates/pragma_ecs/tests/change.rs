//! Change detection (DECISIONS #27, #28).
//!
//! The property test applies random world operations (spawning, inserting, replacing, removing,
//! `get_mut`, writing through `Mut` in a query, moving rows between tables, despawning) and
//! asks long-lived queries with `Added` / `Changed` filters at random points. A naive oracle
//! keeps, per entity, the sequence numbers of the operations that added and last changed its
//! component, and per query the sequence number of its previous call: every answer must match.
//! A second test runs writers and readers through a schedule in both orders on 1 and 4 workers.
// Test code: a panic is a test failure.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

use std::collections::HashMap;

use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
use pragma_ecs::{
    Added, Changed, EcsError, Entity, Query, ResMut, Schedule, View, With, Without, World,
};
use proptest::prelude::*;
use proptest::sample::Index;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct A(u32);

/// A component that only moves rows between tables.
#[derive(Clone, Copy, Debug)]
struct Mover;

#[derive(Clone, Debug)]
enum Op {
    Spawn,
    Despawn(Index),
    Insert(Index, u32),
    Remove(Index),
    GetMut(Index),
    /// Writes `A` through `Mut` where the value is odd; reads the others.
    WriteOdd,
    /// Adds or removes `Mover`, moving the row to another table.
    Move(Index),
    /// Asks observer `.0` (see `Observers`).
    Observe(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::Spawn),
        1 => any::<Index>().prop_map(Op::Despawn),
        4 => (any::<Index>(), any::<u32>()).prop_map(|(i, v)| Op::Insert(i, v)),
        1 => any::<Index>().prop_map(Op::Remove),
        2 => any::<Index>().prop_map(Op::GetMut),
        1 => Just(Op::WriteOdd),
        2 => any::<Index>().prop_map(Op::Move),
        4 => (0..OBSERVERS).prop_map(Op::Observe),
    ]
}

/// The oracle's record of one `A`: its value and the sequence numbers of its addition and
/// last change.
#[derive(Clone, Copy, Debug)]
struct Record {
    value: u32,
    added: u64,
    changed: u64,
}

const OBSERVERS: usize = 4;

/// The queries under test, each with the oracle's tick of its previous call.
struct Observers {
    changed: Query<Entity, Changed<A>>,
    added: Query<Entity, Added<A>>,
    /// `Changed<A>` with the data written: writes every row it sees.
    rewrite: Query<(Entity, &'static mut A), Changed<A>>,
    /// A row filter together with table filters.
    moved: Query<Entity, (Changed<A>, With<Mover>)>,
    last: [u64; OBSERVERS],
}

struct Harness {
    world: World,
    known: Vec<Entity>,
    alive: HashMap<Entity, Option<Record>>,
    movers: HashMap<Entity, bool>,
    seq: u64,
    observers: Observers,
    writer: Query<&'static mut A>,
}

fn sorted(mut v: Vec<Entity>) -> Vec<Entity> {
    v.sort();
    v
}

impl Harness {
    fn pick(&self, i: &Index) -> Option<Entity> {
        (!self.known.is_empty()).then(|| *i.get(&self.known))
    }

    fn apply(&mut self, op: Op) {
        self.seq += 1;
        let seq = self.seq;
        match op {
            Op::Spawn => {
                let e = self.world.spawn().unwrap();
                self.known.push(e);
                self.alive.insert(e, None);
                self.movers.insert(e, false);
            }
            Op::Despawn(i) => {
                let Some(e) = self.pick(&i) else { return };
                let alive = self.alive.remove(&e).is_some();
                assert_eq!(self.world.despawn(e).is_ok(), alive);
            }
            Op::Insert(i, v) => {
                let Some(e) = self.pick(&i) else { return };
                let result = self.world.insert(e, A(v));
                match self.alive.get_mut(&e) {
                    Some(record) => {
                        let replaced = result.unwrap();
                        assert_eq!(replaced.map(|a| a.0), record.map(|r| r.value));
                        *record = Some(match *record {
                            Some(r) => Record {
                                value: v,
                                changed: seq,
                                ..r
                            },
                            None => Record {
                                value: v,
                                added: seq,
                                changed: seq,
                            },
                        });
                    }
                    None => assert!(matches!(result, Err(EcsError::NoSuchEntity { .. }))),
                }
            }
            Op::Remove(i) => {
                let Some(e) = self.pick(&i) else { return };
                let removed = self.world.remove::<A>(e);
                match self.alive.get_mut(&e) {
                    Some(record) => {
                        assert_eq!(removed.unwrap().map(|a| a.0), record.map(|r| r.value));
                        *record = None;
                    }
                    None => assert!(removed.is_err()),
                }
            }
            Op::GetMut(i) => {
                let Some(e) = self.pick(&i) else { return };
                let got = self.world.get_mut::<A>(e).is_some();
                let record = self.alive.get_mut(&e).and_then(Option::as_mut);
                assert_eq!(got, record.is_some());
                if let Some(record) = record {
                    record.changed = seq;
                }
            }
            Op::WriteOdd => {
                for mut a in self.writer.iter_mut(&mut self.world).unwrap() {
                    if a.0 % 2 == 1 {
                        a.0 = a.0.wrapping_add(2);
                    }
                }
                for record in self.alive.values_mut().flatten() {
                    if record.value % 2 == 1 {
                        record.value = record.value.wrapping_add(2);
                        record.changed = seq;
                    }
                }
            }
            Op::Move(i) => {
                let Some(e) = self.pick(&i) else { return };
                let Some(mover) = self
                    .movers
                    .get_mut(&e)
                    .filter(|_| self.alive.contains_key(&e))
                else {
                    return;
                };
                if *mover {
                    self.world.remove::<Mover>(e).unwrap();
                } else {
                    self.world.insert(e, Mover).unwrap();
                }
                *mover = !*mover;
            }
            Op::Observe(k) => self.observe(k, seq),
        }
    }

    /// The live entities with an `A` whose record passes `f`, sorted.
    fn expected(&self, f: impl Fn(&Entity, &Record) -> bool) -> Vec<Entity> {
        sorted(
            self.alive
                .iter()
                .filter_map(|(e, r)| Some((e, (*r)?)))
                .filter(|(e, r)| f(e, r))
                .map(|(e, _)| *e)
                .collect(),
        )
    }

    fn observe(&mut self, k: usize, seq: u64) {
        let last = self.observers.last[k];
        let expected = match k {
            1 => self.expected(|_, r| r.added > last),
            3 => self.expected(|e, r| r.changed > last && self.movers[e]),
            _ => self.expected(|_, r| r.changed > last),
        };
        let o = &mut self.observers;
        let actual: Vec<Entity> = match k {
            0 => o.changed.iter(&self.world).unwrap().collect(),
            1 => o.added.iter(&self.world).unwrap().collect(),
            2 => {
                let mut seen = Vec::new();
                for (e, mut a) in o.rewrite.iter_mut(&mut self.world).unwrap() {
                    a.0 = a.0.wrapping_mul(3);
                    seen.push(e);
                }
                for e in &expected {
                    if let Some(Some(record)) = self.alive.get_mut(e) {
                        record.value = record.value.wrapping_mul(3);
                        record.changed = seq;
                    }
                }
                seen
            }
            _ => o.moved.iter(&self.world).unwrap().collect(),
        };
        assert_eq!(sorted(actual), expected, "observer {k}");
        self.observers.last[k] = seq;
    }

    fn check_values(&self) {
        for (e, record) in &self.alive {
            assert_eq!(
                self.world.get::<A>(*e).map(|a| a.0),
                record.map(|r| r.value)
            );
        }
        assert_eq!(self.world.len(), self.alive.len());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Invariant: every `Added` / `Changed` query yields exactly the entities whose component
    /// the oracle saw added / changed since that query's previous call.
    #[test]
    fn change_filters_match_a_per_entity_oracle(ops in proptest::collection::vec(op(), 0..200)) {
        let mut h = Harness {
            world: World::new(),
            known: Vec::new(),
            alive: HashMap::new(),
            movers: HashMap::new(),
            seq: 0,
            observers: Observers {
                changed: Query::new().unwrap(),
                added: Query::new().unwrap(),
                rewrite: Query::new().unwrap(),
                moved: Query::new().unwrap(),
                last: [0; OBSERVERS],
            },
            writer: Query::new().unwrap(),
        };
        for op in ops {
            h.apply(op);
            h.check_values();
        }
        for k in 0..OBSERVERS {
            h.seq += 1;
            let seq = h.seq;
            h.observe(k, seq);
        }
    }
}

// Through a schedule.

#[derive(Default)]
struct Seen(Vec<Vec<u32>>);

#[derive(Default)]
struct Frame(u32);

/// Writes `A` of the entities whose value equals the frame number modulo 4.
fn write_some(mut values: View<&mut A>, frame: ResMut<Frame>) -> Result<(), EcsError> {
    let target = frame.0 % 4;
    values.par_for_each_mut(Grain::Fixed(3), |mut a| {
        if a.0 % 4 == target {
            a.0 += 4;
        }
    })
}

fn read_changed(
    mut changed: View<&A, (Changed<A>, Without<Mover>)>,
    mut seen: ResMut<Seen>,
) -> Result<(), EcsError> {
    let mut values: Vec<u32> = changed.iter()?.map(|a| a.0).collect();
    values.sort_unstable();
    seen.0.push(values);
    Ok(())
}

fn next_frame(mut frame: ResMut<Frame>) {
    frame.0 += 1;
}

fn run(reader_first: bool, workers: usize) -> Vec<Vec<u32>> {
    let pool = ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap();
    let mut world = World::new();
    for i in 0..40 {
        let e = world.spawn().unwrap();
        world.insert(e, A(i)).unwrap();
    }
    world.insert_resource(Seen::default());
    world.insert_resource(Frame::default());
    let mut builder = Schedule::builder();
    builder.add_system("write", write_some).unwrap();
    if reader_first {
        builder
            .add_system_with("read", read_changed, |c| c.before("write"))
            .unwrap();
    } else {
        builder
            .add_system_with("read", read_changed, |c| c.after("write"))
            .unwrap();
    }
    builder
        .add_system_with("next", next_frame, |c| c.after("write"))
        .unwrap();
    let mut schedule = builder.build().unwrap();
    for _ in 0..5 {
        schedule.run(&mut world, &pool).unwrap();
    }
    world.remove_resource::<Seen>().unwrap().0
}

#[test]
fn a_system_sees_every_change_exactly_once_in_either_order() {
    let all: Vec<u32> = (0..40).collect();
    // After the writer: everything on the first run, then the frame's own writes.
    let after = run(false, 1);
    assert_eq!(after[0].len(), all.len());
    assert_eq!(
        after[1],
        (1..40).step_by(4).map(|v| v + 4).collect::<Vec<_>>()
    );
    // Before the writer: everything on the first run, then the previous frame's writes.
    let before = run(true, 1);
    assert_eq!(before[0], all);
    assert_eq!(
        before[1],
        (0..40).step_by(4).map(|v| v + 4).collect::<Vec<_>>()
    );
    assert_eq!(before[2], after[1]);
    // Every write is seen once in both orders.
    let count = |frames: &[Vec<u32>]| frames.iter().skip(1).map(Vec::len).sum::<usize>();
    assert_eq!(count(&after[..4]), count(&before[1..5]));
    assert_eq!(run(false, 4), after);
    assert_eq!(run(true, 4), before);
}
