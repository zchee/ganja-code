<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# ganja-core/src

## Purpose

The engine's modules. The shape to hold in mind: `engine.rs` accepts commands and publishes events, `session.rs` runs one turn's agent loop against a `provider/`, executing `ganja-tool` calls that `ganja-permission` gates, with `ganja-protocol` supplying every type that crosses the boundary. Those last three are separate crates, re-exported by `lib.rs` under the module names they always had — `crate::tool`, `crate::permission`, `crate::project`, `crate::protocol`, `crate::watch` all still resolve, and now resolve across a boundary the compiler enforces.

## Key Files

| File | Description |
|------|-------------|
| `lib.rs` | Module list, the facade that re-exports `ganja-protocol`, `ganja-permission` and `ganja-tool` under their old module names, and the public surface. States the no-terminal-dependency rule that CI enforces. |
| `engine.rs` | `Engine`: commands in, an ordered event stream out. Owns the turn lifecycle and the transcript. |
| `session.rs` | The agent loop — one turn, as many model requests as its tool calls demand. Also `TurnHandle`/`PendingReply`, the plumbing a permission reply is routed through. |
| `provider/` | Sources of assistant text (see `provider/AGENTS.md`). |
| `auth.rs` | Provider credentials: environment first, then `auth.json` under the XDG data directory. Two kinds — an API key, and upstream's OAuth record (`{type, refresh, access, expires, accountId?, enterpriseUrl?, ...}`, where `expires: 0` means never). The file is shared territory, so nothing is ever dropped: entries of a type this build cannot read survive a rewrite, and so do the fields *inside* an OAuth record that it does not model, which upstream leaves open-ended by construction. Keys are upstream's names for the providers — ganja's `grok` is stored as `xai` — so the same file serves both tools. `Refresher` renews an expiring credential once per provider however many callers ask, the way upstream's per-plugin module-scoped promise does; the endpoint that does the renewing is a login flow's to supply through `RefreshOauth`. |
| `catalog.rs` | Context windows, max output and pricing, from a fetched catalog cached under the XDG **cache** directory, falling back to a compiled-in snapshot that never fails. `RwLock<Arc<_>>` behind the accessor functions, so `refresh` swaps the whole table. Per-provider default models stay compiled in — the published catalog has no such concept. |
| `storage.rs` | Session storage: one SQLite database per project, `session`/`message`/`part` with opaque JSON `data` columns, upstream's migration journal and pragma set. Writes go through a dedicated thread so their order is structural; reads take a second connection, which is what WAL buys. Records still carry an explicit version, so one a newer build wrote is left alone and one that will not decode costs its own row and nothing else; a database that cannot be read at all is set aside with its write-ahead log and replaced. A `storage/` tree from a build before this one is carried across on first open and renamed rather than deleted. |
| `subagent.rs` | Crate-private. Runs the second agent loop a `task` call delegates to: the per-session `Host`, the per-call `Spawn` that implements `tool::task::Subagents`, the roster a caller may spawn, the child's derived ruleset, its private event channel and the watcher that turns it into progress on the parent's part. |
| `config.rs` | `ganja.jsonc`/`ganja.json`: discovery (global dir, `GANJA_CONFIG`, project walk-up), JSONC decode of the curated keys, tier merge with flag overrides on top. Unknown top-level keys are refused by name; permission rule order survives exactly as written. |
| `mcp.rs` | MCP servers and the tools they lend: config-named servers dialled concurrently in a background task, `connected \| disabled \| failed{error}` per server, no reconnect. Tools are named `mcp__<server>__<tool>` with upstream's sanitizer on each half, contributed in sorted-server / listed-tool order with a post-sanitization collision refusing the later tool. Schemas are forced to `{type:"object", properties, additionalProperties:false}`; `isError` becomes error text the model reads; a server's `instructions` reach the system prompt as `<mcp_instructions>`. Prompts, resources and OAuth are not ported. |
| `lsp/` | Language servers and the diagnostics the model reads. Opt-in: no `lsp` key means none ever starts. A server is spawned lazily by the first touch of a file it claims, identified by `(root, server-id)`, and a pair that fails to start is never retried this session. Diagnostics arrive on both channels — pushed publishes and `textDocument/diagnostic` pulls — merged and deduped, because rust-analyzer stops pushing for a file edited after its initial analysis. Errors only, capped at 20 per file, appended to the tool result at one seam in `session.rs`. Two builtins ship, `rust` and `gopls`; nothing is ever auto-installed. |
| `snapshot.rs` | Git snapshots of the working tree, and the `/undo` they make possible. A **separate git dir** under the data home, every command `git --git-dir <ours> --work-tree <the project>`; the project's own `.git` is only ever read. No commits — `write-tree` names a tree and that hash is the snapshot. A turn records one `PartBody::Patch{hash, files}` per step that changed files, naming the tree the step **started** from, and reverting is `checkout <hash> -- <file>` per file with a file absent from that tree deleted instead. Also the walk `Engine::undo`/`redo` drive: the anchor, the patches from it on, and the prompt it hands back. Off when there is no `git`, when the directory is not a checkout, or when `snapshot: false`. |
| `command.rs` | Slash commands: the builtin `/init` plus whatever `config.command` describes, and the `$1`/`$ARGUMENTS` expansion that turns one into a prompt. |
| `instruction.rs` | The system prompt: a base prompt per model family (`prompt/*.txt`), the `<env>` block, and `AGENTS.md`-family instruction discovery, assembled in upstream's `Instructions from:` shape. Reaches the engine through `Engine::with_system`/`with_system_parts` — and both halves are written against a model, so the engine recomposes each whenever the active one moves: `Engine::with_base_for_model` calls `base_prompt` again (the family's prompt), `Engine::with_environment` calls the suffix composer again (the block that names the model). |
| `agent.rs` | The agent roster: build, plan, general, explore (upstream's rulesets adapted to ganja's tool surface), config overlay, `default_agent` resolution. An agent is a name, a prompt that replaces the base prompt, and rules layered beneath the user's stored answers. |
| `prompt/` | Upstream prompt texts, byte-verbatim — attributed in the root notices. |

## For AI Agents

### Working In This Directory

The invariants below are what the tests actually pin. Breaking one is a behavioral regression even if everything compiles.

- **Delivery is lossless.** Events travel a bounded `mpsc`, so a producer that outruns its consumer waits instead of dropping fragments; backpressure lands on the turn task and never on the render loop. Do not swap in `broadcast` — lag-drops would silently tear a transcript.
- **One subscriber, one turn.** `subscribe()` after the first returns `AlreadySubscribed`, because splitting one lossless queue between two readers hands each an arbitrary half of the transcript. A prompt sent while a turn is streaming — or waiting on a permission — returns `Busy`. Fanout arrives in P7.
- **The event stream is the whole story.** A frontend that applies every event holds exactly what the next `ChatRequest` will carry. Anything the engine knows and does not report is a bug in the protocol, not something a frontend should reconstruct.
- **Tool results are information, never control flow.** A refused permission, an unknown tool, unparseable arguments, a tool that failed — each becomes error text the model reads on the next request, and the loop continues. (This is a deliberate divergence: upstream stops the turn on a refusal unless `experimental.continue_loop_on_deny` is set.) The only two early exits are the ones that mean it: the user cancelled, or the provider died. There is no step cap — `Command::CancelTurn` is the escape hatch, as upstream.
- **A turn always ends with a terminal event.** The turn task is deliberately never joined and never aborted from outside: cancellation reaches the provider and every running tool, and aborting the task instead would skip the cleanup that releases the busy slot and guarantees the finish event.
- **Failed turns do not enter the history.** An empty reply is dropped rather than carried into the next request.
- **An MCP tool asks by default.** `ganja-permission` decides it by **prefix** (`MCP_PREFIX`, whose one owner it is), not by name, because the names are not known until a server has been asked. Do not fold this into `ASK_BY_DEFAULT` — that list is names, and this is a shape.
- **The tool surface moves only between turns.** `Engine::refresh_mcp` runs at the `start_turn` seam; a turn already holding a registry snapshot keeps the tools it started with, so a connect finishing mid-request cannot change what that request was answered with.
- **No LSP failure may fail a tool call or a turn.** A server that will not start, a file that will not read, a publish that never comes — each costs the model a diagnostics block and costs the turn nothing. The whole subsystem's output is advice.
- **Diagnostics reach the model at one seam.** `session.rs::resolve` appends them after `tool.run` and before `ToolState::Completed`, keyed by tool id — not inside `edit`, `write` and `read`. Adding a fourth tool that wants them is an edit to that list, not to the tool.
- **A reminder belongs to the request, never to the transcript.** The plan/build notices and the stale-files notice are appended to the last user message of the `ChatRequest` a turn builds and are not written through: each is about the state the session is in *now*, and a stored copy would still be telling a later turn about a mode it left or a file it has long since re-read.
- **No watcher failure may fail anything, and a passthrough may not spend a notice.** The stale queue is drained only by a turn that asks the model — a `!` command asks nothing and a compaction asks a question of its own — so the notice waits for the prompt that can actually deliver it.
- **Nothing on a startup path may register a watch.** `Engine::watch_files()` touches no filesystem: the platform watcher is built, and every directory registered, on the watcher's own task. Registering a *recursive* watch on Linux is a synchronous walk of the whole tree — `notify`'s inotify backend blocks the caller while it walks and spends a descriptor per directory — which is why registration follows the read log one directory at a time and why a future background service should be assumed to cost what its worst tree costs, not what it costs here.
- **No snapshot failure may fail a tool call or a turn.** Every git invocation answers with an exit code rather than an error; a step whose snapshot did not happen simply records no patch, and the loop never asks whether one did.
- **A patch names the tree its step started from.** Restoring is checking those files out of that hash, so the direction is not reversible by taste: the hash a step records is *before* it ran, and the step-finish work only decides whether anything moved.
- **A revert deletes nothing.** `Command::Undo` hides messages by recording an anchor; they stay in the history and in storage, which is what makes `Command::Redo` lossless. Deletion happens once, at the next `SendPrompt` or `RunShell`, and it takes the anchor **with** everything after it — a prompt the user took back must not reach the request that replaces it.

### Testing Requirements

Unit tests sit in `#[cfg(test)] mod tests` at the bottom of each module and cover the module's own logic. Cross-module behavior — the loop, cancellation, delivery, permissions round-tripping to disk — belongs in `../tests/`. When adding an invariant to this directory, add the test that would notice if someone removed it.

### Common Patterns

- **Ids sort in creation order.** `MessageId`/`PartId`/`PermissionId` are a millisecond timestamp plus a per-process counter, both fixed-width hex, so ids sort lexicographically by creation and cannot collide in one process. Storage reassembly (P4) leans on this.
- **Ids adopted from disk are taken verbatim.** The prefix is a convention, not an invariant.
- New `PartBody` variants are additive — the tag travels as a `type` field beside the part's id — so adding one changes nothing already on the wire.
- Time is milliseconds since the Unix epoch via `protocol::now()`, saturating rather than failing when the clock is set before 1970.

## Dependencies

### Internal

`ganja-protocol`, `ganja-permission` and `ganja-tool`, re-exported by `lib.rs` so that `crate::protocol`, `crate::permission`, `crate::project`, `crate::tool` and `crate::watch` mean here what they have always meant. The flow is `engine` → `session` → {`provider`, `tool`, `permission`}, with `protocol` beneath all of them and `project` supplying the data directory that `permission` and `storage` write into. The direction is one-way and the compiler enforces it: none of the three may name this crate.

### External

See `../AGENTS.md`.

<!-- MANUAL: -->
