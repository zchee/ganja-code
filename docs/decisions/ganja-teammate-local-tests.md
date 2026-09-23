<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-28 -->

# ganja-teammate-local/tests

## Purpose

Integration suites for the machine-bound teammate backends: tmux panes, the `ganja` and `claude` panes split into them, and the three foreign CLIs (`codex`, `agy`, `grok`) driven either headless or in their own native TUI. Moved here from `crates/ganja-core/tests` in W2 (**D539**) — the nineteen binaries below, `pane_support/`, `shim_support/` and the probe/transcript fixtures they read, byte-for-byte as they were before the move. A twentieth, `teammate_pane_exit.rs`, was born here after the move (**D541**, 2026-08-29). Each file is its own test binary; several deliberately hold exactly one test, for the same process-wide-state reason `ganja-core/tests/AGENTS.md` states.

Ten of the nineteen binaries (`teammate_shim.rs`, `teammate_shim_agy.rs`, `teammate_shim_codex.rs`, `teammate_shim_grok.rs`, `teammate_shim_sweep.rs`, `teammate_shim_env.rs`, `teammate_agy_live.rs`, `teammate_codex_live.rs`, `teammate_grok_live.rs`, `readback.rs`) carry their behavior in their own `//!` module doc rather than in the table below, which only holds the rows that already existed in `ganja-core/tests/AGENTS.md` before the move (two of them split, since they used to share a row with a binary that stayed behind).

## Key Files

| File | Description |
|------|-------------|
| `teammate_doors.rs` | **P25**: the `task` door reaching `spawn_teammate` and writing the member record (AC-14's engine-side half). One test, one binary, environment-mutating. |
| `teammate_backends.rs` | **P25**, explicit-roots binary that may hold several tests: the per-backend refusals and `an_unknown_backend_value_is_refused_naming_the_three` (AC-27). |
| `teammate_no_tmux.rs` | **P25 (AC-16)**, extended by **P27 (AC-3/Dv-1)**: a `ganja` or `claude` spawn without tmux is refused **readably** — the sentence, not just the error kind — and since Dv-1 made `ganja` the default, an **unnamed** backend is refused in that same sentence, while an `in-process` spawn asked for by name still succeeds. Together those three arms are what assert there is no silent fallback in any direction. Mutates `TMUX` — one test, one binary. |
| `teammate_pane_lifecycle.rs` / `teammate_pane_env.rs` | **P25 (D502)**, `harness = false` binaries: the test binary *is* the child, so `current_exe` re-executes it as the impostor teammate a pane would run. That is what lets the pane launch path be driven end to end without a real second product. |
| `teammate_pane_exit.rs` | **D541** (bead `ganja-code-okip`), `harness = false` for the same reason: a `ganja` pane member is spawned on a private server, its pane is `kill-pane`d out from under the lead, and an accumulating drain of `take_exited` must yield `Exited { cli: None, backend: Ganja, pane: PaneFate::Closed, .. }` with `alive()` already false and the member gone from the roster — the bead's own observation reproduced, then cured. |
| `teammate_reaper.rs` | **P25 (AC-12, D506)**: an orphaned pane is reaped at lead startup, and a **recycled** pane id is not killed. The second is the one that matters — the witness re-derives identity from what the pane is running, which is what closed both the suffix-collision kill and the co-tenant-lead kill demonstrated before the fix. The panes are stand-in shells wearing a teammate's two flags, on a private tmux server of each test's own (`ganja_testkit::tmux`); everything drives `reaper::sweep_on` over an explicit `TeamsRoot`. |
| `shim_tui.rs` / `shim_tui_no_tmux.rs` | **P28 (D512)**: the pane-mode shim against a **stub TUI exec'd into a real tmux pane** on a private server, driven by the real codex driver so the floors on the pane's argv are codex's (AC-1); a message lands in the stub's input as **one bracketed body and one Enter**, byte for byte, because the stub turns bracketed paste on (AC-2, F4); queued messages arrive whole and in order; a peer message carrying a paste terminator still arrives as one body (HIGH-1); a TUI that dies inside the readiness window is refused with its own last words and its dead pane closed (AC-5), one that shows its marker and then dies is never a live member (HIGH-2), one that never shows its marker is pasted into but never submitted (HIGH-3 — asserted on the frame cut at the last newline, because a canonical-mode pty withholds the unterminated tail; do not "fix" the stub), a failed delivery mails the sender and is never re-pasted (MEDIUM-5), and shutdown TERMs the group **while the pane is live** (AC-6, ruling F3). Several tests in one binary, environment untouched (`ShimTui::on`/`searching`); the refusal that needs `$TMUX` absent is the second binary (AC-4): refused by name, no headless fallback, stub never run. Hard-fails without tmux. |
| `teammate_claude_live.rs` | **P25 (AC-13)**, `#[ignore]` + `GANJA_LIVE_TEST=1`: a real `claude` pane round-tripping over the shared inbox. Beside the three pane binaries because the whole claim is about a `ganja-teammate-local` backend. Inert until somebody with a real `claude` binary opts in. |
| `pane_support/` | What the three pane binaries share, written once: the pane child, the spawn-and-report spine, and the `task` door (the private tmux server itself is `ganja_testkit::tmux`'s). **Not a test binary** — cargo does not discover `tests/*/mod.rs` as one — but a module both declare. It also explains the `harness = false`: `pane.rs` launches `current_exe()`, which inside a test binary is that binary, so the child arrives carrying `--agent-id` and its four companions, and libtest would refuse those flags on sight and close the pane in milliseconds. Each binary therefore opens by asking whether it is the child. Since **D541** the pane child and the server's first window are bounded at five minutes (`CHILD_LIFE`, `IDLE_WINDOW`): `PrivateServer`'s destructor kills the server on every exit that unwinds, but a test process a signal kills runs no destructor, and an orphan whose panes never end is immortal — bounded, it empties itself (the shim stubs' `cat` panes are not, bead `ganja-code-z471`). |
| `shim_support/` | The same shape for the three shim binaries: a CLI that is not one — POSIX shell scripts on a `PATH` the test owns, standing in for `codex`/`agy`/`grok`, with everything they were handed recorded to a file the test reads back. A process double would assert nothing about argv, environment, process-group death or a deadline, since none of those happen on this side of a `fork`; a script does. Not a test binary, shared the way `pane_support` is. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `fixtures/` | The foreign-CLI probe recordings (`{codex,agy,grok}-posture-probe.txt`, `{codex,agy,grok}-tui-probe.txt`) and the transcript-shape excerpts a shim pane's readback parses (`readback/{codex-rollout,grok-updates,agy-transcript}.jsonl`, D515). See `THIRD_PARTY_NOTICES.md`'s "Foreign CLI probe recordings" and "Transcript shapes" sections for what in them is attributed and why. |

## For AI Agents

### Working In This Directory

**Know what a binary needs before concluding a failure is a regression.**

- **Needs a real tmux binary on `PATH`, hard-fails rather than skips without one** (this build's standing convention, stated explicitly in each binary's own doc): `teammate_pane_lifecycle.rs`, `teammate_pane_env.rs`, `teammate_pane_exit.rs`, `teammate_reaper.rs`, `shim_tui.rs`. All five run against a *private* tmux server of the test's own (`ganja_testkit::tmux`), never the developer's.
- **Manipulate `$TMUX` to test the no-tmux refusal path, and need no real tmux server**: `teammate_no_tmux.rs`, `shim_tui_no_tmux.rs`, and the one `teammate_doors.rs` test that withdraws `TMUX` for the process-wide-state reason above.
- **`harness = false`, driven through `current_exe()`**: `teammate_pane_lifecycle.rs`, `teammate_pane_env.rs` and `teammate_pane_exit.rs` (declared in `Cargo.toml`'s `[[test]]` entries). The test binary is its own pane child — `pane.rs`'s spawn launches `current_exe()`, which inside a test binary is that binary carrying `--agent-id` and its four companions, and libtest would refuse those flags on sight. Each binary opens by calling `pane_support::pane_child_if_asked()` before running its one real test.
- **Drive a fake CLI (a `shim_support` shell script on a test-owned `PATH`), never a real vendor binary or tmux**: `teammate_shim.rs`, `teammate_shim_agy.rs`, `teammate_shim_codex.rs`, `teammate_shim_grok.rs`, `teammate_shim_sweep.rs`, `teammate_shim_env.rs`. These run on any machine with no setup.
- **Need a real vendor CLI on `PATH`, `#[ignore]`d and inert unless `GANJA_LIVE_TEST=1`**: `teammate_agy_live.rs` (`agy`), `teammate_codex_live.rs` (`codex`), `teammate_grok_live.rs` (`grok`), `teammate_claude_live.rs` (`claude`, and also a real tmux server — see above). Opt in with, e.g.:
  ```sh
  GANJA_LIVE_TEST=1 cargo test -p ganja-teammate-local --test teammate_codex_live -- --ignored --nocapture
  ```
  Each spends a real vendor's quota; a contributor running the ordinary suite spends nothing.
- **Need neither tmux nor a CLI, only checked-in state**: `readback.rs` (parses only `fixtures/readback/*.jsonl`) and `teammate_backends.rs` (explicit roots, no process spawn — it names `TMUX` only to point at `teammate_no_tmux.rs`, which owns that variable).

### Testing Requirements

```sh
cargo nextest run -p ganja-teammate-local
cargo nextest run -p ganja-teammate-local -E 'binary(shim_tui)'
```

### Common Patterns

- **One test per binary where process-wide state is mutated** — the same rule `ganja-core/tests/AGENTS.md` states, for the same reason: nextest runs each test in its own process, but a plain `cargo test` runs a binary's tests on parallel threads.
- **A private tmux server, never `$TMUX`**, for anything that needs a real pane: `ganja_testkit::tmux::PrivateServer`/`require_tmux`, so a suite never splits into whatever tmux the developer happens to be running.
- **A script, not a mock, for a foreign CLI double** — see `shim_support/mod.rs`'s own module doc for why: every claim under test is about a *process* (its argv, its environment, its process-group death, a deadline reaching it), none of which a double behind the `Driver` trait would exercise.

## Dependencies

### Internal

`ganja_core`'s public surface (the engine types these backends implement against) and `ganja_teammate_local`'s own public surface (the backends under test), the way `ganja-core/tests` consumes `ganja_core`.

### External

`tokio` (with `net`, for the pane/shim suites that answer a fake CLI over a loopback socket rather than mocking the child), `tempfile`, `serde_json`, plus `ganja_testkit` (dev-dependency) for the scripted-provider/tmux/storage-seeding scaffolding these suites share.

<!-- MANUAL: -->
