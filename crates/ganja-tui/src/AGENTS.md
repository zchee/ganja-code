<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# ganja-tui/src

## Purpose

The frontend's event loop and the state it owns. One `tokio::select!` in `app.rs` folds three event sources into a single enum, mutates state in one place, and draws; the components under `component/` render that state and nothing else.

## Key Files

| File | Description |
|------|-------------|
| `lib.rs` | `run(resume, overrides)`: reads the config, resolves the key bindings, selects a provider, builds the agent registry, builds the `Engine` with builtin tools — plus a `skill` tool over `instruction::skill_roots(&config, &cwd)`, the same value the prompt's `<available_skills>` block is composed from, so the offered list and the loadable list are one list — project permission rules, both halves of the system prompt and its agents, installs the language servers (`Lsp::new`, absent when the config asked for none) and the working-tree snapshots `/undo` restores from (`Snapshots::new`, probed once against `git`), starts the stale-read watcher (`engine.watch_files()` — registration happens on the watcher's own task, so startup never blocks on the shape of the tree), then loads the theme set and applies the configured theme over the stored pick — all **before** the terminal is taken over, so every refusal is readable. Whatever startup had to say — the provider, the theme, why this directory cannot be snapshotted — is joined into the status bar's opening line. Enters the alternate screen with mouse capture and bracketed paste, dials the configured MCP servers in the background, and restores the terminal — and closes those servers — on every exit path; the panic hook restores the terminal only. |
| `app.rs` | `App`: the `select!` loop, key handling, and the state every component renders from. Also holds `Cleared`, which is how a `RevertChanged` carrying no revert is read: the engine sends the same event for a redo that stepped past the newest undone prompt and for the prompt that made an undo permanent, and this side knows which because it sent the command. **P13** added `event_log`/`turn_usages` (the inspector's raw-log tee and per-turn usage retention), `inspector`/`mcp_dialog` (the two new full-screen/two-step overlays, each with its own `modal_open`/key-routing block), `queued_permissions` (a `VecDeque` so several children's dialogs queue rather than collide, one visible at a time), and `poll_jobs()`/`poll_mcp_dialog()` beside the existing `poll_mcp()` on every `Tick`. Also holds the snapshot tests. |
| `event.rs` | `AppEvent { Term, Core, Tick }` — the one enum every event source folds into. Engine events are boxed because they dwarf the other variants. |
| `command.rs` | Both command populations — the UI `Entry` table (upstream's names, plurals and aliases) and the engine's `EngineCommand` roster — plus the `Choice` the `/` dropdown merges them into, and `nucleo-matcher` ranking over the result. Ranking parity with upstream's `fuzzysort` is explicitly not a goal; a total, deterministic order is. |
| `mention.rs` | The `@` trigger and the submit-time scan, sharing one rule: the last `@` before the cursor, preceded by start-or-whitespace, with no whitespace up to it. A mention that could be typed but not read back would attach nothing. A submitted mention must also name a file that is really there — `@alice` in prose is a person, not an attachment (D113). A `#5-9` suffix names lines — split at the last `#`, end kept only when start < end, an unparseable tail stays part of the path — and `ganja-core::command::mentions` mirrors the rule under a cross-cited equality pin. Binary mimes attach too; whether the wire carries one is the provider's question, so submit warns when it will degrade. |
| `clipboard.rs` | The system clipboard behind a trait, so a copy command is testable on a machine with no desktop. `System` builds its arboard handle lazily and a failure is a status notice, never a refusal; a non-text clipboard is one error because arboard without `image-data` cannot tell an image from an empty selection. Also `osc52::sequence` and a hand-rolled base64: every copy also writes upstream's OSC 52 escape (tmux/screen passthrough when `$TMUX`/`$STY` is set) through the app's terminal writer, so a copy from a tmux pane or an SSH session reaches the terminal the user is really attached to. |
| `history.rs` | Prompt history in upstream's shape: `prompt-history.jsonl` under the data home, fifty entries, consecutive-duplicate suppression, a parse-what-parses load that rewrites the file to self-heal, and a guarded walk — Up/Down navigate only from an empty buffer or the entry it came from, so a draft is never clobbered. |
| `transcript.rs` | What `/copy` and `/copy-message` put on the clipboard: upstream's `formatTranscript` markdown shape, and the last assistant message's text parts joined and trimmed. Times are UTC (D24); tool details are carried, upstream's assistant-metadata header is not. |
| `external.rs` | `/editor`: seeds a temp file with the buffer, hands the terminal to `$EDITOR`, takes it back. Split so the seed/readback round trip is testable; the terminal hand-off itself is exercised by hand. |
| `markdown.rs` | Assistant text only: pulldown-cmark → top-level blocks → width-independent styled lines, then a markdown-aware wrap. Carries the plain-text invariant by construction — a source newline is a hard line break, text renders verbatim, blocks separate by one blank line, and an unnamed `markdown*` key falls back to the body role. Syntect highlights fenced code by info string, mapping TextMate scopes onto the nine `syntax*` keys through a documented rule table; the syntax set loads lazily on the first known-language fence. Stage 1 of the transcript's two-stage cache, keyed `(block source hash, theme revision)`. |
| `keybind.rs` | Which keys reach which actions, the small curated set (including `input_newline`, upstream's `shift+enter,ctrl+enter,alt+enter,ctrl+j` — `ctrl+j` is the ASCII-LF break every terminal can deliver), and the `keybinds` config map that rebinds them. An unknown action name and an unparseable key string both fail the run naming what was wrong. **P13**: `Action::TranscriptOpen` claims `ctrl+t`, and `themes_open`'s own default becomes the empty chord — `parse` already treats `""` as zero alternatives rather than a parse error, so the action loads reachable by nothing but `/themes`/the palette (**D453**). |
| `theme/` | Loadable themes: upstream's JSON schema and resolver (`json.rs` — defs, dark/light variants, ANSI integers, cycle refusal), the builtin/custom registry with revisions (`registry.rs` — custom themes under `<config home>/themes`, through `ganja_core::config::config_home` so a moved home moves the themes with it), the persisted pick under the data home (`selection.rs`), and the `Theme` style slots (`mod.rs`). Four upstream themes ship verbatim from `../assets/themes/`; default is `opencode`. |
| `binder.rs` | **P25 (D505)**: the trait `run` is handed a way to bind this session's socket through. It is a trait and not a call because this crate may not name `ganja-serve` — the binary links both and passes one in — and because *when* a socket is asked for is a frontend decision: only for a session that leads a team. A pane member and a build with no config home hand the binder back unused, and a caller with no server passes `None`. Rebinding is one lock and one id compare after every `App::handle`, so a session that starts leading gets its socket at the next event rather than at the next restart. |
| `member.rs` | **P25**: this app in a pane teammate's role — `--agent-name`/`--agent-id`/`--team-name`/`--parent-session-id`, four that arrive together or not at all, plus an optional `--agent-color`. It reads its own inbox on the tick that already reads everything else, forwards its permission dialogs to the lead as §5's `permission_request` instead of raising them, and applies only a `LeadFrame`'s answer — a type a peer's message cannot construct, which is what keeps "only the lead's answer counts" a compiler fact. `planModeRequired` maps to running as the plan agent. |
| `component/` | The three panes plus the modals (see `component/AGENTS.md`). |
| `snapshots/` | `insta` snapshots for the tests in `app.rs` and, since P25, one in `component/chat.rs` (see `snapshots/AGENTS.md`). |

## For AI Agents

### Working In This Directory

- **No `select!` arm may await work of unbounded duration.** A prompt is handed to the engine, which answers through the event stream, and the loop goes straight back to drawing. Anything long-running is the engine's job, reported back as events.
- **`App::handle` is the only place that mutates state.** That is what lets components be tested without a terminal or a running turn — keep new state transitions inside it rather than pushing them into a component.
- **Frames coalesce, keystrokes do not.** A burst of streamed fragments redraws at most once per `FRAME` (16ms, ~60 FPS); a keystroke always redraws immediately. Those two rules together are what keep streaming cheap without making typing feel laggy.
- **A reply is markdown; a prompt is not.** Only assistant text parts reach `markdown.rs` — user messages, tool output, file chips and every dialog stay plain, so nothing a person typed is re-read as markup.
- **A paste is content, not keystrokes.** Bracketed paste is enabled at startup so an Enter inside pasted text is a line break rather than a submit; `ctrl+v` is the fallback for terminals that do not speak it. Neither path may lose what is already typed — a clipboard that cannot be reached is a status notice.
- The engine is shared, not owned: an `Arc<Engine>` whose seams take `&self` (D505 hands a clone to the session socket's server), answering through its event stream; UI state itself is mutated only in `App::handle`.

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

`ratatui` (+ `ratatui-textarea`), `tokio`, `futures`, `anyhow`, `unicode-width`, `serde_json`, `pulldown-cmark` + `syntect` (markdown parsing and fenced-code highlighting, parser only — no syntect theme files), `arboard` (clipboard read and write, text only); `insta` for snapshots.

<!-- MANUAL: -->
