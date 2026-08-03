<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

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

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `fixtures/` | Recorded SSE transcripts and golden task scripts (see `fixtures/AGENTS.md`) |

## For AI Agents

### Working In This Directory

**Know which suites need setup before concluding a failure is a regression.**

- `golden.rs` needs `bun` on `PATH` and an upstream opencode v1.18.11 checkout with `bun install` already run — at `.omc/reference/opencode-v1.18.11` or wherever `GANJA_OPENCODE_DIR` points. A missing checkout or missing `bun` is a **failure, not a skip**: this suite exists to hold the port to upstream's behavior, and a green run that silently compared against nothing would be worth less than no run. Each upstream leg gets 180s.
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

- **One test per binary where process-wide state is mutated.** A file that sets environment variables or depends on the process working directory (`secrets_env.rs`, `fake_script_env.rs`, `golden.rs`) holds exactly one test. nextest runs each test in its own process, so it would tolerate more — but a plain `cargo test` runs a binary's tests on parallel threads, and the one-per-binary rule keeps the suite correct under both runners. Do not add a second test to those files — put it in a new file.
- **Redirect `XDG_DATA_HOME`.** Anything that reads or writes stored state must not touch the real user's credentials, permissions or spilled output.
- **Serve real bytes, don't mock the client.** Provider suites bind a loopback `TcpListener` and speak real HTTP, because mocking would skip the request that is actually built and the frames it is actually split into.
- **Assert on the redacted tail, never a whole key** — a test that printed one would put it in CI output, which is the failure redaction exists to prevent.
- In `golden.rs`, scripts are handed out to *agent* requests only — the ones carrying a `tools` array — because upstream opens a session with an extra toolless title request that would otherwise shift its whole script by one and compare the two legs against different transcripts. Keep any new engine-side bookkeeping request toolless so this discrimination keeps working.

## Dependencies

### Internal

`ganja_core`'s public surface only — these suites consume the crate the way a frontend does.

### External

`tokio` (with `net`), `futures`, `tempfile`, `tracing-subscriber` (the secrets canary needs a subscriber it can read back), `serde_json`, and `bun` as an external binary for `golden.rs`.

<!-- MANUAL: -->
