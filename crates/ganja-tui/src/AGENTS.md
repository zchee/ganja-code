<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# ganja-tui/src

## Purpose

The frontend's event loop and the state it owns. One `tokio::select!` in `app.rs` folds three event sources into a single enum, mutates state in one place, and draws; the components under `component/` render that state and nothing else.

## Key Files

| File | Description |
|------|-------------|
| `lib.rs` | `run()`: selects a provider from the environment, builds the `Engine` with builtin tools and project permission rules, enters the alternate screen with mouse capture, and restores the terminal on every exit path including panic. |
| `app.rs` | `App`: the `select!` loop, key handling, and the state every component renders from. Also holds the snapshot tests. |
| `event.rs` | `AppEvent { Term, Core, Tick }` — the one enum every event source folds into. Engine events are boxed because they dwarf the other variants. |
| `theme.rs` | The P1 palette: three roles, no configuration. Themes become loadable data in P5. |
| `component/` | The three panes plus the permission modal (see `component/AGENTS.md`). |
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
