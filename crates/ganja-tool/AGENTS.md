<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-05 | Updated: 2026-08-05 -->

# ganja-tool

## Purpose

What the model can do besides talk: `read`, `edit`, `write`, `glob`, `grep`, `bash`, `todowrite`, `webfetch`, `task`, `send_message`, the `Tool` trait they implement, the `Registry` that offers them, and the read-before-write log every one of them answers to. The stale-read watcher lives here too, because what it reports is a state on that log. So does `socket.rs`, which is not a tool at all but the one spelling of what a session socket is (**D505**) — four readers at four heights of the tree need that answer, and this crate is the lowest of them.

Its own crate for one reason, and it is the reason worth stating: **the engine is not in this crate's dependency graph, and never may be.** A tool answers to the rules and to the filesystem, never to the loop that called it. That used to be a convention a reviewer had to keep holding; now it is the compiler's rule, and a `use ganja_core::…` here does not build.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest, `publish = false`. Every entry carries the reason it is there. `libc` is unix-only (`killpg` for the shell tool, `openat` and friends for the anchored writes); the dev-dependencies add `tokio/net` for `webfetch`'s loopback HTTP fixtures. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/` | The tools, the read log, and the watcher (see `src/AGENTS.md`) |
| `tests/` | The few tool behaviours that need process-wide state or the crate from outside (see `tests/AGENTS.md`) |

## For AI Agents

### Working In This Directory

- **The dependency direction is the point.** This crate depends on `ganja-permission` (which calls are gated, and the project root a write is checked for containment against) and on nothing else of ours. If a tool appears to need something from the engine, what it actually needs is a value in `ToolCtx` — that type is a bag of values rather than a handle back to a session precisely so this stays true.
- **The engine still owns *when*, not *how*.** It constructs the watcher through this crate's API and registers `task` into the registry once it knows which agents a session may spawn. Neither decision is reachable from in here, and neither should become so.
- **`ToolCtx` grows by value, never by handle.** A new thing a tool needs from its caller is a path, an id, a token or a trait object the caller implements — the credential-store path and the `Subagents` seam are both shaped that way on purpose.
- **A widened item is a claim about a reader.** `shell::{Progress, run_reporting}` and `task::{DESCRIPTION, ROSTER_HEADER}` are `pub` because the engine's `!` passthrough and its subagent module sit in another crate now. `anchor` stays a private module: it is how `write` and `edit` reach the disk, not something a frontend or a third-party tool has any business addressing files through.

### Testing Requirements

```sh
cargo test -p ganja-tool                      # the in-module suites, which travelled with the files
cargo nextest run --workspace                 # and the engine's, which drive these through a turn
! cargo tree -p ganja-tool -e normal | grep -q ganja-core   # the boundary, asserted
```

The last one is inverted for the same reason the core-purity gate is: a plain `grep -c` exits non-zero on *zero* matches, and would fail exactly when the boundary holds.

### Common Patterns

Every dependency is `x.workspace = true`; versions live in the root manifest with their rationale. Where a feature is enabled at the member level (`tokio-util = { workspace = true, features = ["rt"] }`), the comment says why this crate opts into that module.

## Dependencies

### Internal

`ganja-permission` — the rules a call is judged by, and the worktree it is judged against. Nothing else, and specifically not `ganja-core`.

### External

`schemars` (argument schemas generated from the argument structs), `serde`/`serde_json`, `ignore` + `grep-searcher` + `grep-regex` (glob and grep, in-process on ripgrep's own crates), `similar` (`edit`'s unified diffs), `reqwest` + `htmd` (webfetch: the request, and the markdown rendering of what comes back), `notify` (the stale-read watcher), `etcetera` (where spilled output lands), `tokio` (`process`, `fs`), `tokio-util` (`CancellationToken`), `async-trait`, `futures`, `thiserror`, `tracing`, `libc` (unix only).

<!-- MANUAL: -->
