<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-28 -->

# ganja-teammate-local

## Purpose

This crate's `tmux.rs` is **not** the sealed-leaf `tmux` workspace crate — it is this workspace's own handful of `tmux` subprocess calls with the pane-identity rules P25b needs. It is not consumed by, and does not consume, the sealed leaf; CI's inverted gates over `tmux`'s own tree and over every other member's tree (`crates/AGENTS.md`) prove the two never touch, in either direction, and never will by accident.

The teammate backends that can only run on the machine the lead is running on: tmux panes, the `ganja` and `claude` panes split into them, and the three foreign CLIs — `codex`, `grok`, `agy` — driven in their own native TUIs. `ganja_core::teammate::TeammateBackend` is a seam with more than one adapter, which is what makes it a cut worth making (**D538**, **D539**; `.omc/plans/2026-08-28-teammate-seam-crate-split.md`) — every adapter here needs a tmux server, a shell to split into, or somebody else's binary on `PATH`, none of which an engine may hold. So this crate sits **above** `ganja-core` the way a frontend does: it names the engine, and the engine names nothing here, both directions asserted in CI rather than trusted — this crate's own internal dependency list is a closed allowlist, and `ganja-core`'s is unchanged by this crate's existence. The gate that is the split's whole point is the inverted one: **`ganja-serve` never links this crate.** The closure `ganja serve` ships is the closure a cloud worker will ship, and a worker must provably be unable to spawn a pane on somebody else's machine.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest, `publish = false`. Every dependency carries the reason it is there — `which` (PATH lookup for a pane's binary) and `libc` (unix `killpg` for a shim child's process group) both moved here from `ganja-core` with the backends that needed them. The two `harness = false` `[[test]]` entries (`teammate_pane_lifecycle`, `teammate_pane_env`) moved here too: each test binary is its own pane child, spawned as `current_exe()` with spawn flags libtest would refuse on sight. |
| `src/lib.rs` | The crate doc — the source of truth for what this crate is and why its `tmux.rs` is not the sealed leaf — and `pub fn backends(shell: PaneShell, share: PaneShare) -> Backends`, the one assembly a shipped session runs through — `ganja-tui`'s `run()` is its only caller. |
| `src/tmux.rs` | The `tmux` calls the two pane backends are built on (P25b). |
| `src/pane.rs` | A teammate in a `ganja` pane of its own (P25b); mints **D502** — an enumerated environment, and a launch line ≥2 words so tmux's own `$SHELL -c` detour cannot re-import credentials the enumeration left out. |
| `src/claude.rs` | A teammate that is a real `claude` pane (P25b); re-exports `teams_root`/`REFUSED_NO_CONFIG_DIR`/`TEAMS_DIR` (as `TEAMS_DIRECTORY`) from `ganja_core::teammate`, where W1 hoisted them because stayers (`lead_inbox.rs`, `engine.rs`) need them whether or not a Claude pane is ever spawned. |
| `src/shim.rs` (+ `src/shim/records.rs`) | A teammate that is another vendor's CLI driven through its own non-interactive door (**D508**, **D509**); the headless `ShimBackend` here is reachable only through the tests that drive it against a fake CLI, since **no spawn door in this build reaches it** (D512). `shim/records.rs` is the per-lead orphan record a shim child leaves behind under `/tmp/ganja-<uid>/`. |
| `src/shim_tui.rs` | The same three CLIs rendered in their own native TUI, in a pane of their own, spoken to through bracketed paste (P28, **D512**) — the door every shim spawn actually reaches, and where the `TuiDriver` companion trait is defined and implemented for all three CLIs. |
| `src/codex.rs` | A teammate that is a headless `codex exec` child, implementing the shared `Driver` trait both the headless shim door (`shim.rs`) and the TUI door (`shim_tui.rs`) dispatch through (**D508**, **D509**). |
| `src/grok.rs` | The same for a headless `grok` child (**D508**, **D509**, **D510**). |
| `src/agy.rs` | The teammate that **ships write-capable, and says so** (**Dv-7**, amending W4's measured no-ship): `--sandbox` bounds the terminal only, with no enforced filesystem bound, and the posture row names that absence rather than hiding it. |
| `src/reaper.rs` | Killing panes the lead left behind when it died (P25b), and sweeping shim orphan records under "no owner proof, no signal" (**D506**). |
| `src/readback.rs` | Carrying a shim pane teammate's answers back to its lead (**D515**): the per-CLI transcript readers, and `answers_clause`, the one clause that says what each CLI carries. |

## Architecture

`TeammateBackend` (`ganja_core::teammate`) and `Spawned` are the seam this crate implements — `spawn` yields a `Spawned` that owns launch, the bridge loop, liveness, the recent-calls ring and kill, so nothing outside a backend's own module knows *how* a member of that kind runs. Five external adapters implement it: `GanjaPane`/`ClaudePane` (`pane.rs`/`claude.rs`, a `ganja` or a real `claude` split into a tmux pane) and three `ShimTui` instances — one per foreign CLI (`codex.rs`, `grok.rs`, `agy.rs`) — each spoken to through bracketed paste in its own native TUI. A sixth implementation, the headless `ShimBackend` in `shim.rs`, exists and is fully tested but is reachable by **no spawn door in this build**: since **D512** every shim spawn opens a pane instead, so `ShimBackend` is exercised only by the tests that drive it against a fake CLI, through `ganja_testkit`.

`pub fn backends(shell: PaneShell, share: PaneShare) -> Backends` in `lib.rs` is the one assembly a shipped session runs through, called from `ganja-tui`'s `run()` and nowhere else (`ganja-cli` reaches the same adapters by starting that frontend; `ganja_testkit::externals()` deliberately assembles its own headless set instead): it builds the five external adapters and returns them for the caller to hand to `Engine::with_teammates`, which inserts its own `in-process` sixth entry — the one adapter an engine can build entirely out of its own provider, tool set and store, and the only `MemberBackend` this crate does not implement. A backend absent from an assembled map is refused by name at spawn rather than silently downgraded to `in-process` — `Backends::of` returns `Option`, and both call sites in `ganja-core`'s `subagent.rs` turn a miss into that refusal.

## For AI Agents

### Working In This Directory

- **This crate may name `ganja-core`; `ganja-core` must never name this crate.** The direction is the compiler's rule (CI's closed allowlist), not a convention a reviewer remembers — the same shape `ganja-tool`'s and `ganja-provider`'s own splits use.
- **Never link the sealed-leaf `tmux` workspace crate.** This crate's own `tmux.rs` is a from-scratch control-mode driver, not that crate; CI's `tmux consumes no ganja crate` / `no workspace member consumes tmux` pair proves the two stay unrelated in both directions, and the derived-member loop covers this crate with no edit.
- **`ganja-serve` must never be able to reach this crate**, even transitively — that is the split's whole reason to exist. An edge that would put this crate in `ganja-serve`'s normal tree is the one change that undoes it; CI's inverted gate catches it, but the design intent is to never need the catch.
- **A pane-mode shim has no per-turn deadline.** `teammates.shim_turn_timeout` governs only the headless `ShimBackend` this crate also holds, which no spawn door here reaches — do not wire it to `shim_tui.rs`'s panes.
- **A backend that cannot spawn refuses by name.** `backends()` never falls back silently; an absent or unassembled backend is a `Backends::of` miss the caller turns into a named refusal, never a downgrade to `in-process`.

### Testing Requirements

```sh
cargo nextest run -p ganja-teammate-local     # unit + integration, each test its own process
cargo test -p ganja-teammate-local --doc      # doctests
```

Nineteen `tests/` binaries carried over by the seam split, most needing either a reachable tmux server (`pane_support/mod.rs`) or a fake-CLI harness (`shim_support/mod.rs`); `teammate_no_tmux.rs`/`shim_tui_no_tmux.rs` are the pair that must pass on a machine with **no** tmux at all — the refusal-by-name path. Two groups sit outside that, and for opposite reasons. `teammate_pane_lifecycle.rs` and `teammate_pane_env.rs` are `harness = false` — each is its own `fn main()`, because the program `pane.rs` starts in a teammate's pane is `current_exe()` and libtest would refuse the spawn flags on sight — so `#[ignore]` is not a thing they can express: they need a reachable **tmux** and **hard-fail** without one, on the golden differential's posture that a run which tested nothing must not look green. The `*_live.rs` binaries are the other way round: `#[ignore]`d, inert unless `GANJA_LIVE_TEST=1` is set, and needing the real vendor CLI (`claude`, `codex`, `grok`, `agy`) on `PATH` — opt-in the same way `ganja-core`'s own live provider tests are. See the crate's own `tests/AGENTS.md`.

### Common Patterns

Every dependency is `x.workspace = true`; versions live in the root manifest with their rationale. `#[cfg(test)] #[path = "..._tests.rs"] mod tests;` is the sibling-file pattern the whole workspace uses (see the root `AGENTS.md`'s Testing Requirements); anything needing a real socket, a real tmux server, or process-wide state lives in `tests/` instead.

## Dependencies

### Internal

Exactly `ganja-core ganja-permission ganja-protocol ganja-provider ganja-storage ganja-team ganja-tool `, the string CI's allowlist asserts against this crate's `-e normal` tree. Four are named in the manifest: `ganja-core` (the engine this crate is an adapter for: `TeammateBackend`, `Spawned`, `Lent`, `SpawnSpec` and the `Backends` map `backends()` fills), `ganja-protocol` (`MemberBackend` and the team frames a preamble and a delivery are spelled in, named directly rather than through the engine's re-export), `ganja-team` (the teams directory format a spawn's record and a delivery's mailbox both write into), and `ganja-tool` — for `socket` alone: the session-socket directory scheme (**D505**/**D509**) a headless shim's orphan records are kept under, and the `vet_directory`/`prepare_directory` pair a sweep runs before it walks one. The remaining three arrive through `ganja-core` and are named nowhere here: `ganja-permission`, which the engine sits on, `ganja-provider`, whose Messages decoder `grok.rs`'s module doc cites as the thing it deliberately does *not* reuse — an intra-doc link through the engine's own `ganja_core::provider` facade, not a dependency of this crate — and `ganja-storage` (**D540**, W3), which this crate reaches only as whatever the engine it drives happens to persist through.

### External

`async-trait` (every backend is `Arc<dyn TeammateBackend>`, dispatched by name at spawn), `shlex` (a pane's launch line is a shell command line, and the config-named shell it splits into arrives already split — the one lexer both halves agree on with tmux's own), `tempfile` (a shim child's records and a pane's staged files are written through a temporary and renamed, the same rule the team file follows against the same reader), `which` (a pane-backed teammate resolves a binary this process may execute by asking the operating system rather than reading mode bits — moved here with the backends; nothing left in `ganja-core` looks a binary up), `libc` (unix only — a shim child is ended by signalling its process group), `serde`/`serde_json`, `thiserror`, `tokio` + `tokio-util` (`rt`, for `CancellationToken` — a backend's own token is a child of the registry's), `tracing`.

<!-- MANUAL: -->
