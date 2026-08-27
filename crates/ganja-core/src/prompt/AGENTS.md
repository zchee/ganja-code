<!-- Parent: ../AGENTS.md -->

# prompt

## Purpose

Upstream prompt texts — four of them forked to speak in ganja's own name (**D522**) — compiled in with `include_str!`. Nothing here is code.

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

- **`plan.txt`, `build-switch.txt` and `explore.txt` are byte-verbatim copies** of upstream files at v1.18.22 — the two reminders from `packages/opencode/src/session/prompt/`, `explore.txt` from `packages/opencode/src/agent/prompt/` — attributed in the root `THIRD_PARTY_NOTICES.md`. Do not edit, reflow, or "fix" them; a byte diff against upstream must stay empty, because that is what the notices claim. Upstream never names itself in any of the three, which is the whole reason they were untouched by the fork below.
- **The other four are derived from upstream with the identity substituted** (**D522**): `anthropic.txt`, `gpt.txt` and `default.txt` from `packages/opencode/src/session/prompt/`, `initialize.txt` from `packages/opencode/src/command/template/initialize.txt`. Upstream's prose is otherwise untouched, so a diff against upstream must contain nothing outside these three substitution classes — the agent's own name (`OpenCode`/`opencode` → `Ganja Code`), the repository and docs URLs it hands a user (`anomalyco/opencode` and `opencode.ai` → `https://github.com/zchee/ganja-code`, each keeping the path upstream gave it — `default.txt`'s feedback line keeps its `/issues`, `anthropic.txt`'s stays the bare repo), and the config file it names (`opencode.json` → `ganja.toml`, the `instructions` key beside it being real here). That last one is a *substitution*, not a constant: it names whatever this build's config file is called, so the format change to TOML (**D536**, 2026-08-28) moved it here rather than leaving `/init` telling a person to write a file the loader now refuses. A reflowed line, an improved sentence or a corrected fact is a bug, not an improvement: this is a name fork, and the narrowness is what keeps the files diffable against upstream at all.
- Base-prompt selection lives in `../instruction.rs` (`base_prompt`, substring match on the model id, first match wins). An agent's own `prompt` replaces the base prompt entirely. Reminder injection lives in the engine/session loop, request-side only — reminders never enter stored history.
- Porting another upstream text takes three coordinated changes: the text file here, its consumer (`../instruction.rs` or `../agent.rs`), and a notices entry.
