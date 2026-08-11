<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# component

## Purpose

The three panes the layout draws — transcript, prompt editor, status bar — plus the strip of messages waiting on a running turn, the modals that overlay them: the permission dialog while a tool call waits on the user, the question dialog while the model waits on an answer, the sessions picker while a stored conversation is being chosen, the theme list while a palette is being previewed, the model, agent and effort lists while a session is being pointed somewhere else, the command palette, the reference card, a fuzzy search over remembered prompts, a two-step rewind picker over the session's own checkpoints, and the two inline menus the editor raises — one on a leading slash, one on an `@`.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | Module list. |
| `chat.rs` | The transcript viewport: message and part rendering, wrapping, scrolling, tail-following, wheel handling (`WHEEL_LINES`), and the revert state — `revert`/`unrevert`/`drop_reverted` hide, restore or delete the tail an `/undo` took back, with one marker row (`N message(s) reverted — /redo to restore` plus the files) standing in its place. |
| `editor.rs` | The prompt editor — a `ratatui-textarea` `TextArea` with ganja's submit rules layered on top. |
| `queue.rs` | The strip of messages typed while a turn was already running (**F4**): steered entries (`Command::Steer`, cleared on `Event::SteerConsumed`) and the fallback lane — a refused or still-unconsumed steer, a slash command, a shell line — replayed once the engine goes idle. Caps at 5 visible rows; the status bar carries the true depth so a deep queue is never silently clipped. |
| `status.rs` | The status bar: what the engine is doing (`Activity`), what the session has spent (`Totals`), and the keys that matter. |
| `permission.rs` | The centered modal blocking on one pending tool call. Spec: upstream `packages/tui/src/routes/session/permission.tsx`, trimmed to the one-shot shape `PermissionReply` offers today. |
| `sessions.rs` | The centered modal listing this project's stored sessions to resume. Spec: upstream `packages/tui/src/routes/session/list.tsx`, trimmed to the columns a person picks by. |
| `search.rs` | The Ctrl+R history search modal (**F2**): a fuzzy membership filter (`nucleo_matcher`, matching but not re-ranking) over remembered prompts, shown newest-first with a preview pane for the entry under the cursor. No upstream counterpart — upstream's own Ctrl+R is `session_rename`, which ganja has never bound (D447). Spec: Claude Code's own panel, screenshot 2026-08-11. |
| `themes.rs` | The centered modal listing loadable themes. Owns only the cursor; the app applies the live preview on every move and reverts on Esc. Spec: upstream `packages/tui/src/component/dialog-theme-list.tsx`. |
| `list.rs` | `ListDialog`: one centered modal for choosing a model or an agent — the same dialog over two lists, because what differs between them is the rows and the command Enter sends, and neither of those is drawing. Marks the row the session is already on; previews nothing. |
| `rewind.rs` | The rewind picker (**F7**): the session's own checkpoints — its user messages, newest first, plus a `(Current)` row — each annotated with files-changed or Claude Code's `⚠ No code restore`; Enter opens a second step choosing `RevertScope::Both`/`Conversation`/`Files` (D451) before sending `Command::RevertTo`. Semantics are upstream's message-level revert (`session/revert.ts`); the picker layout and the second-step scope choice are Claude Code's. |
| `palette.rs` | The command palette, grouped by category with a filter line and a block of suggested commands pinned while nothing is typed. Spec: upstream `packages/tui/src/component/command-palette.tsx`. |
| `dropdown.rs` | The inline command menu, anchored above the editor. Opens only when `/` is the first character of the buffer and the cursor has not left the first whitespace-free span; matches descriptions as well as names, which the palette does not. Spec: upstream `packages/tui/src/component/prompt/autocomplete.tsx`. |
| `files.rs` | The inline file menu, raised by an `@`. The same box `dropdown.rs` draws, over paths `ganja-tool`'s `glob` walked in-process — and **not re-ranked**, because upstream says twice in comments to trust the backend's order. |
| `question.rs` | The question dialog: one question, its options, and — unless the tool said `custom: false` — upstream's last-row free-text entry, opened with Enter, submitted as the answer when non-empty, abandoned back to the options when empty. Esc inside the editor cancels the edit; outside it rejects the question. A batch answers its first question only. |
| `effort.rs` | The `/effort` picker: "Default" first, then the active model's catalog effort names; a selection sends `SwitchEffort`, and the status bar's `model (effort)` segment follows `EffortChanged`. |
| `help.rs` | The reference card: every command with its key, then the bindings no command row shows, named as a config file rebinds them. Sizes itself to its content and **scrolls** when the window cannot hold it — upstream's card is one sentence and never needed to (deviation: `help-card-scrolls`); the footer counts which rows are showing, so a clip is never silent. |

## For AI Agents

### Working In This Directory

- **The transcript is built from engine events alone — the frontend never invents an entry.** The same event stream must replay into the same screen; that property is what P4's resumed sessions and P7's remote clients depend on. If the transcript needs information the engine does not send, the fix is an event in `ganja-core`.
- **The app decides what a keystroke means before the widget sees it.** Enter's meaning (submit vs newline) is resolved in `App::handle`, not inside the editor; the permission dialog's keys are intercepted while it is open. Components render and report — they do not own key semantics.
- **The chat viewport measures its own wrap widths** with `unicode-width` rather than leaning on `Paragraph`, because scroll positions have to agree with what was actually drawn. A change to wrapping is a change to scrolling; keep them in one place.
- The permission modal is centered and must fit in small terminals; the pty suite depends on it landing below the transcript's last line in a tall window (see `../../../ganja-cli/tests/AGENTS.md` for why that matters).

### Testing Requirements

Components are exercised through `App::handle` and rendered into a `TestBackend` — no terminal, no running turn. Screen output is asserted with `insta` snapshots in `../snapshots/`. After an intentional visual change: `cargo insta review`.

Snapshot coverage today: the permission dialog open and overflowing (the cut flagged, the reply keys kept), the sessions picker open and after moving the selection, a tool call in each of its states (pending, running, completed with a diff, error), the theme list open, one style-aware frame per ported theme, the palette open and filtered plus one style-aware frame pinning its selection fill, the inline command menu, the agent list, the reference card, a transcript with a revert marker in place of the messages an `/undo` hid, the history search modal open, the queue strip holding waiting entries, and the rewind picker at both its checkpoint and scope steps. A new tool state or dialog needs its own snapshot.

### Common Patterns

Each component owns its own state struct and exposes methods the app calls (`scroll_pages`, `follow_tail`, …); none reaches back into the engine. Styling goes through `crate::theme` rather than literal colors — themes are loadable data now, and that discipline is why a palette switch needs no component changes. The one exception it forced is explicit: the editor bakes styles into its `TextArea` at construction, so `Editor::restyle` must be called after any theme change.

## Dependencies

### Internal

`crate::theme`, `crate::event::AppEvent`, and the `ganja_protocol` types each pane renders (`PartBody`, `ToolState`, `Usage`, `PermissionReply`, `Role`, `RevertScope`, `MessageId`).

### External

`ratatui`, `ratatui-textarea`, `unicode-width`, `nucleo-matcher` (fuzzy filtering behind the history search modal, the same matcher the palette's own narrowing uses), `serde_json` (tool metadata).

<!-- MANUAL: -->
