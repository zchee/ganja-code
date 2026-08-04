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
cargo run -- --model anthropic/claude-sonnet-4-5 --agent plan   # also --config <file>
cargo run -- --continue         # or --session <id>; the two are mutually exclusive
cargo run -- auth login         # also: auth list, auth logout
cargo run -- sessions           # this project's stored conversations, roots only
cargo run -- models anthropic --refresh        # the catalog, narrowed to a provider and re-fetched first
cargo run -- config import-opencode --dry-run  # translate an opencode config, naming what it skipped

# The gates CI runs, in order
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo nextest run --workspace       # the suite; each test in its own process
cargo test --workspace --doc        # nextest skips doctests, so run them beside it
! cargo tree -p ganja-core -e normal | grep -q ratatui   # core stays terminal-free
```

The last gate is inverted deliberately: a plain `grep -c` exits non-zero on zero matches and would fail exactly when the core is pure.

Single tests (nextest filters by name substring; `cargo test` still works and is what runs doctests):

```sh
cargo nextest run -E 'binary(golden)'                # one integration binary
cargo nextest run -E 'binary(mcp)'                   # shares golden's bun + upstream-checkout prerequisites
cargo nextest run -p ganja-core permission           # tests whose name matches "permission"
cargo nextest run -p ganja-tui snapshot_tool_error   # one snapshot test
cargo insta review                                   # TUI snapshots: crates/ganja-tui/src/snapshots/
```

Two suites need setup, and both are documented in `crates/ganja-core/tests/AGENTS.md`:

- **Golden differential** runs in the default test run (`cargo nextest run`, or `cargo test`) and **hard-fails rather than skips** when its prerequisites are missing — a green run that compared against nothing would be worthless. It needs `bun` on `PATH` and an upstream checkout with `bun install` already run, at `.omc/reference/opencode-v1.18.11` or wherever `GANJA_OPENCODE_DIR` points (CI checks it out to `upstream/`). The MCP suite (`tests/mcp.rs`) shares those prerequisites — its reference server runs on the checkout's installed `@modelcontextprotocol/sdk` — and hard-fails the same way.
- **Live provider tests** are `#[ignore]`d *and* inert unless opted in: `GANJA_LIVE_TEST=1 ANTHROPIC_API_KEY=… cargo test -p ganja-core --test live -- --ignored`.

## Environment

| Variable | Meaning |
|---|---|
| `GANJA_PROVIDER` | `anthropic` \| `openai` \| `fake`. Unset means `fake` with a notice in the status bar. |
| `GANJA_MODEL` | Overrides the catalog's default model for the selected provider. |
| `GANJA_FAKE_SCRIPT` | Path to a JSON script the fake provider plays (text + tool calls per turn). How PTY tests and demos drive a deterministic agent. Read only on the `from_env` route, never by `FakeProvider::default()`. |
| `GANJA_CONFIG` | An extra config file merged between the global tier and the project files. Naming a file that does not exist is an error, where an absent discovered file is nothing to merge. |
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` | Credential; outranks the stored `auth.json` key. |
| `EDITOR` | What `/editor` hands the prompt buffer to. Unset or empty falls back to `vi`. |
| `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` | Endpoint override; must be `https` or loopback or the provider refuses. |
| `GANJA_MODELS_URL` | Base URL the live model catalog is fetched from (`/api.json` appended); default `https://models.opencode.ai`. |
| `GANJA_MODELS_PATH` | Read-only override of the catalog cache path; writes keep going to the canonical cache file. |
| `GANJA_DISABLE_MODELS_FETCH` | Truthy (`1`/`true`) disables all catalog fetching — the disk cache and the compiled-in snapshot still serve. |
| `GANJA_LIVE_TEST`, `GANJA_OPENCODE_DIR` | Test opt-ins, above. |

## Architecture

Three crates, and the boundary between the first two is load-bearing.

**`ganja-core`** — the engine. No terminal dependency (CI-enforced), so it is testable headless and can later be served over a socket. Every `Command`, `Event` and message type is serde-derived from day one; that serialization constraint — not a trait — is what preserves the path to `ganja serve` (P7).

- `engine.rs` — commands in, an ordered event stream out. Delivery is **lossless**: a bounded `mpsc`, so backpressure lands on the turn task and never on the render loop. **One subscriber**, **one turn at a time**. The event stream is complete — a frontend that applies every event holds exactly what the next `ChatRequest` will carry.
- `session.rs` — the agent loop. Mark a step, ask the model, execute the tools it called, repeat until a request ends without tool calls. **Tool results are information, never control flow**: a refusal, an unknown tool, unparseable arguments or a failed tool all become error text the model reads next, and the loop continues. Only a cancel or a dead provider ends a turn early; there is no step cap.
- `provider/` — `Provider` trait, Anthropic Messages and OpenAI chat completions over a hand-rolled SSE splitter, plus the fake provider. Two failure channels, never a completed turn. Retry only before the first byte. Credential-travel bounds (no redirects, https-or-loopback) live in `provider/mod.rs`.
- `tool/` — `Tool` trait + `Registry::with_builtins()` (read, edit, write, glob, grep, `bash`, todowrite, webfetch), plus `task`, which the engine registers once it knows which agents this session may spawn. Schemas generated from the argument structs; `FileTimes` enforces read-before-write; glob/grep run in-process on the ripgrep crates. `write` and `edit` reach the disk through a directory descriptor (`anchor.rs`), never twice through a path, so a link swapped in after the permission dialog has nothing to redirect.
- `permission.rs` — a call becomes one or more patterns; the **last matching rule wins**, and **every** pattern must be allowed for the call to run unasked. Rules layer builtin defaults < the agent's < the config's < the answers a person stored. Shell "always" answers remember the *kind* of command via upstream's arity table. A subagent inherits the refusals and never the allows: nobody is watching its turn.
- `mcp.rs` — config-named MCP servers, dialled concurrently in the background at startup and never reconnected. Their tools join the registry as `mcp__<server>__<tool>`, every one of them asking by default.
- `agent.rs` / `instruction.rs` / `command.rs` — who a turn runs as (build, plan, general, explore, plus whatever the config adds), the system prompt it runs under (a base prompt per model family, the `<env>` block, the `AGENTS.md` family), and the slash commands it can run.
- `config.rs` — `ganja.jsonc`/`ganja.json` across three tiers, under the environment and the flags. Unknown top-level keys are refused by name.
- `auth.rs` / `project.rs` / `catalog.rs` / `storage.rs` — credentials (env beats file, `SecretString` throughout), project resolution by walking up to `.git`, model sizing and pricing (fetched, cached under the XDG cache home, falling back to a compiled-in snapshot that never fails), and versioned session storage.

**`ganja-tui`** — every pixel, no engine logic. One `tokio::select!` owns all mutable UI state; `App::handle` is the only mutator, which is what makes components testable without a terminal. Frames coalesce to 16ms for streaming bursts; a keystroke redraws immediately. Themes are loadable data (four ported from upstream, plus whatever `~/.config/ganja/themes/` holds), the palette and the `/` menu are two views of the same command set, `@` raises a file menu, and a leading `!` hands the line to a shell. `/copy` and `/copy-message` put the conversation or the last reply on the clipboard, pasted text arrives whole through bracketed paste, and a configured MCP server that cannot be reached is named in the status bar.

**`ganja-cli`** — clap; no subcommand starts the TUI.

## For AI Agents

### Working In This Directory

- **Respect phase discipline.** P0–P5 have landed: the workspace, the TUI shell, providers, the agent loop with tools and permissions, sessions and compaction, and now config, agents, commands, themes, the model catalog, the task tool, `@file` mentions and `!` passthrough. Scope, acceptance criteria and the ADR live in `.omc/plans/2026-08-03-opencode-rust-port.md`, and each phase's frozen contract in `.omc/handoffs/`. Do not build ahead of the current phase — P6 (markdown rendering, MCP, LSP, undo, the `/share` family) is not started.
- **Check `git status` before editing.** Phase execution assigns **one owner per file**. A dirty file belongs to another lane — do not finish somebody else's work in flight.
- **Port behavior, not code.** Module docs cite the upstream file they port (`//! Spec: upstream packages/opencode/src/tool/edit.ts`). Deliberate divergences are documented at the point they occur, with the reason.
- **Comments explain why, not what** — including in `Cargo.toml`. Match the surrounding density.
- Never pick a dependency version in a member crate; add it to the workspace manifest with its rationale.

### Testing Requirements

The four gates above, all green, before a phase is called done. Unit tests live beside the code in `#[cfg(test)] mod tests`; anything needing a real socket, a real filesystem layout, or process-wide environment mutation goes in a crate's `tests/`.

### Common Patterns

- Test names are sentences about behavior: `a_denied_edit_leaves_the_file_untouched`, `a_second_subscriber_is_refused`.
- A test that mutates process-wide state gets its **own test binary**. nextest already gives each test its own process, but the separation still holds the line under a plain `cargo test` (which runs a binary's tests on parallel threads) and keeps the intent legible.
- Anything touching stored state redirects `XDG_DATA_HOME` so it cannot read or write the real user's credentials, permissions or spilled output.
- Nothing may render a whole API key; `crates/ganja-core/tests/secrets_env.rs` pins that with a canary.
- Commit subjects are `scope: intent` — the crates touched, then why (`core,tui,cli: let the model act through permission-gated tools`).

## Dependencies

### External

Load-bearing choices, all pinned in the workspace manifest: `tokio` (runtime), `ratatui` 0.30 + `ratatui-textarea` (TUI), `reqwest` with rustls (provider HTTP; no OpenSSL), `secrecy` (key material wiped on drop), `schemars` (tool argument schemas generated from the argument structs), `ignore`/`grep-searcher`/`grep-regex` (ripgrep internals, so glob and grep run in-process instead of shelling out to `rg`), `similar` (unified diffs from `edit`), `etcetera` (XDG paths), `jsonc-parser` (config files in the dialect upstream's are written in, decoded in document order so permission rules keep theirs), `nucleo-matcher` (fuzzy ranking behind the palette), `libc` (the unix calls the shell tool and the anchored file I/O are built on), `insta` + `expectrl` + `assert_cmd` (snapshot, pty and CLI tests).

<!-- MANUAL: Any manually added notes below this line are preserved on regeneration -->
