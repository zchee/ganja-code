<!-- Parent: ../AGENTS.md -->

# crates

The fourteen workspace members. The split is architectural: what each crate may depend on is a fact the compiler and `cargo depgate check` verify, not a rule a reviewer remembers.

## Graph

Arrows point at what a crate depends on. Every edge out of a crate that carries an `internal` rule in `depgate.toml` is asserted by that rule; `ganja-cli` has no rule, and `ganja-tui` and `ganja-serve` carry deny rules only. Each rule's rationale is the comment above it there.

```
ganja-cli ──► ganja-tui ──► ganja-teammate-local ──► ganja-core ──► ganja-provider ──► ganja-tool ──► ganja-permission
    │             │                                      │              │                                  ▲
    │             └──────────────────────────────────────┤              └──► ganja-protocol                │
    ├──► ganja-serve ──► ganja-core                      ├──► ganja-storage ──► ganja-permission, ganja-protocol
    └──► ganja-client ──► ganja-protocol                 └──► ganja-team ──► ganja-protocol
tmux            (sealed leaf: no edge in either direction)
ganja-testkit   (dev-dependency only; no shipped binary links it)
```

| Rule kind | Crate | Rule |
|---|---|---|
| deny | `ganja-core` | no `ratatui*`, no `axum*` |
| deny | `ganja-provider` | no `ratatui*`, `crossterm*`, `arboard*` |
| deny | `ganja-tui` | no `axum*` |
| deny | `ganja-serve` | no `ratatui*`, no `ganja-teammate-local` |
| deny | `ganja-client` | no `axum*` |
| internal | `ganja-core` | exactly permission, protocol, provider, storage, team, tool |
| internal | `ganja-provider` | exactly permission, protocol, tool |
| internal | `ganja-storage` | exactly permission, protocol |
| internal | `ganja-tool` | exactly permission |
| internal | `ganja-team`, `ganja-client` | exactly protocol |
| internal | `ganja-teammate-local` | exactly core plus the six beneath it |
| leaf | `ganja-permission`, `ganja-protocol`, `tmux` | no internal dependency |
| direct | `ganja-protocol` | exactly serde, serde_json, uuid |
| direct | `tmux` | exactly futures, thiserror, tokio |
| sealed | `tmux` | no member consumes it; the member set is resolved when the gate runs |
| manifest | all | every dependency version lives in the root `Cargo.toml` |

## Conventions

- A member manifest declares `foo.workspace = true` and never a version or a path. A feature enabled at the member level (`tokio-util = { workspace = true, features = ["rt"] }`) carries a comment saying why.
- `ganja-core` re-exports the crates beneath it under their old module names (`ganja_core::protocol`, `::permission`, `::project`, `::tool`, `::watch`, `::auth`, `::catalog`, `::storage`, `::snapshot`, `::team`). Code that wants one of them alone depends on it directly, as `ganja-cli` does for `auth login`.
- `ganja_core::provider` is the one facade that is not a bare re-export: the wires live in `ganja-provider`, the half that reads a `Config` (`select`, `Selection`, `selectable`) stays in the engine.
- What a tool needs from its caller arrives as a value in `ToolCtx`; what a wire needs arrives on its `ChatRequest`; where a teams directory is arrives as a `TeamsRoot`. That is how the bottom crates stay ignorant of the engine.
- Teammate backends that need a tmux server, a shell or somebody else's binary live in `ganja-teammate-local`, above the engine; a frontend assembles them and hands them to `Engine::with_teammates`. The `tmux` module inside that crate is not the `tmux` workspace crate.
- Adding an internal edge to a crate that carries an `internal` rule is a deliberate edit to `depgate.toml`; a new member gets a rule of its own. A new member that must not consume `tmux` needs no edit: the sealed rule resolves the member set at gate time.

## Commands

```sh
cargo nextest run -p <crate>                 # any member
cargo depgate check --config depgate.toml    # after touching any Cargo.toml
cargo metadata --no-deps --format-version 1 | jq -r '.packages[].name'   # the member list the gate uses
```

## History

Decisions before 2026-09-23: `docs/decisions/crates.md`, frozen from commit 35d1720. New decisions go in `docs/decisions/ledger.md`, not here.
