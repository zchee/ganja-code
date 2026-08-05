<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# ganja-cli/tests

## Purpose

Assertions on the shipped binary rather than on library functions: the command-line surface, and a pty suite that drives the real terminal UI through a fake turn, a scripted tool chain, and every exit path.

## Key Files

| File | Description |
|------|-------------|
| `cli.rs` | The command-line surface — subcommands, credential storage, redaction, the model listing and the session listing. Every credential assertion is on the redacted tail: a test that printed a whole key would put it in CI output, which is the failure redaction exists to prevent. |
| `mcp.rs` | `ganja mcp` through the built binary: one server of each outcome there is — a loopback endpoint that answers, a program no machine has, and one configured off — proving each standing is its own and that a connected server's tools are listed under the names the model would call them by. The peer is a socket rather than a process, so this crate gains neither `bun` nor the upstream checkout as a prerequisite; the transport's own correctness is pinned beside the engine, which already pays for both. |
| `import_opencode.rs` | `ganja config import-opencode` through the built binary: which files discovery reads and in what order, where the result lands, and that a run which would overwrite or leak refuses to. What the *mapping* does with a key is settled beside the mapping, in `src/import.rs`. |
| `import_round_trip.rs` | One env-mutating test in its own binary: import, then `Config::load` in-process, so what the importer wrote is proved to be what the next launch reads — permission order included. It is also the only place the `mcp` and `lsp` mappings are fully answered for: the importer's own `validate` decodes, where `Config::load` additionally refuses a server with no program, a custom language server that names no extensions, and an endpoint whose headers would travel in the clear. |
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
cargo test -p ganja-cli --test pty_smoke         # unix only, slower
```

A pty run drives the binary through `GANJA_PROVIDER=fake` with a `GANJA_FAKE_SCRIPT` written into a temp directory, and `XDG_DATA_HOME` redirected so stored permission rules land in the fixture, not in the real user's data directory.

Anything that reads or writes a *config* redirects `XDG_CONFIG_HOME` as well, so the machine running the suite cannot contribute a config of its own. Anything that reads the model catalog redirects `XDG_CACHE_HOME` and switches fetching off for the same reason, one step further: `ganja models` adopts whatever catalog is cached there, so a run that inherited the developer's would assert on whatever their last session happened to fetch — and a run that inherited nothing would reach the published endpoint from CI. Prefer a subprocess `.env()` over `set_var`: the former is per-invocation and lets tests share a binary, and only a test that must also read the environment in-process — `import_round_trip.rs` — earns a binary of its own.

### Common Patterns

Timeouts are generous on purpose (`EXIT_DEADLINE` is 10s): a timeout here should mean "hung", not "slow machine". Assert on stored files wherever a filesystem assertion is available; reach for the screen only when nothing else can observe the behavior.

## Dependencies

### Internal

The built `ganja` binary, located by `assert_cmd`.

### External

`expectrl` (pty sessions, unix), `assert_cmd`, `predicates`, `tempfile`, `serde_json` (fake scripts and stored permission rules are JSON documents, not text).

<!-- MANUAL: -->
