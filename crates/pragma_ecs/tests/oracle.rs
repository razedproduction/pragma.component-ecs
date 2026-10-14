//! Property test: random world operations against a naive `HashMap<Entity, components>` oracle.
//!
//! Queries are checked against the same oracle: after every operation each long-lived query
//! (its archetype cache follows the world) yields exactly the entities and components the oracle
//! computes, and mutating queries, sequential and parallel, change exactly those entities. The
//! oracle (`common/world.rs`) is shared with the `world_ops` fuzz target (DECISIONS #30).

mod common;

use common::world::{Kind, Op, check, decode};
use proptest::prelude::*;

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
        1 => any::<usize>().prop_map(Op::Despawn),
        5 => (any::<usize>(), kind(), any::<i32>()).prop_map(|(i, k, v)| Op::Insert(i, k, v)),
        2 => (any::<usize>(), kind()).prop_map(|(i, k)| Op::Remove(i, k)),
        1 => (any::<usize>(), any::<i32>()).prop_map(|(i, v)| Op::Mutate(i, v)),
        1 => any::<i32>().prop_map(Op::QueryAdd),
        1 => (any::<i32>(), 1..8_usize).prop_map(|(v, size)| Op::ParAdd(v, size)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Invariant: after every operation the world agrees with the oracle on every entity ever
    /// spawned, stale ones included, and owns exactly the components the oracle says it does.
    #[test]
    fn world_matches_oracle(ops in proptest::collection::vec(op(), 0..300)) {
        check(ops);
    }

    /// The fuzz target's decoder: arbitrary bytes are a valid sequence of operations.
    #[test]
    fn decoded_bytes_match_oracle(data in proptest::collection::vec(any::<u8>(), 0..1024)) {
        check(decode(&data));
    }
}
