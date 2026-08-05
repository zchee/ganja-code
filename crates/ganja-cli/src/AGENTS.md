<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# ganja-cli/src

## Purpose

The binary's whole surface: clap parsing, the credential subcommands, the config importer, the model listing, and the terminal prompt that reads an API key without echoing it.

## Key Files

| File | Description |
|------|-------------|
| `main.rs` | `Cli`/`Command`/`Auth`/`Config` clap types, `login`/`list`/`logout`, `models`, `mcp`, and the raw-mode key prompt. No subcommand delegates straight to `ganja_tui::run()`. |
| `import.rs` | `ganja config import-opencode`: discovery of opencode's config tiers, the key mapping, the mapped/skipped table, and the JSON writer that produces a `ganja.json`. |

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

The importer inherits the same posture, for the same reason — it reads a file that may hold a credential:

- **`provider.<id>.options.apiKey` is never written**, only reported, with a warning naming `ganja auth login`. Neither the table nor the warning repeats the key.
- **`{env:VAR}`/`{file:path}` is never expanded**, in either direction. A value that *is* a token is left out (carrying it would name a model or a path that does not exist); a value that merely contains one is carried verbatim and warned about, because ganja will then read it literally.
- **Every key is either mapped or reported.** A key that vanished without a row would be a setting its author still believes is in force — the table is the command's output and the file is a side effect of it. A container (`agent`, `mcp`, an `lsp` map) is covered by the rows its entries carry rather than one of its own.
- **An `mcp` or `lsp` entry is written whole or not at all**, so its fields are read into a report of their own and only adopted if it survives: a `mapped` row under an entry that was then refused would name a setting that was never written. Only what such a pass had to *say* outlives the refusal.
- **An `lsp` entry is judged by what it leans on, not by its name.** opencode ships definitions for thirty-eight language servers and ganja ships two, but an entry naming one of the other thirty-six is only a problem when it relied on that definition: upstream lets an entry give just a `command` (or just `disabled`) and inherit the extensions and the root. An entry naming both its `command` and its `extensions` is a whole server description already, so it imports as a custom server under its own name and does here what it did there; anything less is named, skipped and explained.
- **Nothing is completed on an entry's behalf.** A language server with no `command` is left out rather than given one — a fabricated command starts a program nobody chose — and the two shapes `ganja_core::config` refuses at load (a command-less server that is not disabled, a custom one that does not name its `extensions`) are refused here instead, because a file this wrote that the next launch will not read is the failure the round trip exists to prevent. The same holds for a remote MCP endpoint that is neither `https` nor loopback; the check here is deliberately the conservative half of core's, which parses the URL properly and is the authority.
- **A command line and an extension list travel whole or not at all**, where `instructions` drops entries one by one: a command missing an argument runs a different program, and an extension list emptied of what could be carried is `[]`, which ganja reads as *every* file.
- **An existing `ganja.json` *or* `ganja.jsonc` at the destination is refused by name** rather than overwritten; the write itself uses `create_new`, because the check and the write are not the same moment.
- **What is written is decoded back into `ganja_core::config::Config` before it lands**, so a mapping bug is an error at import time rather than a broken file discovered at the next launch. Decoding is not the whole of what `Config::load` does — its `mcp` and `lsp` checks run after it — which is why `../tests/import_round_trip.rs` loads the imported file for real and is the assertion those two mappings answer to.
- Object keys keep the order they were written in throughout: `permission` is evaluated last-match-wins, so a reader or writer that sorted them would change which rule decides a call.

The two listings each have one rule that is not obvious from their code:

- **`models` calls `catalog::load_cached()` before it reads the table.** The disk tier is a layer somebody installs, not one a lookup reaches for, so a listing that skipped installing it would answer from the compiled-in snapshot however recently a session had fetched something newer. `--refresh` fetches on top of that and is never fatal — fetching switched off and an endpoint that refuses the connection both leave the table standing and say so on stderr. Only a named provider the table does not carry is a failure, because a header over no rows is indistinguishable from the typo it usually is.
- **`mcp` dials.** A standing nothing has tried is not a standing, so the listing connects every enabled server and reports what came of it — and reads the statuses and the tools it lends *before* it shuts them down again, because closing a connection takes its tools with it. The rows are driven by the config rather than by the statuses, which deliberately omit a server nothing has finished trying: a row that could silently vanish is worse than one reporting it has no standing. Nothing here wants a credential, so no engine is built, for the reason `sessions` reads the store directly.
- **`sessions` lists roots.** A session carrying a `parent` belongs to the `task` call that spawned it; the picker in `ganja-tui` filters the same way, and filtering before the count is what makes a project whose every session is delegated read as one with none.

### Testing Requirements

```sh
cargo test -p ganja-cli --test cli
cargo test -p ganja-cli --bin ganja            # the mapping table lives beside the mapping
cargo test -p ganja-cli --test import_opencode
```

Adding a subcommand means adding its assertion there; adding anything that handles key material means proving the key does not reach stdout, stderr, or a stored file in the clear.

### Common Patterns

`ProviderId` is a clap `ValueEnum` that maps to the string ids `ganja-core` knows (`anthropic`, `openai`), so the CLI's spelling and the engine's cannot drift. Prices render with trailing zeros trimmed rather than padded, so a fraction of a cent shows as itself instead of rounding to a different number.

## Dependencies

### Internal

`ganja_core::auth` (store, list, remove, redaction), `ganja_core::catalog` (the `models` table), `ganja_core::config::Config` (what the importer's output has to decode as), `ganja_core::lsp::server::BUILTIN_IDS` (which language servers this build ships, so the CLI's answer and the engine's cannot drift), `ganja_core::Project` (the import's project walk and destination), `ganja_tui::run`.

### External

`clap`, `anyhow`, `tokio`, `secrecy` (+ `zeroize`), `ratatui`'s crossterm re-export for raw mode, `jsonc-parser` (the importer reads someone else's config, in document order).

<!-- MANUAL: -->
