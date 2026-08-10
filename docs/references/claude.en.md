# Claude Code feature reference (compared against ganja)

> [!IMPORTANT]
> **This document is a reference inventory, not a roadmap. Not every feature
> listed here will be ported.** ganja's charter is behavioral parity with
> opencode v1.18.13; Claude Code is a separate product, catalogued here for
> comparison only. A ❌ in these tables is an observation, never a promise.

Snapshot: 2026-08-11, against the Claude Code 2.1.x generation. Claude Code
moves quickly — treat stale rows as stale, not as ganja regressions. Rows
marked *(low confidence)* rest on community sources rather than official
documentation.

Legend: ✅ present in ganja (parity or a near equivalent) · ⚠️ partial · ❌ absent.

## 1. Composer input

| Feature | Keys | ganja |
|---|---|---|
| [File path tab completion](https://code.claude.com/docs/en/interactive-mode) | `@path` + Tab | ⚠️ `@` menu exists; no Tab-accept (Enter only) |
| [Slash command autocomplete](https://code.claude.com/docs/en/slash-commands) | `/` | ✅ dropdown + palette |
| [File mentions](https://code.claude.com/docs/en/common-workflows) | `@` | ✅ incl. `#line-range` and image/PDF attachments |
| [Vim mode](https://code.claude.com/docs/en/interactive-mode) | `/vim` | ❌ |
| [Prompt history](https://code.claude.com/docs/en/interactive-mode) | Up / Down | ✅ fifty entries, dedupe, self-healing store |
| [Reverse history search](https://code.claude.com/docs/en/interactive-mode) | Ctrl+R | ❌ |
| [Clipboard image paste](https://code.claude.com/docs/en/interactive-mode) | Ctrl+V | ❌ (`@`-mention attachments only) |
| [Long-paste collapsing](https://code.claude.com/docs/en/interactive-mode) | automatic | ❌ |
| [External editor](https://code.claude.com/docs/en/interactive-mode) | Ctrl+G | ⚠️ `/editor` command; no key binding |
| [Multiline input](https://code.claude.com/docs/en/interactive-mode) | Shift+Enter / Ctrl+J | ✅ upstream's four-chord default |
| [Bracketed paste](https://code.claude.com/docs/en/terminal-config) | paste | ✅ |
| [Bash mode](https://code.claude.com/docs/en/interactive-mode) | leading `!` | ✅ |
| [Memory shortcut](https://code.claude.com/docs/en/memory) | leading `#` | ❌ |
| [Input clear](https://code.claude.com/docs/en/interactive-mode) | Ctrl+C | ❌ (ganja's Ctrl+C exits) |
| [Text selection](https://code.claude.com/docs/en/interactive-mode) | Shift+arrows / Shift+Home/End | ❌ no selection machinery |
| [Visual-line motions](https://code.claude.com/docs/en/interactive-mode) | Alt+A / Alt+E | ❌ |
| [Input undo/redo](https://code.claude.com/docs/en/interactive-mode) | Ctrl+- / Ctrl+. | ⚠️ textarea built-ins only; not rebindable |
| [Kill and word operations, rebindable](https://code.claude.com/docs/en/interactive-mode) | Ctrl+K/U, Alt+F/B, … | ⚠️ built-ins work; outside the keybind table |
| [Submit key rebinding](https://code.claude.com/docs/en/settings) | config | ❌ Enter is fixed |
| [Message queueing](https://code.claude.com/docs/en/interactive-mode) | type while working | ❌ busy turns refuse input |
| [Agent mentions](https://code.claude.com/docs/en/sub-agents) | `@agent-…` | ❌ `@` is files only |
| [Dropped-path mentions](https://code.claude.com/docs/en/interactive-mode) | drag & drop | ❌ |
| [Screen redraw](https://code.claude.com/docs/en/interactive-mode) | Ctrl+L | ❌ |
| [Staged interrupt](https://code.claude.com/docs/en/interactive-mode) | Ctrl+C once/twice | ⚠️ single-stage |

## 2. Modes and session keys

| Feature | Keys | ganja |
|---|---|---|
| [Permission mode cycling](https://code.claude.com/docs/en/iam) | Shift+Tab | ❌ no mode concept; the plan agent approximates plan mode |
| [Extended thinking toggle](https://code.claude.com/docs/en/interactive-mode) | Tab / Cmd+T | ❌ (`/effort` selects a level instead) |
| [Rewind / checkpoints](https://code.claude.com/docs/en/checkpointing) | Esc Esc, `/rewind` | ⚠️ `/undo`·`/redo` restore files only; no conversation rewind |
| [Background a running task](https://code.claude.com/docs/en/interactive-mode) | Ctrl+B | ❌ no background execution |
| [Transcript / verbose toggle](https://code.claude.com/docs/en/interactive-mode) | Ctrl+O | ❌ one rendering |
| [Agent switching](https://code.claude.com/docs/en/sub-agents) | — | ✅ Tab cycles agents (ganja's own default); reverse cycle ❌ |

## 3. Slash commands

| Command | Purpose | ganja |
|---|---|---|
| [`/help`](https://code.claude.com/docs/en/slash-commands) | command reference | ✅ |
| [`/clear`](https://code.claude.com/docs/en/slash-commands) | fresh conversation | ✅ `/new` |
| [`/model`](https://code.claude.com/docs/en/model-config) | switch model | ✅ |
| [`/effort`](https://code.claude.com/docs/en/model-config) | reasoning effort | ✅ catalog-driven roster |
| [`/compact`](https://code.claude.com/docs/en/costs) | manual compaction | ✅ plus auto-compaction |
| [`/resume`](https://code.claude.com/docs/en/common-workflows) | session picker | ✅ `/sessions`, `--continue`, `--session` |
| [`/copy`](https://code.claude.com/docs/en/slash-commands) | copy output | ✅ `/copy`, `/copy-message` (arboard + OSC 52) |
| [`/theme`](https://code.claude.com/docs/en/settings) | theme picker | ✅ `/themes` + loadable theme files |
| [`/agents`](https://code.claude.com/docs/en/sub-agents) | manage/create agents | ⚠️ switching only; no create/edit UI |
| [`/config`](https://code.claude.com/docs/en/settings) | interactive settings | ❌ config files only |
| [`/permissions`](https://code.claude.com/docs/en/iam) | rules viewer/editor | ❌ stored rules, no UI |
| [`/mcp`](https://code.claude.com/docs/en/mcp) | MCP manage/auth dialog | ❌ `ganja mcp` listing + status-bar notice |
| [`/memory`](https://code.claude.com/docs/en/memory) | memory file editor | ❌ |
| [`/hooks`](https://code.claude.com/docs/en/hooks) | hooks manager | ❌ no hooks system |
| [`/statusline`](https://code.claude.com/docs/en/statusline) | status bar scripting | ❌ fixed status bar |
| [`/output-style`](https://code.claude.com/docs/en/output-styles) | response styles | ❌ |
| [`/context`](https://code.claude.com/docs/en/costs) | context usage grid | ❌ totals only |
| [`/todos`](https://code.claude.com/docs/en/interactive-mode) | task checklist view | ⚠️ todos render in-chat only |
| [`/usage`](https://code.claude.com/docs/en/costs) | usage/cost breakdown | ⚠️ session totals only |
| [`/doctor`](https://code.claude.com/docs/en/troubleshooting) | self-diagnostics | ❌ |
| [`/export`](https://code.claude.com/docs/en/slash-commands) | export conversation | ⚠️ `/copy` only |
| [`/cd`](https://code.claude.com/docs/en/slash-commands) *(low confidence)* | change directory | ❌ launch-directory-only is a design stance |
| [`/vim`](https://code.claude.com/docs/en/interactive-mode) | vim editing | ❌ |

## 4. Core agent capabilities

| Feature | Notes | ganja |
|---|---|---|
| [Project memory files](https://code.claude.com/docs/en/memory) | CLAUDE.md hierarchy | ✅ AGENTS.md family, three tiers |
| [Scoped rule files](https://code.claude.com/docs/en/memory) *(low confidence)* | glob-triggered `.claude/rules/*.md` | ❌ |
| [Auto memory](https://code.claude.com/docs/en/memory) | persistent MEMORY.md across sessions | ❌ |
| [Hooks](https://code.claude.com/docs/en/hooks) | deterministic lifecycle scripts | ❌ |
| [Subagents](https://code.claude.com/docs/en/sub-agents) | isolated-context delegation | ✅ `task` tool, isolated child transcript |
| [Parallel subagents](https://code.claude.com/docs/en/sub-agents) | concurrent execution | ❌ one turn at a time |
| [Custom agent definitions](https://code.claude.com/docs/en/sub-agents) | `.claude/agents/*` files | ⚠️ config-declared agents; no per-agent tool grants |
| [Skills](https://code.claude.com/docs/en/skills) | SKILL.md loading | ✅ ganja's two homes + `skills.paths` |
| [Skill auto-triggering](https://code.claude.com/docs/en/skills) | description matching | ❌ explicit load only |
| [Forked skill context](https://code.claude.com/docs/en/skills) *(low confidence)* | `context: fork` | ❌ |
| [Plugins & marketplaces](https://code.claude.com/docs/en/plugins) | bundled skills/agents/hooks/MCP | ❌ |
| [Checkpointing](https://code.claude.com/docs/en/checkpointing) | pre-edit snapshots + conversation restore | ⚠️ worktree snapshots via `/undo` only |
| [Background tasks](https://code.claude.com/docs/en/interactive-mode) | async execution, notifications | ❌ |
| [Auto-compaction](https://code.claude.com/docs/en/costs) | summarize near the limit | ✅ |
| [Permission system](https://code.claude.com/docs/en/iam) | allow/ask/deny with stored answers | ✅ last-match rules, arity-aware "always" |
| [Sandboxed execution](https://code.claude.com/docs/en/sandboxing) | OS/container isolation | ❌ permission gating only |
| [MCP stdio + HTTP](https://code.claude.com/docs/en/mcp) | client transports | ✅ |
| [MCP CLI management](https://code.claude.com/docs/en/mcp) | `claude mcp add/list` | ⚠️ `ganja mcp` lists; adding is config-file only |
| [MCP OAuth](https://code.claude.com/docs/en/mcp) | remote server auth | ❌ refused loudly by config key |
| [MCP reconnection](https://code.claude.com/docs/en/mcp) | recover a dead server | ❌ dialled once |
| [Web search / fetch tools](https://code.claude.com/docs/en/settings) | built-in web tools | ✅ `websearch` (Exa/Parallel), `webfetch` |
| [Todo tool](https://code.claude.com/docs/en/interactive-mode) | task tracking | ✅ `todowrite` |
| [LSP diagnostics](https://code.claude.com/docs/en/troubleshooting) | editor-grade feedback | ✅ opt-in LSP, errors appended to edits |

## 5. CLI, headless, SDK

| Feature | Notes | ganja |
|---|---|---|
| [Print mode](https://code.claude.com/docs/en/cli-reference) | `claude -p` | ✅ `ganja run` |
| [Streaming JSON output](https://code.claude.com/docs/en/cli-reference) | `--output-format stream-json` | ✅ `--format json` (nd-JSON) |
| [Session continuation](https://code.claude.com/docs/en/cli-reference) | `--continue` / `--resume` | ✅ |
| [Session forking](https://code.claude.com/docs/en/cli-reference) | `--fork-session` | ❌ |
| [Permission modes](https://code.claude.com/docs/en/cli-reference) | dontAsk / acceptEdits / plan / bypass | ⚠️ `--auto` single tier |
| [Per-invocation tool allowlists](https://code.claude.com/docs/en/iam) | `--allowedTools` patterns | ❌ config rules only |
| [System prompt flags](https://code.claude.com/docs/en/cli-reference) | append/replace × inline/file | ❌ |
| [Hermetic run](https://code.claude.com/docs/en/cli-reference) *(low confidence)* | `--bare` | ❌ |
| [Schema-constrained output](https://code.claude.com/docs/en/cli-reference) | `--json-schema` | ❌ |
| [Agent SDK](https://docs.claude.com/en/api/agent-sdk/overview) | TypeScript / Python engine embedding | ❌ nearest is `ganja-serve` + `ganja-client` over HTTP/SSE |
| [MCP server mode](https://code.claude.com/docs/en/mcp) | `claude mcp serve` | ❌ |

## 6. Enterprise and platform

| Feature | Notes | ganja |
|---|---|---|
| [Amazon Bedrock](https://code.claude.com/docs/en/amazon-bedrock) | IAM auth, regioned inference | ❌ |
| [Google Vertex AI](https://code.claude.com/docs/en/google-vertex-ai) | ADC/IAM, VPC-SC | ❌ |
| [Managed policy settings](https://code.claude.com/docs/en/iam) | org-enforced settings, MDM | ❌ |
| [OpenTelemetry export](https://code.claude.com/docs/en/monitoring-usage) | OTLP traces/metrics/logs | ❌ |
| [Network sandboxing](https://code.claude.com/docs/en/network-config) | egress allowlists, proxy masking | ❌ |
| [Devcontainer feature](https://code.claude.com/docs/en/devcontainer) | isolated container reference setup | ❌ |
| [GitHub Actions](https://code.claude.com/docs/en/github-actions) | `@claude` mentions, automated review | ❌ |
| [Desktop app](https://code.claude.com/docs/en/desktop) | session sync | ❌ |
| [Web / mobile sessions](https://code.claude.com/docs/en/claude-code-on-the-web) | cloud runs, `--teleport` handoff | ❌ |
| [Auto-update](https://code.claude.com/docs/en/setup) | self-updating install | ❌ packaging deferred by design |
