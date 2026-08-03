<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# ganja-cli

## Purpose

The `ganja` binary. Running it with no subcommand starts the terminal UI, which is what the tool is for; the subcommands exist to set it up (`auth login`/`list`/`logout`) and to answer questions about it (`models`) without taking the screen over.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest. Declares `[[bin]] name = "ganja"`. Depends on `ratatui` for exactly one thing — the raw-mode read that keeps a typed API key off the screen — and on `secrecy` so a key is wrapped the moment it is whole. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/` | `main.rs`: clap surface and the credential prompt (see `src/AGENTS.md`) |
| `tests/` | CLI assertions and pty smoke tests (see `tests/AGENTS.md`) |

## For AI Agents

### Working In This Directory

This crate is where a secret is most likely to escape, because it is the only place a human types one. Before touching credential paths, read `src/AGENTS.md` — the rules there (no echo, wipe the buffer, print only the redacted tail, warn when an environment variable shadows a stored key) are each pinned by a test.

### Testing Requirements

```sh
cargo test -p ganja-cli                    # includes pty tests on unix
cargo test -p ganja-cli --test cli         # CLI surface only, fast
```

The pty suite drives the real binary through a terminal and is unix-only (`#![cfg(unix)]`).

### Common Patterns

Subcommands print to stdout and diagnostics to stderr, so a caller capturing stdout gets a clean channel; the API-key prompt writes to stderr for the same reason.

## Dependencies

### Internal

`ganja-core` (`auth`, `catalog`), `ganja-tui` (`run()`).

### External

`clap` (derive), `tokio`, `anyhow`, `secrecy`, `ratatui` (raw mode only); dev: `assert_cmd`, `predicates`, `tempfile`, `serde_json`, and `expectrl` on unix.

<!-- MANUAL: -->
