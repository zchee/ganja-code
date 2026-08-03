<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# snapshots

## Purpose

`insta` snapshots of rendered screens, written by the tests in `../app.rs`. They are the frontend's regression net: a change in layout, wrapping or tool rendering shows up here as a diff instead of going unnoticed.

## Key Files

| File | Description |
|------|-------------|
| `ganja_tui__app__tests__snapshot_permission_dialog_open.snap` | The permission modal over a transcript. |
| `ganja_tui__app__tests__snapshot_tool_pending.snap` | A tool call reported but not yet running. |
| `ganja_tui__app__tests__snapshot_tool_running.snap` | A tool call in flight. |
| `ganja_tui__app__tests__snapshot_tool_completed_with_a_diff.snap` | A completed call whose metadata carries a unified diff. |
| `ganja_tui__app__tests__snapshot_tool_error.snap` | A call that failed, as the user sees it. |
| `ganja_tui__app__tests__snapshot_sessions_picker_open.snap` | The sessions picker over a transcript, newest selected. |
| `ganja_tui__app__tests__snapshot_sessions_picker_after_moving_the_selection.snap` | The same picker after `j`, proving the marker follows the selection. |

## For AI Agents

### Working In This Directory

- **Never hand-edit a `.snap` file.** Change the code, run the test, then accept the new output with `cargo insta review` (or `INSTA_UPDATE=always cargo test -p ganja-tui`). Editing the snapshot directly makes the test agree with a screen nobody rendered.
- **A snapshot diff is a question, not a failure.** Read it before accepting: they cover exactly the states where a rendering regression would otherwise be invisible, so an unexpected change in one is usually a real bug in wrapping, layout or state mapping.
- Filenames are generated from the module path and test name (`ganja_tui__app__tests__<test>`), so renaming a test orphans its snapshot — delete the stale file in the same change.
- `.snap.new` files are unaccepted results; they must not be committed.

### Testing Requirements

```sh
cargo test -p ganja-tui --lib
cargo insta review
```

<!-- MANUAL: -->
