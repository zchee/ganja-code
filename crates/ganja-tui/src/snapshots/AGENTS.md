<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# snapshots

## Purpose

`insta` snapshots of rendered screens, written by the tests in `../app.rs`. They are the frontend's regression net: a change in layout, wrapping or tool rendering shows up here as a diff instead of going unnoticed.

## Key Files

| File | Description |
|------|-------------|
| `ganja_tui__app__tests__snapshot_permission_dialog_open.snap` | The permission modal over a transcript. |
| `ganja_tui__app__tests__snapshot_permission_dialog_with_a_call_too_long_to_fit.snap` | The same modal overflowing: the reply keys stay, the cut is flagged with an explicit `+N lines not shown`. |
| `ganja_tui__app__tests__snapshot_tool_pending.snap` | A tool call reported but not yet running. |
| `ganja_tui__app__tests__snapshot_tool_running.snap` | A tool call in flight. |
| `ganja_tui__app__tests__snapshot_tool_completed_with_a_diff.snap` | A completed call whose metadata carries a unified diff. |
| `ganja_tui__app__tests__snapshot_tool_error.snap` | A call that failed, as the user sees it. |
| `ganja_tui__app__tests__snapshot_sessions_picker_open.snap` | The sessions picker over a transcript, newest selected. |
| `ganja_tui__app__tests__snapshot_sessions_picker_after_moving_the_selection.snap` | The same picker after `j`, proving the marker follows the selection. |
| `ganja_tui__app__tests__snapshot_themes_dialog_open.snap` | The theme list over a transcript, active theme marked. |
| `ganja_tui__app__tests__snapshot_theme_{opencode,tokyonight,gruvbox,aura}.snap` | One style-aware frame per ported theme — symbol runs with fg/bg/modifiers, which is what actually pins a palette. |
| `ganja_tui__app__tests__snapshot_palette_open.snap` | The command palette over a transcript: the suggested block pinned above the groups, keys on the right. |
| `ganja_tui__app__tests__snapshot_palette_filtered.snap` | The same palette after one character, proving the pinned block drops out and the list narrows. |
| `ganja_tui__app__tests__snapshot_palette_selection_styling.snap` | One style-aware frame of the palette: the selected row is filled rather than tinted, which a symbol-only dump cannot pin. |
| `ganja_tui__app__tests__snapshot_command_menu_open.snap` | The inline command menu above the editor, raised by a leading slash. |
| `ganja_tui__app__tests__snapshot_agents_dialog_open.snap` | The agent list, the running agent marked and the status bar naming it. |
| `ganja_tui__app__tests__snapshot_help_dialog_open.snap` | The reference card: commands with their keys, then the bindings no command shows. |
| `ganja_tui__app__tests__snapshot_file_menu_open.snap` | The inline file menu above the editor, raised by an `@` and offering what the project holds. |
| `ganja_tui__app__tests__snapshot_shell_mode.snap` | The composer flipped to shell mode: the box titled `Shell`, the footer offering the way out. |
| `ganja_tui__app__tests__snapshot_shell_output_streaming.snap` | A `!` passthrough mid-command, its newest output redrawn under the running row. |
| `ganja_tui__app__tests__snapshot_task_running.snap` | A delegated turn as one inline row: the agent, the ask, and the tool the child is in. |
| `ganja_tui__app__tests__snapshot_task_completed.snap` | The same row finished — count and duration, and never the child's own answer. |
| `ganja_tui__app__tests__snapshot_permission_dialog_with_directories.snap` | The permission modal for a call that reaches outside the project, listing where. |

## For AI Agents

### Working In This Directory

- **Never hand-edit a `.snap` file.** Change the code, run the test, then accept the new output with `cargo insta review` (or `INSTA_UPDATE=always cargo test -p ganja-tui`). Editing the snapshot directly makes the test agree with a screen nobody rendered.
- **A snapshot diff is a question, not a failure.** Read it before accepting: they cover exactly the states where a rendering regression would otherwise be invisible, so an unexpected change in one is usually a real bug in wrapping, layout or state mapping.
- Filenames are generated from the module path and test name (`ganja_tui__app__tests__<test>`), so renaming a test orphans its snapshot — delete the stale file in the same change.
- `.snap.new` files are unaccepted results; they must not be committed.
- Two dump shapes exist: `screen()` captures symbols only (palette-independent — a theme change must NOT diff these), while `styled_screen()` captures symbol runs plus fg/bg/modifiers (the per-theme snapshots). Pick the one that matches what the test is pinning.

### Testing Requirements

```sh
cargo test -p ganja-tui --lib
cargo insta review
```

<!-- MANUAL: -->
