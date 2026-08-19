<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-19 | Updated: 2026-08-19 -->

# tmux

## Purpose

A Rust client for tmux over **both** of the transports tmux answers on, and this repository's second application of the standing porting rule: `pandaemonium pkg/tmux`, an Apache-2.0 Go package by the same author as this workspace, is the behavioral specification, never source to translate. The specification lives at `~/go/src/github.com/zchee/pandaemonium/pkg/tmux`.

- **Control mode** — `control_mode/`, the port. One persistent `tmux -C` subprocess spoken to over piped standard I/O, with guarded `%begin`/`%end`/`%error` command responses and typed asynchronous `%` notifications. This is the half the Go package specifies, and every file under that directory names the Go file it ports on its own `Spec:` line.
- **Client invocations** — the crate root, synthesized. One `tmux <command>` run to completion per call, owning nothing between calls: the transport every shell script already speaks. The Go package spells no such surface, so `server.rs` and `commands/` cite no Go file and say why in their first paragraph instead.

**Directory is provenance, with one honest exception.** The rule the layout states is that everything under `control_mode/` is ported and everything at the root is not — but `ids.rs` and `error.rs` are at the root *and* carry `Spec:` lines, because their contents were hoisted **out of** the port rather than invented: a `%0` in an `%output` notification is the same `%0` a `list-panes` prints, and one `Error` enum has to span both transports or every caller holding both would join two. So the working rule is: **provenance is stated at the smallest scope where it is true** — by directory under `control_mode/`, by module for the one-shot surface, and by item where the two meet (`Error`'s last three variants each say `Synthesized, with no Go counterpart`). Anything looser would be a sentence somebody has to remember an exception to.

**The split inside the control-mode half is sans-io beneath async.** `protocol`, `commandline`, `output`, `notification`, `flow`, and `options` keep parsing, rendering, decoding, typing, and validation process-free and synchronously testable. `Client` is the async shell over `tokio::process`: it owns the child and its pipes, serializes execution onto one pending command, delivers asynchronous notifications without making the stdout reader wait for a slow consumer, and owns shutdown.

**The split inside the one-shot half is words beneath a process.** `Server` owns the socket pin and the subprocess; a builder in `commands/` owns nothing at all and only answers *which argv words does this command want*. That is why the same words can be sent to any server, why every builder is testable without tmux running, and why nothing in `commands/` had to grow a second copy of `Server`'s addressing.

## The sealed leaf (P26)

The P26 user directive of 2026-08-18 seals this crate outside the ganja dependency graph in both directions: no `ganja-*` crate may depend on `tmux`, and `tmux` may depend on no `ganja-*` crate. CI's `lint` job asserts that boundary with the steps `tmux consumes no ganja crate` and `no workspace member consumes tmux`; the latter derives the member list from `cargo metadata --no-deps`, so a new member is checked automatically rather than omitted by a stale list. The same job's `tmux's direct dependencies are exactly futures, thiserror and tokio` step pins the normal depth-1 external set to `futures thiserror tokio `. There is deliberately no `tmux` entry in the root `[workspace.dependencies]`: that entry would be the opt-in handle another member used to consume this crate, and the absence is the point.

None of the three steps changed when the one-shot surface landed, which was the point of building it crate-internally: 92 typed commands and a second transport cost the boundary nothing, because none of them needed anything from outside.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest, `publish = false`. Exactly three normal dependencies (`futures`, `thiserror`, `tokio`) and one dev-only dependency (`tempfile`), each inherited from the workspace manifest. The sealed-leaf prohibition is stated above the dependency table. |
| `src/lib.rs` | Crate doc: the two transports, which one a caller wants, what they share, and the words-on-a-line/words-in-an-argv distinction, with a `no_run` example per transport. Also the P26 sealed-leaf statement, the hard-fail integration-test divergence, module declarations, and the headline re-exports. `#![warn(missing_docs)]` makes AC-10 mechanical when CI promotes warnings with `-D warnings`. No logic. |
| **Shared vocabulary** | *Hoisted out of the port; both transports speak it.* |
| `src/ids.rs` | Validated `PaneId`/`WindowId`/`SessionId` newtypes and their `InvalidId`. Parse-don't-validate, against Go's validate-at-each-call-site; a not-applicable wire value is `Option::None`, never a sentinel string. |
| `src/error.rs` | One `#[non_exhaustive]` `Error` spanning both transports, plus the structured `CommandError`/`ProtocolError` leaves. `Clone` (so a closed client can hand out its abort cause repeatedly), which is why the `io::Error`s are `Arc`-wrapped. Its last three variants — `NotInTmux`, `ClientStart`, `ClientRefused` — are the one-shot surface's and have no Go counterpart. |
| **One-shot surface** | *Synthesized; no `Spec:` lines.* |
| `src/server.rs` | `Server`: the socket and asking pane read off `$TMUX`/`$TMUX_PANE` the way the tmux client reads them (`current()`), or named outright (`at()`); `run()`, which pins `-S`, closes stdin, sets `kill_on_drop`, and passes `OsString` argv through byte for byte; and `Captured`, the stdout bytes with strict and lossy text views over them. |
| `src/commands/mod.rs` | The declaration mechanism and the register. The `invocations!` macro turns one table entry — Rust name, tmux name, abbreviation, doc, and one line per flag naming its method and argument kind — into a builder, its docs, its `Invocation` impl and its `ENTRIES` row. `Words` is the shared accumulator holding the three set-semantics (`switch`/`value`/`repeat`) and the `--` fence. `REGISTRY` and `EXCLUDED` are what the inventory test diffs the running tmux against. |
| `src/commands/panes.rs` | F1, **34** commands: tmux(1)'s own `WINDOWS AND PANES` section minus the five interactive modes, which `options_misc` holds. Panes, then windows, then layouts. |
| `src/commands/sessions.rs` | F2, **19** commands: `CLIENTS AND SESSIONS`, minus `source-file` (an options relative) and plus `lock-server` (the manual defines the other two locks in terms of it). Sessions, then clients, then the server. |
| `src/commands/buffers_keys.rs` | F3, **21** commands: `BUFFERS` and `KEY BINDINGS` plus the prompt-and-display commands, joined by what they have in common — every one carries *a caller's own text* into tmux or back out, which is the job the `--` fence and the `OsString` argument type exist for. |
| `src/commands/options_misc.rs` | F4, **18** commands: the options and their two window-scoped synonyms, the hooks, the session environment, the five interactive modes `panes.rs` sends here, `source-file` (from `CLIENTS AND SESSIONS`, filed beside the options it reads), and the four tmux itself files under `MISCELLANEOUS`. The residue family, defined by what the other three are not. |
| **Control mode** | *The port; each file carries its `Spec:` line.* |
| `src/control_mode/mod.rs` | The module doc that states the provenance convention from the ported side, the private submodule declarations, and the flat `pub use` re-exports that let a type be named `tmux::control_mode::Client`. |
| `src/control_mode/protocol.rs` | The synchronous sans-io `Parser`: guarded response blocks, marker identity `(time, command)`, the first-matching-terminator rule and its adversarial-mimic caveat, plus response/notification `Event`s. |
| `src/control_mode/commandline.rs` | `Command`, `Arg`, `CommandLine`, and the bare/empty/single-quoted/double-quoted rendering ladder. Raw fragments are explicit; embedded newlines are refused. Quarantined here: the root layer never renders a line. |
| `src/control_mode/output.rs` | Byte-preserving decoding for tmux's `\NNN` octal escapes, with strict UTF-8 and lossy text helpers used by typed output notifications. |
| `src/control_mode/notification.rs` | The twenty known notification kinds, the client-synthesized `%protocol-error`, an `Other` catch-all that preserves a newer tmux's unknown token, and typed accessors for output, extended output, subscriptions, exit, pause, continue, and messages. |
| `src/control_mode/flow.rs` | Client flags, subscription targets, the four command constants, and the `refresh-client` helper methods on `Client`. Its module doc records that the id newtypes it once held now live in `src/ids.rs`. |
| `src/control_mode/options.rs` | The consuming `Options::with_*` builder, its validation matrix, launch arguments, initial command line, and the defaults that bound event buffering, stderr retention, and shutdown. |
| `src/control_mode/client.rs` | The persistent `tokio::process` client, single pending slot, drop-poisoning guard, drop-oldest notification queue and counter, bounded stderr ring, scripted duplex test constructor, reader/stderr tasks, and close/reap path. |
| **Tests and examples** | |
| `tests/inventory.rs` | The coverage gate, in two halves: what the running tmux *prints* against `REGISTRY ∪ EXCLUDED`, and what it *accepts*. Needs a `tmux` on `PATH` and hard-fails without one, but starts no server — `list-commands` is answered by the client alone. |
| `tests/live.rs` | Default-suite integration tests against one private `-S` socket, empty `-f` config, and unique session per test — for both transports, and for both at once in `both_transports_see_one_server`. Hard-fails when `tmux` is unavailable and never touches the invoking user's default server. |
| `examples/control_mode_session.rs` | Port of Go's isolated-server example. Retains the `RUN_REAL_TMUX_TESTS=1` opt-in because running an example is an intentional real-system action. |
| `examples/existing_session.rs` | Read-only attachment example. Requires `TMUX_RS_SESSION`; accepts optional mutually exclusive `TMUX_RS_SOCKET_PATH`/`TMUX_RS_SOCKET_NAME` and optional `TMUX_RS_CONFIG_FILE`. |

The shipped source map is:

| Go file (spec) | Rust module | Notes |
|---|---|---|
| `doc.go`, `README.md` | `src/lib.rs` | Crate behavior, sealed-leaf posture, re-exports, and one `no_run` example per transport. The crate doc's second half is synthesized, because the Go package documents one transport and this crate has two. |
| `errors.go` | `src/error.rs` | Go's error shapes become one non-exhaustive `Error` enum plus structured command and protocol errors. Hoisted to the root: the one-shot surface adds three variants Go has no counterpart for. |
| `protocol.go` | `src/control_mode/protocol.rs` | Sans-io `Parser::feed`/`close`, guarded response blocks, marker identity, and response/notification events. |
| `commandline.go` | `src/control_mode/commandline.rs` | `Command`, explicit string/raw `Arg`s, `CommandLine`, validation, and the original quoting ladder. Invalid UTF-8 vanishes into Rust's `str` type. |
| `output.go` | `src/control_mode/output.rs` | Octal escape decoding to bytes, partial decoding, and strict/lossy text conversion. |
| `notification.go` | `src/control_mode/notification.rs`, `src/ids.rs` | Raw notifications, all twenty known kinds, `%protocol-error`, forward-compatible unknown kinds, typed accessors, and session-scope `-` sentinels stay in `control_mode/`; Go's three id validators live at the root with the newtypes they guard, because an id is not a control-mode concept. |
| `flow.go` | `src/control_mode/flow.rs`, `src/ids.rs` | The `refresh-client` methods, client flags, subscription targets and command constants stay in `control_mode/`; Go spells the pane-id validator here and this port keeps it with the other two in `ids.rs`. |
| `options.go` | `src/control_mode/options.rs` | Builder, validation, launch arguments, initial command, environment pairs, and bounded-resource defaults. All of it is `-C` launch configuration, so none of it moved: a `Server` configures nothing, having nothing that outlives a call. |
| `transport.go` | `src/control_mode/client.rs` | Folded into the client rather than exposed as a public trait; scripted tests inject `tokio::io::duplex`, while `Parser` remains the public external-transport surface. |
| `client.go`, `process_unix.go`, `process_other.go` | `src/control_mode/client.rs` | Persistent child, serialized execution, cancellation poisoning, event/stderr queues, close, bounded wait/kill, and platform-specific expected-kill recognition. |
| *(none)* | `src/server.rs`, `src/commands/**` | The one-shot transport and its 92 typed commands. No Go counterpart at all: the specification speaks only the persistent protocol. |
| `integration_test.go` | `tests/live.rs` | Real-tmux behavior over private sockets/configs; unlike Go, the Rust suite has no opt-in skip. Its one-shot and dual-transport tests have no Go counterpart and say so. |
| *(none)* | `tests/inventory.rs` | The coverage gate. Nothing in Go enumerates tmux's command set, because nothing in Go types it. |
| `examples/01_control_mode_session/main.go` | `examples/control_mode_session.rs` | Isolated private server, still behind `RUN_REAL_TMUX_TESTS=1`. |
| `examples/02_existing_session/main.go` | `examples/existing_session.rs` | Existing-session inspection with the documented `TMUX_RS_*` environment names. |

## The flag convention

A builder's methods are tmux's flags, and the question every one of them answers is *does this flag exist, and what does it take*. Four rules settle it, in order:

1. **The binary's own usage string is the first source.** `tmux list-commands <name>` prints the synopsis that tmux itself was compiled with, which is the closest thing to an authoritative list.
2. **The manual shipped beside that binary is the cross-check.** The usage strings are hand-written sentences in tmux's source and go stale in both directions — `list-clients` omits a `-r` the manual documents and the binary accepts; `server-access` prints a `-t` the manual has never had and the binary refuses with `unknown flag -t`.
3. **The parser arbitrates.** When the two documents disagree, the deciding question is what the binary *accepts*, probed against a private socket. A flag is a method when the parser takes it **and** at least one of the two documents names it.
4. **A parser-only letter is left to the escape hatch.** `customize-mode -y` and `run-shell -s` are accepted by the parser and named by neither document, so there is nothing to write a doc line from — and a method whose documentation would be a guess is worse than no method. `Server::run` carries them today, as it carries everything.

**Every divergence is documented where the flag lives**, not in a central ledger: six in `options_misc.rs`'s module doc (the modes, where the usage strings and the manual disagree most), three in `buffers_keys.rs` (`load-buffer -w`, `list-buffers -r`, `choose-buffer -y` — parser-accepted and manual-documented while the usage string omits them), two in `sessions.rs` (the `-r`/`-t` pair above). A reader who wants to know why a method exists or does not finds the answer in the file that would have held it.

Three typing rules follow from the same posture of not being stricter than the program being addressed:

- **A size is a word, not a number.** tmux spells `-x 10`, `-x 10%` and `-S -` in one argument position, so narrowing to an integer would refuse values tmux accepts.
- **A flag whose argument is tmux's own language takes `&str`** (a format, a filter, a sort order, a style); everything coming from the caller's world takes `impl Into<OsString>`, because a path is not obliged to be UTF-8.
- **A required positional is an ordinary method.** Omitting `rename-window`'s new name builds fine and fails at tmux, which answers in its own words through `Error::ClientRefused`. This crate ships the material and leaves the judgment — including "you forgot something" — to tmux.

The baseline is tmux next-3.8. An older tmux refuses a flag it does not know, in its own words; a newer one grows commands, and the inventory test names them.

## The inventory contract

Coverage is measured, not claimed. `tests/inventory.rs` asks the tmux on this machine two different questions:

1. **Everything installed is accounted for.** Every command in `tmux list-commands` must appear in `REGISTRY` or in `EXCLUDED`, with the abbreviation spelled the way tmux spells it. A command in neither fails the test *by name*, with the instruction to write a builder or a row.
2. **Everything claimed is accepted.** For each register entry, `tmux list-commands <abbreviation>` must resolve back to exactly the name and abbreviation the register holds. A listing proves tmux *prints* a word; only this proves tmux *accepts* it — which is the whole reason `Entry` carries an alias, since a consumer's config may be written in abbreviations. An **ambiguous** word is a hard failure: that is a claim about this tmux that this tmux contradicts.

Both assertions are deliberately **one-way** — installed ⊆ claimed, never the reverse. The tables are written against a newer tmux than the oldest they are meant to serve, so a register entry this tmux has never heard of is *reported* rather than failed; an older tmux must not fail this suite for having fewer commands. A misspelling in either table is still caught, and by the same assertion: the correctly spelled command then appears in neither table.

`EXCLUDED` is **empty by achievement, and stays in the source anyway**: 92 installed, 92 typed, 0 awaiting. It is the holding state for a tmux that grows a command this crate has not met — the red test's fix is either a builder or a row carrying its reason, and a row nobody can write a reason for is an omission wearing a table's clothes. Deleting the table would delete the honest half of that choice.

## For AI Agents

### Working In This Directory

- **Port behavior, never source.** Read the matching Go file before changing a control-mode behavior, then write idiomatic Rust and keep the module-level `Spec:` citation. A deliberate departure is a prose `Divergence:` callout with the Go behavior and the reason; this crate does not use the ganja crates' `D<n>` decision-ledger numbering.
- **A new file's provenance is decided by where it goes.** Under `control_mode/`, it ports a Go file and names it. At the root it is synthesized and says so — unless it is vocabulary hoisted out of the port, which keeps its `Spec:` line and marks the synthesized additions at the item. Do not add a `Spec:` line to a one-shot module to make the tree look uniform; the asymmetry is the information.
- **The one-shot surface imports nothing from `control_mode`.** No `use`, no path, in `server.rs` or `commands/**`. The accepted exception is a **doc link that states the boundary**: `server.rs` links `crate::control_mode` several times precisely to say what it is *not* doing, `commands/mod.rs` and `sessions.rs` link `Client` to send a caller to the other transport, and the crate doc names `control_mode::CommandLine` to say where the quoting ladder stays. A link crosses nothing at compile time; a `use` does. `error.rs` is not an exception but the other case entirely: it *does* import `control_mode::protocol::Response`, because `CommandError` wraps one, and that is what makes it shared vocabulary rather than a one-shot module.
- **An argv word is never quoted.** The root layer hands `OsString`s to execve; adding quotes would put them *inside* the argument tmux reads. If a value that begins with `-` must be safe, it belongs after the `--` fence `Words::render` already emits — which is exactly why positional and trailing arguments go through `positional`/`trailing` rather than being pushed as flags.
- **A new command is one table entry, not a hand-written builder.** Add it to its family's `invocations!` block with a doc line per flag; the macro produces the struct, the methods, their docs, the `Invocation` impl and the `ENTRIES` row. Hand-writing an impl block beside them would break the property that makes `warn(missing_docs)` satisfiable across ~630 generated methods.
- **The sealed leaf is a hard boundary.** `tmux consumes no ganja crate` is the inverted `cargo tree -p tmux -e normal` grep; `no workspace member consumes tmux` loops over names from `cargo metadata --no-deps` and refuses an exact `tmux` node in every other normal tree; the depth-1 allowlist is exactly `futures thiserror tokio `. Do not add an internal edge, a root `tmux` workspace-dependency handle, or a fourth normal external without an explicit user directive superseding P26 and a deliberate CI change.
- **Keep the sans-io half process-free.** `Parser::feed`, command-line rendering, output decoding, typed notification parsing, and id/target validation must remain directly testable without a subprocess or runtime. Transport ownership and asynchronous coordination belong in `client.rs`. The one-shot half has the same property for a different reason: a builder assembles words and runs nothing, so every argv assertion is a unit test.
- **One pending command means no pipelining** (control mode). `Client::exec_raw` holds the async write lock across write and response wait. If its future is dropped after the write has been attempted and its own pending registration still exists, `PendingDropGuard` poisons the client: a late response cannot safely belong to a future command. A caller reconnects rather than retries on that client. None of this applies to a `Server`, which has no connection to poison — concurrent `run` calls are simply concurrent processes.
- **The reader never awaits notification capacity** (control mode). `EventQueue::push` synchronously drops the oldest buffered notification when full and increments the approximate dropped counter; `recv` and `events` drain what remains. Do not replace this with a bounded send that can suspend the stdout reader behind a slow consumer.
- **Shutdown is owned here** (control mode). `close()` is idempotent, aborts pending work, attempts `detach-client` only through a non-blocking write-lock acquisition, drops stdin, waits for the child within `shutdown_timeout`, then kills and re-waits on expiry. Dropping an unclosed real client is also required to reap its `kill_on_drop` child.
- **Policy stays out of this crate.** Identity-checked kill, refusal sentences a person reads, enumerated environment lists: all consumer judgment. This crate ships the material — the split/list/kill primitives, the `-e NAME=VALUE` words a caller hands over, `Error::NotInTmux` as a bare fact — and never the verdict.
- **Keep public documentation complete.** The crate-level `warn(missing_docs)` is intentionally only a warning in source; CI's `cargo clippy ... -- -D warnings` promotes it to the hard AC-10 gate, matching the workspace convention against source-level `deny(warnings)`.

### Testing Requirements

Unit tests live beside the code in `#[cfg(test)] mod tests`. The process-free control-mode suites cover `protocol`, `commandline`, `output`, `notification`, `flow`, and `options`; `client.rs` uses scripted `tokio::io::duplex` peers to pin serialization, both drop-poisoning edges, command errors, drop-oldest delivery, `%exit`, protocol-error synthesis, close idempotency, detach lock contention, and blocked-write shutdown. The one-shot suites are process-free for the same reason in reverse: `server.rs` asserts the `$TMUX` decomposition and the argv a call would run, and each family asserts the words its builders assemble — including that a non-UTF-8 value survives into argv byte for byte.

`tests/live.rs` is the second tier. Every test creates a tempdir, a scratch `-S` socket, an empty `-f` config, and a unique session, so it never reaches the invoking user's default tmux server. It hard-fails with a named requirement when `tmux -V` cannot run. Go's `integration_test.go` instead requires `RUN_REAL_TMUX_TESTS=1` and skips without it; the Rust divergence is deliberate because a green default run that exercised no real protocol would be false evidence.

Live coverage is **one round trip per family, not one per command**: what varies between builders is which flags they render, which is a process-free question, and a live test per command would buy tmux's parser being exercised 92 times for the same answer. What the live tier is for is the question units cannot ask — whether the words were *right*. On top of that sits one e2e, `both_transports_see_one_server`: a world built entirely through the typed builders (session, two windows, a split whose `-c` and `-e` are both read back off the pane, a buffer paste, a resize), then a control-mode `Client` attached to the same socket, listing the very ids the one-shot side minted and observing a one-shot `kill-window` arrive as `%window-close @1`.

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
env -u RUSTFLAGS cargo --config ~/.config/rust/config.dev.toml nextest run -p tmux -E 'binary(inventory)' --no-capture
RUN_REAL_TMUX_TESTS=1 cargo run -p tmux --example control_mode_session
TMUX_RS_SESSION=my-session cargo run -p tmux --example existing_session
```

The inventory run is worth `--no-capture`: both halves print their counts, and the shrinking half of the first (`92 installed, 92 typed, 0 awaiting a family`) is the coverage measure.

### Common Patterns

- Validity moves into types: `PaneId`, `WindowId`, and `SessionId` parse once at construction, while a not-applicable wire value is `Option::None`, never a string sentinel after decoding.
- Normal control-mode command data uses `Arg::string`; `Arg::raw` is reserved for trusted tmux syntax such as flags, fixed words, and deliberately pre-quoted fragments. The one-shot layer has no such distinction, because it renders no line.
- A builder is words, not a call: it holds no socket, runs nothing, and can be asserted whole in a unit test. `Server` holds the socket, and every command is sent through the same `run`.
- Test names are sentences about behavior: `concurrent_execs_are_serialized_onto_one_pending_slot`, `a_value_set_twice_keeps_the_last_in_the_first_position`, `every_abbreviation_the_register_claims_resolves_to_the_command_it_names`.
- Module docs state provenance first — the Go specification, or the fact of synthesis — and explain divergences where they occur. Comments explain the protocol or correctness boundary, not the line immediately below them.

## Dependencies

### Internal

None, in either dependencies table. That is the P26 invariant: this crate consumes no `ganja-*` crate and no workspace member consumes it. The missing root `[workspace.dependencies]` handle is part of the same boundary rather than an omission to repair.

### External

`thiserror` (typed, message-carrying errors across the rendering, decoding, protocol and invocation surfaces — the same choice as the rest of the workspace), `tokio` (both transports' subprocesses: the persistent `tmux -C` with its piped async I/O and reader/stderr tasks, and the one-shot client each `Server::run` spawns), and `futures` (the `Stream` adapter over the drop-oldest event queue). `tempfile` is a dev-dependency only, used by tests for private filesystem/socket state; it is below the normal dependency gate's horizon. The one-shot surface added no dependency at all — argv is `std::ffi::OsString` and the process is `tokio`'s.

<!-- MANUAL: -->
