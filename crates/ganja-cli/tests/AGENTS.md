<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# ganja-cli/tests

## Purpose

Assertions on the shipped binary rather than on library functions: the command-line surface, and a pty suite that drives the real terminal UI through a fake turn, a scripted tool chain, and every exit path.

## Key Files

| File | Description |
|------|-------------|
| `cli.rs` | The command-line surface — subcommands, credential storage, redaction, the model listing and the session listing. Every credential assertion is on the redacted tail: a test that printed a whole key would put it in CI output, which is the failure redaction exists to prevent. |
| `auth_login.rs` | The login flows through the built binary, against an issuer this suite stands up on loopback and points `GANJA_AUTH_ISSUER` at. Its one structural trick is a **gate**: the token exchange is held open until the suite has read the device code off the child's standard error, so "the code is shown before the login blocks on it" is asserted rather than assumed — a build that printed afterwards deadlocks against its own login, and the deadline turns that into a failure instead of a hung suite. The interrupt test is unix-only and sends the signal with `kill`, so the crate grows no dependency to do it. A test asking *which* login ran rather than when it printed opens the gate up front (`unblocked()`) and drives the whole invocation with one `output()` call — the three defects this file grew tests for were all found by running the binary with a non-terminal standard input, which is what `fed()` reproduces. **A browser login's socket is not driven here**: its callback port is fixed by the provider's client registration, so binding it would contend with every other process on the machine — what this suite asserts is that `--method browser` reaches that flow and that a provider without one refuses it, and the socket is driven end to end beside the flow in `ganja-core`. |
| `mcp.rs` | `ganja mcp` through the built binary: one server of each outcome there is — a loopback endpoint that answers, a program no machine has, and one configured off — proving each standing is its own and that a connected server's tools are listed under the names the model would call them by. The peer is a socket rather than a process, so this crate gains neither `bun` nor the upstream checkout as a prerequisite; the transport's own correctness is pinned beside the engine, which already pays for both. |
| `import_opencode.rs` | `ganja config import-opencode` through the built binary: which files discovery reads and in what order, where the result lands, and that a run which would overwrite or leak refuses to. What the *mapping* does with a key is settled beside the mapping, in `src/import.rs`. |
| `import_round_trip.rs` | One env-mutating test in its own binary: import, then `Config::load` in-process, so what the importer wrote is proved to be what the next launch reads — permission order included. It is also the only place the `mcp` and `lsp` mappings are fully answered for: the importer's own `validate` decodes, where `Config::load` additionally refuses a server with no program, a custom language server that names no extensions, and an endpoint whose headers would travel in the clear. |
| `run.rs` | `ganja run` through the built binary: the exit-code table, the nd-JSON shape (six `type` names, and every object carrying the session `ganja sessions` reports for this project), the two permission mechanisms, and what the model was actually asked. Every invocation runs the fake provider against a written script, in its own project directory and against its own `XDG_DATA_HOME`, with stdin closed — `run` reads a pipe whole when standard input is not a terminal, so a test that inherited the harness's would be asking a different question each time. |
| `serve.rs` | Unix-only. `ganja serve` end to end: the real binary spawned in its own project and data home, the address line parsed off stdout, `/global/health` answered over a raw socket at the reported port, the unsecured warning found on stderr, and a SIGTERM producing exit 0. The signal *is* the clean-shutdown half, which is why the suite is unix-only. |
| `fixtures/opencode.jsonc` | One opencode config holding every shape the mapping has a rule for. Shared with the table test in `src/import.rs`, so the two cannot drift apart. |
| `pty_smoke.rs` | Unix-only (`#![cfg(unix)]`). Runs the binary under a pty: a fake turn streams into the transcript, a scripted turn runs a read, an edit and a shell command past the permission dialog, and the terminal is left restored however the process exits. |
| `resume_drill.rs` | Unix-only. The crash drill: a scripted turn is SIGKILLed mid tool call, the store must hold an unfinished envelope with the streamed text, and `--continue` must show that text marked interrupted. The store is read through `ganja_core::Storage` — the same reader the binary uses — rather than by opening files, so the drill pins stored state rather than a layout. Kills wait for pty EOF before reaping — a session leader cannot finish dying while its terminal has unread output. |

## For AI Agents

### Working In This Directory

**What a pty test may assert on** is the subtle part, and getting it wrong produces a flaky suite rather than an honest failure:

- A terminal is only sent the cells that *changed*, so a string arrives whole only when it was drawn over cells it differs from everywhere — in practice, over blank ones. Anything drawn on top of other text comes back split around the characters that happened to already match. That is why the status bar is never waited for, and why scripted tests run in a window tall enough that the centered permission dialog lands *below* the transcript's last line rather than across it.
- So the screen is used **for synchronization only**: waiting for the dialog, and waiting for the turn to reach its closing word. What a run actually proves is read back off the filesystem — the file the edit changed, the files the shell commands wrote, the rules an "always" answer stored.
- Waiting for the dialog is safe because a step's tool calls are resolved after the model's stream ends, so no fragment of the reply can race the dialog open. A script's `cadence_ms` therefore only decides how long a run takes, never what it proves — which is why these wait for the options line rather than for a tool's name in the reply.

### Testing Requirements

```sh
cargo test -p ganja-cli --test cli               # fast
cargo test -p ganja-cli --test import_opencode   # fast
cargo test -p ganja-cli --test run               # fast; drives real turns, no pty
cargo test -p ganja-cli --test pty_smoke         # unix only, slower
```

A pty run drives the binary through `GANJA_PROVIDER=fake` with a `GANJA_FAKE_SCRIPT` written into a temp directory, and `XDG_DATA_HOME` redirected so stored permission rules land in the fixture, not in the real user's data directory.

Anything that runs a login redirects `GANJA_AUTH_ISSUER` at an endpoint of its own and `XDG_DATA_HOME` at a directory of its own, so a suite can neither reach a real issuer nor touch the developer's credentials; `cli.rs` clears the variable for the same reason in reverse, since a developer who left one exported would otherwise decide what its key-path tests do. Anything that reads or writes a *config* redirects `XDG_CONFIG_HOME` as well — and pins `HOME` while clearing `GANJA_CONFIG_HOME`, because the global tier and the importer's `--global` destination resolve through ganja's config-home seam, two of whose three places reach past the XDG redirect — so the machine running the suite cannot contribute a config of its own. Anything that reads the model catalog redirects `XDG_CACHE_HOME` and switches fetching off for the same reason, one step further: `ganja models` adopts whatever catalog is cached there, so a run that inherited the developer's would assert on whatever their last session happened to fetch — and a run that inherited nothing would reach the published endpoint from CI. Prefer a subprocess `.env()` over `set_var`: the former is per-invocation and lets tests share a binary, and only a test that must also read the environment in-process — `import_round_trip.rs` — earns a binary of its own.

### Common Patterns

Timeouts are generous on purpose (`EXIT_DEADLINE` is 10s): a timeout here should mean "hung", not "slow machine". Assert on stored files wherever a filesystem assertion is available; reach for the screen only when nothing else can observe the behavior.

`run.rs` needs no screen at all, which is what makes it the cheapest place to assert on a real turn: it drives the same engine the pty suite drives, but reads its answers off stdout, off stderr and off the filesystem. Two of its assertions lean on surfaces worth knowing about — `ganja sessions` is the independent witness for which session a run created, and the fallback title a completed turn earns is the first fifty characters of the first prompt, which is the one place a headless run records *what it was asked*.

## Dependencies

### Internal

The built `ganja` binary, located by `assert_cmd`.

### External

`expectrl` (pty sessions, unix), `assert_cmd`, `predicates`, `tempfile`, `serde_json` (fake scripts, stored permission rules and `run`'s nd-JSON are JSON documents, not text; it is a normal dependency of the crate rather than a dev one, because `run --format json` writes with it).

<!-- MANUAL: -->
