//! Property test: random world operations against a naive `HashMap<Entity, components>` oracle.
//!
//! Queries are checked against the same oracle: after every operation each long-lived query
//! (its archetype cache follows the world) yields exactly the entities and components the oracle
//! computes, and mutating queries, sequential and parallel, change exactly those entities.
// Test code: a panic is a test failure.
#![allow(clippy::unwrap_used, clippy::panic, clippy::arithmetic_side_effects)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
use pragma_ecs::{EcsError, Entity, Query, With, Without, World};
use proptest::prelude::*;
use proptest::sample::Index;

#[derive(Clone, Debug, PartialEq)]
struct Pos(i32);

#[derive(Clone, Debug, PartialEq)]
struct Name(String);

/// A zero-sized component.
#[derive(Clone, Debug, PartialEq)]
struct Tag;

/// Counts its live copies through the `Arc`, so leaks and double drops show up.
#[derive(Clone, Debug)]
struct Tracked(#[expect(dead_code, reason = "held only for its reference count")] Arc<()>);

/// What the oracle knows about one live entity.
#[derive(Debug, Default)]
struct Row {
    pos: Option<Pos>,
    name: Option<Name>,
    tag: Option<Tag>,
    tracked: bool,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Pos,
    Name,
    Tag,
    Tracked,
}

#[derive(Clone, Debug)]
enum Op {
    Spawn,
    Despawn(Index),
    Insert(Index, Kind, i32),
    Remove(Index, Kind),
    Mutate(Index, i32),
    /// Adds the value to `Pos` of every entity with a `Name` (plus 1 if it has a `Tag`).
    QueryAdd(i32),
    /// Adds the value to `Pos` of every entity without a `Tag`, in parallel, chunks of the size.
    ParAdd(i32, usize),
}

fn kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        Just(Kind::Pos),
        Just(Kind::Name),
        Just(Kind::Tag),
        Just(Kind::Tracked)
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::Spawn),
        1 => any::<Index>().prop_map(Op::Despawn),
        5 => (any::<Index>(), kind(), any::<i32>()).prop_map(|(i, k, v)| Op::Insert(i, k, v)),
        2 => (any::<Index>(), kind()).prop_map(|(i, k)| Op::Remove(i, k)),
        1 => (any::<Index>(), any::<i32>()).prop_map(|(i, v)| Op::Mutate(i, v)),
        1 => any::<i32>().prop_map(Op::QueryAdd),
        1 => (any::<i32>(), 1..8_usize).prop_map(|(v, size)| Op::ParAdd(v, size)),
    ]
}

/// One pool for every case: the worker count does not change results (DECISIONS #15).
fn pool() -> &'static ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| ThreadPool::new(PoolConfig::default().with_workers(3)).unwrap())
}

type Mixed = (
    Entity,
    Option<&'static Pos>,
    &'static Name,
    Option<&'static Tag>,
);

/// Long-lived queries: their caches see the archetypes created between operations.
struct Queries {
    pos: Query<(Entity, &'static Pos)>,
    mixed: Query<Mixed, Without<Tracked>>,
    tracked: Query<Entity, With<Tracked>>,
    add: Query<(&'static mut Pos, Option<&'static Tag>), With<Name>>,
    par_add: Query<(Entity, &'static mut Pos), Without<Tag>>,
}

impl Queries {
    fn new() -> Self {
        Self {
            pos: Query::new().unwrap(),
            mixed: Query::new().unwrap(),
            tracked: Query::new().unwrap(),
            add: Query::new().unwrap(),
            par_add: Query::new().unwrap(),
        }
    }
}

fn sorted<T: Ord>(mut rows: Vec<T>) -> Vec<T> {
    rows.sort();
    rows
}

/// The entity of an `EcsError::NoSuchEntity`; any other error fails the test.
fn stale(err: EcsError) -> Entity {
    match err {
        EcsError::NoSuchEntity { entity, .. } => entity,
        other => panic!("unexpected error: {other}"),
    }
}

struct Harness {
    world: World,
    oracle: HashMap<Entity, Row>,
    /// Every entity ever spawned, live or not: stale ids are exercised too.
    known: Vec<Entity>,
    token: Arc<()>,
    queries: Queries,
}

impl Harness {
    fn apply(&mut self, op: Op) {
        let pick = |i: &Index, known: &[Entity]| (!known.is_empty()).then(|| *i.get(known));
        match op {
            Op::Spawn => {
                let e = self.world.spawn().unwrap();
                assert!(!self.oracle.contains_key(&e), "{e} reused while alive");
                assert!(!self.known.contains(&e), "{e} issued twice");
                self.oracle.insert(e, Row::default());
                self.known.push(e);
            }
            Op::Despawn(i) => {
                let Some(e) = pick(&i, &self.known) else {
                    return;
                };
                let expected = match self.oracle.remove(&e) {
                    Some(_) => Ok(()),
                    None => Err(e),
                };
                assert_eq!(self.world.despawn(e).map_err(stale), expected);
            }
            Op::Insert(i, kind, v) => {
                let Some(e) = pick(&i, &self.known) else {
                    return;
                };
                let row = self.oracle.get_mut(&e);
                match kind {
                    Kind::Pos => {
                        let expected = row.map(|r| r.pos.replace(Pos(v))).ok_or(e);
                        assert_eq!(self.world.insert(e, Pos(v)).map_err(stale), expected);
                    }
                    Kind::Name => {
                        let name = Name(v.to_string());
                        let expected = row.map(|r| r.name.replace(name.clone())).ok_or(e);
                        assert_eq!(self.world.insert(e, name).map_err(stale), expected);
                    }
                    Kind::Tag => {
                        let expected = row.map(|r| r.tag.replace(Tag)).ok_or(e);
                        assert_eq!(self.world.insert(e, Tag).map_err(stale), expected);
                    }
                    Kind::Tracked => {
                        let expected = row.map(|r| std::mem::replace(&mut r.tracked, true));
                        let actual = self
                            .world
                            .insert(e, Tracked(Arc::clone(&self.token)))
                            .map_err(stale);
                        match expected {
                            Some(had) => assert_eq!(actual.unwrap().is_some(), had),
                            None => assert_eq!(actual.unwrap_err(), e),
                        }
                    }
                }
            }
            Op::Remove(i, kind) => {
                let Some(e) = pick(&i, &self.known) else {
                    return;
                };
                let row = self.oracle.get_mut(&e);
                match kind {
                    Kind::Pos => {
                        let expected = row.map(|r| r.pos.take()).ok_or(e);
                        assert_eq!(self.world.remove::<Pos>(e).map_err(stale), expected);
                    }
                    Kind::Name => {
                        let expected = row.map(|r| r.name.take()).ok_or(e);
                        assert_eq!(self.world.remove::<Name>(e).map_err(stale), expected);
                    }
                    Kind::Tag => {
                        let expected = row.map(|r| r.tag.take()).ok_or(e);
                        assert_eq!(self.world.remove::<Tag>(e).map_err(stale), expected);
                    }
                    Kind::Tracked => {
                        let expected = row.map(|r| std::mem::replace(&mut r.tracked, false));
                        let actual = self.world.remove::<Tracked>(e).map_err(stale);
                        match expected {
                            Some(had) => assert_eq!(actual.unwrap().is_some(), had),
                            None => assert_eq!(actual.unwrap_err(), e),
                        }
                    }
                }
            }
            Op::Mutate(i, v) => {
                let Some(e) = pick(&i, &self.known) else {
                    return;
                };
                let expected = self.oracle.get_mut(&e).and_then(|r| r.pos.as_mut());
                let actual = self.world.get_mut::<Pos>(e);
                assert_eq!(actual.is_some(), expected.is_some());
                if let (Some(actual), Some(expected)) = (actual, expected) {
                    actual.0 = actual.0.wrapping_add(v);
                    expected.0 = expected.0.wrapping_add(v);
                }
            }
            Op::QueryAdd(v) => {
                let mut visited = 0;
                for (pos, tag) in self.queries.add.iter_mut(&mut self.world).unwrap() {
                    pos.0 = pos.0.wrapping_add(v).wrapping_add(i32::from(tag.is_some()));
                    visited += 1;
                }
                let mut expected = 0;
                for row in self.oracle.values_mut() {
                    if let (Some(pos), Some(_)) = (&mut row.pos, &row.name) {
                        pos.0 = pos
                            .0
                            .wrapping_add(v)
                            .wrapping_add(i32::from(row.tag.is_some()));
                        expected += 1;
                    }
                }
                assert_eq!(visited, expected);
            }
            Op::ParAdd(v, size) => {
                let visited = Mutex::new(Vec::new());
                self.queries
                    .par_add
                    .par_for_each_mut(&mut self.world, pool(), Grain::Fixed(size), |(e, pos)| {
                        pos.0 = pos.0.wrapping_add(v);
                        visited.lock().unwrap().push(e);
                    })
                    .unwrap();
                let mut expected = Vec::new();
                for (e, row) in &mut self.oracle {
                    if let (Some(pos), None) = (&mut row.pos, &row.tag) {
                        pos.0 = pos.0.wrapping_add(v);
                        expected.push(*e);
                    }
                }
                // Every matching entity exactly once.
                assert_eq!(sorted(visited.into_inner().unwrap()), sorted(expected));
            }
        }
    }

    /// Every query yields what the oracle computes.
    fn check_queries(&mut self) {
        let q = &mut self.queries;
        let actual: Vec<_> = q
            .pos
            .iter(&self.world)
            .unwrap()
            .map(|(e, p)| (e, p.0))
            .collect();
        let expected: Vec<_> = self
            .oracle
            .iter()
            .filter_map(|(e, r)| Some((*e, r.pos.as_ref()?.0)))
            .collect();
        assert_eq!(sorted(actual), sorted(expected.clone()));

        // The same rows through a parallel read.
        let count = AtomicUsize::new(0);
        q.pos
            .par_for_each(&self.world, pool(), Grain::Fixed(3), |(e, p)| {
                assert_eq!(self.oracle.get(&e).and_then(|r| r.pos.as_ref()), Some(p));
                count.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
        assert_eq!(count.into_inner(), expected.len());

        let actual: Vec<_> = q
            .mixed
            .iter(&self.world)
            .unwrap()
            .map(|(e, p, n, t)| (e, p.cloned().map(|p| p.0), n.0.clone(), t.is_some()))
            .collect();
        let expected: Vec<_> = self
            .oracle
            .iter()
            .filter(|(_, r)| !r.tracked)
            .filter_map(|(e, r)| {
                let name = r.name.as_ref()?.0.clone();
                Some((*e, r.pos.as_ref().map(|p| p.0), name, r.tag.is_some()))
            })
            .collect();
        assert_eq!(sorted(actual), sorted(expected));

        let actual: Vec<_> = q.tracked.iter(&self.world).unwrap().collect();
        let expected: Vec<_> = self
            .oracle
            .iter()
            .filter_map(|(e, r)| r.tracked.then_some(*e))
            .collect();
        assert_eq!(sorted(actual), sorted(expected));
    }

    fn check(&mut self) {
        self.check_queries();
        assert_eq!(self.world.len(), self.oracle.len());
        assert_eq!(self.world.is_empty(), self.oracle.is_empty());
        let mut tracked = 0;
        for e in &self.known {
            let row = self.oracle.get(e);
            assert_eq!(self.world.contains(*e), row.is_some(), "{e}");
            assert_eq!(
                self.world.get::<Pos>(*e),
                row.and_then(|r| r.pos.as_ref()),
                "{e}"
            );
            assert_eq!(
                self.world.get::<Name>(*e),
                row.and_then(|r| r.name.as_ref()),
                "{e}"
            );
            assert_eq!(
                self.world.get::<Tag>(*e),
                row.and_then(|r| r.tag.as_ref()),
                "{e}"
            );
            let has_tracked = row.is_some_and(|r| r.tracked);
            assert_eq!(self.world.get::<Tracked>(*e).is_some(), has_tracked, "{e}");
            // A type that was never inserted.
            assert_eq!(self.world.get::<u8>(*e), None);
            tracked += usize::from(has_tracked);
        }
        // No leaked and no double-dropped components.
        assert_eq!(Arc::strong_count(&self.token), 1 + tracked);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Invariant: after every operation the world agrees with the oracle on every entity ever
    /// spawned, stale ones included, and owns exactly the components the oracle says it does.
    #[test]
    fn world_matches_oracle(ops in proptest::collection::vec(op(), 0..300)) {
        let mut h = Harness {
            world: World::new(),
            oracle: HashMap::new(),
            known: Vec::new(),
            token: Arc::new(()),
            queries: Queries::new(),
        };
        for op in ops {
            h.apply(op);
            h.check();
        }
        let token = Arc::clone(&h.token);
        drop(h);
        prop_assert_eq!(Arc::strong_count(&token), 1, "dropping the world drops every component");
    }
}
