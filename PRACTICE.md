# Learning Guide: Porting opencode to Rust with ratatui

This file maps the ganja-code build phases (P0–P13) to hands-on learning exercises in Rust and ratatui. It complements four plan documents, each authoritative for its own span: `.omc/plans/2026-08-03-opencode-rust-port.md` (P0–P7 phase goals, acceptance criteria, and the crate-partitioning ADR), `.omc/plans/2026-08-11-tui-ux-port.md` (P8, eight composer behaviors), `.omc/plans/2026-08-11-claude-composer-port.md` (P12, seven more composer behaviors — five ports, two named Claude Code divergences), and `.omc/plans/2026-08-11-claude-runtime-port.md` (P13, five Claude-shaped runtime features with no upstream opencode counterpart at all). P9–P11 shipped against frozen `.omc/handoffs/team-exec-p*.md` contracts rather than standalone plans; see the short interstitial before P12 below.

**Audience**: Solo developer, Go expert, Rust beginner. Each phase ends with a working demo; exercises range from guided implementation to reading and annotating team code.

---

## P0: Workspace Scaffold (M effort)

**Goal**: Establish a three-crate Rust workspace with stable toolchain, CI gates, and a minimal ratatui 0.30 app that runs without crashing.

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| Workspace + crates | `go.mod` + packages |
| `rustfmt` + `clippy` | `gofmt` + `go vet` |
| `Cargo.toml` dependencies | `go.mod` semver constraints |

### Exercises

- [ ] **Read & run**: Create the three-crate structure (`ganja-core`, `ganja-tui`, `ganja-cli`). Run `cargo build --workspace` and verify no errors. Annotate `Cargo.toml` workspace members and note how crates reference each other as dependencies.

- [ ] **Explore formatting**: Run `cargo fmt --check` on the entire workspace. Fix any violations with `cargo fmt`. Explain the difference between `rustfmt` and Go's `gofmt` — what does Rust style enforce that Go does not?

- [ ] **Lint familiarization**: Run `cargo clippy --all-targets -- -D warnings`. Read 2–3 clippy warnings and understand why they're suggestions. Apply the fixes or document why you disagree.

- [ ] **Terminal app hello**: Write a minimal ratatui 0.30 app that enters alternate screen, prints "Hello, ganja!", and exits on `q`. Verify `cargo run` works and the terminal state is restored. Test Ctrl-C handling — does your panic hook work?

- [ ] **Editor widget decision**: Test `ratatui-textarea` 0.9.2 against ratatui 0.30.2 by creating a simple edit field in your hello app. Verify it renders and accepts input. If it fails, document the error and test the fallback (`edtui` 0.11.6).

---

## P1: TUI Shell + Fake Streaming (M effort)

**Goal**: Build a three-pane TUI (chat / editor / status) with a fake provider streaming canned text, demonstrating the entire event loop before touching real providers.

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| `tokio::select!` | Go `select` statement (multiplex channels) |
| `mpsc` / bounded channels | `chan` send/receive |
| Enums + pattern matching | Tagged unions (Go has no native equivalent) |
| Ownership of mutable state | Goroutine-local state (Rust enforces at compile time) |

### Exercises

- [ ] **Hand-code the event loop**: Implement the core `tokio::select!` loop in `ganja-tui/src/app.rs` that multiplexes three event sources: (1) terminal events via `crossterm::EventStream`, (2) core events from a fake provider (via `mpsc`), (3) a tick timer. Write it without copying from upstream or reference code first — understand the branching structure and how Rust forces you to handle all cases. Verify all three arms fire correctly by logging and running `RUST_LOG=debug cargo run`.

- [ ] **Build the Engine stub**: Create a minimal `ganja-core/src/engine.rs` with a `send()` method that accepts a `Command` enum (variant: `Submit(String)`) and a `subscribe()` method returning a `BoxStream<'static, Event>`. The Engine should not actually process commands yet — just validate them and queue events. Wire it to the TUI event loop.

- [ ] **FakeProvider streaming**: Implement a `FakeProvider` in `ganja-core` that streams "Hello, world!" one character at a time (20ms cadence). Emit `ProviderEvent::TextDelta` for each character. In the TUI, render the streamed text in the chat viewport and verify smooth incremental display.

- [ ] **Loss-proof slow-consumer test** (acceptance requirement): Write a unit test in `ganja-core` that spawns the Engine with a deliberately slow event consumer (drains events at 10ms intervals). Emit 10,000 events from the fake provider into a bounded-capacity mpsc queue. Assert that the consumer receives **every event in order** — zero loss, no buffering overflow. This proves the lossless delivery policy; if backpressure causes the producer to stall, document that and adjust the test.

- [ ] **Scrolling & resize**: Add arrow-key and mouse-wheel scrolling to the chat viewport. Implement a resize handler that reflows text without panicking. Test a resize storm (simulate rapidly changing terminal width) with `cargo test` or a manual PTY session.

- [ ] **Cancel on Escape**: Wire the Escape key to emit a `Command::Cancel` that propagates to the fake provider and stops the stream within 100ms. Log the cancel event and verify the stream halts cleanly.

---

## P2: Real Providers + Protocol Types (M effort)

**Goal**: Define serde-derived message and command/event types; implement Anthropic and OpenAI-compatible SSE providers; add auth and model catalog.

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| Traits + `async_trait` | Go interfaces |
| `Result<T, E>` + `?` operator | `if err != nil` + early return |
| `CancellationToken` | `context.Context` (cancellation + timeout) |
| `BoxStream<'static, T>` | `for range ch` (streams of values) |

### Exercises

- [ ] **Port the message model**: Read the plan's spec reference (`session/message-v2.ts`) and the P2 phase description. Implement `ganja-core/src/message.rs` with structs: `Message`, `MessagePart`, `ProviderEvent`. Use `#[derive(Serialize, Deserialize)]` and verify with a serde roundtrip test: serialize to JSON, deserialize, assert equality.

- [ ] **Trait object for providers**: Define a `Provider` trait with `stream(&self, req: ChatRequest, cancel: CancellationToken) -> BoxStream<ProviderEvent>`. Implement `AnthropicProvider` and `OpenAiProvider` as stubs (return `FakeProvider` behavior for now). Store them as `Box<dyn Provider>` in the Engine.

- [ ] **Hand-rolled SSE parser**: Implement a frame splitter (~100 LOC) over `reqwest::Client::get().bytes_stream()`. Parse SSE `data:` lines and emit `ProviderEvent`. Handle multi-line messages, unknown event types (log + skip), and malformed frames. Test against a local recorded SSE transcript (create a `tests/fixtures/anthropic-basic.sse` file with 5–10 events).

- [ ] **Auth setup**: Implement `ganja-core/src/auth.rs` with: read `ANTHROPIC_API_KEY` from env; persist to `~/.local/share/ganja/auth.json` (mode 0600) if not set. Implement `ganja auth login` subcommand that prompts for a key and saves it. Write a test that verifies the file is created with 0600 permissions.

- [ ] **Model catalog**: Implement a static model table in `ganja-core/src/provider/catalog.rs` with entries for Claude 3.5 Sonnet, GPT-4o, etc., including context windows and pricing. Build a CLI command `ganja models` that lists them. Bonus: fetch the upstream `models-dev.ts` and port the numeric values.

- [ ] **Retry + backoff**: Implement exponential backoff (spec: `session/retry.ts`; Go anchor: `time.Sleep` + counter loop). On HTTP 429 or 5xx, retry up to 3 times with delays 1s, 2s, 4s. Test with a fixture that injects errors mid-stream.

---

## P3: Agent Loop, Tools v1, Permissions (L effort)

**Goal**: Build the core agent loop (stream → parse → check permission → execute tool → continue); port ≥4 replacer strategies for the `edit` tool; implement core tools (read, write, edit, glob, grep, shell, todo, webfetch).

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| Trait objects `Box<dyn Tool>` | Go interface values ([]interface{}) |
| `serde_json` for dynamic data | Go `encoding/json` + `json.Unmarshal` |
| Process groups + child termination | `exec.CommandContext` + process group kill |
| `Drop` trait | Go `defer` (resource cleanup) |

### Exercises

- [ ] **Implement one edit-replacer strategy from upstream fixtures**: The `edit` tool is the ceiling on agent quality. Read the upstream `packages/opencode/src/tool/edit.ts` and port at least one of these replacer strategies with its test cases:
  - Exact: line-number range + exact string match
  - Whitespace-normalized: ignore leading/trailing whitespace in the match
  - Indentation-flexible: preserve destination indentation
  - Block-anchor: use surrounding context lines as anchors
  
  Implement the strategy in `ganja-core/src/tool/edit/mod.rs`. Port the upstream test fixtures (available in the local `.omc/reference/opencode-v1.18.13/` checkout if your team has cloned it) into `tests/fixtures/edit_*.rs` as table-driven cases. Assert that edits produce byte-identical output to upstream for ≥3 fixtures.

- [ ] **Port tool descriptions**: Copy the text from upstream `tool/read.txt`, `tool/write.txt`, `tool/glob.txt`, etc. (MIT licensed, already attributed in `THIRD_PARTY_NOTICES.md`). Implement the `Tool` trait with a `description()` method returning these strings verbatim. Create `ganja-core/src/tool/mod.rs` as a registry.

- [ ] **Implement the read tool**: `read(path: String, start_line?: u32, end_line?: u32) -> Result<String>`. Return line-numbered output (1-indexed). Test on a fixture file with ≥50 lines; verify line numbers are correct.

- [ ] **Implement the write tool**: `write(path: String, content: String) -> Result<()>`. Create or overwrite the file. Do NOT add chmod logic yet (permissions are handled at the permission layer). Test that write creates new files and overwrites existing ones.

- [ ] **Implement glob and grep tools** (in-process, no shelling out):
  - `glob(pattern: String) -> Result<Vec<String>>` using `globset::Glob`
  - `grep(pattern: String, paths: Vec<String>) -> Result<Vec<Match>>` using the `grep` crate
  
  Test each with a fixture directory containing ≥5 files.

- [ ] **Implement the shell tool**: `shell(cmd: String, timeout_ms?: u32) -> Result<Output>` using `tokio::process::Command`. Spawn child processes in a process group so Ctrl-C (or a CancellationToken) kills the whole group. Implement output truncation (spec: `packages/opencode/src/tool/truncate.ts` and `shell.ts` read the limit at port time; use 64KB as the default for now). Test that a timeout kills the process without hanging.

- [ ] **Permission engine + dialog**: Implement `ganja-core/src/permission.rs` with: default deny for `write`, `edit`, `shell`, `webfetch`; allow-once and allow-always modes persisted per project in `~/.local/share/ganja/project/<slug>/permissions.json`. Wire a TUI permission dialog that asks the user before executing a tool. Test that a denied edit leaves the file unchanged.

- [ ] **Agent loop** (the orchestration glue): Implement `ganja-core/src/session/processor.rs` (spec: `src/session/processor.ts`). Pseudocode: stream events from the provider → accumulate parts → when a tool_use event arrives, check permission → execute tool sequentially → append tool_result → continue streaming. Test with a scripted `FakeProvider` that emits a read→edit→shell sequence, asserting the event order in the transcript.

---

## P4: Sessions & Compaction (M effort) — MVP Gate

**Goal**: Persist sessions to versioned JSON; implement auto-compaction; support session resume via `--continue`.

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| `#[derive(Serialize, Deserialize)]` | Go `encoding/json` struct tags |
| Lifetimes at API boundaries | Go implicit GC (Rust forces you to name it) |
| Error taxonomies with `thiserror` | Go custom error types (enum-like structs) |

### Exercises

- [ ] **Implement JSON storage**: Create `ganja-core/src/storage.rs` with versioned storage under `~/.local/share/ganja/project/<slug>/storage/`. Each session is a single JSON file. Add a `version` field (e.g., `"v1"`) to allow schema migrations later. Implement `save_session()` and `load_session()`. Test that a session round-trips losslessly through JSON serialization.

- [ ] **Session list & resume**: Implement `ganja-cli` subcommands: `ganja --continue` (resume the most recent session) and `ganja --session <id>` (resume a specific session). If no session ID is provided, show an interactive picker. Test that a resumed session shows the previous transcript and can continue from where it left off.

- [ ] **Auto-title via cheap model call**: Implement `ganja-core/src/session/summary.rs`. After the first assistant turn, spawn a quick model call (same provider + credentials) with the prompt "Summarize this exchange in 1–2 words (e.g., 'fix bug', 'api design')." Use the result as the session title. Log the API call (do NOT include the key). Test with a fixture that ensures the title is persisted.

- [ ] **Compaction**: Implement `ganja-core/src/session/compaction.rs` (spec: `src/session/compaction.ts`). When a session reaches ≥90% of the model's context window, compact following upstream `compaction.ts` semantics (fold older turns into a model-generated summary while preserving system + pinned instructions) — not naive oldest-message deletion. Test that a 200-message session loads in < 150ms (measure with a timing log). Verify post-compaction that the session still fits the model's context budget.

- [ ] **Crash tolerance**: Implement partial-message handling. If the TUI process is killed mid-stream (SIGKILL), the next restart should mark that message as `aborted` (add a `state` enum: `complete`, `in_progress`, `aborted`). Load the session and render the aborted message with a visual marker (e.g., `[aborted]`). Test by spawning a session, killing it during a fake stream, and resuming.

---

## P5: Config, Agents, Commands, Themes (M effort)

**Goal**: Load and merge config (global < project < env < flags); implement slash commands and a fuzzy palette; support agent definitions and subagents via the `task` tool; port ≥3 themes.

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| Builder-like config merging | Go's `flag` + `os.Environ()` precedence |
| Pattern matching exhaustiveness | Go switch without default (linter catches missing cases in Rust at compile time) |

### Exercises

- [ ] **Config precedence**: Implement `ganja-core/src/config/mod.rs` that loads (in order, later overrides earlier): global `~/.config/ganja/config.json` → project `.ganja/config.json` → `GANJA_*` env vars → CLI flags. Implement unit tests with fixtures that verify the precedence order. Create a `ganja config import-opencode` command that reads an `opencode.json` and maps its keys to ganja config keys (one-way; data-only, no storage interop).

- [ ] **AGENTS.md loading**: Implement `ganja-core/src/session/instruction.rs` (spec: `src/session/instruction.ts`). Search for an `AGENTS.md` file in the project root and parse it as YAML frontmatter + fenced code blocks. Load custom agent definitions and merge their instructions into the system prompt. Test with a fixture `AGENTS.md` file that defines 2–3 agents.

- [ ] **Slash commands + palette**: Implement a fuzzy command palette (spec: `src/command/`). Commands: `/help`, `/new`, `/sessions`, `/model`, `/agent`, `/theme`, `/compact`, `/init`, `/editor`, `/exit`. Use `nucleo-matcher` for fuzzy search. When the user types `/`, show a palette filtering by keystrokes. Test that `/mod` matches `/model` and `/compact`.

- [ ] **@file mention**: Implement a mention system where typing `@filename` inserts the file's content as a message part. Use `nucleo-matcher` to fuzzy-search for files in the project. Test that `@src/main.rs` inserts the correct snippet with language tags.

- [ ] **! passthrough**: Implement shell passthrough: if a message starts with `!`, execute it as a shell command and append the output to the message instead of sending to the agent. Test that `!git log -1 --oneline` captures the output correctly.

- [ ] **Themes**: Implement `ganja-tui/src/theme.rs` (spec: `packages/tui/src/theme/`). Port ≥3 upstream themes as JSON (e.g., `default`, `high-contrast`, `solarized`). Implement a `/theme <name>` command that switches themes. Use snapshot tests to verify each theme renders correctly on a `TestBackend` (ratatui testing utilities).

- [ ] **Models.dev catalog fetch**: Implement a 24-hour cache for the upstream `models.dev` catalog (spec: `packages/core/src/models-dev.ts`). Fetch at startup if the cache is stale; fall back to a bundled static table if the network is unavailable. Upgrade `ganja models` to show the cached catalog with up-to-date pricing and context windows.

---

## P6: MCP, LSP, Markdown Polish, Undo (L effort)

**Goal**: Integrate MCP tools and LSP diagnostics; render full markdown with syntax highlighting; implement `/undo` via git snapshots.

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| FFI-free JSON-RPC framing | Go's `net/rpc` or manual JSON marshaling |
| Borrow-checker at scale | Go's freedom to share pointers (Rust forbids it without explicit `Arc`/`Rc`) |
| Caching & interior mutability done right | Go's lack of const-correctness (Rust's `Cell`/`RefCell` is explicit) |

### Exercises

- [ ] **MCP client integration**: Implement `ganja-core/src/mcp/mod.rs` using the `rmcp` crate (spec: `src/mcp/`). Spawn MCP servers in stdio mode. Tools are namespaced as `mcp__<server>__<tool>`. Integrate with the tool registry so MCP tools are permission-gated like built-in tools. Test with the MCP reference server (e.g., `brave-search`, provided by Anthropic).

- [ ] **LSP diagnostics**: Implement `ganja-core/src/lsp/mod.rs` (spec: `src/lsp/`). Spawn an LSP server (rust-analyzer or gopls via config). On each `write` or `edit` tool call, emit `didOpen`/`didChange` and collect diagnostics. Append them to the tool result so the model sees type errors inline. Test on a fixture repo that produces a known error (e.g., a Go file with a missing import). Assert the diagnostic appears in the tool result within 3s.

- [ ] **Full markdown rendering**: Upgrade `ganja-tui/src/markdown.rs` to use `pulldown-cmark` for parsing and `syntect` for syntax highlighting (spec: `packages/tui/src/markdown.rs`). Implement a two-stage cache: (1) parse + highlight keyed by `(message_id, theme_rev)` → (2) line-wrap keyed by `(text_hash, width)`. On resize, re-wrap but skip the expensive parse/highlight. Test with a 10,000-line transcript; verify ≥30 FPS during scrolling (frame-time log).

- [ ] **Git snapshots & /undo**: Implement `ganja-core/src/snapshot.rs` (spec: `src/snapshot/`, `session/revert.ts`). After each user turn, create a git snapshot (a commit with the session's current worktree state). Implement `/undo` and `/redo` commands that check out prior/next snapshots. Test that undoing a turn restores the pre-turn worktree byte-identically (git diff should show nothing).

- [ ] **External file staleness warning**: Integrate `notify` crate to detect file modifications outside the session (spec: `packages/core/src/file.ts`, `filesystem.ts`). If a file is read this session and then modified externally, flag it for the model (e.g., "This file was modified externally since the session started."). Test by reading a file, modifying it in another terminal, and verifying the flag appears in the next tool result.

- [ ] **Clipboard integration**: Integrate `arboard` crate. Implement a `/copy-last` command that copies the last assistant message to the clipboard. Test that pasting works in another application.

---

## P7: Parity Stretch (L effort — mostly landed; unchecked items name what's still open)

**Goal**: Achieve feature parity with opencode v1.18.13 where feasible; support server mode and additional providers. **Status, 2026-08-11**: six of seven items landed in full or in part; the remaining gaps are named per item below rather than the whole phase staying "optional."

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| Trait extraction (the `Transport` abstraction) | Go's `io.Writer` interface (late binding over single impl) |
| `rusqlite` + hand-rolled migrations | Go's `database/sql` + `golang-migrate`/`goose` |
| OAuth device-code and PKCE loopback flows | `golang.org/x/oauth2` device flow + a local `net/http` callback listener |

### Exercises (pick any — most have landed; unchecked items name what's still open)

- [x] **`ganja serve` (HTTP + SSE transport)**: Implement `ganja-core/src/server/` and `ganja-cli/serve` subcommand (spec: `src/server/`, `packages/protocol`). Extract a `Transport` trait that abstracts over direct `Engine` calls (TUI's transport) and HTTP/SSE (server's transport). The serialized `Command`/`Event` protocol is the spec. Test with a `curl`-driven client that sends commands and reads SSE events.
  **Landed**: as its own `ganja-serve` crate (not a `ganja-core` submodule — the engine stays terminal- and HTTP-free by a CI-enforced boundary), with `ganja-client` as the typed consumer `run --attach` drives (commits `f620fee`, `cef7736`; the crate split hardened further in P10, `.omc/plans/2026-08-06-crate-topology-and-ledger-execution.md` stage S5).

- [x] **`ganja run` (non-interactive mode)**: Implement `ganja run -m "prompt" --format json` (spec: `src/cli/`). Run a single turn in headless mode, outputting the final message as JSON. Test that `ganja run -m "read README.md" --format json` returns JSON with the file contents.
  **Landed**: `ganja run "prompt" --format json`, plus `--continue`, `--auto`, and `--attach` beyond the original scope (`CLAUDE.md` Commands section).

- [x] **OAuth providers**: Implement Anthropic subscription OAuth + GitHub Copilot device flow (spec: `packages/core/src/oauth/`, `github-copilot/`). Gate high-end models (Claude Pro, Max) behind authentication. Test with a fixture token.
  **Landed**: both, plus grok (device *and* loopback-browser methods) and cursor's browser-and-long-poll pairing — beyond the original two-provider scope (`.omc/handoffs/team-exec-p8.md` A-4/A-5; hygiene fixes for piped-stdin misrouting and non-interactive Copilot logins followed in P9, `.omc/handoffs/team-exec-p9.md` D348–D350).

- [x] **SQLite storage migration**: Replace JSON storage with rusqlite (spec: `packages/core/src/database/`). Implement a schema with tables for sessions, messages, parts, usage. Write a migration tool that converts existing JSON sessions to SQLite. Test that old and new storage formats can coexist and are queryable.
  **Landed**: `storage.rs`/`snapshot.rs` — one SQLite database per project, converted from the old file tree on first open (`CLAUDE.md` `ganja-core` section; P10 stage S1).

- [ ] **Advanced tools** (websearch, skills, question, plan-mode): Port `tool/websearch.ts`, `tool/skill.ts`, `tool/question.ts`, and a `/plan` mode that spawns multiple agent turns. Each is independently valuable; pick one or more based on interest.
  **Partially landed**: `websearch` (Exa or Parallel, keyed from the environment), `skill`, and `question` are all in `Registry::with_builtins()` (`CLAUDE.md` `ganja-tool` section) — check these off individually as you read/rebuild them. The `/plan` half is complete on both sides: `plan_exit` registers once the agent roster holds a build agent, and `plan_enter` once it holds a plan agent — the latter **synthesized** rather than ported, because upstream ships that door's description and permission vocabulary and no tool body (`CLAUDE.md`, phase-discipline paragraph; **D477**).

- [x] **Windows terminal support**: Verify the TUI works on Windows (spec: `packages/tui/src/terminal-win32.ts`). Test alt-screen mode, color rendering, and input handling. Debug and fix platform-specific issues.
  **Landed**: a dedicated CI lane (`.omc/plans/2026-08-07-windows-support.md`; commits including `f13a102`, `d3f4dea`, `24488bc`, `3a12621`, `8235943`, `2516968`, `18cd9cf`, `4d652c6`, `5cc1c09`).

- [ ] **Packaging**: Use `cargo-dist` to publish releases to GitHub and homebrew. Test that `brew install zchee/tap/ganja` works.
  **Partially landed, and partially a standing decision, not a gap**: `dist-workspace.toml` builds a self-contained local archive (`dist build` → `target/distrib/`) — check that half off once you've run it. Publishing (a CI backend, a homebrew tap) is **deliberately deferred**: "No CI backend on purpose — publishing is deferred, so nothing may generate a workflow file" (`CLAUDE.md` Key Files table). Don't chase the `brew install` half as an exercise; it's a decision, not an omission.

---

## P8: The Composer Catches Up — Eight Upstream UI Behaviors (M effort)

**Goal**: Port eight composer-facing behaviors the TUI gap analysis surfaced — prompt history, `@file#line-range` mentions, local file/image attachments, `question` custom text input, model variant switching, OSC 52 clipboard, template `` !`cmd` `` + `@file` expansion, and Ctrl+J newlines — retiring three pre-declared deviations (D5, D12, D109) in the process. Plan: `.omc/plans/2026-08-11-tui-ux-port.md`.

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| `#[serde(skip_serializing_if = "Option::is_none", default)]` field growth | Go struct-tag `omitempty` + additive JSON fields for backward compat |
| Keybind tables as `const` data (`(Action, name, chord)` rows) | Go `map[string]func()` dispatch tables |
| `BTreeMap<String, serde_json::Map<String, Value>>` for opaque provider options | Go `map[string]any` passthrough at a JSON boundary, no static shape |
| A per-turn `Arc<Mutex<T>>` cell (the permission `pending` cell, reused as the pattern for later state) | a goroutine-scoped `chan` guarded by a mutex, shared by reference into a spawned worker |

### Exercises

- [ ] **Prompt history JSONL**: Read `crates/ganja-tui/src/history.rs` (`MAX_HISTORY_ENTRIES = 50` at line 25, `load_from` at line 148, `append` at line 193). Annotate the parse-what-parses-and-self-heal load path and the consecutive-duplicate suppression on append. Write a unit test that appends 51 entries and asserts exactly 50 remain on disk.

- [ ] **`#line-range` grammar**: Read `mention.rs::parse_range` (`crates/ganja-tui/src/mention.rs:159`) — the suffix grammar `#(\d+)(?:-(\d*))?`, end kept only when `start < end`. Trace how a parsed range becomes `PartBody::File { start: Option<u32>, end: Option<u32>, .. }` in `ganja-protocol/src/lib.rs`. Write a round-trip test: parse → render → parse for `@src/lib.rs#10-20`, and confirm the reversed `#20-10` keeps only `start`.

- [ ] **Serde-pinned protocol growth**: Find the both-directions pin test for `PartBody::File`'s `start`/`end` fields (old JSON deserializes into the new struct unchanged; a `None`-valued new struct serializes back byte-identical to the old JSON). Explain in your own words why `#[serde(default)]` is load-bearing here, and what you'd reach for on Go's `encoding/json` side to get the same backward-compat guarantee.

- [ ] **Model variants, catalog to wire**: Read `ModelInfo::variants` in `crates/ganja-provider/src/catalog.rs:183` (`BTreeMap<String, serde_json::Map<String, Value>>`), then follow one variant selection through `Command::SwitchVariant` → `Event::VariantChanged` (protocol) → the active-slot storage column (`engine.rs`) → a wire's request-body splice (Anthropic `thinking`, OpenAI `reasoning_effort`). Explain the splice-order rule that keeps a wire's own required fields from being overwritten by a variant's option map.

- [ ] **OSC 52, the second clipboard channel**: Read the copy path in `crates/ganja-tui/src/clipboard.rs` and `app.rs`'s writer-serialized emission of `\x1b]52;c;<base64>\x07`. Explain why the escape is written unconditionally rather than only when the `arboard` write succeeds — read `.omc/handoffs/p8-resume.md` deviation #11 for the reasoning (headless/SSH is exactly the case where OSC 52 is the *only* delivering channel).

- [ ] **`question` custom text**: Read `component/question.rs`'s `custom` field (line 55), `offers_custom` (line 96), and `on_custom_row` (line 102). Trace what happens when a tool's `custom` flag is unset vs. `Some(false)` from the tool call through to the rendered free-text row. Write a component test asserting an empty typed answer exits editing **without replying** (not "falls back to the highlighted option" — the plan's own acceptance criterion 4 misstated this; see deviation #12 in `.omc/handoffs/p8-resume.md`, recorded specifically so nobody "fixes" the code toward the wrong wording).

---

## P9–P11: Interstitial (no dedicated exercises)

P9, P10, and P11 shipped against frozen `.omc/handoffs/team-exec-p*.md` contracts rather than standalone `.omc/plans/` documents. They're noted here for continuity, not as learning units — P11 in particular was pure housekeeping.

- **P9 — live-parity fallout** (`.omc/handoffs/team-exec-p9.md`): closed three defects the P8 live-provider pass measured — piped-stdin logins misrouting a Copilot token as a stored key, the Copilot deployment prompt blocking non-interactive logins, and `auth list` hiding a credential the environment shadowed — and moved `openai`'s wire selection from credential-picks-the-wire to vendor-picks-the-wire: every `openai`-id request now speaks the Responses API regardless of credential kind.
- **P10 — crate topology + filesystem-discovery policy** (`.omc/plans/2026-08-06-crate-topology-and-ledger-execution.md`, stages S0–S7): split the engine's HTTP surface into the `ganja-serve`/`ganja-client` crates, landed encrypted-reasoning round-tripping (`PartBody::Reasoning`, the opaque provider state a `store: false` turn hands back), added compat providers, and set the standing policy that skills/agents/MCP/hooks are all config-opt-in — no `~/.claude` or `~/.agents` directory walk-ups, ever. Worth a look if you want it: read `instruction::skill_roots` (cited from `CLAUDE.md`'s `ganja-tool` section) and compare it to how you'd gate a Go CLI's plugin discovery behind an explicit config list instead of a filesystem walk. No formal exercise checkbox — this phase is architecture reading, not a build.
- **P11 — ledger sweep** (`.omc/handoffs/team-exec-p11.md`): closed three outstanding deviations (D388 — wiring the `skill` tool's roots at the three frontend assembly sites; D389 — a permission-guard mutation that no test observed; D422 — a doc-comment split). Pure technical-debt closure, no new user-facing surface. No exercises.

---

## P12: The Composer Catches Up II — Seven Behaviors, Five Ports and Two Divergences (L effort)

**Goal**: file-path Tab completion (both the `@` menu and the slash dropdown), Ctrl+R history search, clipboard image paste, mid-turn message steering, drop→mention, Ctrl+L redraw, and a rewind/checkpoint picker. Five of the seven have upstream opencode sources; two — Ctrl+R search and Ctrl+L redraw — are named Claude Code divergences with no upstream counterpart at all. Plan: `.omc/plans/2026-08-11-claude-composer-port.md`.

**Status at time of writing**: all of P12 has landed — W1–W3, W4a steering and W4b's rewind picker are pushed, and the verification/docs wave that closed the phase is done (D445–D452 all recorded). The exercises below are written against the real landed code throughout, rewind exercise included.

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| `Arc<Mutex<Vec<T>>>` per-turn mailbox (`Steering { waiting, consumed }` in `session.rs`) | a buffered `chan T` one goroutine sends into and another drains, guarded explicitly instead of channel-native |
| Per-turn cells cloned into a handle (`TurnHandle::steer`) | Go closures capturing a shared pointer into a spawned goroutine, with no compiler-enforced ownership transfer |
| A `Vec`-backed fallback lane (`component/queue.rs::Queue.entries`) beside a primary delivery path | a Go slice used as an append/drain worklist for what a fast path (here, mid-turn steering) couldn't take |
| Protocol enum growth under serde pins (`Command::Steer`, `Event::SteerConsumed`, `RevertScope`) | Go's `encoding/json` additive fields + a hand-rolled round-trip test, minus a compile-time exhaustiveness check |
| Exhaustive `match` forcing every call site to acknowledge a new variant (the `Event::SteerConsumed { .. }` ignore arm added to `engine.rs`'s replay helper) | Go's `switch` with no `default`, caught only by `go vet` (or a linter), never by the compiler |

### Exercises

- [ ] **Tab completion, two menus, two behaviors**: Read `app.rs`'s `handle_files_key` (Tab widened to match Enter's arm) and `handle_dropdown_key` (Tab completes the buffer and closes the menu but **runs nothing** — deviation D446). Read the W1→W2 handoff's "Rejected" note in `.omc/handoffs/team-exec-p12.md` and explain why ganja's dropdown Tab is a deliberate divergence rather than a port: what does upstream's Tab actually do in `autocomplete.tsx`, and why does the Claude Code screenshot spec win here instead?

- [ ] **Drop → mention classifier**: Read `mention.rs::classify_drop` (`crates/ganja-tui/src/mention.rs:239`) — a pure function over pasted text and the project root, where every token in a paste must resolve to a path or the whole paste stays raw text. Write three adversarial test cases of your own (a pasted shell one-liner naming `./src`, a `file://` URL with a percent-escaped space, a quoted path with an escaped space) and check whether the existing table tests already cover them.

- [ ] **Ctrl+R search — the age-approximation tradeoff**: Read `component/search.rs::HistorySearch` (struct at line 80) and `history.rs`'s `Recalled`/`times` field (deviation D448: `PromptInfo`'s on-disk JSONL carries no timestamp, so entries loaded from disk share one file mtime while an entry appended this run gets a real instant). Explain the tradeoff the W2→W3 handoff flags as a judgment call made without a reviewer present, and what it would have cost to add a real per-entry timestamp instead, given P8 already pinned the JSONL wire format.

- [ ] **Clipboard image paste — why the error type split**: Read `clipboard.rs`'s `Error::NoImage` (line 53), `struct Image` (line 63), and `read_image` (line 100), and how both `System` and `Recording` implement it. Explain why `Error::NotText` needed a twin (`NoImage`) rather than being reused for "no image either," and trace a clipboard-holds-neither-text-nor-image test through `read` and `read_image` failing independently.

- [ ] **Steering mailbox — plan, handoff, then the landed code**: Read plan section "4s-2. Engine: the steer mailbox" in `.omc/plans/2026-08-11-claude-composer-port.md`, then the W4a→W4b handoff's drain-point section in `.omc/handoffs/team-exec-p12.md`, then the landed code (`9144851`): `struct Steering`/`struct SteerInput` in `crates/ganja-core/src/session.rs` (around lines 252/273), `drain_steers` (line 918), and the private `steer` method on `Engine` in `engine.rs` (around line 2757, refusing `EngineError::NotStreaming` when the turn slot is empty). Answer: why does a drained steer sort *after* the assistant message in both the live request and the stored history, rather than interleaved with the assistant's own parts — and what would a resumed session look like if it sorted differently?

- [ ] **Rewind picker — read the plan first; it isn't built yet**: Read plan section "4b. Engine" and "4c. TUI picker" for `Command::RevertTo { message_id, scope: RevertScope }`. Once W4b lands, read the real `Engine::revert_to_message` in `engine.rs` and confirm: does a `Files`-only scope really record no `RevertState` (nothing to redo, by design), while `Both`/`Conversation` reuse the existing `Command::Undo` machinery unchanged? Write one sentence explaining why that asymmetry is correct rather than a bug — the plan's own ADR calls it out as "the one genuinely new state."

---

## P13: The Runtime Catches Up — Hooks, Background Jobs, Parallel Subagents, the Inspector, MCP Maturity (L effort)

**Goal**: hooks (nine Claude-shaped events, config-declared), background shell jobs (`bash`'s `run_in_background` plus new `bash_output`/`kill_shell` tools), parallel subagent fan-out/fan-in within one running turn, the Ctrl+T inspector overlay, and MCP maturity (a `/mcp` TUI dialog, reconnect, per-server output caps, OAuth for remote servers). All five are Claude Code ports with **no** upstream opencode counterpart at all — none of the deviation numbers this phase minted, D453–D466, cite an opencode source. Plan: `.omc/plans/2026-08-11-claude-runtime-port.md`.

**Status at time of writing**: waves W1 (the inspector) through W5b (MCP OAuth) are landed and pushed, including the mid-plan presentation revision W1fu — two screenshots supplied after W1 shipped reshaped the overlay into a full-terminal takeover without touching its data model. W6 (this documentation section included, plus a verification pass against the plan's six acceptance criteria) is in flight; treat P13 as landed-but-not-yet-closed until that wave's own handoff says otherwise.

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| `tokio::sync::Notify` fired exactly once on a terminal state (`JobRegistry::kill`'s wait) | a buffered `chan struct{}` closed once, where every reader unblocks on the same close instead of racing a value |
| A registry-owned root `CancellationToken`, independent of any turn's own (`job.rs`) | a background `context.Background()` a goroutine derives its own cancel from, deliberately not inheriting a request's `context.Context` |
| `JoinSet` fan-out + apply-in-completion-order, write-in-call-order (`session.rs`'s `resolve_batch`) | `errgroup.Group` plus a mutex-guarded map keyed by call index, instead of `errgroup`'s own single ordered-return shape |
| A `HashMap`/`BTreeMap` keyed by request id replacing a single `Option<T>` cell (`PendingReplies`) | trading one buffered `chan T` for a `map[string]chan T` guarded by a mutex, so N in-flight requests each get their own reply channel |
| `#[serde(tag = "type", rename_all = "lowercase")]` internally tagged enum refusing an unknown variant by name (`HookHandler`) | a Go `json.Unmarshal` into a struct with a `Type string` discriminator, switched by hand — serde refuses the unrecognized tag before your code ever runs |

### Exercises

- [ ] **The four-phase call resolver**: Read `session.rs`'s `resolve`/`resolve_batch` — `prepare` (parse → `PreToolUse` hook → permission gate) → `start` (mark `Running`, build the `ToolCtx`) → `body` (the tool call itself, `Send + 'static`, the only phase that leaves the turn's own thread) → `finish` (LSP diagnostics → `PostToolUse` hook → terminal state). Explain why only `body` needed to become `Send`, and why `Running` is set in `start` rather than at the top of `body` — what would a caller waiting under a concurrency cap observe differently if it were the other way round?
- [ ] **The pending-reply registry migration**: Read the W4→W5a handoff's reader/writer enumeration table in `.omc/handoffs/team-exec-p13.md` (every site that touched the old `Option<PendingReply>` cell and what it became). Trace `open_permission`/`retract_permission` through `session.rs` and `engine.rs`'s `answer_permission`. Write one sentence on why "clear the slot" and "clear my request" had to become different operations once more than one child could be waiting at once.
- [ ] **Hook exit-code semantics, test-first**: Read `hook.rs`'s module doc (the five-bullet contract) and its timeout unit test (asserts `blocked == None && !allowed`). Then read `crates/ganja-core/tests/hooks.rs`'s assertion that a killed hook still lets the turn complete. Explain, in terms of the `Outcome` type, why there is structurally no code path from "the process was killed" to `allowed = true` — an absence of a route there, not a check that blocks one.
- [ ] **Background jobs vs. turn cancellation**: Read `job.rs`'s `# Lifetime` module-doc section and `JobRegistry`'s own root `CancellationToken`. Read `crates/ganja-core/tests/background_jobs.rs::a_turns_cancel_leaves_a_background_job_running`. Explain why a job's token is a child of the *registry's* root rather than of the turn that started it — what would break if it were the other way, and which existing engine invariant would that violation resemble?
- [ ] **Fan-out, fan-in, and why parts stay in call order**: Read `subagent.rs`'s `# Fan-out (D462)` section and `parallel_subagents.rs`'s `a_stored_turn_replays_its_calls_in_call_order` test — three `task` calls, the first deliberately finishing last. Trace how a result is applied to "the part its own call opened, in place" rather than appended in completion order. Why would appending in completion order break resume identity for a stored session?
- [ ] **MCP reconnect's two doors**: Read `mcp.rs`'s `# Reconnect (D463)` module-doc section: `Servers::reconnect` (manual) and `Servers::retry_once` (automatic, exactly once per server per session, via a `retried: Mutex<HashSet<String>>` that is never cleared). Explain the asymmetry the W5a handoff's "Rejected" list records — why does `reconnect` work on a server that never even reached `ever_connected`, while `retry_once` refuses one?
- [ ] **OAuth discovery to a real dial**: Read `ganja-provider/src/auth/mcp_oauth.rs`'s module doc (the four-step flow) and `ganja-core/tests/mcp_oauth.rs`'s one end-to-end test — discovery → registration → PKCE → storage under `mcp:hub` → a real dial → a poisoned access token → a `401` carrying `WWW-Authenticate` → a forced refresh → redial → `Connected`. Find the sentence in `mcp.rs`'s own doc explaining why `rmcp` needs that header present to recognize a `401` as an auth challenge at all, and what a server that omits it would experience instead.
- [ ] **The inspector's presentation, twice-revised**: Read W1's own open questions about tab-strip placement and popup sizing in `.omc/handoffs/team-exec-p13.md`, then the W1fu handoff explaining how two later screenshots (Codex CLI's overlay, Claude Code's Ctrl+O mode) resolved them into a full-terminal takeover with no border or centering. Read `component/inspector.rs`'s `banner_line`/`footer` functions. What does this sequence — landed to spec-as-understood, then reshaped once real references arrived — suggest about writing a UI component from a text description alone versus from a screenshot?

---

## Success Criteria

- [ ] All exercises completed (or deliberately skipped with rationale)
- [ ] `cargo nextest run --workspace` passes (each test in its own process); `cargo test --workspace --doc` passes separately — nextest skips doctests, so it needs its own run
- [ ] `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings` passes
- [ ] The crate-boundary gates in `CLAUDE.md`'s Commands section are green: `ganja-core` reaches neither `ratatui` nor `axum`, `ganja-provider` reaches neither `ratatui` nor the engine, and `ganja-permission`/`ganja-protocol` name nothing else of ours
- [ ] Each phase has a working `cargo run` demo (with `GANJA_PROVIDER=fake` for non-auth phases)
- [ ] You can explain the Go↔Rust mapping for each phase's concepts without notes
- [ ] **Local-machine note**: if your shell's ambient environment exports `RUSTFLAGS` with `panic=abort`, prefix every gate above with `env -u RUSTFLAGS` — this is a workaround for your own shell profile, not a project requirement; CI does not carry it
