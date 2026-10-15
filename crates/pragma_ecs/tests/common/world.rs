//! World operations and queries against a naive `HashMap<Entity, components>` oracle.
//!
//! After every operation each long-lived query (its archetype cache follows the world) yields
//! exactly the entities and components the oracle computes, and mutating queries, sequential and
//! parallel, change exactly those entities. Used by `tests/oracle.rs` and the `world_ops` fuzz
//! target.

use std::any::type_name;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use pragma_core::prelude::{Grain, PoolConfig, ThreadPool};
use pragma_ecs::{EcsError, Entity, Query, With, Without, World};

use super::bytes::{Bytes, pick};

#[derive(Clone, Debug, PartialEq)]
pub struct Pos(pub i32);

#[derive(Clone, Debug, PartialEq)]
pub struct Name(pub String);

/// A zero-sized component.
#[derive(Clone, Debug, PartialEq)]
pub struct Tag;

/// Counts its live copies through the `Arc`, so leaks and double drops show up.
#[derive(Clone, Debug)]
pub struct Tracked(#[allow(dead_code, reason = "held only for its reference count")] Arc<()>);

/// What the oracle knows about one live entity.
#[derive(Debug, Default)]
struct Row {
    pos: Option<Pos>,
    name: Option<Name>,
    tag: Option<Tag>,
    tracked: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Pos,
    Name,
    Tag,
    Tracked,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Pos, Kind::Name, Kind::Tag, Kind::Tracked];
}

/// One operation; entity indices pick from every entity ever spawned (modulo their number).
#[derive(Clone, Debug)]
pub enum Op {
    Spawn,
    Despawn(usize),
    Insert(usize, Kind, i32),
    Remove(usize, Kind),
    Mutate(usize, i32),
    /// Adds the value to `Pos` of every entity with a `Name` (plus 1 if it has a `Tag`).
    QueryAdd(i32),
    /// Adds the value to `Pos` of every entity without a `Tag`, in parallel, chunks of the size.
    ParAdd(i32, usize),
    /// Spawns an entity with the bundle of shape `SHAPES[shape % SHAPES.len()]` (DECISIONS #35).
    SpawnBundle(usize, i32),
    /// Inserts the bundle of the shape into an entity.
    InsertBundle(usize, usize, i32),
}

/// The bundles the operations use: their component kinds, in tuple order. The last shape holds
/// `Pos` twice and is rejected with `EcsError::DuplicateComponent`.
pub const SHAPES: [&[Kind]; 7] = [
    &[Kind::Pos, Kind::Name],
    &[Kind::Name, Kind::Tag, Kind::Pos],
    &[Kind::Tracked, Kind::Pos],
    &[Kind::Tag],
    &[],
    &[Kind::Pos, Kind::Name, Kind::Tag, Kind::Tracked],
    &[Kind::Pos, Kind::Tag, Kind::Pos],
];

/// The shape with a duplicate type.
const DUPLICATE: usize = 6;

/// The bundle of `shape` with the value `v`, passed to `$apply`.
macro_rules! with_bundle {
    ($shape:expr, $v:expr, $token:expr, |$bundle:ident| $apply:expr) => {{
        let (v, token): (i32, &Arc<()>) = ($v, $token);
        let (pos, name, tracked) = (Pos(v), Name(v.to_string()), Tracked(Arc::clone(token)));
        match $shape {
            0 => {
                let $bundle = (pos, name);
                $apply
            }
            1 => {
                let $bundle = (name, Tag, pos);
                $apply
            }
            2 => {
                let $bundle = (tracked, pos);
                $apply
            }
            3 => {
                let $bundle = (Tag,);
                $apply
            }
            4 => {
                let $bundle = ();
                $apply
            }
            5 => {
                let $bundle = (pos, name, Tag, tracked);
                $apply
            }
            _ => {
                let $bundle = (pos, Tag, Pos(v));
                $apply
            }
        }
    }};
}

/// Applies a bundle of `shape` with the value `v` to an oracle row.
fn put_bundle(row: &mut Row, shape: usize, v: i32) {
    for kind in SHAPES[shape] {
        match kind {
            Kind::Pos => row.pos = Some(Pos(v)),
            Kind::Name => row.name = Some(Name(v.to_string())),
            Kind::Tag => row.tag = Some(Tag),
            Kind::Tracked => row.tracked = true,
        }
    }
}

/// `Err(component)` of a `DuplicateComponent`; any other error fails the test.
fn duplicate(err: EcsError) -> &'static str {
    match err {
        EcsError::DuplicateComponent { component, .. } => component,
        other => panic!("unexpected error: {other}"),
    }
}

/// The most operations decoded from one input.
pub const MAX_OPS: usize = 400;

/// Decodes fuzzer input into operations, with the weights of the property test's strategy.
pub fn decode(data: &[u8]) -> Vec<Op> {
    let mut bytes = Bytes::new(data);
    let mut ops = Vec::new();
    while !bytes.is_empty() && ops.len() < MAX_OPS {
        let kind = |b: &mut Bytes<'_>| Kind::ALL[usize::from(b.below(4))];
        let op = match bytes.below(16) {
            0..=2 => Op::Spawn,
            3 => Op::Despawn(bytes.index()),
            4..=8 => {
                let i = bytes.index();
                let k = kind(&mut bytes);
                Op::Insert(i, k, bytes.i32())
            }
            9 | 10 => {
                let i = bytes.index();
                Op::Remove(i, kind(&mut bytes))
            }
            11 => {
                let i = bytes.index();
                Op::Mutate(i, bytes.i32())
            }
            12 => Op::QueryAdd(bytes.i32()),
            13 => {
                let v = bytes.i32();
                Op::ParAdd(v, 1 + usize::from(bytes.below(7)))
            }
            14 => {
                let shape = usize::from(bytes.below(7));
                Op::SpawnBundle(shape, bytes.i32())
            }
            _ => {
                let i = bytes.index();
                let shape = usize::from(bytes.below(7));
                Op::InsertBundle(i, shape, bytes.i32())
            }
        };
        ops.push(op);
    }
    ops
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
        match op {
            Op::Spawn => {
                let e = self.world.spawn().unwrap();
                assert!(!self.oracle.contains_key(&e), "{e} reused while alive");
                assert!(!self.known.contains(&e), "{e} issued twice");
                self.oracle.insert(e, Row::default());
                self.known.push(e);
            }
            Op::Despawn(i) => {
                let Some(e) = pick(&self.known, i) else {
                    return;
                };
                let expected = match self.oracle.remove(&e) {
                    Some(_) => Ok(()),
                    None => Err(e),
                };
                assert_eq!(self.world.despawn(e).map_err(stale), expected);
            }
            Op::Insert(i, kind, v) => {
                let Some(e) = pick(&self.known, i) else {
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
                let Some(e) = pick(&self.known, i) else {
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
                let Some(e) = pick(&self.known, i) else {
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
                for (mut pos, tag) in self.queries.add.iter_mut(&mut self.world).unwrap() {
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
                    .par_for_each_mut(
                        &mut self.world,
                        pool(),
                        Grain::Fixed(size),
                        |(e, mut pos)| {
                            pos.0 = pos.0.wrapping_add(v);
                            visited.lock().unwrap().push(e);
                        },
                    )
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
            Op::SpawnBundle(shape, v) => {
                let shape = shape % SHAPES.len();
                let token = Arc::clone(&self.token);
                let world = &mut self.world;
                let spawned = with_bundle!(shape, v, &token, |bundle| world.spawn_bundle(bundle));
                if shape == DUPLICATE {
                    assert_eq!(spawned.map_err(duplicate), Err(type_name::<Pos>()));
                    return;
                }
                let e = spawned.unwrap();
                assert!(!self.oracle.contains_key(&e), "{e} reused while alive");
                assert!(!self.known.contains(&e), "{e} issued twice");
                let mut row = Row::default();
                put_bundle(&mut row, shape, v);
                self.oracle.insert(e, row);
                self.known.push(e);
            }
            Op::InsertBundle(i, shape, v) => {
                let Some(e) = pick(&self.known, i) else {
                    return;
                };
                let shape = shape % SHAPES.len();
                let token = Arc::clone(&self.token);
                let world = &mut self.world;
                let actual =
                    with_bundle!(shape, v, &token, |bundle| world.insert_bundle(e, bundle));
                match (self.oracle.get_mut(&e), actual) {
                    // A stale entity is reported before the bundle is looked at.
                    (None, actual) => assert_eq!(actual.map_err(stale), Err(e)),
                    (Some(_), Err(err)) if shape == DUPLICATE => {
                        assert_eq!(duplicate(err), type_name::<Pos>());
                    }
                    (Some(row), actual) => {
                        actual.unwrap();
                        put_bundle(row, shape, v);
                    }
                }
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

/// Invariant: after every operation the world agrees with the oracle on every entity ever
/// spawned, stale ones included, and owns exactly the components the oracle says it does;
/// dropping the world drops every component.
pub fn check(ops: impl IntoIterator<Item = Op>) {
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
    assert_eq!(
        Arc::strong_count(&token),
        1,
        "dropping the world drops every component"
    );
}
