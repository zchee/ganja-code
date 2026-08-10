# OpenAI Codex CLI feature reference (compared against ganja)

> [!IMPORTANT]
> **This document is a reference inventory, not a roadmap. Not every feature
> listed here will be ported.** ganja's charter is behavioral parity with
> opencode v1.18.13; Codex CLI is a third product, catalogued here for
> comparison only. A ❌ is an observation, never a promise.

Snapshot: 2026-08-11, against Codex CLI's main branch (Codex has no pin in
this repo — rows drift as upstream moves). Rows marked *(low confidence)*
rest on community sources rather than official documentation.

Legend: ✅ present in ganja (parity or a near equivalent) · ⚠️ partial · ❌ absent.

## 1. TUI — composer and micro features

| Feature | Keys | ganja |
|---|---|---|
| [`@` fuzzy file search with Tab-accept](https://developers.openai.com/codex/cli) | `@` + Tab | ⚠️ `@` menu ✅; no Tab-accept (Enter only) |
| [Esc-Esc backtrack](https://developers.openai.com/codex/cli) | Esc Esc | ❌ pick a past prompt, edit it, rewind the turns after it |
| [Transcript overlay](https://developers.openai.com/codex/cli) | Ctrl+T | ❌ raw logs, per-turn tokens, tool/MCP expansion |
| [Queued messages](https://developers.openai.com/codex/cli) | Enter while running | ❌ busy turns refuse input |
| [Clipboard image paste](https://developers.openai.com/codex/cli) | Ctrl+V | ❌ (`@`-mention attachments ✅) |
| [Slash-command autocomplete](https://developers.openai.com/codex/cli) | `/` | ✅ |
| [Reasoning-effort hotkeys](https://github.com/openai/codex/blob/main/docs/config.md) | Alt+, / Alt+. | ❌ (`/effort` list picker ✅) |
| Prompt history | Up / Down | ✅ |
| Multiline input | Shift+Enter … | ✅ |
| External editor | — | ✅ `/editor` (ganja-side advantage) |

## 2. Slash commands

| Command | Notes | ganja |
|---|---|---|
| [`/model`](https://developers.openai.com/codex/cli) | model **and** reasoning effort in one menu | ⚠️ `/model` ✅ + separate `/effort`; no combined menu |
| [`/review`](https://developers.openai.com/codex/cli) | automated review of uncommitted/commit/branch diffs | ❌ |
| [`/diff`](https://developers.openai.com/codex/cli) | session-wide change viewer | ❌ (per-edit inline diffs ✅) |
| [`/compact`](https://developers.openai.com/codex/cli) | summarize the conversation | ✅ plus auto-compaction |
| [`/prompts` → Agent Skills](https://developers.openai.com/codex/cli) *(medium confidence)* | prompt templates deprecated toward SKILL.md | ⚠️ skills ✅ (SKILL.md-compatible); no template list UI |
| [`/status`](https://developers.openai.com/codex/cli) | model/tokens/context/cost dashboard | ⚠️ status bar + totals only |
| [`/init`](https://developers.openai.com/codex/cli) | generate AGENTS.md | ✅ |
| `/new` / `/quit` | session control | ✅ equivalents |
| [`/mcp`](https://github.com/openai/codex/blob/main/docs/config.md) | MCP connection status | ⚠️ `ganja mcp` CLI listing only |
| `/login` / `/logout` | credential switching in-TUI | ⚠️ `auth` CLI only |

## 3. Security and execution modes

| Feature | Notes | ganja |
|---|---|---|
| [OS-kernel sandboxing](https://github.com/openai/codex/blob/main/docs/sandbox.md) | macOS Seatbelt; Linux Landlock + seccomp | ❌ permission engine only, no isolation |
| [Approval policies](https://github.com/openai/codex/blob/main/docs/getting-started.md) | read-only / workspace-write / full-access / on-request | ⚠️ rule-based allow/ask/deny + single-tier `--auto` |
| [Write-mode network cutoff](https://github.com/openai/codex/blob/main/docs/sandbox.md) | `network_access = false` by default under workspace-write | ❌ no such concept |
| [`--yolo` bypass](https://github.com/openai/codex/blob/main/docs/sandbox.md) | skip sandbox + approvals | ⚠️ `--auto` is allow-unless-denied; no sandbox to bypass |
| [Container posture](https://github.com/openai/codex/blob/main/docs/sandbox.md) | degraded-sandbox flags for Docker/devcontainers | ❌ |

## 4. Configuration and context

| Feature | Notes | ganja |
|---|---|---|
| [`config.toml` + named `[profiles]`](https://github.com/openai/codex/blob/main/docs/config.md) | posture presets via `--profile` | ⚠️ three config tiers ✅; named profiles ❌ |
| [AGENTS.md, project + global](https://agents.md) | `~/.codex/AGENTS.md` auto-loaded | ✅ ganja reads the family plus its global tier |
| [`personality`](https://github.com/openai/codex/blob/main/docs/config.md) | pragmatic / friendly / none tone | ❌ |
| [`notify` hooks](https://github.com/openai/codex/blob/main/docs/config.md) | run a command on completion/approval-needed | ❌ |
| Lifecycle hooks *(low confidence)* | event-triggered scripts | ❌ |
| [Display knobs](https://github.com/openai/codex/blob/main/docs/config.md) | `hide_agent_reasoning` and friends | ❌ |
| [Shell completions](https://developers.openai.com/codex/cli) | bash/zsh/fish/powershell | ❌ (clap could; not wired) |

## 5. Tools and agent machinery

| Feature | Notes | ganja |
|---|---|---|
| [`apply_patch`](https://github.com/openai/codex/blob/main/docs/getting-started.md) | structured unified-diff editing as the primary tool | ❌ ganja follows upstream's `edit`/`write`; the name exists in the permission table only |
| [`update_plan` (plan mode)](https://developers.openai.com/codex/cli) | live checklist rendering and updates | ⚠️ `todowrite` is the nearest; no plan-specific tool |
| [`web_search` tool](https://github.com/openai/codex/blob/main/docs/config.md) | live search opt-in | ✅ `websearch` (Exa/Parallel) |
| Shell execution | | ✅ `bash` |
| Best-of-N *(low confidence)* | parallel candidate generation | ❌ |
| [MCP client](https://github.com/openai/codex/blob/main/docs/config.md) | `[mcp_servers.*]` | ✅ |
| [Codex as an MCP server](https://developers.openai.com/codex/cli) | expose the engine over MCP | ❌ |

## 6. CLI, headless, cloud, integrations

| Feature | Notes | ganja |
|---|---|---|
| [`codex exec`](https://github.com/openai/codex/blob/main/docs/exec.md) | headless runs, `-o <file>` | ✅ `ganja run` (no `-o`; stdout redirection instead) |
| [`codex resume` / `--last` / inline prompt](https://developers.openai.com/codex/cli) | resume and continue in one line | ✅ `--continue` / `--session` + `run --continue "…"` |
| Session forking | | ❌ |
| [`codex cloud` + `codex apply`](https://developers.openai.com/codex/cloud) | delegate to cloud, pull the diff back | ❌ out-of-scope territory |
| [IDE extension](https://developers.openai.com/codex/ide) | VS Code/JetBrains via app server | ❌ out of scope |
| [GitHub Action](https://github.com/openai/codex-action) | CI review and fixes | ❌ |
| [`--image <path>`](https://developers.openai.com/codex/cli) | attach images from the CLI | ❌ |
| [ChatGPT OAuth or API key login](https://github.com/openai/codex/blob/main/docs/authentication.md) | | ✅ the same dual-credential shape (ganja's `openai`) |
| Update notifications | | ❌ |

Where ganja holds its own against Codex (for perspective, not scorekeeping):
loadable TUI themes, `/editor`, the `!` shell passthrough, arity-aware
"always" permission answers, the serve/attach HTTP+SSE surface, and the
golden-differential test discipline.
