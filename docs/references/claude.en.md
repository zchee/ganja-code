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
| [`/add-dir`](https://code.claude.com/docs/en/common-workflows) | grant extra directories mid-session | ❌ |
| [`/plugin`](https://code.claude.com/docs/en/plugins) | marketplace add / install / reload | ❌ |
| [`/vim`](https://code.claude.com/docs/en/interactive-mode) | vim editing | ❌ |

## 4. Built-in tool inventory

| Tool | Notes | ganja |
|---|---|---|
| [`Read`](https://code.claude.com/docs/en/settings) | text with line numbers, plus images, PDFs (~20 pages), notebooks | ⚠️ text ✅; images/PDF reach the model via `@` attachments, not the read tool |
| [`Edit`](https://code.claude.com/docs/en/settings) | exact string replacement, read-before-edit enforced | ✅ same discipline (`FileTimes`) |
| [`Write`](https://code.claude.com/docs/en/settings) | create/overwrite | ✅ plus anchored I/O against symlink swaps |
| [`NotebookEdit`](https://code.claude.com/docs/en/settings) | Jupyter cell operations | ❌ |
| [`Glob`](https://code.claude.com/docs/en/settings) | pattern file search | ✅ in-process (ripgrep crates) |
| [`Grep`](https://code.claude.com/docs/en/settings) | regex content search | ✅ in-process |
| [`Bash`](https://code.claude.com/docs/en/settings) | shell with chain-aware permission checks | ✅ incl. upstream's arity table for "always" answers |
| [`BashOutput` / `KillShell`](https://code.claude.com/docs/en/settings) | background shell readback and kill | ❌ no background shells |
| [`WebFetch`](https://code.claude.com/docs/en/settings) | fetch and parse a URL | ✅ `webfetch` |
| [`WebSearch`](https://code.claude.com/docs/en/settings) | web search | ✅ `websearch` (Exa/Parallel) |
| [`Task`](https://code.claude.com/docs/en/sub-agents) | spawn subagents | ✅ `task` |
| [`TodoWrite`](https://code.claude.com/docs/en/interactive-mode) | session checklist | ✅ `todowrite` |
| [`ExitPlanMode`](https://code.claude.com/docs/en/common-workflows) | leave planning with user confirmation | ✅ `plan_exit` (question-gated switch to build) |
| Skill tool | load a skill on request | ✅ `skill` |
| Question/AskUserQuestion tool | structured user questions | ✅ `question` incl. custom text |

## 5. Permission system detail

| Feature | Notes | ganja |
|---|---|---|
| [Bash command patterns](https://code.claude.com/docs/en/iam) | `Bash(npm run *)`, prefix/suffix/multi wildcards | ⚠️ pattern rules exist (upstream shape); wildcard grammar differs |
| [Chain decomposition](https://code.claude.com/docs/en/iam) | `&&`/`;`/`\|` split; every leg must pass | ⚠️ command-kind analysis via the arity table; not per-leg splitting |
| [Path rules, gitignore-style](https://code.claude.com/docs/en/iam) | `Edit(src/**)`, `Read(.env)`, `//` for absolute | ❌ path-scoped allow/deny rules per tool |
| [MCP tool patterns](https://code.claude.com/docs/en/iam) | `mcp__server__tool`, server-wide grants | ✅ same naming; MCP tools ask by default |
| [Domain-scoped web rules](https://code.claude.com/docs/en/iam) | `WebFetch(domain:github.com)` | ❌ |
| [deny → ask → allow, most restrictive wins](https://code.claude.com/docs/en/iam) | | ⚠️ ganja is last-match-wins with layered tiers — a different, pinned semantics |
| [Settings scopes](https://code.claude.com/docs/en/settings) | user / project / project-local / CLI flags / managed | ⚠️ builtin < agent < config < stored answers; no local-overlay, flags, or managed tier |
| [`env` block in settings](https://code.claude.com/docs/en/settings) | inject environment per scope | ❌ |
| [Stored "always" answers](https://code.claude.com/docs/en/iam) | persist approvals | ✅ per-project store, arity-aware for shell |
| [Sandboxed execution](https://code.claude.com/docs/en/sandboxing) | OS/container isolation | ❌ permission gating only |

## 6. Hooks and automation

| Feature | Notes | ganja |
|---|---|---|
| [Hook events](https://code.claude.com/docs/en/hooks) | PreToolUse · PostToolUse · UserPromptSubmit · Notification · Stop · SubagentStop · SessionStart · SessionEnd · PreCompact (+ permission-decision hooks) | ❌ the whole mechanism |
| [Hook protocol](https://code.claude.com/docs/en/hooks) | JSON payload on stdin; exit 2 blocks the tool call; stdout feeds context back | ❌ |
| [Matchers](https://code.claude.com/docs/en/hooks) | per-tool regex matchers (`Edit\|Write`) | ❌ |

## 7. Custom slash commands and memory internals

| Feature | Notes | ganja |
|---|---|---|
| [Command files](https://code.claude.com/docs/en/slash-commands) | `.claude/commands/*.md` + `~/.claude/commands` | ✅ config-declared commands |
| [`$ARGUMENTS` / `$1`, `$2`](https://code.claude.com/docs/en/slash-commands) | argument expansion | ✅ |
| [`` !`cmd` `` in templates](https://code.claude.com/docs/en/slash-commands) | dynamic shell output at invocation | ✅ (P8) |
| [`@path` in templates](https://code.claude.com/docs/en/slash-commands) | file embedding | ✅ (P8, as mention-grade attachment) |
| [Command frontmatter: `allowed-tools`](https://code.claude.com/docs/en/slash-commands) | per-command tool restriction | ❌ (per-command agent ✅) |
| [Command frontmatter: `model`, `argument-hint`](https://code.claude.com/docs/en/slash-commands) | per-command model + hint text | ❌ |
| [CLAUDE.md hierarchy](https://code.claude.com/docs/en/memory) | global → project root → subdirectory files, walked and concatenated | ⚠️ global + project AGENTS.md family; no subdirectory walk-in |
| [`@path` imports in memory files](https://code.claude.com/docs/en/memory) | modular includes, resolved relative to the importer | ❌ |
| [Auto memory](https://code.claude.com/docs/en/memory) | `~/.claude/projects/<hash>/memory/` with MEMORY.md index + topic files, self-maintained | ❌ |

## 8. Subagents, skills, plugins

| Feature | Notes | ganja |
|---|---|---|
| [Agent definition files](https://code.claude.com/docs/en/sub-agents) | `.claude/agents/*.md` with name/description/model/tools frontmatter | ⚠️ config-declared agents with model + rules; no per-agent tool grants |
| [Auto-delegation by description](https://code.claude.com/docs/en/sub-agents) | the model picks the agent | ⚠️ the task tool offers the roster with descriptions |
| [Parallel subagents](https://code.claude.com/docs/en/sub-agents) | concurrent execution | ❌ one turn at a time |
| [`isolation: worktree`](https://code.claude.com/docs/en/sub-agents) | subagent in its own git worktree | ❌ |
| [Skill preloading (`skills:` on agents)](https://code.claude.com/docs/en/sub-agents) | | ❌ |
| [SKILL.md loading](https://code.claude.com/docs/en/skills) | | ✅ ganja's two homes + `skills.paths` |
| [Skill auto-triggering + `paths` scoping](https://code.claude.com/docs/en/skills) | description- and path-matched invocation | ❌ explicit load only |
| [`context: fork`](https://code.claude.com/docs/en/skills) | run the skill in a forked subagent, return only results | ❌ |
| [Skill `allowed-tools`](https://code.claude.com/docs/en/skills) | tool restriction incl. `mcp__*` wildcards | ❌ |
| [Plugins: 5 component types](https://code.claude.com/docs/en/plugins) | skills, agents, hooks, MCP servers, LSP servers | ❌ |
| [Marketplaces](https://code.claude.com/docs/en/plugins) | `marketplace.json`, `/plugin install`, `/reload-plugins` | ❌ |

## 9. MCP detail

| Feature | Notes | ganja |
|---|---|---|
| [Transports](https://code.claude.com/docs/en/mcp) | stdio, streamable HTTP, SSE | ✅ stdio + streamable HTTP; legacy SSE ❌ |
| [Config scopes](https://code.claude.com/docs/en/mcp) | local (`~/.claude.json`) / project (`.mcp.json`) / user, with precedence | ⚠️ global + project config tiers; no per-user-per-repo local scope |
| [CLI management](https://code.claude.com/docs/en/mcp) | `claude mcp add/list --scope --transport` | ⚠️ `ganja mcp` lists; adding is config-file only |
| [OAuth](https://code.claude.com/docs/en/mcp) | PKCE flows, metadata discovery, token refresh | ❌ refused loudly by config key |
| [Project-scope first-use approval](https://code.claude.com/docs/en/mcp) | guard against repo-injected servers | ✅ stronger: every MCP tool asks by default |
| [Timeout/output knobs](https://code.claude.com/docs/en/settings) | `MCP_TIMEOUT`, `MCP_TOOL_TIMEOUT`, `MAX_MCP_OUTPUT_TOKENS` | ❌ |
| Reconnection | recover a dead server | ❌ dialled once (Claude Code reconnects via /mcp) |

## 10. Model and context configuration

| Feature | Notes | ganja |
|---|---|---|
| [Model aliases](https://code.claude.com/docs/en/model-config) | `sonnet` / `opus` / `haiku` | ⚠️ full catalog ids only |
| [`opusplan`](https://code.claude.com/docs/en/model-config) | Opus for plan mode, Sonnet for execution, automatic | ❌ |
| [1M-context aliases](https://code.claude.com/docs/en/model-config) | `sonnet[1m]`, `opus[1m]` | ❌ |
| [`MAX_THINKING_TOKENS`](https://code.claude.com/docs/en/settings) | thinking budget override | ⚠️ effort variants carry budgets from the catalog instead |
| [Auto-compact threshold override](https://code.claude.com/docs/en/settings) *(low confidence)* | env-tunable trigger percentage | ❌ fixed thresholds |
| [Small/fast model routing](https://code.claude.com/docs/en/settings) | background tasks on a cheaper model | ⚠️ ganja's title requests ride the session model |
| [Env-var surface](https://code.claude.com/docs/en/settings) | `ANTHROPIC_MODEL`, `DISABLE_TELEMETRY`, proxy vars, … | ⚠️ ganja has its own smaller `GANJA_*` surface |

## 11. Workspace and session storage

| Feature | Notes | ganja |
|---|---|---|
| [`/add-dir` / `additionalDirectories`](https://code.claude.com/docs/en/common-workflows) | multi-directory access | ❌ single launch directory is a design stance |
| [`--worktree`](https://code.claude.com/docs/en/common-workflows) | run the session in a linked git worktree | ❌ |
| [Session transcripts on disk](https://code.claude.com/docs/en/data-usage) | JSONL per session, resumable | ✅ SQLite per project, resumable |
| [Checkpoint file history](https://code.claude.com/docs/en/checkpointing) | content-hashed pre-edit backups | ⚠️ worktree snapshots via `/undo` |
| [Shell snapshots](https://code.claude.com/docs/en/settings) *(low confidence)* | captured shell env for reproducibility | ❌ |

## 12. CLI, headless, SDK

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

## 13. Enterprise and platform

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
