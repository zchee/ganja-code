<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# fixtures

## Purpose

Recorded inputs the test suites replay. Two kinds: `.sse` files are captured `text/event-stream` bodies served to the HTTP provider tests, and `golden/` holds the task scripts both agents are driven with in the differential harness.

## Key Files

| File | Description |
|------|-------------|
| `anthropic_happy_path.sse` | A complete Messages stream: text deltas through usage and finish. Also used by `secrets_env.rs`. |
| `openai_happy_path.sse` | A complete chat-completions stream, including a `reasoning_content` delta. |
| `anthropic_truncated.sse` / `openai_truncated.sse` | A body that stops arriving mid-stream — must surface as a failure, never as a model that finished talking. |
| `anthropic_mid_stream_error.sse` | An error frame after streaming began, which is reported rather than retried. |
| `openai_malformed_frame.sse` | Garbage the decoder must skip without panicking. |
| `anthropic_tool_use_interleaved.sse` / `openai_tool_calls_interleaved.sse` | Tool-call fragments interleaved with text, proving argument assembly across chunk boundaries. |
| `openai_tool_calls.sse` | Multiple tool calls in one reply. |
| `anthropic_tool_call_cut_short.sse` / `openai_tool_call_cut_short.sse` | A call whose arguments never complete. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `golden/` | Canned tasks for the upstream differential (see `golden/AGENTS.md`) |

## For AI Agents

### Working In This Directory

- These are consumed with `include_str!`, so a renamed file is a compile error, not a silent skip — which is the intent.
- Fixtures are *recorded shapes*, not invented ones. When adding a case, capture what the vendor actually sends (or reproduce it precisely from their documented format); a hand-waved fixture proves the decoder handles a stream nobody will ever send.
- Every new frame shape a provider learns to handle needs a fixture here. The provider suites are where mapping regressions get caught; unit tests alone do not exercise the split-across-chunks path.

### Testing Requirements

Consumed by `../http.rs` (both providers, over a real loopback socket) and `../secrets_env.rs`. Run `cargo test -p ganja-core --test http`.

### Common Patterns

Files use obviously synthetic identifiers (`chatcmpl-Fixture`, `gpt-test`) and a fixed `created` timestamp so a diff shows a behavioral change rather than a re-recording.

<!-- MANUAL: -->
