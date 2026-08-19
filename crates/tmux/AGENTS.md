<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-19 | Updated: 2026-08-19 -->

# tmux

## Purpose

A tmux control-mode client, and this repository's second application of the standing porting rule: `pandaemonium pkg/tmux`, an Apache-2.0 Go package by the same author as this workspace, is the behavioral specification, never source to translate. The specification lives at `~/go/src/github.com/zchee/pandaemonium/pkg/tmux`; every Rust module doc names the Go file it ports as `Spec: pandaemonium pkg/tmux/<file>.go`. One persistent `tmux -C` subprocess communicates over piped standard I/O, with guarded `%begin`/`%end`/`%error` command responses and typed asynchronous `%` notifications.

**Why this is a crate.** The P26 user directive of 2026-08-18 seals it outside the ganja dependency graph in both directions: no `ganja-*` crate may depend on `tmux`, and `tmux` may depend on no `ganja-*` crate. CI's `lint` job asserts that boundary with the steps `tmux consumes no ganja crate` and `no workspace member consumes tmux`; the latter derives the member list from `cargo metadata --no-deps`, so a new member is checked automatically rather than omitted by a stale list. The same job's `tmux's direct dependencies are exactly futures, thiserror and tokio` step pins the normal depth-1 external set to `futures thiserror tokio `. There is deliberately no `tmux` entry in the root `[workspace.dependencies]`: that entry would be the opt-in handle another member used to consume this crate, and the absence is the point.

**The split inside the crate is sans-io beneath async.** `protocol`, `commandline`, `output`, `notification`, `flow`, and `options` keep parsing, rendering, decoding, typing, and validation process-free and synchronously testable. `Client` is the async shell over `tokio::process`: it owns the child and its pipes, serializes execution onto one pending command, delivers asynchronous notifications without making the stdout reader wait for a slow consumer, and owns shutdown.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest, `publish = false`. Exactly three normal dependencies (`futures`, `thiserror`, `tokio`) and one dev-only dependency (`tempfile`), each inherited from the workspace manifest. The sealed-leaf prohibition is stated above the dependency table. |
| `src/lib.rs` | Crate doc, the P26 sealed-leaf statement, the hard-fail integration-test divergence, module declarations, and headline re-exports. `#![warn(missing_docs)]` makes AC-10 mechanical when CI promotes warnings with `-D warnings`. No logic. |
| `src/protocol.rs` | The synchronous sans-io `Parser`: guarded response blocks, marker identity `(time, command)`, the first-matching-terminator rule and its adversarial-mimic caveat, plus response/notification `Event`s. |
| `src/commandline.rs` | `Command`, `Arg`, `CommandLine`, and the bare/empty/single-quoted/double-quoted rendering ladder. Raw fragments are explicit; embedded newlines are refused. |
| `src/output.rs` | Byte-preserving decoding for tmux's `\NNN` octal escapes, with strict UTF-8 and lossy text helpers used by typed output notifications. |
| `src/notification.rs` | The twenty known notification kinds, the client-synthesized `%protocol-error`, an `Other` catch-all that preserves a newer tmux's unknown token, and typed accessors for output, extended output, subscriptions, exit, pause, continue, and messages. |
| `src/flow.rs` | Validated pane/window/session id newtypes, client flags and subscription targets, the four command constants, and the `refresh-client` helpers on `Client`. |
| `src/options.rs` | The consuming `Options::with_*` builder, its validation matrix, launch arguments, initial command line, and the defaults that bound event buffering, stderr retention, and shutdown. |
| `src/error.rs` | The typed `thiserror` surface: command, protocol, exit, I/O, option, spawn, startup, close, and rendering/cancellation boundary failures. |
| `src/client.rs` | The persistent `tokio::process` client, single pending slot, drop-poisoning guard, drop-oldest notification queue and counter, bounded stderr ring, scripted duplex test constructor, reader/stderr tasks, and close/reap path. |
| `tests/live.rs` | Default-suite integration tests against one private `-S` socket, empty `-f` config, and unique session per test. Hard-fails when `tmux` is unavailable and never touches the invoking user's default server. |
| `examples/control_mode_session.rs` | Port of Go's isolated-server example. Retains the `RUN_REAL_TMUX_TESTS=1` opt-in because running an example is an intentional real-system action. |
| `examples/existing_session.rs` | Read-only attachment example. Requires `TMUX_RS_SESSION`; accepts optional mutually exclusive `TMUX_RS_SOCKET_PATH`/`TMUX_RS_SOCKET_NAME` and optional `TMUX_RS_CONFIG_FILE`. |

The shipped source map is:

| Go file (spec) | Rust module | Notes |
|---|---|---|
| `doc.go`, `README.md` | `src/lib.rs` | Crate behavior, sealed-leaf posture, re-exports, and the `no_run` usage example. |
| `errors.go` | `src/error.rs` | Go's error shapes become one non-exhaustive `Error` enum plus structured command and protocol errors. |
| `protocol.go` | `src/protocol.rs` | Sans-io `Parser::feed`/`close`, guarded response blocks, marker identity, and response/notification events. |
| `commandline.go` | `src/commandline.rs` | `Command`, explicit string/raw `Arg`s, `CommandLine`, validation, and the original quoting ladder. Invalid UTF-8 vanishes into Rust's `str` type. |
| `output.go` | `src/output.rs` | Octal escape decoding to bytes, partial decoding, and strict/lossy text conversion. |
| `notification.go` | `src/notification.rs` | Raw notifications, all twenty known kinds, `%protocol-error`, forward-compatible unknown kinds, typed accessors, and session-scope `-` sentinels. Its three id validators live with the newtypes in `flow.rs`. |
| `flow.go` | `src/flow.rs` | Validated id/flag/target values and `refresh-client` methods, including subscriptions and pane flow control. |
| `options.go` | `src/options.rs` | Builder, validation, launch arguments, initial command, typed environment pairs, and bounded-resource defaults. |
| `transport.go` | `src/client.rs` | Folded into the client rather than exposed as a public trait; scripted tests inject `tokio::io::duplex`, while `Parser` remains the public external-transport surface. |
| `client.go`, `process_unix.go`, `process_other.go` | `src/client.rs` | Persistent child, serialized execution, cancellation poisoning, event/stderr queues, close, bounded wait/kill, and platform-specific expected-kill recognition. |
| `integration_test.go` | `tests/live.rs` | Real-tmux behavior over private sockets/configs; unlike Go, the Rust suite has no opt-in skip. |
| `examples/01_control_mode_session/main.go` | `examples/control_mode_session.rs` | Isolated private server, still behind `RUN_REAL_TMUX_TESTS=1`. |
| `examples/02_existing_session/main.go` | `examples/existing_session.rs` | Existing-session inspection with the documented `TMUX_RS_*` environment names. |

## For AI Agents

### Working In This Directory

- **Port behavior, never source.** Read the matching Go file before changing a behavior, then write idiomatic Rust and keep the module-level `Spec:` citation. A deliberate departure is a prose `Divergence:` callout with the Go behavior and the reason; this crate does not use the ganja crates' `D<n>` decision-ledger numbering.
- **The sealed leaf is a hard boundary.** `tmux consumes no ganja crate` is the inverted `cargo tree -p tmux -e normal` grep; `no workspace member consumes tmux` loops over names from `cargo metadata --no-deps` and refuses an exact `tmux` node in every other normal tree; the depth-1 allowlist is exactly `futures thiserror tokio `. Do not add an internal edge, a root `tmux` workspace-dependency handle, or a fourth normal external without an explicit user directive superseding P26 and a deliberate CI change.
- **Keep the sans-io half process-free.** `Parser::feed`, command-line rendering, output decoding, typed notification parsing, and id/target validation must remain directly testable without a subprocess or runtime. Transport ownership and asynchronous coordination belong in `client.rs`.
- **One pending command means no pipelining.** `Client::exec_raw` holds the async write lock across write and response wait. If its future is dropped after the write has been attempted and its own pending registration still exists, `PendingDropGuard` poisons the client: a late response cannot safely belong to a future command. A caller reconnects rather than retries on that client.
- **The reader never awaits notification capacity.** `EventQueue::push` synchronously drops the oldest buffered notification when full and increments the approximate dropped counter; `recv` and `events` drain what remains. Do not replace this with a bounded send that can suspend the stdout reader behind a slow consumer.
- **Shutdown is owned here.** `close()` is idempotent, aborts pending work, attempts `detach-client` only through a non-blocking write-lock acquisition, drops stdin, waits for the child within `shutdown_timeout`, then kills and re-waits on expiry. Dropping an unclosed real client is also required to reap its `kill_on_drop` child.
- **Keep public documentation complete.** The crate-level `warn(missing_docs)` is intentionally only a warning in source; CI's `cargo clippy ... -- -D warnings` promotes it to the hard AC-10 gate, matching the workspace convention against source-level `deny(warnings)`.

### Testing Requirements

Unit tests live beside the code in `#[cfg(test)] mod tests`. The process-free suites cover `protocol`, `commandline`, `output`, `notification`, `flow`, and `options`; `client.rs` uses scripted `tokio::io::duplex` peers to pin serialization, both drop-poisoning edges, command errors, drop-oldest delivery, `%exit`, protocol-error synthesis, close idempotency, detach lock contention, and blocked-write shutdown.

`tests/live.rs` is the second tier. Every test creates a tempdir, a scratch `-S` socket, an empty `-f` config, and a unique session, so it never reaches the invoking user's default tmux server. It hard-fails with a named requirement when `tmux -V` cannot run. Go's `integration_test.go` instead requires `RUN_REAL_TMUX_TESTS=1` and skips without it; the Rust divergence is deliberate because a green default run that exercised no real protocol would be false evidence.

That default real-tmux cost has already found two specification gaps:

- tmux's command lexer misparses a bare `%<digits>:word` token although a bare `%<digits>` pane id is valid, so `pane_flow` single-quotes the compound `pane:state` argument that Go renders bare.
- A session-scoped `refresh-client -C` subscription reports `-` in the window, window-index, and pane fields — captured against tmux next-3.8 as `%subscription-changed live-test $0 - - - : x`. Go validates all three unconditionally because its real-tmux suite never exercises that scope; this port represents each sentinel as `Option::None`.

The examples are the third tier and keep intentional-run controls. `control_mode_session` does no real work without `RUN_REAL_TMUX_TESTS=1`. `existing_session` requires `TMUX_RS_SESSION`, accepts optional mutually exclusive `TMUX_RS_SOCKET_PATH` and `TMUX_RS_SOCKET_NAME`, and accepts optional `TMUX_RS_CONFIG_FILE`.

The current cargo-fmt exposes `-p`/`--package`, so formatting can be scoped honestly:

```sh
env -u RUSTFLAGS cargo --config ~/.config/rust/config.dev.toml fmt -p tmux --check
env -u RUSTFLAGS cargo --config ~/.config/rust/config.dev.toml clippy -p tmux --all-targets -- -D warnings
env -u RUSTFLAGS RUSTDOCFLAGS="-D warnings" cargo --config ~/.config/rust/config.dev.toml doc -p tmux --no-deps
env -u RUSTFLAGS cargo --config ~/.config/rust/config.dev.toml nextest run -p tmux
RUN_REAL_TMUX_TESTS=1 cargo run -p tmux --example control_mode_session
TMUX_RS_SESSION=my-session cargo run -p tmux --example existing_session
```

### Common Patterns

- Validity moves into types: `PaneId`, `WindowId`, and `SessionId` parse once at construction, while a not-applicable wire value is `Option::None`, never a string sentinel after decoding.
- Normal command data uses `Arg::string`; `Arg::raw` is reserved for trusted tmux syntax such as flags, fixed words, and deliberately pre-quoted fragments.
- Test names are sentences about behavior: `concurrent_execs_are_serialized_onto_one_pending_slot`, `notifications_beyond_the_buffer_drop_the_oldest_and_count`.
- Module docs state the Go specification first and explain divergences where they occur. Comments explain the protocol or correctness boundary, not the line immediately below them.

## Dependencies

### Internal

None, in either dependencies table. That is the P26 invariant: this crate consumes no `ganja-*` crate and no workspace member consumes it. The missing root `[workspace.dependencies]` handle is part of the same boundary rather than an omission to repair.

### External

`thiserror` (typed, message-carrying errors across rendering, decoding, and protocol surfaces — the same choice as the rest of the workspace), `tokio` (the `tmux -C` subprocess, piped async I/O, and the reader/stderr tasks), and `futures` (the `Stream` adapter over the drop-oldest event queue). `tempfile` is a dev-dependency only, used by tests for private filesystem/socket state; it is below the normal dependency gate's horizon.

<!-- MANUAL: -->
