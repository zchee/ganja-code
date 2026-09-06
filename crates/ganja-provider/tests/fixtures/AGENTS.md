<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-08-07 -->

# fixtures

## Purpose

Captured `text/event-stream` bodies, one per shape a wire has to survive, plus the recordings of live probes against a vendor. They live beside the wires that parse them rather than beside the suites that serve them, because what each file records is a fact about a vendor's protocol.

## Key Files

| File | Description |
|------|-------------|
| `anthropic_happy_path.sse` | A complete Messages stream: text deltas through usage and finish. Also used by `ganja-core/tests/secrets_env.rs`. |
| `openai_happy_path.sse` | A complete chat-completions stream, including a `reasoning_content` delta. |
| `anthropic_truncated.sse` / `openai_truncated.sse` | A body that stops arriving mid-stream — must surface as a failure, never as a model that finished talking. |
| `anthropic_mid_stream_error.sse` | An error frame after streaming began, which is reported rather than retried. |
| `openai_malformed_frame.sse` | Garbage the decoder must skip without panicking. |
| `anthropic_tool_use_interleaved.sse` / `openai_tool_calls_interleaved.sse` | Tool-call fragments interleaved with text, proving argument assembly across chunk boundaries. |
| `openai_tool_calls.sse` | Multiple tool calls in one reply. |
| `anthropic_tool_call_cut_short.sse` / `openai_tool_call_cut_short.sse` | A call whose arguments never complete. |
| `cursor-history-probe.txt` | The recording of the live turns that settle D553 (`.omc/plans/2026-09-07-cursor-history-blobs.md`): probe 1 (2026-09-07, two `ganja run` turns, the second under `--continue`) — whether the conversation state this build composes onto cursor's blob channel reaches the model, which of the composed blobs the server fetches (the six root entries and the turn wrapper, never the inner user/step blobs), that every get was answered and no error frame arrived, and the checkpoint arm's sizes; probe 2 (recorded at W3) — whether a Run the bridge lost is recovered under `resume_action` without the tool running twice. Evidence, not a decoder fixture: nothing reads it, and it is never edited to make a test pass. |
| `cursor-mcp-tools-probe.txt` | The recording of five live turns against `api2.cursor.sh` (2026-09-06) settling whether that server will call a tool a third-party client declares on `AgentRunRequest.mcp_tools = 4`, whether a 25-second client-side pause inside one exec survives on run heartbeats alone and with exec heartbeats, how a typed `rejected` on a native kind is treated, whether `system_prompt_spec = 29` is gated, and which of the two schema fields is honored. Not a decoder fixture — nothing `include_str!`s it: it is evidence, the way `ganja-core`'s `codex-identity-probe.txt` is, and it is what the cursor tool bridge's decision gate reads (`.omc/plans/2026-09-04-cursor-tool-bridge.md`, W2/W3). |

## For AI Agents

### Working In This Directory

- Two crates `include_str!` the `.sse` files: this crate's own unit tests in `src/provider/`, and `ganja-core`'s socket suites, which reach across for them. A rename is a compile error in both, not a silent skip — which is the intent. `cursor-mcp-tools-probe.txt` and `cursor-history-probe.txt` are the exceptions and are read by nobody: a probe recording is evidence for a decision, so it is judged by a person and must never be edited to make a test pass.
- **A probe recording is written from the log, never from memory or from a reply.** No token, no tool-argument value and no reply text may appear in one — the same rule `secrets_env.rs` pins for everything else this crate writes down. What it may carry is field names, enum arm names, field numbers, timings, sizes, and any error `code`/`message` verbatim; where there was no error, it says so rather than leaving the section blank.
- Fixtures are *recorded shapes*, not invented ones. When adding a case, capture what the vendor actually sends (or reproduce it precisely from their documented format); a hand-waved fixture proves the decoder handles a stream nobody will ever send.
- Every new frame shape a provider learns to handle needs a fixture here. The socket suites are where mapping regressions get caught; unit tests alone do not exercise the split-across-chunks path.

### Testing Requirements

`cargo nextest run -p ganja-provider` for the unit tests, and `cargo nextest run -p ganja-core --test http` for the suite that serves these over a real loopback socket.

### Common Patterns

Files use obviously synthetic identifiers (`chatcmpl-Fixture`, `gpt-test`) and a fixed `created` timestamp so a diff shows a behavioral change rather than a re-recording.

<!-- MANUAL: -->
