# Contributing

## Before a PR

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
python tools/audit_workspace.py .
cargo deny check
```

## Module contract

A new crate or module is considered done when it has:

1. **A place in a layer**: an entry in `layers.toml`, dependencies only on lower layers.
2. **A typed error**: a `#[non_exhaustive]` enum via `thiserror`, no `unwrap`/`expect`/`panic!`/indexing.
3. **Tests**: unit tests and property tests for invariants (entity reuse, storage moves, query results
   against a naive oracle).
4. **Documentation**: `//!` in `lib.rs` and an example on every public type; `missing_docs` is clean.
5. **A benchmark** (`criterion`), if the module is on a hot path (iteration, spawn/despawn, adding
   and removing components).

## `unsafe`

Forbidden everywhere (`#![forbid(unsafe_code)]`) until a decision record allows it in a specific
crate (DECISIONS #6). Then: the crate goes into `unsafe_allowed` in `layers.toml`, every file gets
a budget in `[unsafe_budget]`, every block a `// SAFETY:` comment, and the crate runs under Miri in CI.

## Dependencies

- Pure Rust only (`deny.toml`). Before adding one, run `cargo tree -e build -i cc` and the audit.
- The version goes only into `[workspace.dependencies]`; in the crate, `{ workspace = true }`.
- Depend on the `pragma_core` facade, not on individual `pragma_*` crates of pragma.core.
- A new dependency or an exception to the rules means a row in `DECISIONS.md`.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/): `feat(storage): ...`, `fix(query): ...`,
`docs: ...`, `ci: ...`, `refactor(world): ...`. A breaking change is `feat(query)!: ...` plus an
entry in `CHANGELOG.md`.
