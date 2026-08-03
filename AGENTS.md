<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# ganja-code

## Purpose

`ganja` is a terminal-first AI coding agent: a **behavioral port of [opencode](https://github.com/anomalyco/opencode) v1.18.11 to Rust**, with a ratatui TUI. Upstream's TypeScript is the *specification*, not source to translate — the port writes idiomatic Rust and matches observable behavior. The workspace is three crates: an engine that carries no terminal dependency, a ratatui frontend, and the `ganja` binary that wires them together.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Workspace manifest. **Every** dependency version is declared here; member crates only opt in with `x.workspace = true`. Each entry carries a comment explaining why that crate (or that feature) is in the tree. |
| `Cargo.lock` | Resolved dependency graph; committed. Large — read only when a specific version is in question. |
| `rust-toolchain.toml` | Pins the **nightly** channel plus clippy, rustfmt, rust-analyzer. Edition 2024. |
| `CLAUDE.md` | Symlink to this file, so tools that look for either name read the same document. |
| `PRACTICE.md` | Phase-to-exercise mapping for the owner, a Go expert learning Rust. Explanations here lean on Go anchors. |
| `THIRD_PARTY_NOTICES.md` | Upstream MIT attribution. Any text ported from opencode (tool prompts, themes) must be recorded here. |
| `README.md` | Stub. |
| `LICENSE` | Apache-2.0. |
| `CODE_OF_CONDUCT.md` | Contributor covenant. |
| `.gitignore` | Ignores `target/`, `.omc/` (operational state) and `upstream/` (CI's spec checkout). |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `crates/` | The three workspace members (see `crates/AGENTS.md`) |
| `.github/` | CI configuration (see `.github/AGENTS.md`) |
| `.omc/` | **Gitignored operational state**, not documented by this tree: `plans/` (the authoritative port plan), `handoffs/` (frozen per-phase contracts), `reference/opencode-v1.18.11/` (the upstream checkout the golden test drives). |
| `target/` | Cargo build output. Never read. |

## Commands

```sh
cargo build --workspace
cargo run                       # TUI; no GANJA_PROVIDER set means the built-in fake provider
cargo run -- auth login         # also: auth list, auth logout, models

# The gates CI runs, in order
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
! cargo tree -p ganja-core -e normal | grep -q ratatui   # core stays terminal-free
```

The last gate is inverted deliberately: a plain `grep -c` exits non-zero on zero matches and would fail exactly when the core is pure.

Single tests:

```sh
cargo test -p ganja-core --test golden               # one integration binary
cargo test -p ganja-core permission::                # unit tests by module path
cargo test -p ganja-tui --lib snapshot_tool_error    # one snapshot test
cargo insta review                                   # TUI snapshots: crates/ganja-tui/src/snapshots/
```

Two suites need setup, and both are documented in `crates/ganja-core/tests/AGENTS.md`:

- **Golden differential** runs in the default `cargo test` and **hard-fails rather than skips** when its prerequisites are missing — a green run that compared against nothing would be worthless. It needs `bun` on `PATH` and an upstream checkout with `bun install` already run, at `.omc/reference/opencode-v1.18.11` or wherever `GANJA_OPENCODE_DIR` points (CI checks it out to `upstream/`).
- **Live provider tests** are `#[ignore]`d *and* inert unless opted in: `GANJA_LIVE_TEST=1 ANTHROPIC_API_KEY=… cargo test -p ganja-core --test live -- --ignored`.

## Environment

| Variable | Meaning |
|---|---|
| `GANJA_PROVIDER` | `anthropic` \| `openai` \| `fake`. Unset means `fake` with a notice in the status bar. |
| `GANJA_MODEL` | Overrides the catalog's default model for the selected provider. |
| `GANJA_FAKE_SCRIPT` | Path to a JSON script the fake provider plays (text + tool calls per turn). How PTY tests and demos drive a deterministic agent. Read only on the `from_env` route, never by `FakeProvider::default()`. |
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` | Credential; outranks the stored `auth.json` key. |
| `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` | Endpoint override; must be `https` or loopback or the provider refuses. |
| `GANJA_LIVE_TEST`, `GANJA_OPENCODE_DIR` | Test opt-ins, above. |

## Architecture

Three crates, and the boundary between the first two is load-bearing.

**`ganja-core`** — the engine. No terminal dependency (CI-enforced), so it is testable headless and can later be served over a socket. Every `Command`, `Event` and message type is serde-derived from day one; that serialization constraint — not a trait — is what preserves the path to `ganja serve` (P7).

- `engine.rs` — commands in, an ordered event stream out. Delivery is **lossless**: a bounded `mpsc`, so backpressure lands on the turn task and never on the render loop. **One subscriber**, **one turn at a time**. The event stream is complete — a frontend that applies every event holds exactly what the next `ChatRequest` will carry.
- `session.rs` — the agent loop. Mark a step, ask the model, execute the tools it called, repeat until a request ends without tool calls. **Tool results are information, never control flow**: a refusal, an unknown tool, unparseable arguments or a failed tool all become error text the model reads next, and the loop continues. Only a cancel or a dead provider ends a turn early; there is no step cap.
- `provider/` — `Provider` trait, Anthropic Messages and OpenAI chat completions over a hand-rolled SSE splitter, plus the fake provider. Two failure channels, never a completed turn. Retry only before the first byte. Credential-travel bounds (no redirects, https-or-loopback) live in `provider/mod.rs`.
- `tool/` — `Tool` trait + `Registry::with_builtins()` (read, edit, write, glob, grep, `bash`, todowrite, webfetch). Schemas generated from the argument structs; `FileTimes` enforces read-before-write; glob/grep run in-process on the ripgrep crates.
- `permission.rs` — a call becomes one or more patterns; the **last matching rule wins**, and **every** pattern must be allowed for the call to run unasked. Shell "always" answers remember the *kind* of command via upstream's arity table.
- `auth.rs` / `project.rs` / `catalog.rs` — credentials (env beats file, `SecretString` throughout), project resolution by walking up to `.git`, and a compiled-in models.dev pricing snapshot.

**`ganja-tui`** — every pixel, no engine logic. One `tokio::select!` owns all mutable UI state; `App::handle` is the only mutator, which is what makes components testable without a terminal. Frames coalesce to 16ms for streaming bursts; a keystroke redraws immediately.

**`ganja-cli`** — clap; no subcommand starts the TUI.

## For AI Agents

### Working In This Directory

- **Respect phase discipline.** P0–P3 have landed (workspace, TUI shell, providers, agent loop + tools + permissions). P4 (sessions & compaction) is **in progress**; `crates/ganja-core/src/storage.rs` is being written against the contract frozen in `.omc/handoffs/team-exec-p4.md`. Scope, acceptance criteria and the ADR live in `.omc/plans/2026-08-03-opencode-rust-port.md`. Do not build ahead of the current phase.
- **Check `git status` before editing.** Phase execution assigns **one owner per file**. A dirty file belongs to another lane — do not finish somebody else's work in flight.
- **Port behavior, not code.** Module docs cite the upstream file they port (`//! Spec: upstream packages/opencode/src/tool/edit.ts`). Deliberate divergences are documented at the point they occur, with the reason.
- **Comments explain why, not what** — including in `Cargo.toml`. Match the surrounding density.
- Never pick a dependency version in a member crate; add it to the workspace manifest with its rationale.

### Testing Requirements

The four gates above, all green, before a phase is called done. Unit tests live beside the code in `#[cfg(test)] mod tests`; anything needing a real socket, a real filesystem layout, or process-wide environment mutation goes in a crate's `tests/`.

### Common Patterns

- Test names are sentences about behavior: `a_denied_edit_leaves_the_file_untouched`, `a_second_subscriber_is_refused`.
- A test that mutates process-wide state gets its **own test binary**, because `cargo test` runs a binary's tests on parallel threads.
- Anything touching stored state redirects `XDG_DATA_HOME` so it cannot read or write the real user's credentials, permissions or spilled output.
- Nothing may render a whole API key; `crates/ganja-core/tests/secrets_env.rs` pins that with a canary.
- Commit subjects are `scope: intent` — the crates touched, then why (`core,tui,cli: let the model act through permission-gated tools`).

## Dependencies

### External

Load-bearing choices, all pinned in the workspace manifest: `tokio` (runtime), `ratatui` 0.30 + `ratatui-textarea` (TUI), `reqwest` with rustls (provider HTTP; no OpenSSL), `secrecy` (key material wiped on drop), `schemars` (tool argument schemas generated from the argument structs), `ignore`/`grep-searcher`/`grep-regex` (ripgrep internals, so glob and grep run in-process instead of shelling out to `rg`), `similar` (unified diffs from `edit`), `etcetera` (XDG paths), `insta` + `expectrl` + `assert_cmd` (snapshot, pty and CLI tests).

<!-- MANUAL: Any manually added notes below this line are preserved on regeneration -->
