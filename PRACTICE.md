# Learning Guide: Porting opencode to Rust with ratatui

This file maps the ganja-code build phases (P0–P7) to hands-on learning exercises in Rust and ratatui. It complements the implementation plan (`/.omc/plans/2026-08-03-opencode-rust-port.md`), which is the authoritative specification for phase goals, acceptance criteria, and architectural decisions.

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
  
  Implement the strategy in `ganja-core/src/tool/edit/mod.rs`. Port the upstream test fixtures (available in the local `.omc/reference/opencode-v1.18.11/` checkout if your team has cloned it) into `tests/fixtures/edit_*.rs` as table-driven cases. Assert that edits produce byte-identical output to upstream for ≥3 fixtures.

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

## P7: Parity Stretch (Optional; L effort; each item independently approvable)

**Goal**: Achieve feature parity with opencode v1.18.11 where feasible; support server mode and additional providers.

### Rust Concepts

| Rust Concept | Go Anchor |
|---|---|
| Trait extraction (the `Transport` abstraction) | Go's `io.Writer` interface (late binding over single impl) |

### Exercises (pick any)

- [ ] **`ganja serve` (HTTP + SSE transport)**: Implement `ganja-core/src/server/` and `ganja-cli/serve` subcommand (spec: `src/server/`, `packages/protocol`). Extract a `Transport` trait that abstracts over direct `Engine` calls (TUI's transport) and HTTP/SSE (server's transport). The serialized `Command`/`Event` protocol is the spec. Test with a `curl`-driven client that sends commands and reads SSE events.

- [ ] **`ganja run` (non-interactive mode)**: Implement `ganja run -m "prompt" --format json` (spec: `src/cli/`). Run a single turn in headless mode, outputting the final message as JSON. Test that `ganja run -m "read README.md" --format json` returns JSON with the file contents.

- [ ] **OAuth providers**: Implement Anthropic subscription OAuth + GitHub Copilot device flow (spec: `packages/core/src/oauth/`, `github-copilot/`). Gate high-end models (Claude Pro, Max) behind authentication. Test with a fixture token.

- [ ] **SQLite storage migration**: Replace JSON storage with rusqlite (spec: `packages/core/src/database/`). Implement a schema with tables for sessions, messages, parts, usage. Write a migration tool that converts existing JSON sessions to SQLite. Test that old and new storage formats can coexist and are queryable.

- [ ] **Advanced tools** (websearch, skills, question, plan-mode): Port `tool/websearch.ts`, `tool/skill.ts`, `tool/question.ts`, and a `/plan` mode that spawns multiple agent turns. Each is independently valuable; pick one or more based on interest.

- [ ] **Windows terminal support**: Verify the TUI works on Windows (spec: `packages/tui/src/terminal-win32.ts`). Test alt-screen mode, color rendering, and input handling. Debug and fix platform-specific issues.

- [ ] **Packaging**: Use `cargo-dist` to publish releases to GitHub and homebrew. Test that `brew install zchee/tap/ganja` works.

---

## Success Criteria

- [ ] All exercises completed (or deliberately skipped with rationale)
- [ ] `cargo test --workspace` passes
- [ ] `cargo fmt --check && cargo clippy --all-targets -- -D warnings` passes
- [ ] Each phase has a working `cargo run` demo (with `GANJA_PROVIDER=fake` for non-auth phases)
- [ ] You can explain the Go↔Rust mapping for each phase's concepts without notes
