<!-- Parent: ../AGENTS.md -->
# ganja-teammate-local

The teammate backends that need the lead's own machine: tmux panes, the `ganja` and `claude` panes split into them, and the `codex`, `grok` and `agy` CLIs driven in their own TUIs. The crate sits above `ganja-core` the way a frontend does: it names the engine, and the engine never names it. It must never be reachable from `ganja-serve`. The crate doc in `src/lib.rs` states the reasons.

## Boundary

- `depgate.toml` `[rules."ganja-teammate-local"]`: `internal = ["ganja-core", "ganja-permission", "ganja-protocol", "ganja-provider", "ganja-storage", "ganja-team", "ganja-tool"]`.
- `depgate.toml` `[rules."ganja-serve"]`: `deny = ["ratatui*", "ganja-teammate-local"]`.
- `depgate.toml` `[rules.tmux]` has `sealed = true`: this crate may not link the workspace `tmux` crate. `src/tmux.rs` here is a separate module of one-shot `tmux` client calls.
- `ganja-core` does not re-export this crate. `ganja-tui` (`src/lib.rs`) calls `backends` and the `tmux`/`reaper` functions directly.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | `backends(shell: PaneShell, share: PaneShare) -> Backends`, the only assembly function: `GanjaPane`, `ClaudePane`, and `ShimTui` over `Codex`, `Agy`, `Grok` |
| `src/pane.rs` | `GanjaPane`, `PaneMember`, `PaneShell` (`teammates.shell`), `PaneShare` (`teammates.pane_share`, `DEFAULT_SHARE` 65), `CARRIED_ENV`, the spawn flags (`AGENT_ID`, `PARENT_SESSION_ID`, ...) |
| `src/claude.rs` | `ClaudePane`: a real `claude` in a pane |
| `src/tmux.rs` | `Server` (`current`, `split`, `column_bottom`, `capture`, `paste`, `paste_submit`), `LAUNCH_HEAD`, `launch_line`, `REFUSED_NO_TMUX` |
| `src/shim_tui.rs` | `ShimTui`, `TuiDriver`, `TuiPane`, readiness constants, `paste_body`, `launch_line` |
| `src/shim.rs`, `src/shim/records.rs` | Headless `ShimBackend` and the `Driver` trait; `admits` (env filter); orphan records under the session socket directory |
| `src/codex.rs`, `src/grok.rs`, `src/agy.rs` | Per-CLI `Driver`/`TuiDriver`: argv, `TUI_ARGV`, `READY_MARKER`, sandbox constants |
| `src/liveness.rs` | `LIVENESS_POLL` (2 s) and `Gone`: the pane liveness check `pane.rs` and `shim_tui.rs` share |
| `src/reaper.rs` | `sweep`, `sweep_on`, `sweep_shims`: cold-start cleanup of a dead lead's panes and shim children |
| `src/readback.rs` | Per-CLI transcript readers (`of`, `Transcript`) and `answers_clause` |
| `tests/pane_support/mod.rs` | Pane-child entry (`pane_child_if_asked`), `CHILD_LIFE`/`IDLE_WINDOW` (300 s) |
| `tests/shim_support/mod.rs` | Shell-script stand-ins for `codex`/`agy`/`grok` on a test-owned `PATH` |
| `tests/fixtures/` | `{codex,agy,grok}-{posture,tui}-probe.txt` and `readback/*.jsonl`; attribution in `THIRD_PARTY_NOTICES.md` "Foreign CLI probe recordings" |

## Commands

```sh
cargo nextest run -p ganja-teammate-local
cargo test -p ganja-teammate-local --doc
cargo nextest run -p ganja-teammate-local -E 'binary(shim_tui)'
GANJA_LIVE_TEST=1 cargo test -p ganja-teammate-local --test teammate_codex_live -- --ignored --nocapture   # also _grok_live, _agy_live
GANJA_LIVE_TEST=1 GANJA_LIVE_CLAUDE_SEED=/path/to/claude-seed.json \
  cargo test -p ganja-teammate-local --test teammate_claude_live -- --ignored --nocapture
```

## Conventions

- Sandbox flags per CLI are constants; change the constant, never an argv literal.
- codex: `-s workspace-write` on a first headless turn, plus `-c sandbox_mode="workspace-write" -c approval_policy="never"` on every turn and in the pane (`SANDBOX_VALUE`, `SANDBOX_OVERRIDE`, `APPROVAL_OVERRIDE`, `TUI_ARGV` in `src/codex.rs`). `codex exec resume` accepts no `-s`, so later turns rely on the `-c` overrides alone.
- grok: `--sandbox workspace --permission-mode acceptEdits` on every turn and in the pane (`SANDBOX_VALUE`, `PERMISSION_MODE`, `TUI_ARGV` in `src/grok.rs`).
- agy: bare `--sandbox`, which bounds the terminal only, not the filesystem (`TUI_ARGV` in `src/agy.rs`).
- A flag change must also re-record the matching `tests/fixtures/*-probe.txt`; the unit tests compare the constants against the recordings (pinned by `src/codex_tests.rs`, `src/grok_tests.rs`, `src/agy_tests.rs`).
- Every pane launch line starts with `tmux::LAUNCH_HEAD`, a `printf` of `ESC[2J ESC[3J ESC[H`, so the idle shell clears its screen and scrollback before `exec` (pinned by `src/tmux_tests.rs`).
- A launch line has at least two words, so tmux does not run it through `$SHELL -c`; the pane gets only the enumerated `CARRIED_ENV` (pinned by `tests/teammate_pane_env.rs`).
- The first teammate splits the lead `-h -l <share>%`; later ones split the column bottom `-v`, found from tmux geometry by `Server::column_bottom` (see `src/tmux.rs`).
- Delivery to a TUI is one client call, `load-buffer -b <name> -` then `paste-buffer -p -d`, with the text on stdin and never in argv; the body passes through `paste_body` first (pinned by `src/tmux_tests.rs`, `tests/shim_tui.rs`).
- `admits` refuses every `GROK_*` variable except `GROK_HOME`; the TUI drivers add `CODEX_HOME`/`GROK_HOME` (pinned by `src/shim_tui_tests.rs`).
- A backend that cannot spawn returns a refusal that names it; nothing falls back to `in-process` (pinned by `tests/teammate_no_tmux.rs`, `tests/teammate_backends.rs`).

## Gotchas

- Readiness poll: `READY_POLL` 250 ms for up to `READY_WAIT` 15 s, looking for the driver's `READY_MARKER`, then `READY_SETTLE` 1 s before the first paste. Without the marker the body is pasted but Enter is not sent (`src/shim_tui.rs`).
- A pane-mode shim has no per-turn deadline. `teammates.shim_turn_timeout` (`shim::TIMEOUT_KEY`) applies only to the headless `ShimBackend`, which no spawn path in `backends` reaches; tests reach it through `ganja_testkit`.
- `reaper` kills a pane only when its running argv carries both `--agent-id` and this lead's `--parent-session-id`, never on a recorded pane id, because tmux reuses `%N` ids (pinned by `tests/teammate_reaper.rs`).
- A pane member's liveness watch posts `Exited` with a `PaneFate` when its pane closes; `kill()` cancels the watch first (`src/pane.rs`; exit pinned by `tests/teammate_pane_exit.rs`).
- `backends` searches the real `PATH`. Tests must build backends with `ganja_testkit::externals()` or `ShimTui::searching`, never with `backends`.

## Tests

Unit tests are sibling `*_tests.rs` files attached with `#[path]`. Each `tests/*.rs` file is its own binary, and its `//!` header states its prerequisites. Binaries that mutate process-wide state (`TMUX`, env) hold one test each.

- Need `tmux` on `PATH` and hard-fail without it (`ganja_testkit::tmux::require_tmux`): `teammate_pane_lifecycle`, `teammate_pane_env`, `teammate_pane_exit`, `teammate_reaper`, `shim_tui`.
- Those binaries each run a private server with `-f /dev/null` (`ganja_testkit::tmux::PrivateServer`). The code needs tmux 3.2 or newer (`split-window -e`, `src/tmux.rs`); CI's `test` job runs Homebrew tmux (3.7c or newer), and `claude-live.yaml` runs the ubuntu apt tmux.
- `teammate_pane_lifecycle`, `teammate_pane_env`, `teammate_pane_exit` are `harness = false` (`Cargo.toml`): the binary is its own pane child and calls `pane_support::pane_child_if_asked()` first.
- Need no tmux: `teammate_no_tmux`, `shim_tui_no_tmux` (they unset `TMUX`), the `teammate_shim*` binaries (fake CLIs from `shim_support`), `readback`, `teammate_backends`, `teammate_doors`.
- `teammate_{codex,grok,agy,claude}_live` are `#[ignore]`, run only with `GANJA_LIVE_TEST=1`, and need the real CLI on `PATH`. `teammate_claude_live` also needs tmux and reads `GANJA_LIVE_CLAUDE_SEED` (a `.claude.json` that pre-approves `ANTHROPIC_API_KEY`); `.github/workflows/claude-live.yaml` writes one.
- Under nextest's `ci` profile a test past 4 minutes is terminated (`.config/nextest.toml`).

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-teammate-local.md` and `docs/decisions/ganja-teammate-local-tests.md`, frozen from commit 35d1720. New decisions are recorded there, not here.
