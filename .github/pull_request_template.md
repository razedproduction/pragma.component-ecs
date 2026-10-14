## What and why

<!-- Briefly: what changes and which roadmap milestone / task it closes. -->

## Checklist

- [ ] `cargo fmt`, `clippy -D warnings`, `cargo test` pass locally
- [ ] `python tools/audit_workspace.py .` is clean
- [ ] New public items are documented, with a doctest example
- [ ] Tests: unit / property tests for invariants
- [ ] No `unsafe` outside crates listed in `unsafe_allowed` (`layers.toml`), every block with `// SAFETY:`
- [ ] A new dependency or exception is recorded in `DECISIONS.md`
- [ ] `CHANGELOG.md` is updated (for notable changes)
