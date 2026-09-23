<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-07 -->

# fixtures

## Purpose

Recorded inputs the test suites replay: `golden/` holds the task scripts both agents are driven with in the differential harness, and `mcp/` holds servers the MCP client is certified against.

The captured `text/event-stream` bodies these suites also read live in `../../../ganja-provider/tests/fixtures/`, beside the wires that parse them. `http.rs`, `secrets_env.rs`, `oauth_wire.rs` and the two `compat_*_wire.rs` suites `include_str!` them across the crate boundary, which is deliberate: a recorded vendor transcript is a fact about that vendor's wire, and duplicating it here would leave two copies to re-record.

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `golden/` | Canned tasks for the upstream differential (see `golden/AGENTS.md`) |
| `mcp/` | MCP servers on the upstream checkout's `@modelcontextprotocol/sdk`, spawned by `tests/mcp.rs` so the client is certified against somebody else's implementation rather than against rmcp talking to itself. `reference-server.mjs` answers, fails, and dies on request; `stubborn-server.mjs` ignores stdin EOF, so only a kill ends it; `changing-server.mjs` swaps one tool for another and sends `tools/list_changed`, which is the only fixture that makes a server *tell* the client something. |

## For AI Agents

### Working In This Directory

- These are consumed with `include_str!` or spawned by path, so a renamed file is a compile error or a failing test, not a silent skip — which is the intent.
- Fixtures are *recorded shapes*, not invented ones. When adding a case, capture what the real implementation actually sends; a hand-waved fixture proves the decoder handles a stream nobody will ever send.
- A new event-stream shape belongs in `ganja-provider/tests/fixtures/`, not here.

### Testing Requirements

`cargo nextest run -E 'binary(golden)'` and `cargo nextest run -E 'binary(mcp)'`; both need `bun` and the upstream checkout, and both hard-fail rather than skip without them.

### Common Patterns

Files use obviously synthetic identifiers and fixed timestamps so a diff shows a behavioral change rather than a re-recording.

<!-- MANUAL: -->
