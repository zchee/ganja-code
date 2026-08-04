<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# ganja-tui

## Purpose

The ratatui frontend. It owns every pixel and no engine logic: terminal events become `ganja_core::Command`s, engine `Event`s become frames. Entry point is `run()` in `src/lib.rs`, which reads the config, resolves the key bindings, selects a provider, builds the `Engine` with the builtin tools, the project's permission rules, the agent roster, the MCP servers, the language servers, the working-tree snapshots `/undo` restores from, and both halves of the system prompt, loads the theme set — all before the terminal is taken over, so every refusal is readable — and then hands the engine to the `App` loop.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest. Note `crossterm` is a dependency **not named in the source**: it exists to turn on the `event-stream` feature on the crate instance `ratatui` re-exports, which the re-export alone cannot request. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/` | Event loop, components, theme (see `src/AGENTS.md`) |

## For AI Agents

### Working In This Directory

- **Terminal restoration is an invariant, not a nicety.** `run()` restores on every exit path including panic: the hook installed here undoes mouse capture, then defers to the one `ratatui::try_init` installed, which leaves raw mode and the alternate screen. A new terminal mode must be undone the same way, or a crash leaves the user's shell unusable.
- The frontend keeps its own copy of the selected model so it can price a turn without reaching into the engine.
- Rendering is driven entirely by engine events. If the UI needs to show something the engine does not report, the fix is a protocol event in `ganja-core`, not local bookkeeping in the frontend.

### Testing Requirements

```sh
cargo test -p ganja-tui
cargo insta review     # after intentional visual changes
```

Components are tested without a terminal by feeding `AppEvent`s to `App::handle` and rendering into a `TestBackend`; screen assertions are `insta` snapshots under `src/snapshots/`.

### Common Patterns

Widget state lives in the component struct, never in a global; components expose `handle`-shaped methods that the app calls, so no component reaches back into the engine on its own.

## Dependencies

### Internal

`ganja-core` — for `Engine`, `Command`, `Event`, `catalog` pricing, and the permission types the dialog renders.

### External

`ratatui` 0.30 + `ratatui-textarea` (editor widget), `crossterm` (feature enablement only), `tokio`, `futures`, `anyhow`, `unicode-width` (the chat viewport measures its own wrap widths rather than leaning on `Paragraph`), `serde_json` (rendering tool metadata), `pulldown-cmark` + `syntect` (assistant-reply markdown and fenced-code highlighting), `arboard` (clipboard, text only), `insta` (dev).

<!-- MANUAL: -->
