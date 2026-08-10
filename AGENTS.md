<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# ganja-code

## Purpose

`ganja` is a terminal-first AI coding agent: a **behavioral port of [opencode](https://github.com/anomalyco/opencode) v1.18.13 to Rust**, with a ratatui TUI. Upstream's TypeScript is the *specification*, not source to translate — the port writes idiomatic Rust and matches observable behavior. The workspace is nine crates: the protocol, the permission engine, the tools and the provider wires beneath an engine that carries no terminal dependency, then a ratatui frontend, an HTTP frontend, the client that attaches to it, and the `ganja` binary that wires them together.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Workspace manifest. **Every** dependency version is declared here; member crates only opt in with `x.workspace = true`. Each entry carries a comment explaining why that crate (or that feature) is in the tree. |
| `Cargo.lock` | Resolved dependency graph; committed. Large — read only when a specific version is in question. |
| `rust-toolchain.toml` | Pins a **date-pinned nightly** (`nightly-2026-08-03`) plus clippy, rustfmt, rust-analyzer; CI's toolchain steps repeat the same date because the action does not read this file. Edition 2024. |
| `CLAUDE.md` | Symlink to this file, so tools that look for either name read the same document. |
| `PRACTICE.md` | Phase-to-exercise mapping for the owner, a Go expert learning Rust. Explanations here lean on Go anchors. |
| `THIRD_PARTY_NOTICES.md` | Upstream MIT attribution. Any text ported from opencode (tool prompts, themes) must be recorded here. |
| `dist-workspace.toml` | Local packaging only: `dist build` produces a self-contained archive (`target/distrib/`). No CI backend on purpose — publishing is deferred, so nothing may generate a workflow file. |
| `README.md` | Stub. |
| `LICENSE` | Apache-2.0. |
| `CODE_OF_CONDUCT.md` | Contributor covenant. |
| `.gitignore` | Ignores `target/`, `.omc/` (operational state) and `upstream/` (CI's spec checkout). |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `crates/` | The workspace members (see `crates/AGENTS.md`) |
| `.github/` | CI configuration (see `.github/AGENTS.md`) |
| `.omc/` | **Gitignored operational state**, not documented by this tree: `plans/` (the authoritative port plan), `handoffs/` (frozen per-phase contracts), `reference/opencode-v1.18.13/` (the upstream checkout the golden test drives). |
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
cargo run -- models cursor      # cursor's live roster from the wire itself — uncataloged, so no sizing or pricing
cargo run -- mcp                # the configured MCP servers, dialled, with the tools they lend
cargo run -- run "what does this crate do"        # one headless turn; --format json for a script
cargo run -- run --continue --auto "now fix it"   # --auto allows what a headless run otherwise refuses
cargo run -- config import-opencode --dry-run  # translate an opencode config, naming what it skipped
cargo run -- serve --port 4096  # the engine over HTTP + SSE; loopback unless GANJA_SERVER_PASSWORD is set

# The gates CI runs, in order
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo nextest run --workspace       # the suite; each test in its own process
cargo test --workspace --doc        # nextest skips doctests, so run them beside it
! cargo tree -p ganja-core -e normal | grep -q ratatui   # core stays terminal-free
! cargo tree -p ganja-core -e normal | grep -q axum      # the engine never grows an HTTP server
! cargo tree -p ganja-provider -e normal | grep -q ratatui  # and neither does a wire
# The internal-dependency allowlists, which fail closed where a `! grep` would not:
#   ganja-tool     = "ganja-permission "
#   ganja-provider = "ganja-permission ganja-protocol ganja-tool "
#   ganja-core     = "ganja-permission ganja-protocol ganja-provider ganja-tool "
#   ganja-client   = "ganja-protocol "
! cargo tree -p ganja-permission -e normal | tail -n +2 | grep -q ganja-  # the rules need nothing of ours
! cargo tree -p ganja-protocol -e normal | tail -n +2 | grep -q ganja-    # the wire types even less
```

The inverted gates are deliberate: a plain `grep -c` exits non-zero on zero matches and would fail exactly when the boundary holds. Together they assert every direction that matters — the engine reaches no terminal, nothing beneath the engine reaches the engine, and the bottom crates stay leaves.

Single tests (nextest filters by name substring; `cargo test` still works and is what runs doctests):

```sh
cargo nextest run -E 'binary(golden)'                # one integration binary
cargo nextest run -E 'binary(mcp)'                   # shares golden's bun + upstream-checkout prerequisites
cargo nextest run -E 'binary(lsp)'                   # needs rust-analyzer on PATH (rustup component); hard-fails without it
cargo nextest run -p ganja-permission                # the permission engine's own tests
cargo nextest run -p ganja-provider                  # the wires, the logins and the catalog
cargo nextest run --workspace permission             # every test whose name matches "permission"
cargo nextest run -p ganja-tui snapshot_tool_error   # one snapshot test
cargo insta review                                   # TUI snapshots: crates/ganja-tui/src/snapshots/
```

Two suites need setup, and both are documented in `crates/ganja-core/tests/AGENTS.md`:

- **Golden differential** runs in the default test run (`cargo nextest run`, or `cargo test`) and **hard-fails rather than skips** when its prerequisites are missing — a green run that compared against nothing would be worthless. It needs `bun` on `PATH` and an upstream checkout with `bun install` already run, at `.omc/reference/opencode-v1.18.13` or wherever `GANJA_OPENCODE_DIR` points (CI checks it out to `upstream/`). The MCP suite (`tests/mcp.rs`) shares those prerequisites — its reference server runs on the checkout's installed `@modelcontextprotocol/sdk` — and hard-fails the same way.
- **Live provider tests** are `#[ignore]`d *and* inert unless opted in: `GANJA_LIVE_TEST=1 ANTHROPIC_API_KEY=… cargo test -p ganja-core --test live -- --ignored`.

## Environment

| Variable | Meaning |
|---|---|
| `GANJA_PROVIDER` | `anthropic` \| `openai` \| `grok` \| `github-copilot` \| `fake` \| `cursor`. Unset means `fake` with a notice in the status bar. `grok`, `github-copilot` and `cursor` are entered by `ganja auth login`, not by a key variable; `openai` speaks the **Responses API on either credential** — an API key against `api.openai.com/v1`, a stored ChatGPT login against the codex backend — because that vendor's wire is the vendor's, not the credential's. `cursor` speaks Connect to cursor's agent backend and still rides the uncataloged tier — no sizing, pricing or auto-compaction — but it defaults to `default`, the server-side Auto id its own wire publishes, so a session runs unnamed; the full roster is one `ganja models cursor` away, and the `/model` chooser serves the same listing. Its login pairs a browser with a long-polling terminal, and a turn carries only the newest message until history composition on cursor's blob channel lands. |
| `GANJA_MODEL` | Overrides the catalog's default model for the selected provider. A config's `provider` table adds ids of its own to what `GANJA_PROVIDER` accepts: an entry names a `dialect` (`openai-chat-completions` or `anthropic-messages`), a `base_url`, optionally the `key_env` holding its key and the `headers` it wants. Such a provider is *selectable* but not *cataloged* — sizing, pricing and auto-compaction are off for it, and it must be told which model to ask for. |
| `GANJA_FAKE_SCRIPT` | Path to a JSON script the fake provider plays (text + tool calls per turn). How PTY tests and demos drive a deterministic agent. Read only on the `from_env` route, never by `FakeProvider::default()`. |
| `GANJA_CONFIG` | An extra config file merged between the global tier and the project files. Naming a file that does not exist is an error, where an absent discovered file is nothing to merge. |
| `GANJA_CONFIG_HOME` | The directory ganja keeps its own things in — the global `ganja.jsonc`/`ganja.json`, the global `AGENTS.md`, and the `skills/` and `themes/` beneath it. Set, it outranks both discovered locations unconditionally; unset, the home is `$XDG_CONFIG_HOME/ganja` when that directory exists, else `~/.ganja` when that one does, else `$XDG_CONFIG_HOME/ganja` as the place a writer should create. One home, not a merge — somebody holding both directories is served the XDG one. Distinct from `GANJA_CONFIG`, which names a config **file** to merge in and moves nothing else. |
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` | Credential; outranks the stored `auth.json` key. |
| `EDITOR` | What `/editor` hands the prompt buffer to. Unset or empty falls back to `vi`. |
| `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` | Endpoint override; must be `https` or loopback or the provider refuses. `OPENAI_BASE_URL` now points a *Responses* client at what it names, so a chat-completions-only server (a local llama.cpp) is no longer reachable as `GANJA_PROVIDER=openai`. |
| `GANJA_MODELS_URL` | Base URL the live model catalog is fetched from (`/api.json` appended); default `https://models.opencode.ai`. |
| `GANJA_MODELS_PATH` | Read-only override of the catalog cache path; writes keep going to the canonical cache file. |
| `GANJA_DISABLE_MODELS_FETCH` | Truthy (`1`/`true`) disables all catalog fetching — the disk cache and the compiled-in snapshot still serve. |
| `GANJA_AUTH_ISSUER` | Origin every login endpoint is reached at, so a test can complete a login against endpoints it owns. **Loopback only**, matched as a whole origin — a value that is set and is not loopback is refused rather than ignored, because silently using the real issuer is the one outcome whoever set it cannot have wanted. |
| `GANJA_SERVER_PASSWORD` | When set, every `ganja serve` route requires HTTP Basic auth with it; when unset, only loopback binds are allowed. |
| `GANJA_SERVER_USERNAME` | The Basic username `serve` expects; default `ganja`. |
| `EXA_API_KEY` | Credential `websearch` presents to Exa, in that service's query string. Absent, a search that would have gone to Exa is refused naming this variable rather than sent unauthenticated — a divergence from upstream, which sends it anyway. |
| `PARALLEL_API_KEY` | The same for Parallel, presented as a bearer token. |
| `GANJA_WEBSEARCH_PROVIDER` | `exa` \| `parallel`. Names the service `websearch` asks. Unset, the service is the one whose key this machine holds — `exa` when both are. Upstream instead splits sessions by hashing the session id, which a tool here cannot see. |
| `GANJA_LIVE_TEST`, `GANJA_OPENCODE_DIR` | Test opt-ins, above. |

## Architecture

Nine crates. The load-bearing boundaries are asserted rather than trusted: nothing in the engine may reach a terminal or an HTTP server, nothing beneath the engine may reach the engine, and the two bottom crates name nothing else of ours. The graph is a DAG, not a chain — frontends sit on `ganja-core`, core on `ganja-provider` and `ganja-tool`, both of those on `ganja-permission`, while `ganja-protocol` is a leaf that core, the provider crate, the frontends and `ganja-client` consume directly and that tool and permission never touch. Every "nothing beneath may reach the engine" claim is an **allowlist** in CI rather than a `! grep ganja-core`, because a blocklist names one crate and goes quiet the day a new one appears.

**`ganja-protocol`** — the types every side of the app speaks: `Command`, `Event`, `Message`/`Part`, `ToolState`, `Usage`, and the ids that sort in creation order. Serde-derived from day one; that serialization constraint — not a trait — is what preserves the path to serving the engine over a socket and to transcripts that replay from disk. Its dependency list is `serde` and the value type a tool call's arguments arrive as, which is the whole reason it is a crate: a frontend that renders a transcript builds nothing else. `PartBody::Reasoning` is the opaque provider state a `store: false` turn hands back on its next request.

**`ganja-permission`** — a call becomes one or more patterns; the **last matching rule wins**, and **every** pattern must be allowed for the call to run unasked. Rules layer builtin defaults < the agent's < the config's < the answers a person stored. Shell "always" answers remember the *kind* of command via upstream's arity table. A subagent inherits the refusals and never the allows: nobody is watching its turn. `project.rs` rides along — which worktree this is decides where the answers are stored and what counts as outside it — and the crate's own docs own that the name is a small lie about that.

**`ganja-tool`** — `Tool` trait + `Registry::with_builtins()` (read, edit, write, glob, grep, `bash`, todowrite, webfetch, `websearch` — Exa or Parallel over one JSON-RPC POST, keys from the environment, refused politely without one — `skill`, a `SKILL.md` under one of ganja's own two homes — `<config home>/skills` and `<project root>/.ganja/skills` — or a directory `skills.paths` named, loaded into the conversation on request; **nothing foreign is discovered**, a standing ruling that leaves upstream's `~/.claude`, `~/.agents` and their walk-ups unread and one config line away, and `question`, the one tool whose whole purpose is to wait for a person), plus `task`, which the engine registers once it knows which agents this session may spawn. The roster's `skill` holds no directories at all — where ganja keeps its things is not a question a tool may answer — so each of the three real frontends installs one over it holding `instruction::skill_roots`, the same value its prompt's `<available_skills>` block was composed from; a registry a *fixture* builds stays rootless, which is what keeps the golden differential comparing two agents rather than two laptops. Schemas generated from the argument structs; `FileTimes` enforces read-before-write; glob/grep run in-process on the ripgrep crates. `write` and `edit` reach the disk through a directory descriptor (`anchor.rs`), never twice through a path, so a link swapped in after the permission dialog has nothing to redirect. `watch.rs` is here too, beside the log it reports into. It depends on `ganja-permission` and on nothing else of ours — the engine is deliberately outside its graph, so a tool that seems to need the loop is a tool that needs another value in its `ToolCtx`.

**`ganja-provider`** — talking to a model vendor: the wires, the credentials they present, and the table that sizes and prices what they serve. A wire turns a `ChatRequest` into HTTP and an HTTP response back into a stream of `ProviderEvent`s; it knows nothing about sessions, tools or storage, and with the engine outside its dependency graph that is the compiler's rule rather than a convention. Anthropic Messages, OpenAI's Responses API, the chat-completions wire grok and Copilot ride, the Connect wire cursor's agent backend speaks, the fake provider, and `compat` — not another wire but the two real ones reached at an endpoint a config named, under the id it named. The cursor wire has no upstream TypeScript to port, so its spec is a recorded live probe: Connect, not bare gRPC — enveloped streams whose failures arrive as in-body EndStream frames — with the framing hand-written over the same reqwest/rustls stack, the messages riding `buffa` (pure-Rust protobuf, license-matched), and ganja's own `.proto` checked in beside its generated module under a drift test. Its Run RPC is a duplex: the request body is held open because the server generates by *asking* — a context exec whose answer is the one system-prompt channel cursor honors, and kv blob get/set served from a per-turn map — and an ask this build cannot answer fails the turn naming it, never hangs it; the prost/tonic fence that once reserved a separate crate for a gRPC stack retired with the reason it existed. Everything filed under `openai` speaks Responses and asks it to seal the model's reasoning, which the transcript carries and the next request hands back; the credential picks the *backend* and what the request carries beside the bearer. Two failure channels, never a completed turn; retry only before the first byte; credential-travel bounds (no redirects, https-or-loopback) live here. **Auth and the catalog fold in rather than standing alone**: the auth→provider edge is one function (`reachable_in_the_clear`, the OpenAI login's redirect check) against some forty provider→auth references reaching per-provider submodule internals, and the catalog names providers by the ids `auth::storage_key` maps to disk — a boundary between any two of them would carry no invariant anyone would gate. What did *not* move is selection: `select` reads a `Config`, so it stayed in the engine over a facade that re-exports the wires.

**`ganja-core`** — the engine. No terminal dependency (CI-enforced), so it is testable headless and can later be served over a socket. It re-exports the four crates above under the module names they always had — `ganja_core::protocol`, `::permission`, `::project`, `::tool`, `::watch`, `::auth`, `::catalog` — so each split cost no caller a rewrite; new code that wants one of them alone should depend on it directly, as `ganja-cli` does for `auth login`. `ganja_core::provider` is the one facade that is not a bare re-export: the wires left and the half that reads a `Config` stayed, over a glob of what moved.

- `engine.rs` — commands in, an ordered event stream out. Delivery is **per-subscriber**: every subscriber has a bounded queue of its own — a lossless subscriber (`subscribe()`) makes the publisher wait, so backpressure lands on the turn task and never on the render loop, while a droppable one (`subscribe_droppable()`) is evicted with an observable error rather than waited for. The first subscriber inherits everything buffered since the engine was born; later ones join from registration. Every event names its session, filled from the engine's one current-session slot (minted at construction, adopted by the first prompt's row, replaced by resume, re-minted by `NewSession`); a subagent's crossing permission dialogs carry the parent's. **One turn at a time.** The event stream is complete — a frontend that applies every event holds exactly what the next `ChatRequest` will carry.
- `session.rs` — the agent loop. Mark a step, ask the model, execute the tools it called, repeat until a request ends without tool calls. **Tool results are information, never control flow**: a refusal, an unknown tool, unparseable arguments or a failed tool all become error text the model reads next, and the loop continues. Only a cancel or a dead provider ends a turn early; there is no step cap.
- `provider.rs` — half a module, and the half that reads a `Config`: `select`'s four-tier chain, `Selection`, `selectable`, the `Wire`/`openai_provider` dispatch that picks a backend by *kind* of credential, and the config→wire translation. The wires themselves are `ganja-provider`'s, globbed in so every path a caller already writes still resolves.
- `mcp.rs` — config-named MCP servers, dialled concurrently in the background at startup and never reconnected. Their tools join the registry as `mcp__<server>__<tool>`, every one of them asking by default.
- `lsp/` — language servers, opt-in by config and spawned lazily by the first touch of a file they claim. Diagnostics — errors only, pushed and pulled, merged and deduped — are appended to `edit`/`write` results at one seam in `session.rs`; no LSP failure may fail a tool call or a turn.
- `agent.rs` / `instruction.rs` / `command.rs` — who a turn runs as (build, plan, general, explore, plus whatever the config adds), the system prompt it runs under (a base prompt per model family, the `<env>` block, the `AGENTS.md` family), and the slash commands it can run.
- `config.rs` — `ganja.jsonc`/`ganja.json` across three tiers, under the environment and the flags. Unknown top-level keys are refused by name.
- `storage.rs` / `snapshot.rs` — versioned session storage in one SQLite database per project, converted from the old file tree on first open, and the working-tree snapshots `/undo` walks. Credentials and the model catalog moved to `ganja-provider` and are still named here as `ganja_core::auth` and `ganja_core::catalog`.

**`ganja-tui`** — every pixel, no engine logic. It links `ganja-protocol` for the types it renders, `ganja-permission` for the stored rules it loads and hands to the engine, and `ganja-tool` for the one thing it runs in-process, the `@` menu's glob walk. One `tokio::select!` owns all mutable UI state; `App::handle` is the only mutator, which is what makes components testable without a terminal. Frames coalesce to 16ms for streaming bursts; a keystroke redraws immediately. Themes are loadable data (four ported from upstream, plus whatever `<config home>/themes/` holds), the palette and the `/` menu are two views of the same command set, `@` raises a file menu, and a leading `!` hands the line to a shell. `/copy` and `/copy-message` put the conversation or the last reply on the clipboard, pasted text arrives whole through bracketed paste, and a configured MCP server that cannot be reached is named in the status bar.

**`ganja-serve`** — the engine over a socket: the legacy `/session/…` REST surface and a `GET /event` SSE stream, driving the same `Engine` the TUI does and inventing no state of its own — `GET /permission` is the one derived view, kept by a lossless subscriber that only moves map entries. Three postures are pinned by its tests: a non-loopback bind with no password is refused at startup (a deliberate divergence from upstream's warn-and-serve), the launch directory is the only directory served (anything else is `400`, never a silent answer about the wrong worktree), and no query string ever reaches a log line, because `?auth_token=` is a credential in a URL. The SSE stream opens with a `connected` frame, heartbeats every ten seconds, and ends an evicted subscriber with an observable `evicted` frame rather than a silent close; `EngineError` maps `SessionNotFound`→404 and `Busy`→409, with unparseable payloads at 400.

**`ganja-client`** — the other end of `ganja-serve`'s wire, and nothing else: the typed routes and the SSE reader that `run --attach` drives. Its internal dependency list is exactly `ganja-protocol` (CI-gated) — a client that linked the engine would be a second frontend rather than a consumer of the first one's socket. The server's frame vocabulary (`connected`, `heartbeat`, `evicted`, and the event payload) is declared on this side and pinned against a real server; a frame or field outside the declaration is refused readably as version skew, never guessed at, and the credential comes from `GANJA_SERVER_PASSWORD`/`GANJA_SERVER_USERNAME` through serve's own resolver so the two ends cannot disagree about which variable means what.

**`ganja-cli`** — clap; no subcommand starts the TUI, one of them — `run` — takes a whole turn without it, driving the same engine headless and writing either readable lines or one JSON object per event (or, under `--attach`, the same account of a turn taken on a running `ganja serve`), and another — `serve` — puts the same engine behind HTTP and SSE until a signal ends it.

## For AI Agents

### Working In This Directory

- **Respect phase discipline.** P0–P6 have landed: the workspace, the TUI shell, providers, the agent loop with tools and permissions, sessions and compaction, config, agents, commands, themes, the model catalog, the task tool, `@file` mentions, `!` passthrough — and now MCP servers, LSP diagnostics, working-tree snapshots with `/undo`/`/redo`, markdown rendering, the stale-read watcher and the system clipboard. Scope, acceptance criteria and the ADR live in `.omc/plans/2026-08-03-opencode-rust-port.md`, and each phase's frozen contract in `.omc/handoffs/`. Do not build ahead of the current phase — P7 is underway: the `mcp` listing subcommand, SQLite session storage, the windows CI lane, the toolchain date-pin, local packaging and `ganja run` — a headless turn with an nd-JSON mode — have landed, and the credential store has room for a login rather than a key; since then `ganja serve` and the `ganja-client` it answers, the OAuth logins (ChatGPT, grok, Copilot, and cursor's browser-and-long-poll pairing), the websearch/skills/question tools and the cursor wire — Connect, duplex, its context and kv asks answered — have landed too. The `plan` tool has not started.
- **Check `git status` before editing.** Phase execution assigns **one owner per file**. A dirty file belongs to another lane — do not finish somebody else's work in flight.
- **Port behavior, not code.** Module docs cite the upstream file they port (`//! Spec: upstream packages/opencode/src/tool/edit.ts`). Deliberate divergences are documented at the point they occur, with the reason.
- **Comments explain why, not what** — including in `Cargo.toml`. Match the surrounding density.
- Never pick a dependency version in a member crate; add it to the workspace manifest with its rationale.

### Testing Requirements

The four gates above, all green, before a phase is called done. Unit tests live beside the code in `#[cfg(test)] mod tests`; anything needing a real socket, a real filesystem layout, or process-wide environment mutation goes in a crate's `tests/`.

### Common Patterns

- Test names are sentences about behavior: `a_denied_edit_leaves_the_file_untouched`, `a_second_subscriber_sees_the_same_events_the_first_does`.
- A test that mutates process-wide state gets its **own test binary**. nextest already gives each test its own process, but the separation still holds the line under a plain `cargo test` (which runs a binary's tests on parallel threads) and keeps the intent legible.
- Anything touching stored state redirects `XDG_DATA_HOME` so it cannot read or write the real user's credentials, permissions or spilled output.
- Nothing may render a whole API key; `crates/ganja-core/tests/secrets_env.rs` pins that with a canary.
- Commit subjects are `scope: intent` — the crates touched, then why (`core,tui,cli: let the model act through permission-gated tools`).

## Dependencies

### External

Load-bearing choices, all pinned in the workspace manifest — which is also where each member crate is declared as a path dependency, with the reason it exists: `tokio` (runtime), `ratatui` 0.30 + `ratatui-textarea` (TUI), `reqwest` with rustls (provider HTTP; no OpenSSL), `secrecy` (key material wiped on drop), `schemars` (tool argument schemas generated from the argument structs), `ignore`/`grep-searcher`/`grep-regex` (ripgrep internals, so glob and grep run in-process instead of shelling out to `rg`), `similar` (unified diffs from `edit`), `etcetera` (XDG paths), `jsonc-parser` (config files in the dialect upstream's are written in, decoded in document order so permission rules keep theirs), `nucleo-matcher` (fuzzy ranking behind the palette), `libc` (the unix calls the shell tool and the anchored file I/O are built on), `insta` + `expectrl` + `assert_cmd` (snapshot, pty and CLI tests). `crates/ganja-provider` is where `secrecy`, `sha2`, `getrandom` and the SSE-facing half of `reqwest` now live.

<!-- MANUAL: Any manually added notes below this line are preserved on regeneration -->
