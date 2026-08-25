<!-- Parent: ../AGENTS.md -->

# prompt

## Purpose

Upstream prompt texts, compiled in with `include_str!`. Nothing here is code.

What is *not* here: a tool's own description. Those live beside the tool in `ganja-tool`, which is where upstream keeps them too (`packages/opencode/src/tool/*.txt`).

## Key Files

| File | Description |
|------|-------------|
| `anthropic.txt` | Base system prompt for model ids containing `claude`. |
| `gpt.txt` | Base system prompt for model ids containing `gpt`. |
| `default.txt` | Base system prompt for every other model id. |
| `plan.txt` | The reminder injected into every request made under the `plan` agent. |
| `build-switch.txt` | The one-time reminder injected when a `plan` turn is followed by a `build` turn. |
| `explore.txt` | The `explore` subagent's own system prompt (replaces the base prompt). |
| `initialize.txt` | What `/init` sends. `${path}` is filled with the worktree; `$ARGUMENTS` with whatever the user typed after the name. |

## For AI Agents

- **These are byte-verbatim copies** of upstream files at v1.18.22 — the base prompts and reminders from `packages/opencode/src/session/prompt/`, `explore.txt` from `packages/opencode/src/agent/prompt/` — attributed in the root `THIRD_PARTY_NOTICES.md`. Do not edit, reflow, or "fix" them; a byte diff against upstream must stay empty, because that is what the notices claim.
- Base-prompt selection lives in `../instruction.rs` (`base_prompt`, substring match on the model id, first match wins). An agent's own `prompt` replaces the base prompt entirely. Reminder injection lives in the engine/session loop, request-side only — reminders never enter stored history.
- Porting another upstream text takes three coordinated changes: the verbatim file here, its consumer (`../instruction.rs` or `../agent.rs`), and a notices entry.
