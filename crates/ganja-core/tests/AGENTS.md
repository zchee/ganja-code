<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# ganja-core/tests

## Purpose

Integration suites for behavior that spans modules, touches a real socket or a real filesystem, or has to be observed from outside the crate. Each file is its own test binary; several deliberately hold exactly one test.

## Key Files

| File | Description |
|------|-------------|
| `agent_loop.rs` | The loop end to end: a turn spans as many model requests as its tool calls demand, every call is gated, executed and answered in order, and the event stream tells the whole story. Providers and tools are scripted doubles. |
| `cancel.rs` | A cancelled turn stops promptly and stays stopped. |
| `cancel_process_group.rs` | A cancelled turn takes the whole process group of the command it was running with it. |
| `delivery.rs` | The lossless guarantee: a consumer slower than the producer still sees every event, in order. |
| `permissions.rs` | Rules from a working directory to a file and back — resolve the project, find its data directory, store an answer, see a later session honour it. |
| `persistence.rs` | A conversation outlives the process: write-through as it streams, resume with interrupted calls closed, auto-title, compaction at the context ceiling. |
| `http.rs` | Both HTTP providers against a real loopback socket: the request actually built, the retry actually scheduled, the body actually split into frames. |
| `golden.rs` | **The differential harness.** Drives ganja *and* real upstream opencode against one replay endpoint and compares the tool calls each executed. See below. |
| `secrets_env.rs` | A canary key planted in the environment must not come back out through a `Debug`, a `tracing` field, or an error body the provider echoed. One test, one binary. |
| `credentials_env.rs` | A refused credential store must say so and say what repairs it — "no credential" and "there is one and it was refused" are different situations with different fixes. |
| `fake_script_env.rs` | `GANJA_FAKE_SCRIPT` is read on both routes a session takes to a provider, and *not* on the route the rest of the suite takes. One test, one binary. |
| `live.rs` | Opt-in vendor smoke tests: the request this build sends is one the vendor still accepts today. |
| `live_agent.rs` | Opt-in: one live turn through the real agent loop — a real model, offered this build's real tools, calls them and the arguments parse. |
| `config.rs` | The five-tier precedence table: global file < explicit file < project file < environment < flags, each tier proven to outrank the one below. Mutates environment variables — one test, one binary. |
| `agents.rs` | Agents at the engine level: the planning agent's refusals, config rules deciding unasked, prompt swap on switch, plan reminders reaching requests and not history, switch persistence across a resume, mid-turn refusal, an agent's model adopted from the `provider/model` spelling a config writes, and a passthrough between the switch and the next prompt not spending the build-switch notice. |
| `permission_directories.rs` | A shell call naming a directory outside the project surfaces that directory in the permission event. Mutates `XDG_DATA_HOME` — one test, one binary. |
| `task.rs` | The task tool end to end: one ordered script drives the parent *and* the child, the child's transcript stays off the frontend's stream, its progress arrives as metadata on the parent's tool part, its registry has no task tool, a parent's denial reaches it while a parent's stored "always" does not, a `task_id` naming a root session starts a fresh child, and a delegated child is stored as its own session naming its parent. |
| `task_tool.rs` | The task tool's own half, driven through a second implementation of the `Subagents` seam (`ganja_testkit::ScriptedSubagents`) with no engine behind it: the model's arguments reach the seam as written, a finished delegation becomes upstream's XML plus the part's metadata, and `Unknown`/`Cancelled`/`Failed` become the three things the model reads instead. |
| `credential_guard.rs` | A live engine hands its tools the credential store it resolved, so a model that asks `read` for `auth.json` is refused and the planted canary never reaches the transcript. Mutates `XDG_DATA_HOME` — one test, one binary. |
| `mcp.rs` | MCP end to end against a **reference** `@modelcontextprotocol/sdk` server (not an rmcp one): the stdio round-trip, the namespaced names, the permission gate (ask → once, ask → always, a config `deny` wildcard), `isError`, structuredContent-only, binary omission, a server that dies mid-session losing its tools, the `<mcp_instructions>` block, the remote streamable-HTTP transport over a loopback socket, a server that announces a changed tool set moving what the *next* turn is offered (`tools/list_changed`, fired by a fixture that adds one tool and drops another in the same call), a dropped session leaving no server process behind (a stubborn fixture EOF cannot end), and a check that no golden fixture has gained MCP anything. |
| `lsp.rs` | The diagnostics accept: an edit that introduces a type error comes back with rust-analyzer's complaint attached, inside three seconds. Gated on a readiness signal (a pre-seeded error the fixture ships) so the timed edit measures diagnostics latency and not server startup. The 3s budget is the default a development machine is held to; `GANJA_LSP_EDIT_BUDGET_MS` loosens it where scheduling is not the drill's to control — CI sets 6s, just past the client's own 5s ceiling, at which the client stops waiting and the missing diagnostics block fails the run regardless; the margin covers only the drill's own bookkeeping around the call. |
| `undo.rs` | The undo accept: a scripted turn edits a tracked file and writes a new one in a real temp checkout, `/undo` restores the first byte for byte and removes the second, `/redo` puts both back, and the prompt sent after a second undo reaches the model carrying **no** trace of the one it replaced — captured off the fake provider, because the worktree comparison alone is blind to history. Mutates `XDG_DATA_HOME` — one test, one binary. Needs `git`. |
| `passthrough.rs` | `!` passthrough: the exact synthetic user text, the `bash` part completing with the output and no exit code, ungated even where a rule refuses the model, running at the project root rather than the process directory, and a cancel that stops the command. |
| `commands.rs` | Slash commands, compaction on demand, and starting over: `/init` writing `AGENTS.md` through the ordinary loop, argument expansion reaching the prompt, a configured command, and `NewSession` leaving the old session on disk. |
| `watcher.rs` | The stale-read watcher against a real `notify` watch over a real directory: a file edited from outside is named to the model and refused afterwards even once its stamp is put back (so what refuses is the watcher's verdict and not a comparison), a tool's own writes are not condemned by the events they cause, and a subtree the session never reads is never watched at all. Every test fences on the watched-set accessor before changing anything — registration is lazy and stats what it registers, so a change made before the watch existed would be caught by that stat rather than by an event, and the drill would pass on a platform where watching does nothing. |
| `mentions.rs` | `@file`: the reference on the message, the content in the request, read at **send** time (a file rewritten between two turns reaches the model rewritten), and never recorded as a read. |
| `catalog_fetch.rs` | The catalog against a real socket: the startup loop fetches, the payload parses through fields this build has never heard of, the table is replaced wholesale, the body is cached verbatim under a name derived from the source, and the five-minute debounce keeps a second refresh off the wire. Mutates environment variables — one test, one binary. |
| `catalog_offline.rs` | Fetching disabled and nothing cached: the compiled-in snapshot answers every question, and a loopback listener that would have counted a request counts none. Mutates environment variables — one test, one binary. |
| `catalog_retry.rs` | A refused catalog request is retried twice and then reported as the status it gave up on, and a refresh that failed leaves the table it started with. Mutates environment variables — one test, one binary. |
| `spill_sweep.rs` | The spill sweep over the directories it really resolves: both candidates a clamp would have written to, week-old `tool_*` gone, fresh and foreign kept. Mutates `XDG_DATA_HOME` and `TMPDIR` — one test, one binary. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `fixtures/` | Recorded SSE transcripts and golden task scripts (see `fixtures/AGENTS.md`) |

## For AI Agents

### Working In This Directory

**Know which suites need setup before concluding a failure is a regression.**

- `golden.rs` needs `bun` on `PATH` and an upstream opencode v1.18.13 checkout with `bun install` already run — at `.omc/reference/opencode-v1.18.13` or wherever `GANJA_OPENCODE_DIR` points. A missing checkout or missing `bun` is a **failure, not a skip**: this suite exists to hold the port to upstream's behavior, and a green run that silently compared against nothing would be worth less than no run. Each upstream leg gets 180s.
- `mcp.rs` needs the same `bun` + upstream checkout `golden.rs` does, and additionally that checkout's installed `@modelcontextprotocol/sdk` (resolved at `packages/opencode/node_modules/@modelcontextprotocol/sdk`, with the hoisted copy as fallback). Missing is a **failure, not a skip**, for golden's reason: the whole point is that the server is somebody else's implementation, so a run that talked to nothing would prove nothing.
- `lsp.rs` needs `rust-analyzer` on `PATH` (`rustup component add rust-analyzer`). Missing is a **failure, not a skip**, for golden's reason: a run that started no language server would prove nothing.
- `undo.rs` needs `git` on `PATH`, and fails rather than skips without it for the same reason. It builds its own checkout in a temporary directory, so nothing it does depends on the repository it is run from.
- `live.rs` / `live_agent.rs` are `#[ignore]`d *and* inert unless `GANJA_LIVE_TEST=1` plus the provider key are both set:
  ```sh
  GANJA_LIVE_TEST=1 ANTHROPIC_API_KEY=… cargo test -p ganja-core --test live -- --ignored
  ```
  That combination is deliberate: a contributor running the full suite spends nothing, and CI can opt in without failing on a machine with no key.

### Testing Requirements

```sh
cargo nextest run -E 'binary(agent_loop)'
cargo nextest run -E 'binary(golden)' --no-capture
```

### Common Patterns

- **One test per binary where process-wide state is mutated.** A file that sets environment variables or depends on the process working directory (`secrets_env.rs`, `fake_script_env.rs`, `golden.rs`, `credential_guard.rs`) holds exactly one test. nextest runs each test in its own process, so it would tolerate more — but a plain `cargo test` runs a binary's tests on parallel threads, and the one-per-binary rule keeps the suite correct under both runners. Do not add a second test to those files — put it in a new file.
- **Redirect `XDG_DATA_HOME`.** Anything that reads or writes stored state must not touch the real user's credentials, permissions or spilled output.
- **Serve real bytes, don't mock the client.** Provider suites bind a loopback `TcpListener` and speak real HTTP, because mocking would skip the request that is actually built and the frames it is actually split into.
- **Assert on the redacted tail, never a whole key** — a test that printed one would put it in CI output, which is the failure redaction exists to prevent.
- In `golden.rs`, scripts are handed out to *agent* requests only — the ones carrying a `tools` array — because upstream opens a session with an extra toolless title request that would otherwise shift its whole script by one and compare the two legs against different transcripts. Keep any new engine-side bookkeeping request toolless so this discrimination keeps working.

## Dependencies

### Internal

`ganja_core`'s public surface only — these suites consume the crate the way a frontend does.

### External

`tokio` (with `net`), `futures`, `tempfile`, `tracing-subscriber` (the secrets canary needs a subscriber it can read back), `serde_json`, and `bun` as an external binary for `golden.rs` and `mcp.rs`.

<!-- MANUAL: -->
