<!-- Parent: ../AGENTS.md -->
# ganja-core

The engine: the agent loop, event delivery, subagents and in-process teammates, MCP and LSP clients, config loading, slash commands and the system prompt. It must never depend on a terminal library or an HTTP server, so the engine runs headless in tests and can be served over a socket.

It sits above `ganja-protocol`, `ganja-permission`, `ganja-tool`, `ganja-provider`, `ganja-team` and `ganja-storage`. `ganja-tui`, `ganja-cli`, `ganja-serve`, `ganja-teammate-local` and `ganja-testkit` depend on it.

## Boundary

- `[rules."ganja-core"] deny = ["ratatui*", "axum*"]`: anything a frontend renders is a serde type in `ganja-protocol`, never a widget or a style.
- `[rules."ganja-core"] internal = ["ganja-permission", "ganja-protocol", "ganja-provider", "ganja-storage", "ganja-team", "ganja-tool"]`: an exact set. None of those six may depend on this crate; each has its own `internal` rule in `depgate.toml`.
- A tool that needs engine state gets a field on its `ToolCtx`; a wire gets a field on its `ChatRequest`.
- `[rules."ganja-teammate-local"] internal = [..., "ganja-core", ...]`: the tmux-pane and foreign-CLI backends live above the engine and implement `teammate::TeammateBackend` there. `InProcess` in `src/teammate.rs` is the only backend in this crate.
- `src/lib.rs` re-exports the six crates as the modules `protocol`, `permission`, `project`, `tool`, `watch`, `auth`, `catalog`, `storage`, `snapshot` and `team`.
- `provider` is not a bare re-export: `src/provider.rs` globs `ganja_provider::provider::*` and adds the half that reads a `Config` (`select`, `configured_provider`, `defaulted_model`). The crate root also flat-re-exports a few types from beneath (`Credential`, `ModelInfo`, `Storage`, `Snapshots` and their kin); `src/lib.rs` is the list.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | Module list, the facade re-exports, the crate root's flat re-exports. |
| `src/engine.rs` | `Engine`: commands in, ordered events out; `Fanout` delivery; turn lifecycle; current session id. |
| `src/session.rs` | One turn's agent loop, the call resolver, mention and skill expansion, request-only blocks. |
| `src/subagent.rs` | Crate-private. The child loop a `task` call runs, and the cross-session postbox arm. |
| `src/teammate.rs` | `TeammateRegistry`, `TeammateBackend`, `Spawned`, `InProcess`. |
| `src/teammate/` | Teammate internals: inbound admission, lead inbox, identity, preamble, dialog routing, receipts, task list, runner, lead-turn guards. |
| `src/provider.rs` | Provider and model selection from a `Config`, over a glob of `ganja-provider`. |
| `src/responses_ladder.rs` | Per-request Responses options (`service_tier`, `/fast`) resolved from a per-turn seed. |
| `src/config.rs`, `src/config/legacy.rs` | `ganja.toml` discovery, decode and tier merge; the legacy JSONC reader used by `ganja config migrate` and import. |
| `src/hook.rs` | Config hooks for nine events, spawned under the `bash` tool's shell. |
| `src/job.rs` | `JobRegistry`: background `bash` jobs, `bash_output`, `kill_shell`. |
| `src/mcp.rs` | MCP servers and the `mcp__<server>__<tool>` tools they lend; reconnect, OAuth. |
| `src/lsp/` | Opt-in language servers (`rust`, `gopls` builtins) and the diagnostics appended to tool results. |
| `src/plugin.rs` | Claude Code plugin and marketplace manifests, install store, per-surface merge. |
| `src/command.rs` | Slash commands: builtins `/init` and `/team`, config and file commands, expansion. |
| `src/agent.rs` | Agent roster (build, plan, general, explore, six `/team` roles), config and file overlay. |
| `src/instruction.rs` | System prompt assembly: `base_prompt`, the `<env>` block, `AGENTS.md` discovery. |
| `src/attachment.rs` | Mime table for `@` attachments and `read_bounded`. |
| `src/prompt/` | Prompt texts compiled in with `include_str!`. Tool descriptions live in `ganja-tool/src`. |
| `tests/` | Over 100 integration binaries; each file's `//!` header states what it pins and what it needs. |
| `tests/fixtures/golden/` | Task scripts for the upstream differential in `tests/golden.rs`. |
| `tests/fixtures/mcp/` | MCP servers on upstream's `@modelcontextprotocol/sdk`, spawned by `tests/mcp.rs`. |
| `tests/fixtures/*-identity-probe.txt` | Recordings of what the codex and xAI backends were told; cited by `ganja-provider` auth code. |

## Commands

```sh
cargo nextest run -p ganja-core                        # unit + integration
cargo nextest run -p ganja-core --profile ci           # CI's profile (.config/nextest.toml): no retries, no fail-fast
cargo nextest run -p ganja-core -E 'binary(golden)' --no-capture
cargo test -p ganja-core --doc                         # doctests; nextest does not run them
cargo depgate check --config depgate.toml              # the Boundary rules above
# live suites: #[ignore]d and inert without GANJA_LIVE_TEST=1 plus the vendor key
GANJA_LIVE_TEST=1 ANTHROPIC_API_KEY=... cargo test -p ganja-core --test live_agent -- --ignored
GANJA_LIVE_TEST=1 OPENAI_API_KEY=... cargo test -p ganja-core --test live -- --ignored --nocapture --test-threads=1
# rewrites tests/fixtures/codex-identity-probe.txt; needs `ganja auth login chatgpt`
GANJA_LIVE_TEST=1 cargo nextest run -p ganja-core -E 'binary(codex_identity_probe)' --run-ignored all --no-capture
```

## Conventions

- `src/prompt/plan.txt`, `build-switch.txt` and `explore.txt` are byte-verbatim copies of opencode v1.18.22. Do not edit, reflow or correct them (attributed in `THIRD_PARTY_NOTICES.md`).
- `anthropic.txt`, `gpt.txt`, `default.txt` and `initialize.txt` are derived from upstream. A diff against upstream may contain only three substitution classes:
  - the agent name: `OpenCode`/`opencode` → `Ganja Code`;
  - the repository and docs URLs: `anomalyco/opencode` and `opencode.ai` → `https://github.com/zchee/ganja-code`, keeping the path upstream gave (`/issues` in `default.txt`);
  - the config file name: `opencode.json` → `ganja.toml`.
- No test enforces the two rules above. Before committing a change to an upstream-derived prompt, diff it against the upstream checkout. `THIRD_PARTY_NOTICES.md` states these three classes and points at this file.
- `team.txt` and the six role prompts (`analyst`, `executor`, `verifier`, `critic`, `debugger`, `reviewer`) are this project's own prose: never add them to `THIRD_PARTY_NOTICES.md`. Porting an upstream text takes three changes: the file, its consumer (`src/instruction.rs` or `src/agent.rs`), and a notices entry.
- The usage line in `team.txt` must equal `TEAM_ARGUMENT_HINT` in `src/command.rs`, and the expanded template must not name `teammate_terminated` (pinned by `tests/team_command.rs`).
- Tool results are information, not control flow: a refused permission, an unknown tool, bad arguments or a failed tool become error text the model reads next request. Only a user cancel or a dead provider end a turn early, and there is no step cap (pinned by `tests/agent_loop.rs`).
- Delivery is per subscriber: `subscribe()` is lossless and makes the publisher wait; `subscribe_droppable()` is evicted whole and its stream ends with `Evicted`. Do not replace `Fanout` with `tokio::sync::broadcast` (pinned by `tests/delivery.rs`).
- Reminders (plan and build notices, the stale-file notice) and `Message::request_only_user` blocks go on the request only and never into the stored transcript (pinned by `src/session_tests.rs`, `src/engine_tests.rs`).
- A wording a test asserts byte-for-byte changes in the same commit as that test's literal.

## Gotchas

- Root turns are serial: a prompt sent while a turn streams or waits on a permission returns `Busy`. Only consecutive `task` calls run concurrently, inside a turn.
- `JobRegistry` and `TeammateRegistry` hold their own root cancellation tokens; cancelling a turn does not reach a background job or a teammate.
- LSP, snapshot and file-watch failures never fail a tool call or a turn. LSP diagnostics are appended in `src/session.rs` after `tool.run`, not inside `edit`, `write` or `read`.
- `Engine::watch_files()` touches no filesystem; directories are registered on the watcher's own task. Nothing on a startup path may register a watch.
- The tool surface changes only between turns: `Engine::refresh_mcp` runs at turn start, so an MCP connect finishing mid-request does not change that request.
- An MCP tool asks by default through `MCP_PREFIX` in `ganja-permission`, a prefix match. Do not add MCP tool names to `ASK_BY_DEFAULT`.
- `Command::Undo` deletes nothing: it records an anchor. The anchor and everything after it are deleted at the next `SendPrompt` or `RunShell`.
- A hook runs with the user's authority and crosses no permission dialog. Only an explicit `permissionDecision: "allow"` on a clean exit allows a call; a killed or timed-out hook never does (`src/hook.rs`).
- `config.rs` refuses unknown keys, and a legacy config file (`LEGACY_FILES`) is a refusal naming `ganja config migrate`. A new key goes into `schema/ganja-config.schema.json` too (pinned by `tests/config_schema.rs`). Permission rules keep document order (pinned by `src/config_tests.rs`).
- Ids are UUIDv7 (`Uuid::now_v7` in `ganja-protocol`) and sort by creation. Ids read from disk are taken verbatim.
- `storage` and `snapshot` are re-exports of `ganja-storage`; edit them there.

## Tests

- Unit tests live in a sibling `<module>_tests.rs`, declared `#[cfg(test)] #[path = "<module>_tests.rs"] mod tests;` (`src/lsp/mod.rs` uses `mod_tests.rs`). The one inline `mod tests` block is in `src/job.rs`; do not add another.
- CI runs `cargo nextest run --locked --workspace --profile ci` with `GANJA_OPENCODE_DIR` pointing at a fresh upstream checkout (`.github/workflows/ci.yaml`). The `ci` profile gives `binary(cancel)` the whole runner.
- Suites that need setup, and hard-fail rather than skip without it:
  - `golden.rs`: `bun` on `PATH` and an opencode v1.18.22 checkout with `bun install` run, at `GANJA_OPENCODE_DIR`, or else at the gitignored default path the `golden.rs` header names (vendor it yourself).
  - `mcp.rs`, and the MCP rebuild tests in `plan_enter.rs` and `plan_exit.rs`: the same checkout plus its installed `@modelcontextprotocol/sdk`. The test passes the SDK path to the fixture server as `GANJA_MCP_SDK_DIR`; you do not set it. `mcp_oauth.rs` needs no setup.
  - `lsp.rs`: `rust-analyzer` on `PATH`. `GANJA_LSP_EDIT_BUDGET_MS` widens the timed-edit budget (default 3000 ms) on a slow machine.
  - `undo.rs`, `rewind.rs`: `git` on `PATH`.
- `#[ignore]`d binaries: `live.rs`, `live_agent.rs`, `codex_identity_probe.rs`. They are inert unless `GANJA_LIVE_TEST=1` and the vendor credential are set (see Commands).
- Environment and working directory are process-wide, and plain `cargo test` runs a binary's tests on parallel threads. A binary that mutates either does it in exactly one test, or once under a `LazyLock`/`Once`/lock that every test enters first (`team_command.rs`, `effort.rs`, `nested_agents.rs`). Its other tests must not read the environment.
- Exceptions to one-test-per-binary: `golden.rs` and `undo.rs` each hold a second test that reads no environment, although their `SAFETY` comments say "one test"; `rewind.rs` and `config_schema.rs` hold one env-mutating test beside others. Add a new env-mutating test in a new file.
- Redirect `XDG_DATA_HOME` (`ganja_testkit::fs::redirect_xdg_data_home`) in anything that touches stored state. Provider suites serve real HTTP on a loopback listener instead of mocking the client. Assert on a key's redacted tail, never the whole key.
- `golden.rs` serves its scripts only to requests carrying a `tools` array, because upstream opens with a toolless title request. Keep any new engine bookkeeping request toolless.
- Golden task files are hand-written scripts, not recordings: `golden.rs` runs both agents live each time, and nothing regenerates them. `steps` are canned model answers served in order; no step may depend on a tool's output.
- Golden tool names and argument keys are upstream's (`filePath`, `oldString`), and absolute paths compare through a `<CWD>` placeholder. A task must not call `websearch` (pinned by `no_golden_fixture_asks_for_websearch`).
- Recorded fixtures (the two `*-identity-probe.txt` files, and the SSE bodies these suites `include_str!` from `ganja-provider/tests/fixtures/`) capture what a real implementation sent. A person judges them; never edit one to make a test pass. A new event-stream shape belongs in `ganja-provider/tests/fixtures/`.
- `codex-identity-probe.txt` is regenerated only by running `codex_identity_probe.rs` (see Commands). `grok-identity-probe.txt` has no probe binary; it is composed by hand from a real login's CLI output, as its header says.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-core.md`, `ganja-core-src.md`, `ganja-core-src-prompt.md`, `ganja-core-tests.md`, `ganja-core-tests-fixtures.md` and `ganja-core-tests-fixtures-golden.md`, frozen from commit 35d1720. New decisions are recorded there, not here.
