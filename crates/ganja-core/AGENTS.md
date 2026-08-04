<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# ganja-core

## Purpose

The engine: session orchestration, providers, and the agent loop that drives them. This crate carries **no terminal-backend dependency** — no `ratatui`, no `crossterm` — so the engine stays testable without a terminal and can later be driven over a network transport.

Three things it is built on are crates of their own — `ganja-protocol`, `ganja-permission`, `ganja-tool` — and this crate re-exports each under the module name it always had, so `ganja_core::protocol`, `ganja_core::permission`, `ganja_core::project`, `ganja_core::tool` and `ganja_core::watch` all keep resolving. A caller that only wants one of the three should depend on it directly; the facade is for the callers that want the engine.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest. Every dependency entry carries the reason it is there — read the comments before adding or removing one. `libc` is unix-only (signalling a spawned server's process group); dev-dependencies enable `tokio/net` for tests that serve real HTTP over loopback. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/` | Engine modules (see `src/AGENTS.md`) |
| `tests/` | Integration suites, several of them one-test-per-binary on purpose (see `tests/AGENTS.md`) |

## For AI Agents

### Working In This Directory

- **The purity rule is the crate's reason to exist.** CI asserts `cargo tree -p ganja-core -e normal` never mentions `ratatui`. Anything the UI must render is expressed as a serde type in `ganja-protocol`, never as a widget or a style.
- **The three crates beneath this one may not name it.** `ganja-tool` in particular is asserted the same inverted way: `! cargo tree -p ganja-tool -e normal | grep -q ganja-core`. A tool that appears to need something from the engine needs a value in its `ToolCtx` instead.
- **Everything on the protocol is serde-derived from day one.** That serialization constraint — not a trait — is what preserves the path to serving the engine and to persisted transcripts. A new field or variant should be considered wire-visible even though nothing serves it yet.
- The public surface is re-exported from `src/lib.rs`; a new module that frontends use should re-export its types there alongside the rest.

### Testing Requirements

```sh
cargo nextest run -p ganja-core               # unit + integration, each test its own process
cargo nextest run --workspace permission      # tests whose name matches "permission", wherever they live
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

`ganja-protocol`, `ganja-permission` and `ganja-tool`, all re-exported from `src/lib.rs`. `ganja-tui` and `ganja-cli` depend on this crate, and additionally on the protocol (and, for the frontend's `@` file menu, on the tool crate) where they name those types directly.

### External

`tokio` + `tokio-util` (runtime, `CancellationToken`), `reqwest`/rustls (provider HTTP), `futures` (stream combinators), `secrecy` (key material), `serde`/`serde_json`, `schemars` (the two schemas the engine builds rather than derives), `ignore` (the `AGENTS.md` walk, under the same ignore rules the tools search by), `etcetera` (XDG paths), `rmcp` (MCP client transports), `lsp-types` (the LSP wire types), `jsonc-parser` (config files in upstream's dialect), `thiserror`, `tracing`, `url` (host comparison for endpoint checks), `async-trait`, `libc` (unix only). `similar`, `notify`, `htmd` and the ripgrep search crates left with the tools that used them.

<!-- MANUAL: -->
