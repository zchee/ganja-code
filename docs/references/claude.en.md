# Claude Code feature reference (compared against ganja)

> [!IMPORTANT]
> **This document is a reference inventory, not a roadmap. Not every feature
> listed here will be ported.** ganja's charter is behavioral parity with
> opencode v1.18.13; Claude Code is a separate product, catalogued here for
> comparison only. A ❌ in these tables is an observation, never a promise.

Snapshot: 2026-08-12, against the Claude Code 2.1.x generation. Claude Code
moves quickly — treat stale rows as stale, not as ganja regressions. Rows
marked *(low confidence)* rest on community sources rather than official
documentation.

Sections follow the shared outline all three references use (claude, codex,
opencode), so the same topic sits at the same section number in each.

Legend: ✅ present in ganja (parity or a near equivalent) · ⚠️ partial · ❌ absent.

## 1. TUI — composer and input

| Feature | Keys | ganja |
|---|---|---|
| [File path tab completion](https://code.claude.com/docs/en/interactive-mode) | `@path` + Tab | ✅ Tab accepts in both menus — `@` inserts exactly as Enter does, `/` completes the buffer without running it; directory-descent (`@dir` → `@dir/`) not built, the walker is files-only |
| [Slash command autocomplete](https://code.claude.com/docs/en/slash-commands) | `/` | ✅ dropdown + palette |
| [File mentions](https://code.claude.com/docs/en/common-workflows) | `@` | ✅ incl. `#line-range` and image/PDF attachments |
| [Vim mode](https://code.claude.com/docs/en/interactive-mode) | `/vim` | ❌ |
| [Prompt history](https://code.claude.com/docs/en/interactive-mode) | Up / Down | ✅ fifty entries, dedupe, self-healing store |
| [Reverse history search](https://code.claude.com/docs/en/interactive-mode) | Ctrl+R | ✅ fuzzy-filtered, newest-first search modal with a preview pane; upstream's own Ctrl+R is unrelated (`session_rename`, never bound in ganja) |
| [Clipboard image paste](https://code.claude.com/docs/en/interactive-mode) | Ctrl+V | ✅ PNG-encoded in-process (no OS shell-out) and attached through the existing `@`-mention pipeline |
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
| [Message queueing](https://code.claude.com/docs/en/interactive-mode) | type while working | ✅ steers into the running turn at its next step boundary (`Command::Steer`); what cannot steer (refused, unconsumed, slash commands) falls back to a replayed FIFO |
| [Agent mentions](https://code.claude.com/docs/en/sub-agents) | `@agent-…` | ❌ `@` is files only |
| [Dropped-path mentions](https://code.claude.com/docs/en/interactive-mode) | drag & drop | ✅ a drop or paste of one or more existing/`file://` paths becomes `@`-mentions; any token that fails to resolve leaves the whole paste literal |
| [Screen redraw](https://code.claude.com/docs/en/interactive-mode) | Ctrl+L | ✅ forces the next frame through a full redraw (Claude Code binding; no upstream counterpart) |
| [Staged interrupt](https://code.claude.com/docs/en/interactive-mode) | Ctrl+C once/twice | ⚠️ single-stage |

## 2. TUI — larger surfaces and keybinds

*New in this revision (2026-08-12), researched; rows lean on the official
docs unless marked otherwise.*

| Feature | Notes | ganja |
|---|---|---|
| [Verbose transcript viewer](https://code.claude.com/docs/en/interactive-mode) | Ctrl+O overlay: full history, tool payloads, thinking blocks | ✅ Ctrl+T inspector — full-terminal takeover, three tabs (expanded transcript, raw event log, per-turn tokens); presentation synthesizes Codex CLI's own overlay and Claude Code's Ctrl+O footer wording |
| [Todo checklist panel](https://code.claude.com/docs/en/interactive-mode) | Ctrl+T toggles a task side-panel | ⚠️ todos render in-chat only; ganja's Ctrl+T went to the inspector |
| [Permission dialog](https://code.claude.com/docs/en/iam) | tool call preview, approve/deny, mode switch in-dialog | ✅ upstream's dialog semantics (`a`/`A`/`d`), queued when several children ask at once; no in-dialog mode switching (no mode concept) |
| [Trust dialog](https://code.claude.com/docs/en/iam) | first-launch directory trust prompt | ❌ no trust tier; permission rules gate everything instead |
| [Status line scripting](https://code.claude.com/docs/en/statusline) | `/statusline`, `statusLine` command fed session JSON on stdin | ❌ fixed status bar (themes ✅) |
| [Terminal setup](https://code.claude.com/docs/en/terminal-config) | `/terminal-setup`: keybinding + terminal profile tuning | ❌ nothing to configure; bracketed paste and OSC 52 are unconditional |
| [Spinner tips](https://code.claude.com/docs/en/settings) | `spinnerTipsEnabled` | ❌ |
| [Customizable keybindings](https://code.claude.com/docs/en/interactive-mode) *(low confidence on the file's schema)* | `~/.claude/keybindings.json`: context-aware bindings and chords | ⚠️ `keybinds` config map — comma-separated alternates per action, an empty value unbinds; no contexts, no chords |

## 3. Modes and session keys

| Feature | Keys | ganja |
|---|---|---|
| [Permission mode cycling](https://code.claude.com/docs/en/iam) | Shift+Tab | ❌ no mode concept; the plan agent approximates plan mode |
| [Extended thinking toggle](https://code.claude.com/docs/en/interactive-mode) | Tab / Cmd+T | ❌ (`/effort` selects a level instead) |
| [Rewind / checkpoints](https://code.claude.com/docs/en/checkpointing) | Esc Esc, `/rewind` | ✅ `/rewind` + idle Esc Esc open a two-step checkpoint picker (Both/Conversation/Files scopes, `Command::RevertTo`); upstream's part-level anchor (`partID`) not ported — checkpoints are whole user messages |
| [Background a running task](https://code.claude.com/docs/en/interactive-mode) | Ctrl+B | ⚠️ background execution exists (`bash`'s `run_in_background`, `bash_output`/`kill_shell`); no gesture backgrounds an *already-running* foreground call |
| [Agent switching](https://code.claude.com/docs/en/sub-agents) | — | ✅ Tab cycles agents (ganja's own default); reverse cycle ❌ |

## 4. Slash commands

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
| [`/mcp`](https://code.claude.com/docs/en/mcp) | MCP manage/auth dialog | ✅ two-step dialog: server list (status, tool count, error) → Reconnect/Login actions, `ganja mcp` CLI listing beside it |
| [`/memory`](https://code.claude.com/docs/en/memory) | memory file editor | ❌ |
| [`/hooks`](https://code.claude.com/docs/en/hooks) | hooks manager | ⚠️ the hook system itself is ✅ (config-declared, nine events); no interactive manager UI to view/edit them |
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

## 5. Built-in tools

| Tool | Notes | ganja |
|---|---|---|
| [`Read`](https://code.claude.com/docs/en/settings) | text with line numbers, plus images, PDFs (~20 pages), notebooks | ⚠️ text ✅; images/PDF reach the model via `@` attachments, not the read tool |
| [`Edit`](https://code.claude.com/docs/en/settings) | exact string replacement, read-before-edit enforced | ✅ same discipline (`FileTimes`) |
| [`Write`](https://code.claude.com/docs/en/settings) | create/overwrite | ✅ plus anchored I/O against symlink swaps |
| [`NotebookEdit`](https://code.claude.com/docs/en/settings) | Jupyter cell operations | ❌ |
| [`Glob`](https://code.claude.com/docs/en/settings) | pattern file search | ✅ in-process (ripgrep crates) |
| [`Grep`](https://code.claude.com/docs/en/settings) | regex content search | ✅ in-process |
| [`Bash`](https://code.claude.com/docs/en/settings) | shell with chain-aware permission checks | ✅ incl. upstream's arity table for "always" answers |
| [`BashOutput` / `KillShell`](https://code.claude.com/docs/en/settings) | background shell readback and kill | ✅ `bash_output` (delta polling + regex `filter`), `kill_shell` — no upstream opencode counterpart (D454) |
| [`WebFetch`](https://code.claude.com/docs/en/settings) | fetch and parse a URL | ✅ `webfetch` |
| [`WebSearch`](https://code.claude.com/docs/en/settings) | web search | ✅ `websearch` (Exa/Parallel) |
| [`Task`](https://code.claude.com/docs/en/sub-agents) | spawn subagents | ✅ `task` |
| [`TodoWrite`](https://code.claude.com/docs/en/interactive-mode) | session checklist | ✅ `todowrite` |
| [`ExitPlanMode`](https://code.claude.com/docs/en/common-workflows) | leave planning with user confirmation | ✅ `plan_exit` (question-gated switch to build) |
| Skill tool | load a skill on request | ✅ `skill` |
| Question/AskUserQuestion tool | structured user questions | ✅ `question` incl. custom text |

## 6. Permissions

| Feature | Notes | ganja |
|---|---|---|
| [Bash command patterns](https://code.claude.com/docs/en/iam) | `Bash(npm run *)`, prefix/suffix/multi wildcards | ⚠️ pattern rules exist (upstream shape); wildcard grammar differs |
| [Chain decomposition](https://code.claude.com/docs/en/iam) | `&&`/`;`/`\|` split; every leg must pass | ⚠️ command-kind analysis via the arity table; not per-leg splitting |
| [Path rules, gitignore-style](https://code.claude.com/docs/en/iam) | `Edit(src/**)`, `Read(.env)`, `//` for absolute | ❌ path-scoped allow/deny rules per tool |
| [MCP tool patterns](https://code.claude.com/docs/en/iam) | `mcp__server__tool`, server-wide grants | ✅ same naming; MCP tools ask by default |
| [Domain-scoped web rules](https://code.claude.com/docs/en/iam) | `WebFetch(domain:github.com)` | ❌ |
| [deny → ask → allow, most restrictive wins](https://code.claude.com/docs/en/iam) | | ⚠️ ganja is last-match-wins with layered tiers — a different, pinned semantics |
| [Settings scopes](https://code.claude.com/docs/en/settings) | user / project / project-local / CLI flags / managed | ⚠️ builtin < agent < config < stored answers; no local-overlay, flags, or managed tier |
| [Stored "always" answers](https://code.claude.com/docs/en/iam) | persist approvals | ✅ per-project store, arity-aware for shell |
| [Sandboxed execution](https://code.claude.com/docs/en/sandboxing) | OS/container isolation | ❌ permission gating only |

## 7. Hooks and automation

| Feature | Notes | ganja |
|---|---|---|
| [Hook events](https://code.claude.com/docs/en/hooks) | PreToolUse · PostToolUse · UserPromptSubmit · Notification · Stop · SubagentStop · SessionStart · SessionEnd · PreCompact (+ permission-decision hooks) | ✅ all nine, config-declared (`hooks` key, Claude's own `{matcher, hooks:[...]}` shape kept verbatim) — no upstream opencode counterpart at all (D456) |
| [Hook protocol](https://code.claude.com/docs/en/hooks) | JSON payload on stdin; exit 2 blocks the tool call; stdout feeds context back | ⚠️ same envelope and exit-code semantics; blocking is v1-scoped to PreToolUse/UserPromptSubmit (the two events blocking means something for); no `transcript_path` (SQLite storage, D457); `updatedInput` rewriting and Stop-hook forced continuation not built |
| [Matchers](https://code.claude.com/docs/en/hooks) | per-tool regex matchers (`Edit\|Write`) | ✅ regex matchers, plus enumerated vocabularies for PreCompact/SessionStart |

## 8. Rules, custom commands and memory

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

## 9. Agents and skills

| Feature | Notes | ganja |
|---|---|---|
| [Agent definition files](https://code.claude.com/docs/en/sub-agents) | `.claude/agents/*.md` with name/description/model/tools frontmatter | ⚠️ config-declared agents with model + rules; no per-agent tool grants |
| [Auto-delegation by description](https://code.claude.com/docs/en/sub-agents) | the model picks the agent | ⚠️ the task tool offers the roster with descriptions |
| [Parallel subagents](https://code.claude.com/docs/en/sub-agents) | concurrent execution | ✅ consecutive `task` calls in one assistant step fan out concurrently (capped by `agents.concurrency`, default 4) and fan back in on completion order; root turns still stay serial (D462) |
| [`isolation: worktree`](https://code.claude.com/docs/en/sub-agents) | subagent in its own git worktree | ❌ |
| [Skill preloading (`skills:` on agents)](https://code.claude.com/docs/en/sub-agents) | | ❌ |
| [SKILL.md loading](https://code.claude.com/docs/en/skills) | | ✅ ganja's two homes + `skills.paths` |
| [Skill auto-triggering + `paths` scoping](https://code.claude.com/docs/en/skills) | description- and path-matched invocation | ❌ explicit load only |
| [`context: fork`](https://code.claude.com/docs/en/skills) | run the skill in a forked subagent, return only results | ❌ |
| [Skill `allowed-tools`](https://code.claude.com/docs/en/skills) | tool restriction incl. `mcp__*` wildcards | ❌ |
| [Plugins: 5 component types](https://code.claude.com/docs/en/plugins) | skills, agents, hooks, MCP servers, LSP servers | ❌ |
| [Marketplaces](https://code.claude.com/docs/en/plugins) | `marketplace.json`, `/plugin install`, `/reload-plugins` | ❌ |

## 10. MCP and LSP

| Feature | Notes | ganja |
|---|---|---|
| [Transports](https://code.claude.com/docs/en/mcp) | stdio, streamable HTTP, SSE | ✅ stdio + streamable HTTP; legacy SSE ❌ |
| [Config scopes](https://code.claude.com/docs/en/mcp) | local (`~/.claude.json`) / project (`.mcp.json`) / user, with precedence | ⚠️ global + project config tiers; no per-user-per-repo local scope |
| [CLI management](https://code.claude.com/docs/en/mcp) | `claude mcp add/list --scope --transport` | ⚠️ `ganja mcp` lists; adding is config-file only |
| [OAuth](https://code.claude.com/docs/en/mcp) | PKCE flows, metadata discovery, token refresh | ✅ RFC 8414 discovery + RFC 7591 registration (fallback client id) + PKCE/loopback + refresh-then-redial on 401; deliberately minimal — no resource-metadata discovery, no per-request reactive re-auth mid-call (D466) |
| [Project-scope first-use approval](https://code.claude.com/docs/en/mcp) | guard against repo-injected servers | ✅ stronger: every MCP tool asks by default |
| [Timeout/output knobs](https://code.claude.com/docs/en/settings) | `MCP_TIMEOUT`, `MCP_TOOL_TIMEOUT`, `MAX_MCP_OUTPUT_TOKENS` | ⚠️ per-server `timeout`/`output_limit` config keys (bytes, not tokens); no global env-var knobs |
| Reconnection | recover a dead server | ✅ `/mcp` dialog's manual Reconnect (any `Failed` server) + a bounded once-per-session automatic retry for a server whose first dial never succeeded (D463) |
| [LSP servers via plugins](https://code.claude.com/docs/en/plugins) | plugins may bundle LSP servers | ⚠️ ganja's LSP is first-party config (`lsp` key: rust/gopls builtins + custom entries), not a plugin surface |

## 11. Models, providers and auth

| Feature | Notes | ganja |
|---|---|---|
| [Model aliases](https://code.claude.com/docs/en/model-config) | `sonnet` / `opus` / `haiku` | ⚠️ full catalog ids only |
| [`opusplan`](https://code.claude.com/docs/en/model-config) | Opus for plan mode, Sonnet for execution, automatic | ❌ |
| [1M-context aliases](https://code.claude.com/docs/en/model-config) | `sonnet[1m]`, `opus[1m]` | ❌ |
| [`MAX_THINKING_TOKENS`](https://code.claude.com/docs/en/settings) | thinking budget override | ⚠️ effort variants carry budgets from the catalog instead |
| [Auto-compact threshold override](https://code.claude.com/docs/en/settings) *(low confidence)* | env-tunable trigger percentage | ❌ fixed thresholds |
| [Small/fast model routing](https://code.claude.com/docs/en/settings) | background tasks on a cheaper model | ⚠️ ganja's title requests ride the session model |
| [Subscription OAuth vs Console API key](https://code.claude.com/docs/en/iam) | `/login` picks claude.ai OAuth (PKCE) or a metered API key | ⚠️ ganja's `anthropic` is API-key only (env or stored); subscription OAuth was never in the upstream spec (removed for terms compliance) |
| [`apiKeyHelper`](https://code.claude.com/docs/en/settings) | settings-declared command that emits the key on demand | ❌ nearest is `key_env` on a config-declared provider |
| [`ANTHROPIC_AUTH_TOKEN`](https://code.claude.com/docs/en/settings) | custom bearer for gateways/proxies | ❌ |
| [OS keychain credential storage](https://code.claude.com/docs/en/iam) *(low confidence on per-OS detail)* | macOS Keychain / Credential Manager / libsecret | ⚠️ ganja stores `auth.json` with owner-only permission bits; no OS keychain integration |
| Credential precedence chain | env token > stored login, checked in a documented order | ✅ same shape: `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` outrank the stored credential |

## 12. Configuration surface

*New in this revision (2026-08-12), researched.*

| Feature | Notes | ganja |
|---|---|---|
| [Settings scopes and precedence](https://code.claude.com/docs/en/settings) | managed > CLI flags > `.claude/settings.local.json` > `.claude/settings.json` > `~/.claude/settings.json` | ⚠️ three JSONC tiers (global home < `GANJA_CONFIG` < project files), later beats earlier; no local-overlay, flag or managed tiers |
| [`$schema` reference](https://code.claude.com/docs/en/settings) | JSON Schema for editor completion | ✅ ganja ships `schema/ganja-config.schema.json`, drift-tested against the loader |
| [`permissions` block](https://code.claude.com/docs/en/iam) | `allow`/`ask`/`deny` arrays + `defaultMode` | ⚠️ ganja's `permission` block is upstream opencode's grammar instead (§6) |
| [`env` block](https://code.claude.com/docs/en/settings) | inject environment per scope | ❌ |
| [`hooks` key](https://code.claude.com/docs/en/hooks) | event-keyed matcher/handler groups | ✅ Claude's shape kept verbatim (§7) |
| [`model` / `effortLevel` keys](https://code.claude.com/docs/en/model-config) | default model and reasoning depth | ⚠️ `model` ✅; effort is picked per-session (`/effort`), not a config key |
| [`statusLine`, `outputStyle`, `spinnerTipsEnabled`](https://code.claude.com/docs/en/statusline) | display scripting and styles | ❌ (themes are ganja's one display knob) |
| [`attribution`](https://code.claude.com/docs/en/settings) *(low confidence)* | commit/PR trailer text | ❌ ganja writes no commits of its own |
| [`claude config` CLI](https://code.claude.com/docs/en/settings) | `get`/`set`/`list`, `--global` | ❌ config files only |
| [`--setting-sources`](https://code.claude.com/docs/en/settings) | choose which tiers load | ❌ |
| Housekeeping keys | `cleanupPeriodDays`, `language`, `autoUpdatesChannel`, `companyAnnouncements` | ❌ collectively |

## 13. Sessions and storage

| Feature | Notes | ganja |
|---|---|---|
| [`/add-dir` / `additionalDirectories`](https://code.claude.com/docs/en/common-workflows) | multi-directory access | ❌ single launch directory is a design stance |
| [`--worktree`](https://code.claude.com/docs/en/common-workflows) | run the session in a linked git worktree | ❌ |
| [Session transcripts on disk](https://code.claude.com/docs/en/data-usage) | JSONL per session, resumable | ✅ SQLite per project, resumable |
| [Checkpoint file history](https://code.claude.com/docs/en/checkpointing) | content-hashed pre-edit backups | ⚠️ worktree snapshots via `/undo` |
| [Shell snapshots](https://code.claude.com/docs/en/settings) *(low confidence)* | captured shell env for reproducibility | ❌ |

## 14. CLI and headless

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

## 15. Server surface and SDK

| Feature | Notes | ganja |
|---|---|---|
| [Agent SDK](https://docs.claude.com/en/api/agent-sdk/overview) | TypeScript / Python engine embedding | ❌ nearest is `ganja-serve` + `ganja-client` over HTTP/SSE |
| [MCP server mode](https://code.claude.com/docs/en/mcp) | `claude mcp serve` — the engine as an MCP server | ❌ |
| HTTP server surface | none — Claude Code serves no REST/SSE API of its own | n/a — ganja-side advantage: `ganja serve` (REST + SSE, Basic auth) with a typed `ganja-client` against it |

## 16. Environment variables

*New in this revision (2026-08-12), researched. The documented surface is
large; these are the rows that shape behavior.*

| Variable | Meaning | ganja |
|---|---|---|
| [`ANTHROPIC_API_KEY`](https://code.claude.com/docs/en/settings) | API-key credential | ✅ the same variable, outranking the stored key |
| [`ANTHROPIC_BASE_URL`](https://code.claude.com/docs/en/settings) | endpoint override | ✅ the same variable, refused unless https or loopback |
| [`ANTHROPIC_AUTH_TOKEN`](https://code.claude.com/docs/en/settings) | custom bearer for gateways | ❌ |
| [`ANTHROPIC_MODEL`](https://code.claude.com/docs/en/model-config) | default model override | ⚠️ `GANJA_MODEL` (catalog-checked for cataloged providers) |
| [`ANTHROPIC_DEFAULT_*_MODEL` / `ANTHROPIC_SMALL_FAST_MODEL`](https://code.claude.com/docs/en/settings) | alias pinning, cheap-model routing | ⚠️ `small_model` config key is the nearest |
| [`CLAUDE_CODE_USE_BEDROCK` / `_VERTEX` / `_FOUNDRY`](https://code.claude.com/docs/en/amazon-bedrock) | cloud-platform routing | ❌ |
| [`MAX_THINKING_TOKENS`](https://code.claude.com/docs/en/settings) | thinking budget | ⚠️ effort variants carry catalog budgets |
| [`CLAUDE_CODE_MAX_OUTPUT_TOKENS`](https://code.claude.com/docs/en/settings) | response cap | ❌ |
| [`BASH_DEFAULT_TIMEOUT_MS` / `BASH_MAX_TIMEOUT_MS` / `BASH_MAX_OUTPUT_LENGTH`](https://code.claude.com/docs/en/settings) | shell tool budgets | ⚠️ ganja's shell has fixed defaults + a per-call `timeout` argument; no env knobs |
| [`MCP_TIMEOUT` / `MCP_TOOL_TIMEOUT` / `MAX_MCP_OUTPUT_TOKENS`](https://code.claude.com/docs/en/settings) | MCP budgets | ⚠️ per-server `timeout`/`output_limit` config keys instead |
| [`DISABLE_TELEMETRY` / `DISABLE_ERROR_REPORTING` / `DISABLE_AUTOUPDATER` / `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`](https://code.claude.com/docs/en/settings) | phone-home switches | n/a — ganja has no telemetry, error reporting or self-updater to disable |
| [`HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY`](https://code.claude.com/docs/en/network-config) | proxy routing | ⚠️ reqwest honors the standard proxy vars; untested surface, no ganja-side docs |
| [`CLAUDE_CODE_OAUTH_TOKEN`](https://code.claude.com/docs/en/cli-reference) *(low confidence)* | headless OAuth token | ❌ |

ganja's own `GANJA_*` surface (config home, fake provider script, catalog
knobs, serve credentials, websearch keys, test opt-ins) is documented in the
repository root's `AGENTS.md` and has no Claude Code counterpart.

## 17. Enterprise and platform

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
