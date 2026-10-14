# Architecture

Status: **intended design** (M0). Sections marked *open* are decided in the milestone named there
and recorded in `DECISIONS.md`.

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

## 3. Component storage — *open (M1)*

Candidates:

- **Archetype tables**: entities with the same component set share a table of columns. Fastest
  iteration; adding/removing a component moves the entity between tables.
- **Sparse sets**: one dense array per component type. Cheap add/remove; iteration over several
  components needs a join.
- **Hybrid**: tables by default, sparse sets opt-in per component type.

A type-erased column (bytes + layout + drop fn) needs `unsafe`; the alternative is
`Box<dyn Any>`-per-column storage that is safe but slower. The choice decides DECISIONS #6.

## 4. Queries and systems

A query declares which component types it reads and writes. A system is a function plus the union
of its queries' and resources' accesses. Systems are registered as frame-graph tasks with those
accesses, so pragma.core rejects conflicting systems without an explicit order at build time
rather than letting them race at run time.

## 5. Reliability rules

The same as pragma.core: no `unwrap`/`expect`/`panic!`/unchecked indexing in library code (lints),
typed `#[non_exhaustive]` errors, pure-Rust dependencies, layers checked by `tools/audit_workspace.py`.
