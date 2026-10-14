//! Change ticks against a per-entity oracle (DECISIONS #27, #28).
//!
//! Random world operations (spawning, inserting, replacing, removing, `get_mut`, writing through
//! `Mut` in a query, moving rows between tables, despawning) interleave with calls of long-lived
//! queries with `Added` / `Changed` filters. The oracle keeps, per entity, the sequence numbers
//! of the operations that added and last changed its component, and per query the sequence
//! number of its previous call: every answer must match. Used by `tests/change.rs` and the
//! `change_ticks` fuzz target.

use std::collections::HashMap;

use pragma_ecs::{Added, Changed, EcsError, Entity, Query, With, World};

use super::bytes::{Bytes, pick};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct A(pub u32);

/// A component that only moves rows between tables.
#[derive(Clone, Copy, Debug)]
pub struct Mover;

/// One operation; entity indices pick from every entity ever spawned (modulo their number).
#[derive(Clone, Debug)]
pub enum Op {
    Spawn,
    Despawn(usize),
    Insert(usize, u32),
    Remove(usize),
    GetMut(usize),
    /// Writes `A` through `Mut` where the value is odd; reads the others.
    WriteOdd,
    /// Adds or removes `Mover`, moving the row to another table.
    Move(usize),
    /// Asks observer `.0` (`0..OBSERVERS`).
    Observe(usize),
}

pub const OBSERVERS: usize = 4;

/// The most operations decoded from one input.
pub const MAX_OPS: usize = 300;

/// Decodes fuzzer input into operations, with the weights of the property test's strategy.
pub fn decode(data: &[u8]) -> Vec<Op> {
    let mut bytes = Bytes::new(data);
    let mut ops = Vec::new();
    while !bytes.is_empty() && ops.len() < MAX_OPS {
        let op = match bytes.below(18) {
            0..=2 => Op::Spawn,
            3 => Op::Despawn(bytes.index()),
            4..=7 => {
                let i = bytes.index();
                Op::Insert(i, bytes.u32())
            }
            8 => Op::Remove(bytes.index()),
            9 | 10 => Op::GetMut(bytes.index()),
            11 => Op::WriteOdd,
            12 | 13 => Op::Move(bytes.index()),
            _ => Op::Observe(usize::from(bytes.below(4))),
        };
        ops.push(op);
    }
    ops
}

/// The oracle's record of one `A`: its value and the sequence numbers of its addition and
/// last change.
#[derive(Clone, Copy, Debug)]
struct Record {
    value: u32,
    added: u64,
    changed: u64,
}

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
                let Some(e) = pick(&self.known, i) else {
                    return;
                };
                let alive = self.alive.remove(&e).is_some();
                assert_eq!(self.world.despawn(e).is_ok(), alive);
            }
            Op::Insert(i, v) => {
                let Some(e) = pick(&self.known, i) else {
                    return;
                };
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
                let Some(e) = pick(&self.known, i) else {
                    return;
                };
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
                let Some(e) = pick(&self.known, i) else {
                    return;
                };
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
                let Some(e) = pick(&self.known, i) else {
                    return;
                };
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
            Op::Observe(k) => self.observe(k % OBSERVERS, seq),
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

/// Invariant: every `Added` / `Changed` query yields exactly the entities whose component the
/// oracle saw added / changed since that query's previous call; at the end every observer is
/// asked once more.
pub fn check(ops: impl IntoIterator<Item = Op>) {
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
