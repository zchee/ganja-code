<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# ganja-tui/src

## Purpose

The frontend's event loop and the state it owns. One `tokio::select!` in `app.rs` folds three event sources into a single enum, mutates state in one place, and draws; the components under `component/` render that state and nothing else.

## Key Files

| File | Description |
|------|-------------|
| `lib.rs` | `run(resume, overrides)`: reads the config, resolves the key bindings, selects a provider, builds the agent registry, builds the `Engine` with builtin tools, project permission rules, both halves of the system prompt and its agents, then loads the theme set and applies the configured theme over the stored pick — all **before** the terminal is taken over, so every refusal is readable. Enters the alternate screen with mouse capture and restores the terminal on every exit path including panic. |
| `app.rs` | `App`: the `select!` loop, key handling, and the state every component renders from. Also holds the snapshot tests. |
| `event.rs` | `AppEvent { Term, Core, Tick }` — the one enum every event source folds into. Engine events are boxed because they dwarf the other variants. |
| `command.rs` | Both command populations — the UI `Entry` table (upstream's names, plurals and aliases) and the engine's `EngineCommand` roster — plus the `Choice` the `/` dropdown merges them into, and `nucleo-matcher` ranking over the result. Ranking parity with upstream's `fuzzysort` is explicitly not a goal; a total, deterministic order is. |
| `mention.rs` | The `@` trigger and the submit-time scan, sharing one rule: the last `@` before the cursor, preceded by start-or-whitespace, with no whitespace up to it. A mention that could be typed but not read back would attach nothing. |
| `external.rs` | `/editor`: seeds a temp file with the buffer, hands the terminal to `$EDITOR`, takes it back. Split so the seed/readback round trip is testable; the terminal hand-off itself is exercised by hand. |
| `keybind.rs` | Which keys reach which actions, the five-action curated set, and the `keybinds` config map that rebinds them. An unknown action name and an unparseable key string both fail the run naming what was wrong. |
| `theme/` | Loadable themes: upstream's JSON schema and resolver (`json.rs` — defs, dark/light variants, ANSI integers, cycle refusal), the builtin/custom registry with revisions (`registry.rs`), the persisted pick under the data home (`selection.rs`), and the `Theme` style slots (`mod.rs`). Four upstream themes ship verbatim from `../assets/themes/`; default is `opencode`. |
| `component/` | The three panes plus the modals (see `component/AGENTS.md`). |
| `snapshots/` | `insta` snapshots for the tests in `app.rs` (see `snapshots/AGENTS.md`). |

## For AI Agents

### Working In This Directory

- **No `select!` arm may await work of unbounded duration.** A prompt is handed to the engine, which answers through the event stream, and the loop goes straight back to drawing. Anything long-running is the engine's job, reported back as events.
- **`App::handle` is the only place that mutates state.** That is what lets components be tested without a terminal or a running turn — keep new state transitions inside it rather than pushing them into a component.
- **Frames coalesce, keystrokes do not.** A burst of streamed fragments redraws at most once per `FRAME` (16ms, ~60 FPS); a keystroke always redraws immediately. Those two rules together are what keep streaming cheap without making typing feel laggy.
- Nothing is shared with the engine but channels — no locks, no shared state.

### Testing Requirements

```sh
cargo test -p ganja-tui --lib
cargo insta review     # after an intentional visual change
```

Tests build an `App`, feed it `AppEvent`s directly, and render into a `TestBackend`; screen output is asserted with `insta::assert_snapshot!`. A behavior worth testing should be reachable by constructing the event, not by driving a terminal.

### Common Patterns

- Key handling reads as a match on `KeyCode` in one function, with the permission dialog's keys (`y` once / `a` always / `n` or `Esc` reject) resolved before the editor ever sees the keystroke.
- Enter submits; Enter with Shift, Alt or Control inserts a line break — all three modifiers mean the same thing because terminals disagree about which of them they can report.
- `Esc` cancels a streaming turn; `Ctrl-C`/`q` quit.

## Dependencies

### Internal

`ganja_core` — `Engine`, `Command`, `Event`, `PartBody`, `ToolState`, `PermissionReply`, `Usage`, `catalog` (pricing for the status bar).

### External

`ratatui` (+ `ratatui-textarea`), `tokio`, `futures`, `anyhow`, `unicode-width`, `serde_json`; `insta` for snapshots.

<!-- MANUAL: -->
