<!-- Parent: ../AGENTS.md -->
# tmux

An async tmux client with two transports: a persistent control-mode connection (`control_mode::Client` over `tmux -C`) and one-shot client invocations (`Server::run` plus typed command builders in `commands/`). The control-mode half is a behavioral port of pandaemonium `pkg/tmux` (Go); the one-shot half has no Go counterpart.

It is a sealed leaf: it depends on no `ganja-*` crate and no member depends on it. It is not `ganja-teammate-local`'s `src/tmux.rs` module, which is unrelated code.

## Boundary

- `depgate.toml` `[rules.tmux]`: `leaf = true`, `sealed = true`, `direct = ["futures", "thiserror", "tokio"]`. CI's `lint` job runs `cargo depgate check --config depgate.toml`. `tempfile` is a dev-dependency and below the `direct` horizon.
- The root `Cargo.toml` has no `[workspace.dependencies]` entry for `tmux`; do not add one.
- `ganja-core` does not re-export it.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | Crate doc: the two transports, which to use, provenance rules; `#![warn(missing_docs)]` |
| `src/control_mode/client.rs` | `Client` (`new`, `exec`, `exec_line`, `exec_raw`, `recv`, `events`, `close`), `EventQueue`, `PendingDropGuard` |
| `src/control_mode/{protocol,commandline,output,notification,flow,options}.rs` | Sans-io parsing, rendering, decoding and validation; each names its Go file on a `Spec:` line |
| `src/server.rs` | `Server` (`current`, `at`, `run`), `Captured`; no `Spec:` line (synthesized) |
| `src/commands/mod.rs` | `invocations!` macro, `Entry`, `Flag`, `REGISTRY`, `EXCLUDED`, `Words` |
| `src/commands/{sessions,panes,buffers_keys,options_misc}.rs` | Builder tables per command family; flag divergences documented in each module doc |
| `src/ids.rs`, `src/error.rs` | `PaneId`/`WindowId`/`SessionId` and the shared `Error`; root files that keep `Spec:` lines |
| `tests/live.rs` | Real-tmux suite, one round trip per family, plus `both_transports_see_one_server` |
| `tests/inventory.rs` | The running tmux's command, abbreviation and flag inventory against `REGISTRY`/`EXCLUDED` |
| `examples/control_mode_session.rs`, `examples/existing_session.rs` | Opt-in demos (env vars below) |

## Commands

```sh
cargo nextest run -p tmux
cargo test -p tmux --test inventory -- --nocapture     # prints the three inventory counts
cargo clippy -p tmux --all-targets -- -D warnings      # promotes missing_docs to an error
RUSTDOCFLAGS='-D warnings' cargo doc -p tmux --no-deps
RUN_REAL_TMUX_TESTS=1 cargo run -p tmux --example control_mode_session
TMUX_RS_SESSION=my-session cargo run -p tmux --example existing_session   # optional: TMUX_RS_SOCKET_PATH or TMUX_RS_SOCKET_NAME, TMUX_RS_CONFIG_FILE
```

## Conventions

- Port behavior, not source. Read the Go file named on a module's `Spec:` line before changing control-mode behavior; the specification is `~/go/src/github.com/zchee/pandaemonium/pkg/tmux`, outside this repository. If it is absent, work from the module docs and ask before changing protocol behavior.
- Provenance follows location: files under `control_mode/` carry a `Spec:` line; root files are synthesized and say so, except `lib.rs`, `ids.rs` and `error.rs`, which keep a `Spec:` line. Do not add a `Spec:` line to `server.rs` or `commands/`.
- `server.rs` and `commands/**` import nothing from `control_mode`; doc links that state the boundary are allowed.
- A new command is one row in its family's `invocations!` block with a doc line per flag; the macro generates the struct, methods, docs, `Invocation` impl and `ENTRIES` row. Do not hand-write builder impls (see `src/commands/mod.rs`).
- A flag the 3.7c floor refuses gets an `ahead_` prefix: it stays in the table as data in `Entry::ahead` and generates no method (see `src/commands/mod.rs`; pinned by `tests/inventory.rs`).
- Argv words are never quoted; values that may start with `-` go through `positional`/`trailing`, which `Words::render` places after `--` (pinned by the `src/commands/*_tests.rs` files).
- Control-mode data uses `Arg::string`; `Arg::raw` is for trusted tmux syntax only (see `src/control_mode/commandline.rs`).
- Policy stays out: identity-checked kills, user-facing refusal text and environment allowlists belong to the consumer. `Error::NotInTmux` is a plain fact.

## Gotchas

- `Client` runs one pending command at a time. If an `exec_raw` future is dropped after its write, `PendingDropGuard` poisons the client; reconnect instead of retrying (pinned by `src/control_mode/client_tests.rs`).
- `EventQueue::push` drops the oldest notification when full and never awaits the consumer; `dropped_notifications` reports the count. Do not replace it with a bounded send.
- `close()` is idempotent: it attempts `detach-client` without blocking on the write lock, drops stdin, waits `shutdown_timeout`, then kills.
- `tests/live.rs` and `tests/inventory.rs` hard-fail when `tmux -V` cannot run. They do not skip and do not read `RUN_REAL_TMUX_TESTS`. Both use a private `-S` socket and an empty `-f` config; `inventory` also removes `TMUX`/`TMUX_PANE`.
- The floor is tmux 3.7c, the Homebrew bottle CI's `install tmux` step pours. The ubuntu apt tmux lacks `new-pane` and some served flags; an older tmux is reported, not failed, for missing commands (`tests/inventory.rs` module doc).
- On tmux next-3.9 the inventory prints `92 installed, 92 typed, 0 awaiting a family`, `78 by a genuine abbreviation, 14 by their full name`, and `526 declared across 92 commands — 524 verified`. A new command on a newer tmux fails the first test until it gets a builder row or an `EXCLUDED` row with a reason.
- Under `cargo nextest run --profile ci`, `package(tmux) and binary(live)` runs with `threads-required = "num-cpus"`, so each of its tests takes every test thread and runs alone (`.config/nextest.toml`).

## Tests

Unit tests are sibling `<module>_tests.rs` files attached with `#[path]` (for example `src/server_tests.rs`, `src/control_mode/client_tests.rs`); the sans-io modules and every builder family are tested without a tmux process.

`client_tests.rs` drives `Client` through scripted `tokio::io::duplex` peers. The two integration binaries, `live` and `inventory`, need tmux on `PATH`; each `//!` header states its prerequisites.

## History

Decisions before 2026-09-23 (phase ledgers, port notes): `docs/decisions/tmux.md`, frozen from commit 35d1720. New decisions are recorded there, not here.
