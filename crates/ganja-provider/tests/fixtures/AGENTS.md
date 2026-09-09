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
| `claude-code-sdk-mcp-probe.txt` | The recording of nineteen `claude` invocations over stdio stream-json in four passes (2026-09-09/10, `2.1.266`; run 0b's argv preflight on `2.1.263`, the build every `L<n>` cite in the plan was read on) settling whether an unmodified Claude Code CLI will call a tool a third-party host declares under `--tools ""`, whether it asks that host's permission first, whether it waits out a 25-second hold on `tools/call` under a one-hour `timeout`, what a turn costs under the host's own system prompt against the vendor's preset, and what a resume, a replay, an interrupt, an in-use `--session-id` and an empty config directory each do. Three of the nineteen spend no turn. The last three passes (runs 2b, 9c, 2c, 2d, 9d, 2e-1, 2e-2) map the **safe space of the vendor's `reasoning_extraction` safeguard**, which refused ten of the twenty-one paid turns: a preamble is served when it carries the user's asks and the tool trail but not the model's own replies (9d against 9a/9b/9c), a clean record resumed under either its own or a changed system prompt is served (2c, 2e-1) while run 1's record is refused whatever the prompt (2, 2b, 2d) — and the same shape that served 2e-1 refused 2e-2 five minutes later with argv, prompt and record held fixed, so the map is a measured tendency and not a rule. Evidence for D556's gate (`.omc/plans/2026-09-08-claude-code-wire.md` §W1b/§W2): nothing reads it, and it is never edited to make a test pass. Read the header's table and the "what this recording does NOT settle" section before citing it. |
| `cursor-mcp-tools-probe.txt` | The recording of five live turns against `api2.cursor.sh` (2026-09-06) settling whether that server will call a tool a third-party client declares on `AgentRunRequest.mcp_tools = 4`, whether a 25-second client-side pause inside one exec survives on run heartbeats alone and with exec heartbeats, how a typed `rejected` on a native kind is treated, whether `system_prompt_spec = 29` is gated, and which of the two schema fields is honored. Not a decoder fixture — nothing `include_str!`s it: it is evidence, the way `ganja-core`'s `codex-identity-probe.txt` is, and it is what the cursor tool bridge's decision gate reads (`.omc/plans/2026-09-04-cursor-tool-bridge.md`, W2/W3). |

## For AI Agents

### Working In This Directory

- Two crates `include_str!` the `.sse` files: this crate's own unit tests in `src/provider/`, and `ganja-core`'s socket suites, which reach across for them. A rename is a compile error in both, not a silent skip — which is the intent. `cursor-mcp-tools-probe.txt`, `cursor-history-probe.txt` and `claude-code-sdk-mcp-probe.txt` are the exceptions and are read by nobody: a probe recording is evidence for a decision, so it is judged by a person and must never be edited to make a test pass.
- **A probe recording is written from the log, never from memory or from a reply.** No token, no tool-argument value and no reply text may appear in one — the same rule `secrets_env.rs` pins for everything else this crate writes down. What it may carry is field names, enum arm names, field numbers, timings, sizes, and any error `code`/`message` verbatim; where there was no error, it says so rather than leaving the section blank.
- Fixtures are *recorded shapes*, not invented ones. When adding a case, capture what the vendor actually sends (or reproduce it precisely from their documented format); a hand-waved fixture proves the decoder handles a stream nobody will ever send.
- Every new frame shape a provider learns to handle needs a fixture here. The socket suites are where mapping regressions get caught; unit tests alone do not exercise the split-across-chunks path.

### Testing Requirements

`cargo nextest run -p ganja-provider` for the unit tests, and `cargo nextest run -p ganja-core --test http` for the suite that serves these over a real loopback socket.

### Common Patterns

Files use obviously synthetic identifiers (`chatcmpl-Fixture`, `gpt-test`) and a fixed `created` timestamp so a diff shows a behavioral change rather than a re-recording.

<!-- MANUAL: -->
