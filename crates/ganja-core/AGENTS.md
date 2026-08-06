<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# ganja-core

## Purpose

The engine: session orchestration, the agent loop, and the state a conversation leaves behind. This crate carries **no terminal-backend dependency** — no `ratatui`, no `crossterm` — so the engine stays testable without a terminal and can later be driven over a network transport.

Four things it is built on are crates of their own — `ganja-protocol`, `ganja-permission`, `ganja-tool`, `ganja-provider` — and this crate re-exports each under the module name it always had, so `ganja_core::protocol`, `ganja_core::permission`, `ganja_core::project`, `ganja_core::tool`, `ganja_core::watch`, `ganja_core::auth` and `ganja_core::catalog` all keep resolving. The crate root names the engine's own types and nothing else; a caller that only wants one of the four should depend on it directly — the facade's module names are for the callers that want the engine.

`ganja_core::provider` is the one facade that is not a bare re-export. The wires left; the half that reads a `Config` — which provider a session runs as, which model it asks for — stayed, over a glob of the crate that took them. `src/AGENTS.md` says which functions are on which side.

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
- **The four crates beneath this one may not name it.** Each is asserted as a closed allowlist of its own internal dependencies rather than a `! grep ganja-core`, which would go quiet the day a fifth crate appeared: `ganja-tool` is exactly `ganja-permission`, `ganja-provider` is exactly `ganja-permission ganja-protocol ganja-tool`, and this crate is exactly those four. A tool that appears to need something from the engine needs a value in its `ToolCtx` instead; a wire that appears to need one needs another field on its `ChatRequest`.
- **Everything on the protocol is serde-derived from day one.** That serialization constraint — not a trait — is what preserves the path to serving the engine and to persisted transcripts. A new field or variant should be considered wire-visible even though nothing serves it yet.
- The crate root's flat re-exports in `src/lib.rs` name the engine's own types only; a new core module that frontends use re-exports its types there alongside the rest, while a type that belongs to `ganja-protocol`, `ganja-permission`, `ganja-tool` or `ganja-provider` is reached through those module names, never flattened into the root. A frontend that only renders must be able to build against `ganja-protocol` alone, and a root that flattens protocol types into the engine's vocabulary invites the opposite.

### Testing Requirements

```sh
cargo nextest run -p ganja-core               # unit + integration, each test its own process
cargo nextest run --workspace permission      # tests whose name matches "permission", wherever they live
cargo nextest run -E 'binary(golden)'         # one integration binary
cargo test -p ganja-core --doc                # doctests, which nextest does not run
```

Unit tests live in `#[cfg(test)] mod tests` at the bottom of the module they cover; anything that needs a real socket, a real filesystem layout, or process-wide environment mutation lives in `tests/` instead.

### Common Patterns

- Errors are `thiserror` enums whose messages say what the caller can do about it; the taxonomies are deliberately transport-agnostic (`SelectionError` wraps `ganja-provider`'s `ProviderError` across the crate boundary, which is what `#[error(transparent)]` is for).
- Trait objects only where dyn-safety demands it: `Tool`, the subagent seam and (in `ganja-provider`) `Provider` live behind `Arc<dyn _>`, which is why `async-trait` is a dependency.
- Shared mutable state is `Arc<std::sync::Mutex<_>>` for short critical sections and `tokio::sync::Mutex` only where a lock is held across an `.await`. No lock is ever held across an await in a path the render loop depends on.

## Dependencies

### Internal

`ganja-protocol`, `ganja-permission`, `ganja-tool` and `ganja-provider`, re-exported as modules from `src/lib.rs`. `ganja-tui` and `ganja-cli` depend on this crate, and additionally on the protocol, the permission crate, (for the frontend's `@` file menu) the tool crate and (for `auth login`) the provider crate, where they name those types directly.

### External

`tokio` + `tokio-util` (runtime, `CancellationToken`), `reqwest`/rustls (the MCP client's HTTP transport), `futures` (stream combinators), `serde`/`serde_json`, `schemars` (the two schemas the engine builds rather than derives), `ignore` (the `AGENTS.md` walk, under the same ignore rules the tools search by), `etcetera` (XDG paths), `rusqlite` (the session store), `rmcp` (MCP client transports), `lsp-types` (the LSP wire types), `jsonc-parser` (config files in upstream's dialect), `base64`, `thiserror`, `tracing`, `url` (host comparison for endpoint checks), `async-trait`, `libc` (unix only). `similar`, `notify`, `htmd` and the ripgrep search crates left with the tools that used them; `secrecy`, `sha2` and `getrandom` left with the wires and the logins, and `secrecy` remains only as a dev-dependency of the credential tests.

<!-- MANUAL: -->
