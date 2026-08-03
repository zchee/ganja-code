<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# component

## Purpose

The three panes the layout draws — transcript, prompt editor, status bar — plus the modals that overlay them: the permission dialog while a tool call waits on the user, and the sessions picker while a stored conversation is being chosen.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | Module list. |
| `chat.rs` | The transcript viewport: message and part rendering, wrapping, scrolling, tail-following, wheel handling (`WHEEL_LINES`). |
| `editor.rs` | The prompt editor — a `ratatui-textarea` `TextArea` with ganja's submit rules layered on top. |
| `status.rs` | The status bar: what the engine is doing (`Activity`), what the session has spent (`Totals`), and the keys that matter. |
| `permission.rs` | The centered modal blocking on one pending tool call. Spec: upstream `packages/tui/src/routes/session/permission.tsx`, trimmed to the one-shot shape `PermissionReply` offers today. |
| `sessions.rs` | The centered modal listing this project's stored sessions to resume. Spec: upstream `packages/tui/src/routes/session/list.tsx`, trimmed to the columns a person picks by. |

## For AI Agents

### Working In This Directory

- **The transcript is built from engine events alone — the frontend never invents an entry.** The same event stream must replay into the same screen; that property is what P4's resumed sessions and P7's remote clients depend on. If the transcript needs information the engine does not send, the fix is an event in `ganja-core`.
- **The app decides what a keystroke means before the widget sees it.** Enter's meaning (submit vs newline) is resolved in `App::handle`, not inside the editor; the permission dialog's keys are intercepted while it is open. Components render and report — they do not own key semantics.
- **The chat viewport measures its own wrap widths** with `unicode-width` rather than leaning on `Paragraph`, because scroll positions have to agree with what was actually drawn. A change to wrapping is a change to scrolling; keep them in one place.
- The permission modal is centered and must fit in small terminals; the pty suite depends on it landing below the transcript's last line in a tall window (see `../../../ganja-cli/tests/AGENTS.md` for why that matters).

### Testing Requirements

Components are exercised through `App::handle` and rendered into a `TestBackend` — no terminal, no running turn. Screen output is asserted with `insta` snapshots in `../snapshots/`. After an intentional visual change: `cargo insta review`.

Snapshot coverage today: the permission dialog open and overflowing (the cut flagged, the reply keys kept), the sessions picker open and after moving the selection, and a tool call in each of its states (pending, running, completed with a diff, error). A new tool state or dialog needs its own snapshot.

### Common Patterns

Each component owns its own state struct and exposes methods the app calls (`scroll_pages`, `follow_tail`, …); none reaches back into the engine. Styling goes through `crate::theme` rather than literal colors, so P5 can make themes loadable data without touching component code.

## Dependencies

### Internal

`crate::theme`, `crate::event::AppEvent`, and the `ganja_core` protocol types each pane renders (`PartBody`, `ToolState`, `Usage`, `PermissionReply`, `Role`).

### External

`ratatui`, `ratatui-textarea`, `unicode-width`, `serde_json` (tool metadata).

<!-- MANUAL: -->
