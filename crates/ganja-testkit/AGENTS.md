<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-05 | Updated: 2026-08-05 -->

# ganja-testkit

## Purpose

Dev-only scaffolding for `ganja-core`'s integration suites (`crates/ganja-core/tests/*.rs`). Before this crate existed, several of those files — themselves standalone binaries, deliberately one-test-per-binary for the reasons in `ganja-core/tests/AGENTS.md` — rebuilt the same handful of fixtures from scratch under different names: a scripted `Provider` double, a recorder or blocking `Tool` double, the drain loop that collects a turn's events (optionally answering permission dialogs along the way), and the storage builders that seed a session directly on disk. This crate holds exactly that, and nothing else — a helper genuinely specific to one suite (a bun fixture's spawn dance, a provider-failure-and-repeat schedule only one file needs) stays in that file. Never depended on outside `[dev-dependencies]`.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest, `publish = false`. Every dependency is what `ganja-core` itself already needs to speak `Provider`/`Tool`, plus `tempfile` for the storage builders. |
| `src/lib.rs` | Crate doc and the public re-exports; nothing else lives here. |
| `src/provider.rs` | [`ScriptedProvider`] — a `Provider` double answering each request from a queued script, configurable on what it does once the script runs dry ([`OnExhausted`]: complete forever, or panic). Also [`says`] and [`tool_call`], the two script-fragment builders identical across every suite that used them verbatim. |
| `src/tool.rs` | [`RecorderTool`] (records a call's arguments, answers with a caller-supplied title/output) and [`BlockingTool`] (blocks until the turn's cancel token fires, with an optional entry signal for tests that must land a cancel deterministically mid-call). Both share [`placeholder_schema`], a permissive schema stub for a tool double whose script never exercises argument validation. |
| `src/drain.rs` | [`drain`] (collect a turn's events to its finish), [`drain_answering`] (the same, answering every permission dialog with a given reply), [`drain_allowing`] (the same, always `Once`). |
| `src/session.rs` | [`seeded_session_info`] (a pre-titled `SessionInfo` for seeding storage directly), [`seed_session`] (write one and return its id), [`seed_message`] (write a `Message`'s envelope and parts the way the engine does). |
| `src/fs.rs` | [`temp_dir`] and [`redirect_xdg_data_home`] (`unsafe`, mutates process environment — see its doc comment for the invariant a caller must uphold). |
| `src/subagent.rs` | [`ScriptedSubagents`] — the `Subagents` seam a `task` call delegates through, answering each delegation from a queued script and recording what it was asked. The second implementation of that trait, which is what makes it a seam: no provider, no agents, no turn. |
| `src/agent.rs` | [`agent_registry`], building an `AgentRegistry` from a fixture `Config` for suites that need one to construct an engine but are not testing config resolution itself. |

## For AI Agents

### Working In This Directory

- **This crate repays duplication; it does not collect everything.** A helper moves here only once at least two of `ganja-core`'s test binaries need the identical shape. A binary's own one-off fixture (mcp.rs's bun-fixture discovery and reference-server spawn helpers, persistence.rs's `LaneProvider` and its 30-second-timeout `drain`, task.rs's `Canned` tool) stays in that file — see each one's own doc comment for why.
- **Unify on values, keep separate on shapes.** A provider id, a canned title/output string, an exhaustion policy — these are constructor arguments here. A script that never sends `ToolCallEnd`, or a request-count assertion tied to a specific handle type, is a different *shape* of test double and stays local rather than being forced through a shared type with a flag nobody else needs.
- **No `#[cfg(test)] mod tests` here.** `ganja-core`'s workspace test count is pinned; a unit test in this crate would inflate `cargo nextest run --workspace`'s total for no coverage this crate's own doctests don't already give. Exercise a builder with a doctest instead where one is cheap to write (most of the pure builders have one already).
- `redirect_xdg_data_home` is `unsafe` on purpose: mutating `XDG_DATA_HOME` is process-wide, and the safety invariant (call before any other thread starts) is something only the caller's test binary can know it upholds. Callers still write their own `// SAFETY:` comment at the call site — the function's doc comment states the invariant, not the specific reason a given binary meets it.

### Testing Requirements

```sh
cargo build -p ganja-testkit          # this crate alone
cargo test -p ganja-testkit --doc     # its doctests
cargo nextest run --workspace         # ganja-core's suites, rewired onto this crate
```

### Common Patterns

- Every public item is documented in the house's sentence-comment voice, matching the sibling crates. Doc comments on the pure builders (`says`, `tool_call`, `seeded_session_info`, `seed_message`, `temp_dir`, `agent_registry`) carry a runnable example; the ones that need a live `Engine` turn (`ScriptedProvider`, `RecorderTool`, `BlockingTool`, the `drain*` functions) do not — the doctest cost was judged higher than the payoff for those.
- `ScriptedProvider::new`/`::named`/`::strict` all return `(Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>)` — the request log alongside the provider — matching the tuple shape every rewired suite already expected from its own local `Recorder`.

## Dependencies

### Internal

`ganja-core` (path dependency) — every type here is built on its public `Provider`/`Tool`/`Engine`/`Storage` surface.

### External

`async-trait` (the `Provider`/`Tool` impls), `futures` (`BoxStream`, `StreamExt`), `schemars` (the placeholder schema), `serde_json`, `tempfile` (the storage/XDG builders), `tokio` + `tokio-util` (`mpsc` for the blocking tool's entry signal, `CancellationToken`).

<!-- MANUAL: -->
