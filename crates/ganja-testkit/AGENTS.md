<!-- Parent: ../AGENTS.md -->
# ganja-testkit

Shared test doubles and builders for the workspace's integration suites. It is dev-only: every consumer lists it under `[dev-dependencies]`, and no shipped binary links it. It depends on `ganja-core`, `ganja-protocol`, `ganja-tool` and `ganja-teammate-local` as normal dependencies, so it sits above the engine.

## Boundary

- No `depgate.toml` rule names this crate. The dev-only rule is a convention: check `[dev-dependencies]` placement when adding a consumer.
- Consumers today (`rg ganja-testkit crates/*/Cargo.toml`): `ganja-core`, `ganja-provider`, `ganja-tui`, `ganja-serve`, `ganja-cli`, `ganja-teammate-local`, all under `[dev-dependencies]`.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | Module declarations and crate-root re-exports; the full public list is there. |
| `src/provider.rs` | `ScriptedProvider`, `Director`, and the script builders `says`, `tool_call`, `served`, `transcript`. |
| `src/tool.rs` | `RecorderTool`, `BlockingTool`, `placeholder_schema`, `tool_ctx`. |
| `src/drain.rs` | `drain`, `drain_answering`, `drain_allowing`, `held_at_dialog`: collect a turn's events, optionally answering permission dialogs. |
| `src/command.rs` | `prompt` (a bare-text `SendPrompt`) and `deadline_millis`. |
| `src/session.rs` | Storage seeding: `seeded_session_info`, `seed_session`, `seed_message`, `entries`, `plant_preuuid_store`, `PRE_UUID_ID`, `set_aside_of`. |
| `src/fs.rs` | `temp_dir`, `redirect_xdg_data_home` (`unsafe`), `Homes`, `plant`, `plant_pre_split_chatgpt_login`. |
| `src/subagent.rs` | `ScriptedSubagents`, `RecordingSpawner`. |
| `src/tasklist.rs` | `StaticTasks`, `task`, `task_summary`. |
| `src/agent.rs` | `agent_registry` from a fixture `Config`. |
| `src/teammate.rs` | Team fixtures: `externals`, `backends`, `team`, `team_file`, `seed_team_file`, `spawn`, `RunnerHarness`, `eventually` and the rest re-exported in `src/lib.rs`. |
| `src/title.rs` | `is_title_request`, `is_title_body`. |
| `src/log.rs` | `LogCapture` (`tracing-subscriber` capture, level chosen by the caller). |
| `src/tmux.rs` (`pub mod`) | `PrivateServer` (a tmux server on its own socket, killed on drop) and `require_tmux`. |
| `src/cursor_server.rs` (`pub mod`) | `CursorServer`, a scripted mock of the cursor Connect endpoint (`Step`, `finished`, `envelope`, `decoded`). |
| `src/responses_server.rs` (`pub mod`) | `serve() -> Endpoint`, a loopback Responses-API server recording each request as `Recorded`; `responses_transcript` is its canned SSE body. |
| `src/fake_claude.rs` (`pub mod`) | The fake `claude` CLI that replays a recorded stream-json `Script`, as a re-executed binary and in process. |
| `tests/tmux_scrub.rs` | Checks that `PrivateServer` scrubs inherited `$TMUX`/`$TMUX_PANE`. |

## Commands

```sh
cargo build -p ganja-testkit
cargo test -p ganja-testkit --doc           # the builder examples
cargo test --workspace --doc                # CI's doc-tests step (.github/workflows/ci.yaml), which runs these examples
cargo nextest run -p ganja-testkit          # the two sibling unit suites and tests/tmux_scrub.rs
```

## Conventions

- **A helper moves here only when two or more test binaries anywhere in the workspace need the identical shape.** A fixture one suite needs stays in that suite's file.
- **Share values, not shapes.** Provider ids, canned strings and exhaustion policy (`OnExhausted` in `src/provider.rs`) are constructor arguments. A double with a different behaviour, such as a script that never sends `ToolCallEnd`, stays local instead of gaining a flag.
- **Prefer a doctest for a pure builder.** Builders such as `says`, `tool_call`, `seeded_session_info`, `seed_message`, `temp_dir` and `agent_registry` carry runnable examples; `nextest` does not run doctests, which is why CI has a separate doc-tests step. Items that need a live engine turn carry no example.
- **A sibling `*_tests.rs` file only for a double a doctest cannot pin.** Two exist: `src/teammate_tests.rs` and `src/fake_claude_tests.rs` (the fake CLI speaks another tool's protocol, so drift in the fake must fail here, not look like wire drift).
- **`ScriptedProvider::new`, `::named` and `::strict` return `(Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>)`**: the provider and its request log.

## Gotchas

- **`redirect_xdg_data_home` is `unsafe`** because it mutates process-wide environment. Call it before any other thread starts and write a `// SAFETY:` comment at the call site stating why the binary meets that; the function's doc states the invariant.
- **`require_tmux` asserts, it does not skip.** Pane suites call it before starting a `PrivateServer`, so they fail with an assertion that names `tmux` on a machine without it (`tests/tmux_scrub.rs` does this).
- `tests/tmux_scrub.rs` mutates the environment, so it is one test in its own binary.
- `externals` points the foreign-CLI backends at an empty search path, so a spawn is refused by naming the missing binary, the same as on a machine without it. `backends` adds an in-process entry backed by `FakeProvider`, for `Teammates::new`; `externals` is what `Engine::with_teammates` takes.
- `CursorServer` bounds a hanging fixture with `PATIENCE` (20 s) per case; a test that waits on a drop should step the script instead of relying on the timeout.

## Tests

Unit tests exist only in `src/teammate_tests.rs` and `src/fake_claude_tests.rs`, wired through `#[path]`; no inline test module exists. `tests/tmux_scrub.rs` needs `tmux` on `PATH`. Doctests run through `cargo test --doc`, not nextest.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-testkit.md`, frozen from commit 35d1720. New decisions are recorded there, not here.
