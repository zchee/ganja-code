<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# ganja-cli

## Purpose

The `ganja` binary. Running it with no subcommand starts the terminal UI — optionally pointed somewhere by `--model`, `--agent`, `--config`, and by `--continue` or `--session <id>` — which is what the tool is for; the subcommands exist to set it up (`auth login` — a key, or a browser or device login where the provider has one — plus `auth list`/`logout` and `config import-opencode`), to answer questions about it (`models`, `sessions`, `mcp`) without taking the screen over, — with `run` — to take one turn with no screen at all, and — with `serve` — to put the same engine behind a socket until a signal ends it.

`run --deadline <DURATION|HH:MM>` is **D557**'s headless half (`ganja-code-qecz`): it tells a headless turn how long it has, exactly as `/deadline` tells a screen's. The value is `/deadline`'s own grammar, read by the one parser both doors share — `ganja_tui::command::resolve_deadline`, called from the flag's clap value parser — so the flag and the slash command cannot come to mean two things by one span. Resolving at the clap boundary is the point: a value the grammar has not got, a clock time already behind, and `off` (the slash command's word for clearing, and a fresh run has nothing to clear) are refused before an engine is assembled, so no session is created and no request is spent to report a typo; and the clock is read once, there, so the instant is the one that reading names. `run` sends the instant as `Command::SetDeadline` after the session is selected and before the prompt, so the turn's **first** request already carries the request-only block — a budget that bit from the second step on would leave the step most likely to wander unhurried. Nothing is cancelled when it passes, as on a screen. `--attach` with it is a parse error for `--effort`'s reason: the attached client's surface has no deadline route, and a flag that parsed and then hurried nothing is what that flag table refuses to hold. `tests/run.rs` sees the block reach the first request through the one output of this binary that measures a request rather than a reply — the fake provider's word count, reported as the step's `step_finish` input tokens.

`run --json-schema <FILE|JSON>` is **D563**'s headless half: the JSON Schema a run's answer has to satisfy, sent as the Responses API's `text.format` on every step request of that run's own turn and on nothing else — never the title request, never a compaction, never a `Turn::child`. The value is read as a **file first** and as an inline document only when no such path is there, because a path is never valid JSON and the other order would report a typo'd filename as a syntax error at column 1; the branch is `Path::exists`, so a directory named here fails as a path that could not be read rather than as one that is not there (**Dv-50**). Four refusals rather than one, each about the thing that actually went wrong (**Dv-33**, **Dv-49**): a value that is neither a file nor JSON is E2 (`--json-schema takes a path to a JSON file or an inline JSON document; …`), a path that is there and cannot be read or parsed says so about the file, and a document that parses and is not a JSON **object** names its source and the type that arrived — a schema is an object by definition, and a bare `42` or an array would otherwise ride all the way to the vendor and come back as somebody else's error about a request this build assembled. E3 refuses the flag on a provider that does not speak Responses, checked right after `assemble` returns, before hooks and before any session exists — which is why `Assembled` carries the selection's provider id (**Dv-34**): an `Engine` answers for no provider. The document is wrapped `{type: "json_schema", name: "ganja_run", schema, strict: true}` and installed through `Engine::set_text_format` before the prompt. There is deliberately no `--fast` flag: a headless run takes its tier from config.

`assemble.rs` is the **one** install site for the Responses options tables (**Dv-29**): `run` and `serve` both build their engine through it, so `.with_provider_options(config.responses_options())` is one line rather than a pair that can drift, and `Config::responses_options()` is the accessor this site and `ganja-tui`'s two both read.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest. Declares `[[bin]] name = "ganja"`. Depends on `tokio-util` for exactly one thing — the `CancellationToken` a login flow's wait takes, which only the binary can fire because only the binary catches the keystroke — on `ratatui` for exactly one other — the raw-mode read that keeps a typed API key off the screen — on `secrecy` so a key is wrapped the moment it is whole, on `futures` because `run` consumes the engine's event stream and the `Stream` trait behind a `BoxStream` has to be named to be reached, and on `serde_json` because `run --format json` writes one serde-derived object per event. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/` | `main.rs`: clap surface and the credential prompt; `run.rs`: the headless turn; `serve.rs`: the HTTP server (see `src/AGENTS.md`) |
| `tests/` | CLI assertions, the headless-turn suite, the serve smoke, and pty smoke tests (see `tests/AGENTS.md`) |

## For AI Agents

### Working In This Directory

This crate is where a secret is most likely to escape, because it is the only place a human types one. Before touching credential paths, read `src/AGENTS.md` — the rules there (no echo, wipe the buffer, print only the redacted tail, warn when an environment variable shadows a stored key) are each pinned by a test.

### Testing Requirements

```sh
cargo test -p ganja-cli                    # includes pty tests on unix
cargo test -p ganja-cli --test cli         # CLI surface only, fast
cargo test -p ganja-cli --test auth_login  # the login flows, against an issuer the suite owns
cargo test -p ganja-cli --test run         # the headless turn, fast
cargo test -p ganja-cli --test serve       # the server end to end, unix only
```

The pty suite drives the real binary through a terminal and is unix-only (`#![cfg(unix)]`).

### Common Patterns

Subcommands print to stdout and diagnostics to stderr, so a caller capturing stdout gets a clean channel; the API-key prompt writes to stderr for the same reason, and so does everything `run` has to say about a turn that is not the turn itself — a warning inside `--format json`'s stream would corrupt it.

## Dependencies

### Internal

`ganja-provider` (`auth`, for the login flows `auth login` drives — named directly because that command assembles no engine), `ganja-core` (`catalog`, and — for `run` and `serve` — `Engine`, `config`, `provider`, `instruction`, `permission`, `tool`), `ganja-tui` (`run()`, and `command::resolve_deadline` for `run --deadline`), `ganja-serve` (`serve()`, behind the `serve` subcommand).

### External

`clap` (derive), `tokio`, `tokio-util` (the login flows' cancellation), `anyhow`, `secrecy`, `futures` (the engine's event stream), `serde_json` (`run --format json`), `ratatui` (raw mode only), `jiff` (the daily log's civil-date rollover), `tempfile` (the staged `mcp add`/`remove` config write); dev: `assert_cmd`, `predicates`, and `expectrl` on unix.

<!-- MANUAL: -->
