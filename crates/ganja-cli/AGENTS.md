<!-- Parent: ../AGENTS.md -->
# ganja-cli

The `ganja` binary (`[[bin]] name = "ganja"`, `src/main.rs`). With no subcommand it runs the `ganja-tui` interface; the subcommands store credentials, write config files, list what ganja knows, take one headless turn, or serve the engine over HTTP. No workspace member depends on it. It is the only member that links both the terminal frontend and the HTTP server.

## Boundary

- No `depgate.toml` rule names this crate (see the comment above the depgate step in `.github/workflows/ci.yaml`).
- `depgate.toml` denies `axum*` to `ganja-core` and `ganja-tui`, and `ganja-tui` depends on neither `ganja-serve` nor `ganja-client`. So the two trait implementations that need them live here.
- `src/binder.rs` implements `ganja_tui::binder::Binder` over `ganja-serve` (the per-session Unix socket). `src/lister.rs` implements `ganja_tui::lister::Lister` over `ganja-tool`'s registry plus a `ganja-client` health probe (the `@` menu).

## Subcommands

Clap definitions: `src/main.rs`, plus `RunArgs` (`src/run.rs`), `ServeArgs` (`src/serve.rs`), `EvaluateArgs` (`src/evaluate.rs`), `PluginAction` (`src/plugin.rs`), `AddArgs`/`RemoveArgs` (`src/mcp.rs`).

| Subcommand | What it does | Flags that matter |
|---|---|---|
| (none) | The TUI. | `--model P/M`, `--agent`, `--config`, `-c/--continue` or `-s/--session <ID>` (clap refuses both), `--auto` (hidden aliases `--yolo`, `--dangerously-skip-permissions`), `--name`. Hidden: `--socket-dir`, and the pane-member set `--agent-id`, `--agent-name`, `--team-name`, `--agent-color`, `--parent-session-id`. |
| `auth login` / `list` / `logout` | Store, list, or forget a provider credential. | `login --provider <ID> --key -m/--method api\|browser\|device --deployment public\|enterprise --enterprise-url`; `logout --provider`. Builtin ids are `ProviderId` in `src/main.rs`. |
| `config import-opencode` / `migrate` / `import-claude-hooks` | Write a `ganja.toml` from opencode's config, a legacy `ganja.jsonc`/`ganja.json`, or Claude Code's `hooks` block. | Each takes `--file`, `--global`, `--dry-run`. |
| `evaluate` | The TypeSafe client with no engine, for scripts and `PreToolUse` hooks. | `--questions JSON\|@PATH` (required), `--state JSON\|@PATH\|-` (default stdin), `--model`, `--format json\|text`. |
| `mcp` [`list`] / `add` / `get` / `remove` / `login` | List (connects every enabled server), or edit the config's `mcp` table, or run one server's OAuth login. | `add <name> --url <URL>` or `add <name> -- <cmd> [args]`; `--global`, `--force`, `--header`, `--oauth`, `--env`, `--cwd`, `--timeout`, `--output-limit`, `--disabled`; `remove <name> --global`. |
| `models [PROVIDER]` | The model catalog. | `--refresh`. |
| `plugin` | `list`, `marketplace add\|list\|remove\|update`, `install`, `enable`, `disable`, `remove`, `details`. | Positional names only. |
| `run [MESSAGE]` | One headless turn, then exit. | `--command`, `-c/--continue`, `-s/--session`, `--fork` (always refused), `--model`, `--agent`, `--effort`, `--deadline DURATION\|HH:MM`, `--json-schema FILE\|JSON`, `--config`, `--attach <URL>`, `--format default\|json`, `--auto`. |
| `serve` | The engine over HTTP + SSE until SIGINT or SIGTERM. | `--port` (absent: 4096, else any free port), `--hostname` (default `127.0.0.1`; a non-loopback host requires `GANJA_SERVER_PASSWORD`). |
| `sessions` | Stored root sessions of this project. | `--live`: sessions answering on a socket now, every project. Hidden `--socket-dir`. |
| `skills` | The skill roster a session can load. | None. |

`-v/--verbose` is global but must follow the subcommand (`ganja models -v`); `ganja -v models` is refused because the root sets `args_conflicts_with_subcommands` (`src/main.rs`).

## Layout

| Path | Holds |
|---|---|
| `src/main.rs` | Clap types, `auth`, `models`, `mcp` list and login, `sessions`, logging, the no-echo key prompt. |
| `src/assemble.rs` | The engine assembly `run` and `serve` share. |
| `src/run.rs` / `src/serve.rs` | The headless turn; the HTTP server. |
| `src/login.rs` | Browser and device logins, method selection, Copilot deployment. |
| `src/import.rs`, `src/migrate.rs`, `src/claude_hooks.rs` | The three `config` writers. |
| `src/report.rs`, `src/staging.rs`, `src/position.rs` | The mapped/skipped table, the staged file write, and source positions the config writers share. |
| `src/mcp.rs`, `src/plugin.rs`, `src/skills.rs`, `src/evaluate.rs` | Their subcommands. |
| `src/binder.rs`, `src/lister.rs` | The socket binder and the live-session lister handed to `ganja-tui`. |
| `tests/pane_lead/`, `tests/served_child/` | Shared helpers for the tmux suites and the `serve` suites. |
| `tests/fixtures/opencode.jsonc` | The importer fixture, also read by `src/import_tests.rs`. |

## Commands

```sh
cargo nextest run -p ganja-cli
cargo nextest run -p ganja-cli -E 'binary(ganja)'   # the unit suites in src/
cargo nextest run -p ganja-cli -E 'binary(run)'     # one integration binary
```

## Conventions

- Stdout carries only the payload; prompts, warnings and diagnostics go to stderr, so `run --format json` stays parseable (`src/run.rs`, pinned by `tests/run.rs`).
- Key material: `secret()` wraps a key in `SecretString` and zeroizes the buffer; the prompt reads in raw mode with no echo and a `Drop` guard; output shows only `auth::RedactedTail`; a key shadowed by an environment variable is reported as shadowed (`src/main.rs`, pinned by `tests/cli.rs`). New key-handling code must prove the key reaches no output or stored file in the clear.
- Anything `run` and `serve` both need is installed once in `src/assemble.rs`, never at the two call sites.
- A flag that would parse and then decide nothing is refused by clap: `--attach` conflicts with `--config`, `--command`, `--effort`, `--deadline` and `--json-schema` (`src/run.rs`).
- `run --deadline` parses with `ganja_tui::command::resolve_deadline`, the same grammar as `/deadline`, at the clap boundary.
- The config writers edit with `toml_edit`, so a target's comments and key order survive; the importers never write an API key and never expand `{env:…}`/`{file:…}` (pinned by `src/import_tests.rs`).
- A new subcommand gets an assertion in `tests/cli.rs`.

## Gotchas

- Provider selection is `ganja_core::provider::select`: `--model`, then `GANJA_PROVIDER`/`GANJA_MODEL`, then the config `model`, then `default_provider`, then the oldest stored login, then the built-in fake provider. A machine with any stored login does not get the fake.
- Every subcommand exits 0 or 1 except `evaluate`: 0 answered, 3 not configured, 4 vendor refused, 5 unavailable, 64 bad argument. Clap's own parse failure still exits 2 (`src/evaluate.rs`).
- `run --json-schema` reads a file first and inline JSON second, requires a JSON object, and is refused unless the provider is `chatgpt` or `openai` (`speaks_options` in `ganja-provider/src/provider/responses/options.rs`).
- `run` refuses every call that would open a dialog unless `--auto` is given, and always refuses `question`, `plan_enter` and `plan_exit`. It waits `SETTLE_LIMIT` (90 s) for hooks before exit; `serve` installs none of these refusals.
- `GANJA_AUTH_ISSUER` redirects every login endpoint and is refused unless it is `http://<loopback>:<port>` (`src/login.rs`).
- The log file is `$XDG_DATA_HOME/ganja/log/ganja.<date>.log` on every platform, macOS included, seven kept; `RUST_LOG` overrides `-v`.
- `models` installs the disk catalog with `catalog::load_cached()` before reading; `--refresh` failures only warn.

## Tests

Unit tests are sibling `<module>_tests.rs` files attached with `#[cfg(test)] #[path = "…"] mod tests;` and run in the `ganja` binary target. Each integration binary's `//!` header states its prerequisites. The ones with setup:

- tmux server required, hard-fails without one: `teammate_env`, `teammate_pane`, `teammate_permission`, `team_tasks_pane`, `team_continuation_pane` (all through `tests/pane_lead/`).
- Two or more processes: `serve` and `attach` (through `tests/served_child/`), `uds`, `peer_drills`, `id_collision`, `claude_code_run`.
- `claude_code_run` is `harness = false` (`Cargo.toml`): it re-execs itself as the fake `claude` CLI when `GANJA_FAKE_CLAUDE_SCRIPT` is set.
- Unix only: 15 of the 30 test files carry `#![cfg(unix)]` (pty through `expectrl`, Unix sockets, or signals), among them `pty_smoke`, `resume_drill`, `rewind_drill`, `yolo_drill` and `serve`; each header says so.

No test here is `#[ignore]` or reaches a real service. Suites set `GANJA_PROVIDER=fake` with `GANJA_FAKE_SCRIPT`, point `XDG_DATA_HOME`, `XDG_CONFIG_HOME` and `GANJA_CONFIG_HOME` at temp dirs, pass the hidden `--socket-dir`, and set `GANJA_DISABLE_MODELS_FETCH`, `GANJA_AUTH_ISSUER`, `OPENAI_BASE_URL` (`ganja_testkit::responses_server`) or `TYPESAFE_BASE_URL` (a loopback listener) as needed.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-cli.md`, `docs/decisions/ganja-cli-src.md`, `docs/decisions/ganja-cli-tests.md`, frozen from commit 35d1720. New decisions go in `docs/decisions/ledger.md`, not here.
