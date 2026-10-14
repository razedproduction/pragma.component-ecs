//! Growing the world without memory yields `EcsError::OutOfMemory` and leaves the world
//! unchanged, not an `abort`; despawning, lookups and moves into tables with room do not allocate.
//!
//! A separate test binary with a failing global allocator (DECISIONS.md #11).
// Test code: a panic is a test failure.
#![allow(clippy::unwrap_used, clippy::panic, clippy::arithmetic_side_effects)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pragma_core::prelude::{PoolConfig, ThreadPool};
use pragma_ecs::{EcsError, Query, Schedule, View, World};

/// The system allocator, refusing every allocation on a thread that has `FAIL` raised.
struct Failing;

thread_local! {
    // Const initialization: accessing it from the allocator does not allocate by itself.
    static FAIL: Cell<bool> = const { Cell::new(false) };
}

fn failing() -> bool {
    FAIL.try_with(Cell::get).unwrap_or(false)
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
        for (a, b) in query.iter_mut(&mut world).unwrap() {
            a.0 += u32::try_from(b.0).unwrap();
        }
    });
    assert_eq!(world.get::<A>(e), Some(&A(3)));
}

fn add_b_to_a(mut rows: View<(&mut A, &B)>) -> Result<(), EcsError> {
    for (a, b) in rows.iter_mut()? {
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
