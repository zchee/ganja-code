<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# ganja-cli/src

## Purpose

The binary's whole surface: clap parsing, the credential subcommands, the model listing, and the terminal prompt that reads an API key without echoing it.

## Key Files

| File | Description |
|------|-------------|
| `main.rs` | `Cli`/`Command`/`Auth` clap types, `login`/`list`/`logout`, `models`, and the raw-mode key prompt. No subcommand delegates straight to `ganja_tui::run()`. |

## For AI Agents

### Working In This Directory

Every rule here exists because a secret passed through this code, and each is pinned by a test in `../tests/`.

- **A key is wrapped the moment it is whole, and the buffer it was assembled in is wiped.** `secret()` trims (a key pasted from a password manager arrives with whitespace that would corrupt the request header), wraps in `SecretString`, and zeroizes the `String` — nothing else will clear it.
- **The prompt does not echo.** An echoed key survives the exchange: it sits in scrollback that may be shared, recorded or logged. Raw mode is the only way crossterm offers to suppress echo, which is why that loop handles Enter, Backspace and Ctrl-C itself — in raw mode the driver does none of that and Ctrl-C raises no signal.
- **Raw mode is left on every path, panic included**, via a `Drop` guard. A terminal left in raw mode is unusable and the shell that owns it will not fix it.
- **What was typed is wiped even when the read is abandoned** — a half-typed key is still a key. The guard's limitation is documented honestly in the source: `zeroize` clears the buffer's capacity, but a `String` that grew as it was typed reallocated, and the prefixes it left behind are not reachable.
- **Only the redacted tail is ever printed** (`auth::RedactedTail`), including in success messages.
- **Say so when an environment variable shadows a stored key.** Otherwise a `login` that appears to have worked changes nothing.
- A key given via `--key` was already in shell history and the process table before this ran; the flag's help says so, and wrapping it is all that is left to do.

Prompts and diagnostics go to stderr so stdout stays a clean channel for whatever a caller is capturing. A piped key is read whole so `pass show … | ganja auth login` works.

### Testing Requirements

```sh
cargo test -p ganja-cli --test cli
```

Adding a subcommand means adding its assertion there; adding anything that handles key material means proving the key does not reach stdout, stderr, or a stored file in the clear.

### Common Patterns

`ProviderId` is a clap `ValueEnum` that maps to the string ids `ganja-core` knows (`anthropic`, `openai`), so the CLI's spelling and the engine's cannot drift. Prices render with trailing zeros trimmed rather than padded, so a fraction of a cent shows as itself instead of rounding to a different number.

## Dependencies

### Internal

`ganja_core::auth` (store, list, remove, redaction), `ganja_core::catalog` (the `models` table), `ganja_tui::run`.

### External

`clap`, `anyhow`, `tokio`, `secrecy` (+ `zeroize`), `ratatui`'s crossterm re-export for raw mode.

<!-- MANUAL: -->
