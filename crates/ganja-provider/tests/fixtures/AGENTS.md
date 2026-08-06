<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-08-07 -->

# fixtures

## Purpose

Captured `text/event-stream` bodies, one per shape a wire has to survive. They live beside the wires that parse them rather than beside the suites that serve them, because what each file records is a fact about a vendor's protocol.

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

## For AI Agents

### Working In This Directory

- Two crates `include_str!` these: this crate's own unit tests in `src/provider/`, and `ganja-core`'s socket suites, which reach across for them. A rename is a compile error in both, not a silent skip — which is the intent.
- Fixtures are *recorded shapes*, not invented ones. When adding a case, capture what the vendor actually sends (or reproduce it precisely from their documented format); a hand-waved fixture proves the decoder handles a stream nobody will ever send.
- Every new frame shape a provider learns to handle needs a fixture here. The socket suites are where mapping regressions get caught; unit tests alone do not exercise the split-across-chunks path.

### Testing Requirements

`cargo nextest run -p ganja-provider` for the unit tests, and `cargo nextest run -p ganja-core --test http` for the suite that serves these over a real loopback socket.

### Common Patterns

Files use obviously synthetic identifiers (`chatcmpl-Fixture`, `gpt-test`) and a fixed `created` timestamp so a diff shows a behavioral change rather than a re-recording.

<!-- MANUAL: -->
