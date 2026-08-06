<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# ganja-cli

## Purpose

The `ganja` binary. Running it with no subcommand starts the terminal UI — optionally pointed somewhere by `--model`, `--agent`, `--config`, and by `--continue` or `--session <id>` — which is what the tool is for; the subcommands exist to set it up (`auth login` — a key, or a browser or device login where the provider has one — plus `auth list`/`logout` and `config import-opencode`), to answer questions about it (`models`, `sessions`, `mcp`) without taking the screen over, — with `run` — to take one turn with no screen at all, and — with `serve` — to put the same engine behind a socket until a signal ends it.

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

`ganja-provider` (`auth`, for the login flows `auth login` drives — named directly because that command assembles no engine), `ganja-core` (`catalog`, and — for `run` and `serve` — `Engine`, `config`, `provider`, `instruction`, `permission`, `tool`), `ganja-tui` (`run()`), `ganja-serve` (`serve()`, behind the `serve` subcommand).

### External

`clap` (derive), `tokio`, `tokio-util` (the login flows' cancellation), `anyhow`, `secrecy`, `futures` (the engine's event stream), `serde_json` (`run --format json`), `ratatui` (raw mode only); dev: `assert_cmd`, `predicates`, `tempfile`, and `expectrl` on unix.

<!-- MANUAL: -->
