# Architecture

Status: §2–3 are **implemented** (M1), queries in §4 too (M2); the rest is the **intended design**. Sections marked
*open* are decided in the milestone named there and recorded in `DECISIONS.md`.

## 1. Place in the stack

```
engine / game            frame loop, rendering, input, assets
pragma_ecs               world, components, queries, systems     <- this repository
pragma_core (1.x)        thread pool, frame graph, async tasks
```

The ECS owns no threads (DECISIONS #2). The engine creates the `pragma_core::ThreadPool`, the ECS
builds a `FrameGraph` out of systems and runs it on that pool once per frame.

## 2. Entities

An `Entity` is a generational id: an index into a slot table plus a generation counter. Despawning
bumps the generation, so a stale `Entity` is detected instead of silently addressing a reused slot.
Growth of the slot table is bounded (`try_reserve` → `EcsError::OutOfMemory`).

The allocator is our own (DECISIONS #10), modeled on `pragma_core`'s `HandlePool`: the slot table
and the free list grow together, so freeing never allocates; a slot whose `u32` generation is
exhausted is retired rather than wrapped. Each slot stores the entity's *location*: an archetype
id and a row. `Entity::to_bits` / `from_bits` pack the id into a `u64`.

## 3. Component storage — archetype tables (M1, DECISIONS #8, #9)

```
World
├── entities: slot table          Entity(index, gen) → Location { archetype, row }
├── archetypes: Vec<Archetype>    id = creation order; 0 is the empty set
│     types:    [TypeId]          sorted, unique — the table's component set
│     columns:  [Box<dyn Column>] parallel to `types`; each is a Vec<T>
│     entities: [Entity]          row → entity, parallel to every column
│     add/remove edges            TypeId → neighbouring archetype (cache)
└── by_types: HashMap<[TypeId], archetype id>
```

- **Tables.** Entities with the same component set share an archetype. A component lives at
  `columns[i][row]` where `types[i]` is its type; `get::<T>` is a binary search over `types`, one
  `downcast` of the column to `Vec<T>` and a bounds-checked index.
- **Structural changes.** `insert` of a new type or `remove` moves the entity's row to the
  neighbouring archetype: every shared column moves its value with `swap_remove` + `push`, the
  last row of the source table takes the vacated row, and the locations of both entities are
  updated. The neighbour is found through the per-table edge cache, then `by_types`, and is
  created if missing. Replacing an existing component is an in-place `mem::replace`.
- **Failure atomicity.** Every allocation happens before the first mutation: the target table
  (types, columns, map entry) and one row of room in each of its columns are reserved with
  `try_reserve`, so a failure returns `EcsError::OutOfMemory` with the world unchanged (except for
  a possibly created empty table). The move itself does not allocate; `despawn` never allocates.
  Checked by `tests/oom.rs` with a failing global allocator (DECISIONS #11).
- **No `unsafe`.** A column is `Vec<T>` behind the crate-private `Column` trait object (DECISIONS
  #9); the trait has the operations a table needs without knowing `T` (an empty clone, reserve,
  `swap_remove` with drop, `swap_remove` into another column of the same type). Iteration (M2)
  downcasts each column once per table and then works on plain slices.
- **Components.** `Component` is implemented for every `Send + Sync + 'static` type; zero-sized
  components cost no memory (`Vec<ZST>`).

Not chosen for now: sparse sets (cheap add/remove, but multi-component iteration needs a join) and
a hybrid (sparse sets opt-in per type). A hybrid can be added later without changing the `World`
methods (per-type configuration would change `Component`, see DECISIONS #8), by a new decision
backed by benchmarks (M5).

## 4. Queries (M2, DECISIONS #12–#15) and systems

```rust
let mut movement = Query::<(&mut Position, &Velocity), Without<Frozen>>::new()?; // checks access
for (position, velocity) in movement.iter_mut(&mut world)? { /* … */ }
movement.par_for_each_mut(&mut world, &pool, Grain::Auto, |(position, velocity)| { /* … */ })?;
```

- **Shape.** `Query<Q, F = ()>` is a state object that does not borrow the world. `Q`
  (`QueryData`) is `&T`, `&mut T`, `Option<&T>`, `Entity` or a tuple of them (up to 8); `F`
  (`QueryFilter`) is `With<T>`, `Without<T>` or a tuple. Read-only `Q` (`ReadOnlyQueryData`) also
  iterates a shared `&World` (`iter`, `par_for_each`). Both traits are sealed: their machinery is
  internal.
- **Declared access.** `Query::new` builds an `Access`: a sorted list of (component type, name,
  `Read` / `Write`). A type that is written and also accessed elsewhere in `Q` is
  `EcsError::ConflictingAccess`; repeated reads are allowed; filters and `Entity` declare
  nothing. `Access::conflict` tells whether two accesses may run at the same time (for systems).
- **Archetype cache.** A table matches if it has every column `Q` requires and passes `F`. The
  query keeps the ids of matching tables plus the number of tables already checked; tables are
  never removed, so each call checks only the new ones. The cache is tied to a process-unique
  world id and is rebuilt for another world. Its growth goes through `try_reserve`; with a warm
  cache iteration does not allocate (`tests/oom.rs`).
- **Borrowing columns without `unsafe`.** For each matching table, one pass of
  `columns.iter_mut()` offers every column to every element of `Q`: a write takes the `&mut`, a
  read turns the column into a shared reference that other reads copy. Each element downcasts its
  column once into `&[T]` / `&mut [T]`, and the table's rows are those slices iterated in
  lockstep. The `&mut World` borrow is split into one `&mut Archetype` per matching table by
  `split_at_mut`. The borrow checker proves the absence of aliasing; nothing is checked per row.
- **Order.** Tables in creation order, rows in table order: deterministic for the same sequence
  of world operations.
- **Parallel iteration.** The chunk size is `grain.chunk_size(total matching rows)`; every table
  is split into chunks of that size (a chunk never spans two tables) and the chunk list goes to
  `ThreadPool::parallel_for` with `Grain::Fixed(1)`. A chunk holds its rows' column slices, so it
  is `Send` because components are `Send + Sync`; the closure is `Fn(Item) + Sync`. The split
  depends only on the data, so the result is the same on 1 and N workers (`tests/parallel.rs`). A
  panic in the closure is caught by the pool and returned as `EcsError::Job`.

First numbers (`cargo bench -p pragma_ecs`, 100 000 entities, `(&mut Position, &Velocity)` with
`[f32; 3]` each, 8 workers on the pragma.core perf machine): a sequential query ≈ 200 µs on one
table and ≈ 250 µs on 8, a plain `Vec<(Position, Velocity)>` ≈ 257 µs, a parallel query ≈ 170 µs
(the update is memory-bound). Comparisons with `hecs` / `bevy_ecs` and a bench gate are M5.

**Systems (M3).** A system is a function plus the union of its queries' and resources' accesses.
Systems are registered as frame-graph tasks with those accesses, so pragma.core rejects
conflicting systems without an explicit order at build time rather than letting them race at run
time.

## 5. Reliability rules

The same as pragma.core: no `unwrap`/`expect`/`panic!`/unchecked indexing in library code (lints),
typed `#[non_exhaustive]` errors, pure-Rust dependencies, layers checked by `tools/audit_workspace.py`.
