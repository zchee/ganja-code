<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# ganja-core

## Purpose

The engine: session orchestration, providers, tools, permissions, and the serde-serializable command/event protocol frontends speak. This crate carries **no terminal-backend dependency** — no `ratatui`, no `crossterm` — so the engine stays testable without a terminal and can later be driven over a network transport.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest. Every dependency entry carries the reason it is there — read the comments before adding or removing one. `libc` is unix-only (process-group kill); dev-dependencies enable `tokio/net` for tests that serve real HTTP over loopback. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/` | Engine modules (see `src/AGENTS.md`) |
| `tests/` | Integration suites, several of them one-test-per-binary on purpose (see `tests/AGENTS.md`) |

## For AI Agents

### Working In This Directory

- **The purity rule is the crate's reason to exist.** CI asserts `cargo tree -p ganja-core -e normal` never mentions `ratatui`. Anything the UI must render is expressed as a serde type in `src/protocol.rs`, never as a widget or a style.
- **Everything on the protocol is serde-derived from day one.** That serialization constraint — not a trait — is what preserves the path to `ganja serve` (P7) and to persisted transcripts (P4). A new field or variant should be considered wire-visible even though nothing serves it yet.
- The public surface is re-exported from `src/lib.rs`; a new module that frontends use should re-export its types there alongside the rest.

### Testing Requirements

```sh
cargo nextest run -p ganja-core               # unit + integration, each test its own process
cargo nextest run -p ganja-core permission    # tests whose name matches "permission"
cargo nextest run -E 'binary(golden)'         # one integration binary
cargo test -p ganja-core --doc                # doctests, which nextest does not run
```

Unit tests live in `#[cfg(test)] mod tests` at the bottom of the module they cover; anything that needs a real socket, a real filesystem layout, or process-wide environment mutation lives in `tests/` instead.

### Common Patterns

- Errors are `thiserror` enums whose messages say what the caller can do about it; the taxonomies are deliberately transport-agnostic (`ProviderError` has to fit both an in-process provider and an HTTP one).
- Trait objects only where dyn-safety demands it: `Provider` and `Tool` live behind `Arc<dyn _>`, which is why `async-trait` is a dependency.
- Shared mutable state is `Arc<std::sync::Mutex<_>>` for short critical sections and `tokio::sync::Mutex` only where a lock is held across an `.await`. No lock is ever held across an await in a path the render loop depends on.

## Dependencies

### Internal

None — this crate is the bottom of the dependency graph. `ganja-tui` and `ganja-cli` depend on it.

### External

`tokio` + `tokio-util` (runtime, `CancellationToken`), `reqwest`/rustls (provider HTTP), `futures` (stream combinators), `secrecy` (key material), `serde`/`serde_json`, `schemars` (tool schemas), `ignore`/`grep-searcher`/`grep-regex` (in-process glob and grep), `similar` (unified diffs), `etcetera` (XDG paths), `thiserror`, `tracing`, `url` (host comparison for endpoint checks), `async-trait`, `libc` (unix only).

<!-- MANUAL: -->
