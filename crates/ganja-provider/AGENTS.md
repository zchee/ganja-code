<!-- Parent: ../AGENTS.md -->
# ganja-provider

The model-vendor layer: wires that turn a `ChatRequest` into `ProviderEvent`s, the credential store and logins (`auth`), and the catalog that sizes and prices models. It never depends on `ganja-core`, renders nothing and executes no tool.
`ganja-core` and `ganja-cli` depend on it. Provider selection reads a `Config`, so it lives in `crates/ganja-core/src/provider.rs`.

## Boundary

- `depgate.toml` `[rules."ganja-provider"]`: `internal = ["ganja-permission", "ganja-protocol", "ganja-tool"]` (exact set; `ganja-permission` arrives through `ganja-tool`) and `deny = ["ratatui*", "crossterm*", "arboard*"]`. A login that needs input from a person returns the question to its caller.
- `ganja-core` re-exports `ganja_provider::provider::*` in `src/provider.rs` and `ganja_provider::{auth, catalog}` in `src/lib.rs`.
- `src/lib.rs` re-exports `ganja-protocol` as `crate::protocol` and `ganja-tool` as `crate::tool`.
- The dev-dependency `ganja-testkit` (the fake `claude` CLI) is a test-only cycle through `ganja-core`; `Cargo.toml` explains it.

## Providers

`PROVIDERS` in `src/provider.rs` lists the eleven builtin ids; `select` in `crates/ganja-core/src/provider.rs` has one arm per id.

| id | wire | credential source | cataloged |
|---|---|---|---|
| `anthropic` | `anthropic.rs` (Messages) | `ANTHROPIC_API_KEY`, else `auth.json` key | yes |
| `openai` | `responses.rs`, `Backend::Platform` | `OPENAI_API_KEY`, else `auth.json` key | yes |
| `chatgpt` | `responses.rs`, `Backend::Codex` | stored login | yes, `openai` rows via `ROW_ALIASES` |
| `openrouter` | `responses.rs`, `Backend::OpenRouter` | `OPENROUTER_API_KEY`, else `auth.json` key | fetched catalog only |
| `opencode`, `opencode-go` | `opencode.rs`: chat, Responses or Messages per model | `OPENCODE_API_KEY` (shared), else `auth.json` key | fetched catalog only |
| `grok` | `grok.rs` over `openai.rs` (chat completions) | stored login, filed as `xai` | yes |
| `github-copilot` | `copilot.rs` over `openai.rs` | stored GitHub device login | yes, zero prices |
| `cursor` | `cursor.rs` (Connect + protobuf) | stored login | no |
| `claude-code` | `claude_code.rs` (spawned `claude`, stdio stream-json) | none; the CLI's own login | no; sizing from `anthropic` via `SERVED_ROWS`, never priced |
| `fake` | `fake.rs` | none | no |

Wire paths are under `src/provider/`. "Fetched catalog only" means the compiled-in `SNAPSHOT` has no rows, so the id is uncataloged until a fetch succeeds (pinned by `src/provider/openrouter_tests.rs`).
A `[provider.<id>]` config entry with a `dialect` becomes a `compat.rs` provider, always uncataloged. An entry for a builtin id may carry only `options`, and only for `openai` and `chatgpt` (`check_builtin_options` in `crates/ganja-core/src/config.rs`).

To add a builtin: add it to `PROVIDERS`, add a `select` arm, and add `catalog.rs` `DEFAULTS` and `SNAPSHOT` rows or an explicit case in `every_selectable_provider_has_a_default_this_table_can_price` (`src/catalog_tests.rs`).

## Layout

| Path | Holds |
|---|---|
| `src/provider.rs` | `Provider`, `ChatRequest`, `ProviderEvent`, `ProviderError`, `PROVIDERS`, the shared HTTP client, `check_base_url`, `Presented` / `CredentialSource` / `Resolved`, `key_for` |
| `src/provider/sse.rs` | SSE frame splitter |
| `src/provider/retry.rs` | Retry schedule and `ProviderError::is_retryable` |
| `src/provider/rate.rs` | Rate and plan windows read from response headers |
| `src/provider/responses.rs` | Responses wire and its `Backend`s |
| `src/provider/responses/options.rs` | `[provider.<id>.options]` vocabulary that `ganja-core`'s config loader reads |
| `src/provider/compat.rs` | Config-declared endpoints (`Dialect`) |
| `src/provider/cursor.rs`, `cursor/` | Cursor wire: `connect.rs`, `decode.rs`, `request.rs`, `bridge.rs` (held Runs), `native.rs` (native exec redirect table), `history.rs` (blob channel), `value.rs` |
| `src/provider/cursor.proto` | Protobuf source; `cursor/ganja.cursor.v1.rs` is generated from it (do not edit) |
| `src/provider/claude_code.rs`, `claude_code/` | `argv.rs`, `frame.rs`, `rpc.rs`, `process.rs`, `held.rs`, `bridge.rs`, `binding.rs`, `preamble.rs` |
| `src/provider/ids.rs` | `derived` / `render_v4`, shared by cursor and claude-code |
| `src/provider/toolname.rs` | Aliases for tool names a vendor's name rules reject |
| `src/auth.rs` | `auth.json` store, env-first lookup (`KEY_VARS`), `Refresher` |
| `src/auth/` | `device.rs` (RFC 8628), `pkce.rs`, `loopback.rs`, and the `openai`, `grok`, `copilot`, `cursor`, `mcp_oauth` logins |
| `src/catalog.rs` | Fetched and snapshot catalog, `DEFAULTS`, `WINDOW_CEILINGS` |
| `src/effort.rs` | Effort names per wire; `standalone` is claude-code's set |
| `src/jitter.rs`, `src/atomic.rs` | Backoff entropy; write-then-rename for the catalog cache |
| `buf.gen.yaml` | Protobuf codegen config |
| `tests/fixtures/` | Recorded SSE bodies, probe recordings, the derived claude-code replay |

## Commands

```sh
cargo nextest run -p ganja-provider
cargo test -p ganja-provider --doc
# live, by hand only: one RPC to api2.cursor.sh with the stored login (`ganja auth login cursor` first)
cargo nextest run -p ganja-provider --run-ignored only -E 'binary(cursor_live)'
# after editing src/provider/cursor.proto; run inside crates/ganja-provider, needs `buf` on PATH
buf generate
# from the repo root: checks this crate's depgate rule with the others
cargo depgate check --config depgate.toml
# the socket suites that replay this crate's fixtures through the engine
cargo nextest run -p ganja-core --test http
```

## Conventions

- Where a credential may travel is decided in `src/provider.rs`, not per wire: the shared client follows no redirects (`reqwest` strips `Authorization` across hosts but not `x-api-key`), and `check_base_url` accepts https or loopback only, comparing a `url::Host`; this covers `ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL`. reqwest's `system-proxy` is on, so proxy variables reroute traffic.
- Failures have two channels: an `Err` from `Provider::stream` before streaming starts, or a terminal `ProviderEvent::Failed` mid-stream. A body that stops arriving is a failure, never a finished turn (pinned by the `*_truncated.sse` cases in `anthropic_tests.rs` and `openai_tests.rs`).
- Retry only the request that opens a turn, before the first byte. The one exception: a retryable in-body failure before any content event reopens the request up to three times (`src/provider/retry.rs` module doc).
- Refresh failures map in `src/provider.rs`: `AuthErrorKind::ReauthRequired` becomes `ProviderError::Auth` (not retried), `RefreshUnavailable` becomes `ProviderError::Transport` (retried). Swapping them causes a retry storm against an identity provider, or a browser login nobody needed.
- Credentials are resolved per request through `CredentialSource` and `auth::Refresher::usable`, never captured at construction.
- In the wires, a key is read only through `Presented::expose`; `Presented::redact` scrubs a credential from echoed text; `Presented`'s `Debug` prints a placeholder (canary: `crates/ganja-core/tests/secrets_env.rs`).
- `auth.json` is shared with upstream opencode: a rewrite keeps entries and fields this build cannot read, `grok` is stored as `xai` (`auth::storage_key`), and `expires: 0` means never expires (`OauthCredential::needs_refresh`). Login flows return a credential and store nothing; the caller writes it.
- Host identity: ChatGPT's auth host, the codex backend and x.ai get `device::GANJA_USER_AGENT` with `originator`/`referrer` `ganja-code`; GitHub's device endpoints and `api.githubcopilot.com` get `device::UPSTREAM_USER_AGENT`. Fields sent to one host change together; `the_borrowed_identity_and_ganjas_own_never_name_the_same_thing` in `src/auth/device_tests.rs` fails if the constants converge.
- No tool runs in this crate: `cursor/native.rs` and `claude_code/bridge.rs` produce a tool name and JSON arguments, and the engine executes (pinned by `crates/ganja-core/tests/cursor_bridge.rs`). `claude_code/rpc.rs` hand-writes its four MCP methods; do not add `rmcp`.

## Gotchas

- claude-code never resumes a CLI record: `--resume` is in `argv::NEVER_ANYWHERE` (28 entries). The child environment strips 87 names (`argv::STRIP`, including `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN`) and sets 10 (`argv::SET`); `CLAUDE_CONFIG_DIR` and `CLAUDE_CODE_OAUTH_TOKEN` pass through untouched. All pinned by `claude_code/argv_tests.rs`.
- claude-code runs `~/.local/bin/claude` or an absolute `GANJA_CLAUDE_BIN`, and `select` refuses a build below `VERSION_FLOOR` (2.1.263) before the first turn.
- An uncataloged session has no cost and no auto-compaction, but a typed `/compact` still asks for and stores a summary (pinned by `a_manual_compaction_summarizes_a_model_nothing_sizes_and_stores_the_summary` in `crates/ganja-core/src/engine_tests.rs`).
- Catalog environment: `GANJA_MODELS_URL` (mirror), `GANJA_MODELS_PATH` (read a file instead of the cache), `GANJA_DISABLE_MODELS_FETCH` (no fetch; cache and snapshot only). The default source is `models.opencode.ai`.
- The proto drift test `the_checked_in_generated_code_matches_the_proto` (`src/provider/cursor_tests.rs`) prints a skip line and passes when `buf` is not on PATH, and CI does not install `buf`. Run `buf generate` yourself after touching `cursor.proto`.
- Tests that touch the credential store point `XDG_DATA_HOME` at a temporary directory so the user's real `auth.json` is never read or written.
- `GANJA_FAKE_SCRIPT` feeds the `fake` provider a JSON script, one entry per model request (`src/provider/fake.rs`).

## Tests

Unit tests are sibling files: `foo.rs` ends with `#[cfg(test)] #[path = "foo_tests.rs"] mod tests;`. There is no inline `mod tests { }` in `src/`. The four integration binaries in `tests/` each state their prerequisites in a `//!` header:

- `cursor_wire.rs`: the cursor wire over a loopback socket; one test in the binary because it mutates `XDG_DATA_HOME`.
- `claude_code_spawn.rs`: `harness = false` in `Cargo.toml`; `main` re-execs itself as the fake CLI when `GANJA_FAKE_CLAUDE_SCRIPT` is set, so no real `claude` is spawned.
- `cursor_live.rs`: `#[ignore]`; network and the stored cursor login (command above).
- `auth_windows_acl.rs`: `#![cfg(windows)]`; empty elsewhere.

Fixture rules (`tests/fixtures/`):

- `.sse` files are recorded vendor shapes with synthetic ids (`chatcmpl-Fixture`, `gpt-test`) and a fixed `"created":1770000000`.
  Readers:  `anthropic_tests.rs`, `openai_tests.rs` and `sse_tests.rs` here; `ganja-core`'s `http.rs`, `compat_anthropic_wire.rs`, `compat_openai_wire.rs`, `compat_uncataloged.rs`, `oauth_wire.rs`, `opencode_dialects.rs` and `secrets_env.rs`. A rename is a compile error in both crates. A new frame shape needs a fixture and a socket test.
- `*-probe.txt` files are evidence recordings judged by a person. Never edit one to make a test pass. The three cursor probes are read by no test; the `{`-prefixed JSON lines of `claude-code-sdk-mcp-probe.txt` are read by `recorded_rate_limit_events` in `src/provider/claude_code_tests.rs`, so editing that file changes a test result.
- `claude-code-replay-run1.json` is derived: run 1's inbound frames sliced from `claude-code-sdk-mcp-probe.txt` under that file's scrub. Tests read the derived slice (`claude_code/frame_tests.rs`, `claude_code_tests.rs`). Re-derive it from the recording; never hand-edit it. If the two disagree, the recording is right.
- Scrub rule for any recording: written from the log, never from memory. No token, no tool-argument value, no reply text. Field names, enum arms, field numbers, timings, sizes and verbatim error `code`/`message` are allowed; where no error occurred, say so. `THIRD_PARTY_NOTICES.md` attributes the claude-code recordings.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers, the cursor and claude-code rulings, per-host identity evidence): `docs/decisions/ganja-provider.md`, `docs/decisions/ganja-provider-src.md`, `docs/decisions/ganja-provider-src-provider.md`, `docs/decisions/ganja-provider-tests-fixtures.md`, frozen from commit 35d1720. New decisions are recorded there, not here.
