//! Property test: random world operations against a naive `HashMap<Entity, components>` oracle.
// Test code: a panic is a test failure.
#![allow(clippy::unwrap_used, clippy::panic, clippy::arithmetic_side_effects)]

use std::collections::HashMap;
use std::sync::Arc;

use pragma_ecs::{EcsError, Entity, World};
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
    ]
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
        }
    }

    fn check(&self) {
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
