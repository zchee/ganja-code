<!-- Parent: ../AGENTS.md -->
# ganja-tui

The ratatui frontend. It draws the terminal and turns key and mouse input into engine commands; it contains no engine logic. `ganja-cli` links it and calls `run()` in `src/lib.rs`. If the screen needs a fact the engine does not report, add a protocol event in `ganja-core`; do not track it locally here.

## Boundary

- `depgate.toml` `[rules."ganja-tui"]`: `deny = ["axum*"]`. The TUI never links an HTTP server; the socket server reaches it only through the `binder::Binder` trait that `ganja-cli` implements.
- `ganja-core`'s rule denies `ratatui*`, so terminal code stays in this crate.
- Internal dependencies are named directly in `Cargo.toml`: `ganja-core`, `ganja-protocol`, `ganja-permission`, `ganja-tool`, `ganja-teammate-local`. Each entry carries a comment with its reason.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | `run(resume, overrides, yolo, member, binder, lister, name, socket_dir)`: startup assembly before the terminal is taken, the kitty keyboard probe, the panic hook, terminal restore. |
| `src/app.rs` | `App`, the `tokio::select!` loop, `App::handle` (the only state mutator), key routing, `FRAME`, `ESC_CHORD`. |
| `src/event.rs` | `AppEvent { Term, Core, Tick }`, the one enum every event source folds into. |
| `src/escrepair.rs` | Rebuilds CSI/SS3 keys split across reads; holds a lone Esc for `HOLDOFF` (25 ms). |
| `src/keybind.rs` | Default key table and the `keybinds` config map. |
| `src/command.rs` | UI command table plus engine commands, merged for the palette and `/` dropdown, ranked by `nucleo-matcher`. |
| `src/mention.rs` | `@` trigger rule, submit-time scan, `#A-B` line ranges, dropped-path classification. |
| `src/clipboard.rs` | Clipboard trait over `arboard` (text and images), plus the OSC 52 writer. |
| `src/graphics.rs` | Inline image previews over the kitty graphics protocol, gated on `KITTY_WINDOW_ID`. |
| `src/notify.rs` | OSC 9 or BEL on turn end or a waiting dialog while the terminal is unfocused. |
| `src/history.rs` | Prompt history file, `MAX_HISTORY_ENTRIES` (50). |
| `src/markdown.rs` | Assistant markdown to styled lines; syntect highlighting mapped onto the theme's `syntax*` keys. |
| `src/transcript.rs` | What `/copy` and `/copy-message` put on the clipboard. |
| `src/external.rs` | `/editor`: hands the buffer to `$EDITOR` through a temp file. |
| `src/binder.rs` | Trait through which `run` binds this session's socket. |
| `src/lister.rs` | Trait for live-session rows in the `@` menu, injected by `ganja-cli`. |
| `src/member.rs` | Running as a pane teammate: inbox polling, permission forwarding to the lead. |
| `src/theme/` | `json.rs` schema and resolver, `registry.rs` builtin and custom themes, `selection.rs` persisted pick, `mod.rs` style slots. |
| `src/component/` | One file per pane or modal: `chat`, `editor`, `status`, `queue`, `dropdown`, `files`, `skill_menu`, `palette`, `help`, `list`, `sessions`, `themes`, `effort`, `permission`, `question`, `inspector`, `search`, `rewind`, `mcp`, `plugin`, `team`, `held`, `context`, `usage`. `mod.rs` wires them. |
| `src/snapshots/` | insta `.snap` files for `app_tests.rs` and `component/chat_tests.rs`. |
| `assets/themes/` | Four upstream themes (`opencode`, `tokyonight`, `gruvbox`, `aura`), compiled in with `include_str!`. |
| `tests/` | Three binaries that set process-wide home directories. |

## Commands

```sh
cargo nextest run -p ganja-tui
cargo nextest run -p ganja-tui -E 'test(snapshot_theme_opencode)'     # one snapshot test
cargo insta test -p ganja-tui --review     # run, then review pending .snap.new files
cargo insta review                         # review .snap.new files left by an earlier run
cargo insta pending-snapshots --manifest-path crates/ganja-tui/Cargo.toml
cargo nextest run -p ganja-tui --test plugin_dialog
# stopwatch tests, #[ignore]d; read the numbers in the log
cargo nextest run -p ganja-tui --run-ignored all -E 'test(scrolls_a_ten_thousand_line)'
cargo test -p ganja-tui -- --ignored the_system_clipboard   # needs a desktop; overwrites the clipboard
cargo nextest run -p ganja-cli --test pty_smoke             # real TUI in a pty; run after key or composer changes
```

CI runs `cargo nextest run --locked --workspace --profile ci` (`.github/workflows/ci.yaml`). GitHub Actions sets `CI=true`, and insta does not write `.snap.new` files under CI: a snapshot mismatch only fails. Accept snapshots locally and commit the `.snap` files.

## Conventions

- Mutate UI state only inside `App::handle`. Components render state and expose methods the app calls; no component calls the engine itself. This is what lets tests drive the app without a terminal (pinned by `src/app_tests.rs`).
- No `select!` arm awaits unbounded work. A prompt goes to the engine, which answers on its event stream (`src/app.rs`).
- Streaming redraws coalesce to one frame per `FRAME` (16 ms); a keystroke redraws immediately (`src/app.rs`).
- Only assistant text goes through `markdown.rs`. User text, tool output and dialogs render as plain text.
- Pad and clip table columns with `chat::pad` and `chat::clip`, which measure display width. `format!("{:<n$}")` counts chars and misaligns CJK and emoji (`src/component/chat.rs`).
- A new terminal mode must be undone on every exit path, including the panic hook in `src/lib.rs`.
- Porting a theme takes four changes: the verbatim upstream file in `assets/themes/`, an `include_str!` row in `src/theme/registry.rs`, the filename in the root `THIRD_PARTY_NOTICES.md`, and a `snapshot_theme_<name>` test in `src/app_tests.rs` using `styled_screen`. Never reformat the theme JSON; it must stay byte-identical to upstream.

## Gotchas

- Quit keys are `ctrl+c`, `ctrl+q`, `ctrl+d` (`app_exit` in `src/keybind.rs`). `Esc` cancels a streaming turn; a second `Esc` within `ESC_CHORD` (500 ms) at an idle composer starts the backtrack walk. Bare `q` only closes the inspector.
- `ctrl+t` opens the inspector; `themes_open` has no default chord and is reached through `/themes` or the palette (`src/keybind.rs`).
- At startup `capture_keys` in `src/lib.rs` probes for the kitty keyboard protocol and pushes `DISAMBIGUATE_ESCAPE_CODES` when the terminal supports it. An unanswered probe blocks up to two seconds; `GANJA_DISABLE_TERM_PROBE=1` (or `true`) skips it. The pty drills in `ganja-cli/tests/` set it.
- The pushed flags are popped on every exit, including the panic hook, and popped and re-pushed around `/editor` (`src/external.rs`), because the flag stack is per screen buffer.
- When the kitty flag is active, `EscRepair` runs in passthrough mode. Otherwise a bare Esc waits `HOLDOFF` for a `[` or `O` continuation; split SGR mouse fragments and split paste markers are dropped, not typed (`src/escrepair.rs`, pinned by `src/escrepair_tests.rs` and `ganja-cli/tests/pty_smoke.rs`).
- Socket bind rule: every session that is not a pane member binds its socket when `ganja-cli` passes a binder. A pane member hands the binder back unused. The gate is one line in `run`, pinned by `the_bind_predicate_is_membership_alone` in `src/lib_tests.rs`.
- Transcript glyphs (`src/component/chat.rs`): `> ` on a prompt, `● ` on replies and settled tool calls, an in-flight call cycles `POINT_GLYPHS` (`· ∙ • ●`), `  ⎿ ` on a call's result, `∴ ` on thinking, `@ <name>❯` heading a teammate message. The working line cycles `WORKING_FRAMES` (`·✢✳✶✻✽` and back); the status bar spinner is braille `SPINNER`.
- `arboard` is built with `image-data`; `ctrl+v` reads a clipboard image and attaches it as a PNG. Every copy also writes OSC 52 (`src/clipboard.rs`).
- Custom themes load from `<config home>/themes` via `ganja_core::config::config_home`; a theme that fails to load is skipped with a `tracing` line, not fatal.
- A session that holds a judge (D567, experimental) says so second in its opening line: `evaluate (experimental): screening <sources> via <host> (lead and subagents)`. `App::with_disclosure` keeps that line through the first socket pass, so a name collision or a refused bind that pass reports stands after it (`set_startup_notice`), never in its place; `App::open` makes the pass and lets the disclosure go, and later notices replace the line as any other does (pinned by `src/app_tests.rs`). The judge is built once in `run`; a `/plugin` Reload rebuilds tools but not the judge, so a changed screen is a restart.

## Tests

Unit tests live in sibling `*_tests.rs` files through `#[path]`; there are no inline test modules. Screen tests build an `App`, feed `AppEvent`s to `App::handle`, render into ratatui's `TestBackend`, and assert with `insta::assert_snapshot!`.

Two dump helpers in `src/app_tests.rs`: `screen` (symbols only; a theme change must not diff these) and `styled_screen` (symbols plus fg, bg and modifiers). `src/component/held_tests.rs` has its own `screen(buffer, area)`.

Snapshots: `src/snapshots/` holds 50 `.snap` files (`ls src/snapshots/*.snap | wc -l`), named `ganja_tui__app__tests__<test>` or `ganja_tui__component__chat__tests__<test>`. Re-bless rules:

- Never hand-edit a `.snap`. Change the code, run the test, review the diff, accept it with `cargo insta review`.
- Read every diff before accepting. Snapshots whose fixture holds assistant text pin the markdown renderer; accepting a change to one needs a justification that names the markdown construct that changed, and the lead's sign-off.
- Renaming a test orphans its snapshot; delete the old file in the same change. Never commit `.snap.new`.

Ignored tests in `src/app_tests.rs`: `a_five_thousand_line_transcript_draws_inside_the_frame_budget` and `scrolls_a_ten_thousand_line_markdown_transcript_at_thirty_frames_a_second` (timing). In `src/clipboard_tests.rs`: `the_system_clipboard_round_trips_what_it_is_handed` (desktop clipboard).

Integration binaries in `tests/` (`theme_paths.rs`, `plugin_dialog.rs`, `seat_model_chooser.rs`) each hold exactly one test, because each sets process-wide `XDG_CONFIG_HOME`, `XDG_DATA_HOME` or `GANJA_CONFIG_HOME` and `cargo test` runs a binary's tests on parallel threads. A second env-mutating test goes in a new file. Each file's `//!` header states what it sets.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-tui.md`, with `ganja-tui-src.md`, `ganja-tui-src-component.md`, `ganja-tui-src-snapshots.md`, `ganja-tui-tests.md` and `ganja-tui-assets.md`, frozen from commit 35d1720. New decisions go in `docs/decisions/ledger.md`, not here.
