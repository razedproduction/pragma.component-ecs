//! Growing the world without memory yields `EcsError::OutOfMemory` and leaves the world
//! unchanged, not an `abort`; despawning, lookups and moves into tables with room do not allocate,
//! bundles included (DECISIONS #35).
//! Applying deferred commands without memory stops with `EcsError::CommandFailed`, keeping what
//! was applied (DECISIONS #25).
//!
//! A separate test binary with a failing global allocator (DECISIONS.md #11).
// Test code: a panic is a test failure.
#![allow(clippy::unwrap_used, clippy::panic, clippy::arithmetic_side_effects)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pragma_core::prelude::{PoolConfig, ThreadPool};
use pragma_ecs::{Commands, EcsError, Query, Res, Schedule, View, World};

/// The system allocator, refusing every allocation on a thread that has `FAIL` raised, or the
/// next `REFUSE` allocations on that thread.
struct Failing;

thread_local! {
    // Const initialization: accessing it from the allocator does not allocate by itself.
    static FAIL: Cell<bool> = const { Cell::new(false) };
    static REFUSE: Cell<u32> = const { Cell::new(0) };
}

fn failing() -> bool {
    if FAIL.try_with(Cell::get).unwrap_or(false) {
        return true;
    }
    REFUSE
        .try_with(|refuse| {
            let left = refuse.get();
            refuse.set(left.saturating_sub(1));
            left > 0
        })
        .unwrap_or(false)
}

// SAFETY: the methods delegate to `System` with the same arguments; failure means returning null,
// which the `GlobalAlloc::alloc`/`realloc` contract explicitly allows (the caller must handle null).
unsafe impl GlobalAlloc for Failing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if failing() {
            return std::ptr::null_mut();
        }
        // SAFETY: the `alloc` contract is passed through from the caller unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if failing() {
            return std::ptr::null_mut();
        }
        // SAFETY: `ptr` was allocated by this same allocator (i.e. `System`); the contract is passed through as is.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `System` through this allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Failing = Failing;

/// Runs `f` while the allocator refuses on this thread.
fn without_memory<R>(f: impl FnOnce() -> R) -> R {
    FAIL.with(|fail| fail.set(true));
    let result = f();
    FAIL.with(|fail| fail.set(false));
    result
}

/// Runs `f` while the allocator refuses the first allocation on this thread; later ones (for
/// example the error's own message) succeed.
fn refusing_once<R>(f: impl FnOnce() -> R) -> R {
    REFUSE.with(|refuse| refuse.set(1));
    let result = f();
    REFUSE.with(|refuse| refuse.set(0));
    result
}

#[derive(Debug, PartialEq)]
struct A(u32);

#[derive(Debug, PartialEq)]
struct B(u64);

#[test]
fn spawn_without_memory_is_an_error_and_world_stays_usable() {
    let mut world = World::new();
    assert!(matches!(
        without_memory(|| world.spawn()),
        Err(EcsError::OutOfMemory { .. })
    ));
    assert!(world.is_empty());

    let e = world.spawn().unwrap();
    world.insert(e, A(1)).unwrap();
    assert_eq!(world.get::<A>(e), Some(&A(1)));
}

#[test]
fn insert_and_remove_without_memory_leave_the_entity_intact() {
    let mut world = World::new();
    let e = world.spawn().unwrap();
    world.insert(e, A(1)).unwrap();

    // {A} → {A, B} needs a new table.
    assert!(matches!(
        without_memory(|| world.insert(e, B(2))),
        Err(EcsError::OutOfMemory { .. })
    ));
    assert_eq!(world.get::<A>(e), Some(&A(1)));
    assert_eq!(world.get::<B>(e), None);

    // {A} → {}: the table exists, but the transition is not cached yet and is looked up.
    assert!(matches!(
        without_memory(|| world.remove::<A>(e)),
        Err(EcsError::OutOfMemory { .. })
    ));
    assert_eq!(world.get::<A>(e), Some(&A(1)));
    assert_eq!(world.len(), 1);

    // With memory the world works as usual, and the transitions are cached afterwards.
    world.insert(e, B(3)).unwrap();
    assert_eq!(world.get::<B>(e), Some(&B(3)));
    assert_eq!(world.remove::<B>(e).unwrap(), Some(B(3)));
    assert_eq!(world.remove::<A>(e).unwrap(), Some(A(1)));
    world.insert(e, A(4)).unwrap();
    assert_eq!(without_memory(|| world.remove::<A>(e)).unwrap(), Some(A(4)));
}

#[test]
fn queries_without_memory_fail_typed_and_a_warm_cache_does_not_allocate() {
    let mut world = World::new();
    let e = world.spawn().unwrap();
    world.insert(e, A(1)).unwrap();
    world.insert(e, B(2)).unwrap();

    // The access list and the cache of matching tables are allocated with `try_reserve`.
    assert!(matches!(
        without_memory(Query::<(&mut A, &B)>::new),
        Err(EcsError::OutOfMemory { .. })
    ));
    let mut query = Query::<(&mut A, &B)>::new().unwrap();
    assert!(matches!(
        without_memory(|| query.iter_mut(&mut world).map(|_| ())),
        Err(EcsError::OutOfMemory { .. })
    ));

    // Once the cache knows every table, iterating does not allocate.
    assert_eq!(query.iter_mut(&mut world).unwrap().count(), 1);
    without_memory(|| {
        for (mut a, b) in query.iter_mut(&mut world).unwrap() {
            a.0 += u32::try_from(b.0).unwrap();
        }
    });
    assert_eq!(world.get::<A>(e), Some(&A(3)));
}

#[test]
fn bundles_without_memory_leave_the_world_unchanged_and_warm_ones_do_not_allocate() {
    let mut world = World::new();
    let e = world.spawn().unwrap();

    // The {A, B} table does not exist yet.
    assert!(matches!(
        without_memory(|| world.spawn_bundle((A(1), B(1)))),
        Err(EcsError::OutOfMemory { .. })
    ));
    assert!(matches!(
        without_memory(|| world.insert_bundle(e, (A(1), B(1)))),
        Err(EcsError::OutOfMemory { .. })
    ));
    assert_eq!(world.len(), 1);
    assert_eq!((world.get::<A>(e), world.get::<B>(e)), (None, None));

    // Warm: the table with room for several rows, the cached transitions of both bundles.
    world.insert_bundle(e, (A(1), B(1))).unwrap();
    world.insert_bundle(e, (A(2), B(2))).unwrap();
    let spawned: Vec<_> = (0..3)
        .map(|i| world.spawn_bundle((A(i), B(u64::from(i)))).unwrap())
        .collect();
    world.despawn(spawned[0]).unwrap();

    without_memory(|| {
        // Into the freed slot and row; a replacement in place.
        let reused = world.spawn_bundle((A(7), B(7))).unwrap();
        assert_eq!(reused.index(), spawned[0].index());
        world.insert_bundle(e, (A(3), B(3))).unwrap();
    });
    assert_eq!(world.get::<A>(e), Some(&A(3)));
    assert_eq!(world.len(), 4);
}

fn add_b_to_a(mut rows: View<(&mut A, &B)>) -> Result<(), EcsError> {
    for (mut a, b) in rows.iter_mut()? {
        a.0 += u32::try_from(b.0).unwrap();
    }
    Ok(())
}

#[test]
fn a_frame_without_memory_for_new_tables_lends_nothing() {
    let pool = ThreadPool::new(PoolConfig::default().with_workers(1)).unwrap();
    let mut builder = Schedule::builder();
    builder.add_system("add", add_b_to_a).unwrap();
    let mut schedule = builder.build().unwrap();

    let mut world = World::new();
    let e = world.spawn().unwrap();
    world.insert(e, A(1)).unwrap();
    world.insert(e, B(2)).unwrap();
    schedule.run(&mut world, &pool).unwrap();
    assert_eq!(world.get::<A>(e), Some(&A(3)));

    // A new table: the lending slots must grow before anything is lent.
    let f = world.spawn().unwrap();
    world.insert(f, B(5)).unwrap();
    world.insert(f, A(0)).unwrap();
    world.insert(f, 0_u8).unwrap();
    assert!(matches!(
        without_memory(|| schedule.run(&mut world, &pool)),
        Err(EcsError::OutOfMemory { .. })
    ));
    assert_eq!(world.get::<A>(e), Some(&A(3)));
    assert_eq!(world.get::<B>(f), Some(&B(5)));

    schedule.run(&mut world, &pool).unwrap();
    assert_eq!(world.get::<A>(e), Some(&A(5)));
    assert_eq!(world.get::<A>(f), Some(&A(5)));
}

struct Phase(u8);

#[test]
fn commands_without_memory_stop_with_command_failed_and_keep_what_was_applied() {
    let pool = ThreadPool::new(PoolConfig::default().with_workers(1)).unwrap();
    let mut world = World::new();
    let first = world.spawn().unwrap();
    let second = world.spawn().unwrap();
    let gone = world.spawn().unwrap();
    world.despawn(gone).unwrap();
    world.insert_resource(Phase(0));

    let mut builder = Schedule::builder();
    builder
        .add_system(
            "edit",
            move |mut commands: Commands<'_>, phase: Res<Phase>| -> Result<(), EcsError> {
                if phase.0 == 0 {
                    // Warms the queue: a lane for `B` (on a dead entity: a no-op) and the {A} table.
                    commands.insert(gone, B(0))?;
                    commands.insert(first, A(1))?;
                } else {
                    commands.insert(first, A(2))?; // a replacement: no allocation
                    commands.insert(second, B(3))?; // the new table {B} needs memory
                    commands.insert(first, A(4))?;
                }
                Ok(())
            },
        )
        .unwrap();
    let mut schedule = builder.build().unwrap();
    schedule.run(&mut world, &pool).unwrap();
    assert_eq!(world.get::<A>(first), Some(&A(1)));

    world.resource_mut::<Phase>().unwrap().0 = 1;
    // The frame itself and the replacement do not allocate (warm buffers); the first
    // allocation is the new table, at the sync point. Building the error allocates again.
    match refusing_once(|| schedule.run(&mut world, &pool)) {
        Err(EcsError::CommandFailed { system, source, .. }) => {
            assert_eq!(system, "edit");
            assert!(matches!(*source, EcsError::OutOfMemory { .. }));
        }
        other => panic!("unexpected {other:?}"),
    }
    // Applied before the failure: kept; the failing command and the rest: discarded.
    assert_eq!(world.get::<A>(first), Some(&A(2)));
    assert_eq!(world.get::<B>(second), None);

    schedule.run(&mut world, &pool).unwrap();
    assert_eq!(world.get::<A>(first), Some(&A(4)));
    assert_eq!(world.get::<B>(second), Some(&B(3)));
}

#[test]
fn reuse_moves_and_lookups_do_not_allocate() {
    let mut world = World::new();
    let entities: Vec<_> = (0..3).map(|_| world.spawn().unwrap()).collect();
    // Creates the {A} table (room for several rows) and caches the {} → {A} transition.
    world.insert(entities[0], A(0)).unwrap();

    without_memory(|| {
        // Replacing a component, moving into a table with room, lookups.
        assert_eq!(world.insert(entities[0], A(10)).unwrap(), Some(A(0)));
        assert_eq!(world.insert(entities[1], A(1)).unwrap(), None);
        world.get_mut::<A>(entities[1]).unwrap().0 += 1;
        assert_eq!(world.get::<A>(entities[1]), Some(&A(2)));

        // Despawning, then spawning into the freed slot.
        world.despawn(entities[2]).unwrap();
        let reused = world.spawn().unwrap();
        assert_eq!(reused.index(), entities[2].index());
        assert!(!world.contains(entities[2]));
    });
    assert_eq!(world.len(), 3);
}
