# Architecture

Status: §2–3 are **implemented** (M1), queries in §4 too (M2), systems, resources and
schedules (M3), and commands, events and change detection (§4, M4), and the hardening tools of M5
(§6); the rest is the **intended design**. Sections marked *open* are decided in the
milestone named there and recorded in `DECISIONS.md`.

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
│                                  plus a Vec<ComponentTicks> (added, changed: u64, M4)
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

## 4. Queries (M2, DECISIONS #12–#15) and systems (M3, #17–#21)

```rust
let mut movement = Query::<(&mut Position, &Velocity), Without<Frozen>>::new()?; // checks access
for (mut position, velocity) in movement.iter_mut(&mut world)? { /* … */ } // Mut<Position>
movement.par_for_each_mut(&mut world, &pool, Grain::Auto, |(mut position, velocity)| { /* … */ })?;
```

- **Shape.** `Query<Q, F = ()>` is a state object that does not borrow the world. `Q`
  (`QueryData`) is `&T`, `&mut T` (yielding `Mut<T>`, M4), `Option<&T>`, `Entity` or a tuple of
  them (up to 8); `F` (`QueryFilter`) is `With<T>`, `Without<T>`, `Added<T>`, `Changed<T>` (M4)
  or a tuple. Read-only `Q` (`ReadOnlyQueryData`) also
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
(the update is memory-bound). Comparisons with `hecs` / `bevy_ecs` and the bench gate: §6.

### Systems and schedules (M3, DECISIONS #17–#21)

```rust
fn movement(mut bodies: View<(&mut Position, &Velocity)>, dt: Res<Dt>) -> Result<(), EcsError> { /* … */ }
fn render(bodies: View<&Position>) { /* … */ }

let mut builder = Schedule::builder();                 // or Schedule::builder().ordered()
builder.add_system("movement", movement)?;
builder.add_system_with("render", render, |c| c.after("movement"))?;
let mut schedule = builder.build()?;                   // AmbiguousOrder without the `after`
schedule.run(&mut world, &pool)?;                      // every frame
```

- **Systems.** A system is a plain function (or closure) of up to 8 parameters, each a sealed
  `SystemParam`: `View<Q, F>` (a query over the frame's world, the counterpart of `Query<Q, F>`
  with the same archetype cache), `Res<T>`, `ResMut<T>`, and since M4 `Commands`,
  `EventWriter<T>` and `EventReader<T>` (below). It returns `()` or `Result<(), E>`
  for any error type. Its `Access` is the union of its parameters' accesses, components and
  resources kept apart; a write together with another use of the same type in one system is
  rejected by `add_system` (`ConflictingAccess` / `ConflictingResourceAccess`). The parameters'
  state lives with the system in the schedule, behind a mutex nobody contends (a graph task runs
  once per frame).
- **Systems as graph tasks.** `ScheduleBuilder` registers every system as a task of a
  `pragma_core` `FrameGraphBuilder`. The access is declared through function pointers stored in
  each `Access` entry: reading component `T` is `reads::<Columns<T>>()`, writing it
  `writes::<Columns<T>>()`, resource `T` is `reads`/`writes::<T>()`, and every system also reads
  the world slot. pragma.core then validates the graph once: conflicting systems without an
  explicit order are `GraphError::AmbiguousOrder`, mapped to `EcsError::AmbiguousOrder` with the
  component or resource name. `ordered()` lets registration order settle conflicts.
- **Lending the world to a frame.** Graph tasks are `'static` and reach data only through the
  graph's `Resources` (one `RwLock` per type), so `Schedule::run` turns the world into resources
  for the frame:

  ```
  before the frame                       during the frame (graph Resources)
  table k: Vec<Position> ──swap──►  Columns<Position>.tables[k]   (RwLock, read or written)
  table k: Vec<Velocity> ──swap──►  Columns<Velocity>.tables[k]
  the World (types, entities,  ──►  WorldSlot(World)             (read by every system)
             lent columns empty)
  user resources               ──►  T                             (RwLock)
  ```

  Only the component types some system uses are lent. The world's resources already are a
  `pragma_core` `Resources`, which also holds the `Columns<T>` slots and the world slot between
  frames, so the frame needs no allocation once the slots exist: lending is `mem::swap` of each
  table's `Vec<T>` with an empty one, and the slots grow (`try_reserve`) only when new tables
  appear — before anything is lent, so an `OutOfMemory` leaves the world untouched. After the
  frame, successful or not, the same swaps put everything back. Structural changes cannot
  happen during a frame (systems get slices, never a `Vec` or `&mut World`); systems record
  them as deferred commands, applied at the sync point after the columns are back (below).
- **A view during the frame.** `View<Q, F>` holds the read or write guards of its `Columns<T>`
  for the system's whole run, so the graph's locks are taken once per system, never per row; a
  lock is never waited on because the graph only runs non-conflicting systems together. The
  view updates its archetype cache from the world slot and walks the matching tables in
  ascending order, splitting the `&mut [Vec<T>]` of a written type off table by table, as
  `iter_mut` on the world does (no `unsafe`, DECISIONS #13 unchanged). `par_for_each(_mut)` use
  the same chunks as queries, on the pool the frame runs on.
- **Errors.** A missing resource is `EcsError::MissingResource` before any system runs. A system
  that returns an error or panics stops the systems ordered after it in that frame (pragma.core
  semantics) and is `SystemFailed { system, message, source }` (the `EcsError`, if it was one) or
  `SystemPanicked`; other graph errors keep pragma.core's error as the source of
  `EcsError::Schedule`. The schedule and the world stay usable after any error.
- **Determinism.** Conflicting systems are always ordered, parallel iteration splits rows by the
  data only, and tables and rows are visited in a fixed order, so a frame gives the same world on
  any number of workers. `examples/ecs_sim` hashes the state after every tick and checks it on
  1, 2 and N workers; `tests/schedule.rs` checks random system sets against a sequential run.

First numbers (`cargo bench -p pragma_ecs --bench schedule`, 8 logical cores, release): one frame
of four systems over 100 000 entities (`movement`, `aging` and `health` parallel over rows,
`energy` a sequential sum, all four independent) ≈ 274 µs on the schedule, against ≈ 565 µs for
the same updates as sequential queries on the world; the fixed cost of a frame of four systems
on an empty world (lending, the graph, returning) ≈ 5.4 µs.

### Commands, events and the sync point (M4, DECISIONS #23–#26)

```rust
fn reap(mut bodies: View<(Entity, &Health)>, mut commands: Commands, mut deaths: EventWriter<Death>)
    -> Result<(), EcsError>
{
    for (entity, health) in bodies.iter()? {
        if health.0 <= 0.0 {
            commands.despawn(entity)?;
            commands.spawn()?.insert(Corpse)?;      // the id is allocated at the sync point
            deaths.send(Death(entity))?;
        }
    }
    Ok(())
}
fn mourn(mut deaths: EventReader<Death>) { for death in deaths.read() { /* next frame */ } }
```

- **Per-system buffers.** `Commands` and `EventWriter<T>` keep what they record in the system's
  own state (next to its query caches), so they declare no access and never order systems; the
  buffers keep their capacity between frames. A command queue is a list of operations plus one
  typed lane per component type (`Vec<T>` behind one `Box<dyn …>`), so inserting a value is a
  `try_reserve` + `push`, never a box per value.
- **Deferred ids.** `Commands::spawn` returns an `EntityCommands` whose entity has no id yet;
  later commands of the same queue address it by its spawn number. The id is allocated when the
  queue is applied, which makes ids independent of which worker ran first.
- **The sync point** ends `Schedule::run`, after the columns are returned (also after a failed
  frame, not when the frame did not start): first every event type the schedule uses is updated,
  then the deferred buffers of the systems that completed are applied, system after system in
  registration order, parameter after parameter, command after command — the world's ordinary
  operations, so each stays atomic. Several stages are several schedules.
- **Failures.** A command on an entity that is gone does nothing (two systems despawning the
  same entity is an ordinary race). Any other failure (out of memory) stops the application:
  what was applied stays, the rest is discarded, and `run` returns
  `EcsError::CommandFailed { system, source }`. A system that failed or panicked has its buffers
  discarded.
- **Events.** `Events<T>` is a world resource with two buffers and a running event id: `update`
  drops the older buffer and keeps the newer one, so an event lives for two sync points. At the
  sync point every writer's events are appended in registration order; an `EventReader<T>`
  reads `Events<T>` (a resource read, it conflicts only with `ResMut<Events<T>>`) from a cursor
  in its system's state. Readers see the events from the next frame on, each once; the schedule
  creates `Events<T>` for every event type its systems use.

### Change detection (M4, DECISIONS #27, #28)

- **Ticks.** Every row of every column has an added and a changed tick (`u64`, never
  wrapping), in two `Vec`s next to the values (separate, so that a write touches only the changed
  ticks), moved and lent together with them. The world has a
  change tick: every mutating world operation (`insert`, `get_mut`, a mutable query call) takes
  a new one. A frame takes one tick per system — the frame's base plus the system's position in
  the schedule's stable topological order — so two conflicting systems get ticks in the order
  they really run, on any number of workers; the commands applied at the sync point come after
  all of them.
- **Writes.** `&mut T` yields a `Mut<'_, T>`: `DerefMut` stamps the row's changed tick with the
  writer's tick, so only rows really written count as changed. `World::get_mut` stamps eagerly;
  `insert` stamps added and changed for a new component, changed for a replacement.
- **Filters.** `Added<T>` / `Changed<T>` keep rows whose tick is newer than the tick at which the
  system (or, outside systems, the `Query`) last ran; the first run sees everything. They are row
  filters: before the data columns are borrowed, the query fills a reusable mask (one byte per
  row of the matching tables) from the tick columns with shared reads, and the iterators and the
  parallel chunks skip the rows the mask clears. That is how `Query<&mut T, Changed<T>>` reads
  and then writes the same tick column without `unsafe`; in a view the filter reads the ticks
  through the data's own column guard when the data holds `T`. Table filters (`With`,
  `Without`) cost nothing per row, as before.

First numbers (`cargo bench -p pragma_ecs`, 8 logical cores, release, the same machine for
M3 and M4): a sequential `(&mut Position, &Velocity)` query that writes every row ≈ 285 µs
(M3: ≈ 195 µs), in parallel ≈ 213 µs (M3: ≈ 166 µs) — the write of the 8-byte changed tick next
to 24 bytes of row data in a memory-bound loop; a read-only query is unchanged (≈ 122 µs). The
frame of four systems ≈ 349 µs (M3: ≈ 272 µs). A `Changed<Position>` read over 100 000 rows
costs ≈ 140 µs when nothing changed, ≈ 161 µs when 10 % did and ≈ 217 µs when all did, against
≈ 122 µs without the filter. Spawning 10 000 entities with two components through commands
≈ 3.1 ms against ≈ 2.5 ms directly (each command is an ordinary world operation, so a spawn
moves the entity through `{}` → `{A}` → `{A, B}`); despawning ≈ 377 µs against ≈ 277 µs.
Batching spawns into their final table: after the M5 baseline (§6), an M6 API item (DECISIONS
#34).

## 5. Reliability rules

The same as pragma.core: no `unwrap`/`expect`/`panic!`/unchecked indexing in library code (lints),
typed `#[non_exhaustive]` errors, pure-Rust dependencies, layers checked by `tools/audit_workspace.py`.

## 6. Hardening (M5, DECISIONS #29–#34)

```
crates/pragma_ecs/tests/common/   oracles: world (HashMap), commands (direct ops), change (ticks)
   ├── tests/oracle.rs, commands.rs, change.rs   proptest strategies + decoded random bytes
   └── fuzz/fuzz_targets/*.rs                     cargo-fuzz, separate workspace (C++ libFuzzer)
examples/ecs_sim --suite          7 scenarios × {1, 2, N} workers, golden.txt
baseline/                         pragma_ecs vs hecs vs bevy_ecs, separate workspace
tools/bench_gate.py               A/B of PR base and head, normalized by a reference
```

- **Determinism suite.** `ecs_sim --suite` runs seven scenarios (the default 100 000 entities, an
  odd count, an empty world, one entity, four tables, a lethal hazard that turns most of a
  frame into commands and events, a calm arena where `Changed<Health>` sees few rows) on 1, 2
  and N workers and requires the same hash on every tick. Each scenario's *history* (an FNV
  fold of all its tick hashes) is compared with `examples/ecs_sim/golden.txt`, so a hash that
  differs between Windows and Linux or an unintended change of behaviour fails; `--bless`
  rewrites the file. The simulation uses only IEEE-exact `f32` operations (`+ − × ÷`, `sqrt`,
  `min`/`max`/`clamp`), which is why one golden file holds on every platform. CI: 200 ticks on
  every PR, 3000 nightly, both platforms; a debug test runs the scenarios capped in size.
- **Fuzzing.** One oracle per property, in `tests/common/`: each module has the operations, a
  harness that applies them to the world and to the naive model and panics on the first
  disagreement, and a decoder from bytes. The property tests generate operations with proptest
  strategies and also feed random bytes through the decoder; the fuzz targets `world_ops`,
  `commands`, `change_ticks` include the same module through `#[path]`. libFuzzer does not link
  on Windows, so the targets run on Linux (60 s per PR, 10 min nightly with the corpus kept in
  the Actions cache); on Windows the same oracles run through proptest.
- **Bench gate.** In a PR, the base and the head are built into separate target directories
  and measured interleaved for three rounds; a criterion measurement fails when it is more than
  10 % slower **and** still more than 10 % slower after dividing by the shift of its reference
  in the same bench (a plain `Vec`, sequential queries, direct world operations, a plain read),
  `ecs_sim` ms/tick when it is slower than 10 % plus its own run-to-run noise.
- **Semver.** `cargo semver-checks -p pragma_ecs` against the PR base: a breaking change needs a
  version bump (the version is 0.5.0 since M5).

Baseline numbers (`cargo bench --manifest-path baseline/Cargo.toml --bench baseline`, 8 logical
cores, Windows, release, medians):

| workload | pragma_ecs | hecs 0.11 | bevy_ecs 0.20 |
|---|---|---|---|
| spawn 10 000 entities with 2 components | 2.46 ms | 0.59 ms | 1.07 ms |
| iterate 100 000 `(&mut Position, &Velocity)` (a `Vec`: 258 µs) | 322 µs | 204 µs | 348 µs |
| iterate 20 rows in each of 26 tables | 1.62 µs | 0.35 µs | 1.04 µs |
| add and remove a component on 10 000 entities | 3.06 ms | 1.85 ms | 2.59 ms |
| despawn 10 000 entities | 277 µs | 344 µs | 874 µs |

`pragma_ecs` and `bevy_ecs` stamp a change tick on every write (`u64` and `u32`), `hecs` has
no change detection; iteration is faster than `bevy_ecs` and pays the tick store against
`hecs`. Spawning is the gap: without bundles every `insert` moves the entity to the next table
(`{}` → `{Position}` → `{Position, Velocity}`). DECISIONS #34 keeps the storage and the ticks as
they are and leaves bundle spawns (for the world and for commands) to the M6 API review.
