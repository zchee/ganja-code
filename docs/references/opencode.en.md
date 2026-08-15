# opencode feature reference (compared against ganja)

> [!IMPORTANT]
> **This document is a reference inventory, not a roadmap. Not every feature
> listed here will be ported.** ganja's charter is behavioral parity with
> opencode **v1.18.13** — the pin, not the moving tip. This revision surveys
> **v1.18.16** (the latest release at the snapshot date); §18 isolates what
> moved after the pin, and everything post-pin is out of charter until a
> deliberate re-pin. A ❌ is an observation, never a promise.

Snapshot: 2026-08-12, against v1.18.16. Source-level rows link to the pinned
tag (`anomalyco/opencode@v1.18.13`), which remains the spec ganja reads —
the v1.18.14–16 delta is bugfix-and-Desktop only (§18). Documented features
link to opencode.ai. The ganja-side cells were refreshed 2026-08-15 against
the post-P22 tree; the upstream survey is still the 2026-08-12 pass.

Sections follow the shared outline all three references use (claude, codex,
opencode), so the same topic sits at the same section number in each; §18 is
this document's own appendix.

Legend: ✅ present in ganja (parity or a near equivalent) · ⚠️ partial · ❌ absent.

## 1. TUI — composer and prompt modules

| Module | Notes | ganja |
|---|---|---|
| [`frecency.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/frecency.tsx) | frequency+recency completion ranking | ❌ |
| [`stash.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/stash.tsx) | draft stash | ❌ |
| [`move.tsx` / `workspace.tsx` / `cwd.ts`](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component/prompt) | session move, workspace, cwd context | ❌ |
| [`local-attachment.ts`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/local-attachment.ts) | mime attachments | ✅ |
| [`history.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/prompt/history.tsx) | prompt history | ✅ |
| [`autocomplete.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/autocomplete.tsx) | `@`/`/` completion + `#line-range` | ✅ |

## 2. TUI — larger surfaces and keybinds

### Larger surfaces

| Surface | Notes | ganja |
|---|---|---|
| [Session rename / tag / move / export dialogs](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component) | | ❌ |
| [Timeline + fork-from-timeline](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/dialog-timeline.tsx) | `<leader>g` | ⚠️ the checkpoint-list half is ported — `/rewind` + idle Esc Esc list every user message newest-first, Timeline's own picker shape; forking a session from a past point is not (Session forking ❌, §14) |
| [Message inspect dialog](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/dialog-message.tsx) | | ⚠️ Revert is ported as `/rewind`'s second step (`Command::RevertTo`, Both/Conversation/Files scopes — a superset of upstream's single revert); Fork isn't (no session forking) and Copy isn't reachable from this picker (`/copy-message` is its own command) |
| [Workspace UI](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component) | create/list/file-changes/destination | ❌ out of scope by design |
| [Sidebar](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/feature-plugins/sidebar) | context/files/lsp/mcp/todo panes | ❌ |
| [Diff viewer](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component/diff-viewer) | file tree, split/unified, hunk nav | ❌ inline unified diffs only |
| [Subagent transcript viewer](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/dialog-subagent.tsx) | | ⚠️ no per-child dialog, but a running task row hangs the child's recent calls under it (a capped log the watcher writes, whole in the Ctrl+T transcript, 2026-08-15) where upstream's metadata named only the current tool |
| [Provider / MCP / skill / status / debug pickers](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component) | | ❌ (`/effort` picker ✅) |
| Delete-failed / retry recovery dialogs | | ❌ |
| [Desktop notifications](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/notifications.ts) | | ❌ |
| Toast overlay | | ⚠️ adapted to status-bar notices, texts verbatim |
| Logo / startup animations / tips | | ❌ |
| TUI plugin runtime | | ❌ |
| Chat + streaming, permission dialog (`a`/`A`/`d` semantics), question dialog incl. free-text, palette + menus, themes, markdown, `/undo` markers | | ✅ — with the chat pane's *presentation* diverged to Claude Code's transcript grammar (D487: ●/⎿/>/✻, verdict-colored bullets); `/copy` keeps upstream's markdown shape on purpose |

### The full keybind registry

Ported and rebindable (6): [`app_exit`, `command_list`, `session_list`, `theme_list`, `agent_cycle`, `input_newline`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/config/keybind.ts).
Everything below is from the same registry. *Tab also completes upstream's own
`@`/`/` menus, through a separate `prompt.autocomplete.complete` binding (not
one of the six ported top-level actions, and not one of the rows below) —
upstream's Tab there additionally expands a directory in place before
selecting (`autocomplete.tsx:618-627`). ganja now Tab-accepts in both menus
too (`@` byte-identical to Enter; `/` completes the buffer without
submitting, a Claude Code presentation divergence, D446), but ganja's `@`
walker is files-only, so directory-descent stays unported. Tab still doubles
as `agent_cycle` (ported) whenever neither menu is open.*

| Action(s) | Default | ganja |
|---|---|---|
| [`leader` + which-key](https://opencode.ai/docs/keybinds) | ctrl+x | ❌ the whole leader system |
| `app_debug` / `app_console` / `app_heap_snapshot` | none | ❌ |
| `app_toggle_animations` / `_file_context` / `_diffwrap` / `_paste_summary` / `_session_directory_filter` | none | ❌ |
| `help_show` / `docs_open` | none | ❌ (`/help` ✅) |
| `diff_*` — open/close/toggle/expand/expand_all/collapse/switch_focus/next_hunk/previous_hunk/next_file/previous_file/toggle_file_tree/single_patch/switch_source/toggle_view/help | esc,q · enter · `]` `[` · n p b s d v ? | ❌ all sixteen (no diff viewer) |
| `editor_open` | `<leader>e` | ⚠️ `/editor` ✅, key ❌ |
| `theme_switch_mode` / `theme_mode_lock` | none | ❌ |
| `sidebar_toggle` / `scrollbar_toggle` / `status_view` / `debug_view` | `<leader>b`,`<leader>s` | ❌ |
| `session_export` / `session_copy` | `<leader>x` / none | ❌ / ⚠️ `/copy` ✅ |
| `session_move` / `session_timeline` / `session_fork` / `session_rename` / `session_delete` / `session_share` / `session_unshare` | ctrl+r, ctrl+d, … | ⚠️ `session_timeline`'s browsing is ported (`/rewind` + idle Esc Esc list every user-message checkpoint newest-first, Timeline's own shape) though its `<leader>g` key isn't; the other six (move/fork/rename/delete/share/unshare) stay ❌ |
| `session_new` / `session_compact` / `session_interrupt` | `<leader>n` / `<leader>c` / escape | ⚠️ `/new`, `/compact`, Esc-cancel ✅; keys not rebindable |
| `session_background` | ctrl+b | ❌ |
| `session_toggle_timestamps` / `_generic_tool_output` | none | ❌ |
| `session_queued_prompts` | `<leader>q` | ⚠️ ganja's queue strip renders steered/unconsumed entries above the composer and Up recalls-and-withdraws the newest one — the same idea, no dedicated leader-key list dialog |
| `session_child_first/child_cycle/child_cycle_reverse/parent` | `<leader>down`, right, left, up | ❌ |
| `session_pin_toggle` / `session_quick_switch_1..9` | ctrl+f / `<leader>1-9` | ❌ |
| `stash_delete` | ctrl+d | ❌ |
| `model_provider_list` / `model_favorite_toggle` / `model_cycle_recent(_reverse)` / `model_cycle_favorite(_reverse)` | ctrl+a, ctrl+f, f2 | ❌ (`/models` list ✅) |
| `mcp_list` / `provider_connect` / `console_org_switch` | none | ⚠️ `mcp_list`'s server-status browsing is ported as the `/mcp` command (P13, no dedicated key either) / ❌ / ❌ |
| `agent_list` / `agent_cycle_reverse` | `<leader>a` / shift+tab | ⚠️ `/agents` ✅ / ❌ |
| `variant_cycle` / `variant_list` | ctrl+t / none | ❌ cycle (`/effort` list ✅, catalog-synthesized roster) |
| `messages_page_up/…/half_page_down` (6) | pageup, … | ⚠️ scrolling ✅, not rebindable |
| `messages_first/last/next/previous/last_user` | ctrl+g, home, … | ❌ message-level navigation |
| `messages_copy` / `messages_undo` / `messages_redo` / `messages_toggle_conceal` | `<leader>y/u/r/h` | ⚠️ `/copy-message`, `/undo`, `/redo` ✅; keys + conceal ❌ |
| `tool_details` / `display_thinking` | none | ❌ |
| `prompt_submit` / `prompt_editor_context_clear` / `prompt_skills` / `prompt_stash(_pop/_list)` / `workspace_set` | none | ❌ |
| `input_clear` / `input_paste` | ctrl+c / ctrl+v | ❌ / ⚠️ bracketed paste (automatic, text) ✅; ctrl+v itself is now wired too, for clipboard image paste (PNG, in-process, D449) since images have no bracketed channel |
| `input_submit` / `input_move_*` / `input_backspace` / `input_delete` | return, arrows, … | ⚠️ behaviors ✅, not rebindable (Up/Down feed history ✅) |
| `input_select_*` (left/right/up/down/line/buffer/visual-line, 10 actions) | shift+… | ❌ no selection machinery |
| `input_line_home/end` / `input_visual_line_home/end` / `input_buffer_home/end` | ctrl+a/e, alt+a/e, home/end | ⚠️ partial built-ins; visual-line ❌ |
| `input_delete_line` / `input_delete_to_line_end` / `input_delete_to_line_start` | ctrl+shift+d, ctrl+k, ctrl+u | ⚠️ k/u built-in, not rebindable |
| `input_undo` / `input_redo` / `input_word_*` | ctrl+-, ctrl+., alt+f/b … | ⚠️ built-ins only |

## 3. Modes and execution

*New in this revision (2026-08-12): opencode has no mode system apart from
its agents, and this section says so in the shared outline's slot.*

| Feature | Notes | ganja |
|---|---|---|
| [Agents as modes](https://opencode.ai/docs/agents) | `build`/`plan` primaries are the mode concept; Tab cycles them | ✅ the same shape — Tab cycles ganja's primaries |
| [Plan-agent posture](https://opencode.ai/docs/agents) | plan denies edits and shell by default; `plan_exit` hands the wheel to build | ✅ incl. the question-gated `plan_exit` |
| [Session start agent](https://opencode.ai/docs/cli) | `--agent`, `default_agent` config | ✅ both |
| [No sandbox isolation](https://opencode.ai/docs/permissions) | runs on the host; the permission engine is the entire boundary | ✅ the same posture, deliberately |

## 4. Commands and skills

| Feature | Notes | ganja |
|---|---|---|
| [`/init` builtin](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/command/index.ts) | guided `AGENTS.md` setup | ✅ template verbatim |
| [`/review` builtin](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/command/index.ts) | `[commit\|branch\|pr]`, runs as a subtask | ❌ |
| [Markdown command files](https://opencode.ai/docs/commands) | `command/` or `commands/` under both scopes, filename = command name | ✅ ganja's two homes (D481): `<config home>/commands/*.md` + `.ganja/commands/*.md`, frontmatter `description`/`agent`/`model`/`argument-hint`, body as template, builtin < global < project < config — the `commands/` spelling only |
| [Frontmatter](https://opencode.ai/docs/commands) | `description`, `agent`, `model` | ✅ config equivalents |
| [`subtask: true`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/command/index.ts) | run the command in a child session | ❌ |
| [`$ARGUMENTS` / `$1..$N`](https://opencode.ai/docs/commands) | whole string, or positional tokens with the highest-numbered placeholder taking the rest | ✅ incl. quoted tokens |
| [`` !`cmd` `` substitution](https://opencode.ai/docs/commands) | shell output spliced into the template | ✅ spawned at the project root; stderr-merge and failure-reporting are named deviations |
| [`@file` references](https://opencode.ai/docs/commands) | attach files like a composer mention | ✅ |
| [MCP prompts as commands](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/command/index.ts) | a server's prompts surface as slash commands | ❌ MCP tools only |
| [Skills (SKILL.md)](https://opencode.ai/docs/skills) | config home + project + `skills.paths`, loaded on request by the `skill` tool | ✅ |
| [Foreign skill discovery](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/skill/index.ts) | walks `.claude/` and `.agents/` too (`OPENCODE_DISABLE_EXTERNAL_SKILLS` opts out) | ❌ standing ruling: nothing foreign discovered, one `skills.paths` line away |

## 5. Tools

| Tool | Notes | ganja |
|---|---|---|
| [`plan_enter`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/plan.ts) | enter plan mode | ✅ synthesized — upstream ships the description and the permission vocabulary and wires no tool (D477) |
| [`plan_exit`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/plan.ts) | hand the wheel to build | ✅ |
| [`lsp`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/lsp.ts) | hover/symbols exposed to the model | ❌ deviation `lsp-tool-unported` |
| [`apply_patch`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/apply_patch.ts) | OpenAI-model-gated patch editing | ❌ named in the permission table only |
| [`execute` (code-mode)](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/codemode) | script over MCP tools | ❌ whole package out of scope |
| [`doom_loop`](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/tool) | experimental | ❌ |
| [Spill/truncation discipline](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/truncate.ts) | oversized tool output truncated to a stale-swept spill dir | ✅ ganja's own spill files, redirected under test |
| [read / edit / write / glob / grep / bash / todowrite / webfetch / websearch / skill / question / task](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/tool) | the working set | ✅ incl. anchored writes, read-before-write, permission gating |

## 6. Permission grammar

| Feature | Notes | ganja |
|---|---|---|
| [Three actions](https://opencode.ai/docs/permissions) | `allow` / `ask` / `deny` per tool, or per pattern under a tool | ✅ |
| [Last match wins](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/permission/index.ts) | `findLast` over the ruleset; every pattern of a call must be allowed to run unasked | ✅ the engine's core rule |
| [Layering](https://opencode.ai/docs/permissions) | builtin defaults < agent's rules < config rules < stored answers | ✅ |
| [Bash patterns](https://opencode.ai/docs/permissions) | wildcards over command text; "always" answers remember the *kind* via the arity table | ✅ |
| [The edit group](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/permission/index.ts) | `edit` governs `edit`, `write` **and** `apply_patch` | ✅ named in the table |
| [`~/` and `$HOME` expansion](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/permission/index.ts) | in path patterns | ✅ |
| [Per-agent overrides](https://opencode.ai/docs/permissions) | agent rules append after global ones | ✅ |
| Subagent inheritance | | ✅ with a documented divergence: a subagent inherits refusals and never allows |
| [`OPENCODE_PERMISSION`](https://opencode.ai/docs/permissions) | inline JSON ruleset from the environment | ❌ |

## 7. Hooks and automation

*New in this revision (2026-08-12): the pin has no hook system, and the
shared outline's slot records that inversion — here ganja carries what
upstream lacks.*

| Feature | Notes | ganja |
|---|---|---|
| Command hooks at lifecycle moments | none at the pin (v1.18.16 likewise); the JS plugin runtime (§17) is upstream's extension seam | ✅ ganja-side addition (D456): the `hooks` config key runs commands at nine Claude-shaped events, PreToolUse/UserPromptSubmit blocking, regex matchers |
| [Plugin hook points](https://opencode.ai/docs/plugins) | `tool.execute.before/after`, `permission.ask`, `chat.message`, event bus — JS functions, not commands | ❌ the JS runtime is out of scope; ganja's command hooks cover the tool-bracketing and permission-wait moments differently |
| Turn-end notification | desktop notifications (§2) | ⚠️ a `Stop`/`Notification` hook can run any notifier command; no built-in desktop notification |

## 8. Rules and instructions

| Feature | Notes | ganja |
|---|---|---|
| [Global `AGENTS.md`](https://opencode.ai/docs/rules) | in the config directory | ✅ ganja's config home |
| [`~/.claude/CLAUDE.md` fallback](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/session/instruction.ts) | Claude Code compatibility, off via `OPENCODE_DISABLE_CLAUDE_CODE_PROMPT` | ✅ read as the global fallback; the disable knob ❌ |
| [Project walk](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/session/instruction.ts) | `AGENTS.md` → `CLAUDE.md` → `CONTEXT.md` (deprecated), first match per level, ancestors not stacked | ✅ |
| [`instructions` config](https://opencode.ai/docs/rules) | extra files and globs appended | ✅ |

## 9. Agents

| Feature | Notes | ganja |
|---|---|---|
| [Builtins](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/agent/agent.ts) | `build`, `plan`, `general`, `explore` | ✅ all four, explore's prompt verbatim |
| [Hidden internal agents](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/agent/agent.ts) | `compaction`, `title`, `summary` modeled as agents | ❌ ganja does those jobs outside the agent roster |
| [Markdown agent files](https://opencode.ai/docs/agents) | `~/.config/opencode/agent/*.md` + `.opencode/agent/*.md`, frontmatter + body-as-prompt | ✅ ganja's two homes (D482): `<config home>/agents/*.md` + `.ganja/agents/*.md`, frontmatter + body-as-prompt; `tools:` compiles to permission rules — an unlisted tool is refused, never hidden |
| [Config fields](https://opencode.ai/docs/agents) | `description`, `mode` (`primary`/`subagent`/`all`), `hidden`, `disable`, `model`, `prompt`, `permission` | ✅ all seven |
| [Sampling fields](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/agent/agent.ts) | `temperature`, `top_p`, `color`, `steps` | ❌ |
| Unknown fields inside an agent | carried, not refused (upstream tolerates) | ✅ same posture |
| Tab cycle / `<leader>a` list | | ⚠️ Tab ✅, list via `/agents` |

## 10. MCP and LSP

| Feature | Notes | ganja |
|---|---|---|
| [Local MCP servers](https://opencode.ai/docs/mcp-servers) | `command[]`, `environment`, `enabled` | ✅ |
| [Remote MCP servers](https://opencode.ai/docs/mcp-servers) | `url`, `headers`, `enabled` | ✅ |
| `<mcp_instructions>`, tools/list_changed | | ✅ |
| [MCP prompts / resources](https://opencode.ai/docs/mcp-servers) | prompts become commands, resources listable | ❌ |
| MCP reconnection / dynamic enable-disable | | ⚠️ reconnection ✅ (P13, no upstream counterpart — manual `/mcp` Reconnect for a `Failed` server + a bounded once-per-session automatic retry, D463); dynamic enable/disable at runtime still ❌, `enabled` is config-file only |
| [LSP builtin breadth](https://opencode.ai/docs/lsp) | typescript, pyright, gopls, rust-analyzer, clangd, zls, elixir-ls, … | ⚠️ `rust` and `gopls` only |
| [LSP auto-install](https://opencode.ai/docs/lsp) | downloads servers (`OPENCODE_DISABLE_LSP_DOWNLOAD` opts out) | ❌ never installs |
| [Custom LSP servers](https://opencode.ai/docs/lsp) | `command`, `extensions`, `env`, `initialization`, per-entry `disabled` | ✅ |
| Diagnostics on edit/write | push + pull, errors only, appended at one seam | ✅ |
| Remaining diagnostics pulls, [`lsp` tool](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/lsp.ts) | | ❌ |

## 11. Auth and providers

| Feature | Notes | ganja |
|---|---|---|
| [models.dev provider catalog](https://opencode.ai/docs/providers) | 75+ providers resolved by id | ❌ nine built-ins + two compat dialects |
| [OpenCode Zen](https://opencode.ai/docs/zen) | hosted gateway, `opencode/` model prefix, one key, rotating free models | ✅ `opencode` and `opencode-go` (D488): one OPENCODE_API_KEY, per-model dialect dispatch off the catalog npm hint across chat/Responses/Messages wires (x-api-key where the gateway demands it); google rows refused by name, the vendor's own client's answer |
| [npm `@ai-sdk/*` provider loaders](https://opencode.ai/docs/providers) | any Vercel-AI-SDK package as a provider | ❌ ganja's `compat` speaks two fixed dialects instead |
| [Provider options](https://opencode.ai/docs/providers) | `baseURL`, `apiKey`, `headers` | ✅ as `base_url`, `key_env`, `headers` |
| [Per-model catalog overrides](https://opencode.ai/docs/providers) | `models.<id>.name` / `limit.context` / `limit.output` | ❌ a config provider stays uncataloged |
| [Per-model options](https://opencode.ai/docs/models) | `reasoningEffort`, `textVerbosity`, `thinking.budgetTokens` passthrough | ⚠️ effort roster synthesizes the reasoning options (incl. the budget arithmetic); `textVerbosity` and raw passthrough ❌ |
| [Variants](https://opencode.ai/docs/models) | catalog-declared + hardcoded per provider; `--variant`, `variant_cycle` ctrl+t | ⚠️ surfaced as `/effort` with the same synthesized roster; no CLI flag, no cycle key |
| [`small_model`](https://opencode.ai/docs/config) | the title request (upstream's `getSmallModel` is read by `ensureTitle` and by nothing else; summaries run on the turn's own model) | ✅ bound to the provider its prefix names |
| Anthropic subscription OAuth (Pro/Max) | upstream removed it to comply with Anthropic's terms; community plugins exist at users' own risk | n/a — ganja never carried it, and no spec existed at the pin |
| xAI device-code login | single-flow since [v1.18.14](https://github.com/anomalyco/opencode/releases/tag/v1.18.14) | ✅ ganja's grok login is already a device flow |
| MCP OAuth | remote MCP auth | ✅ P13 addition, no upstream counterpart — the v1.18.13 checkout still refuses the `oauth` key by name: RFC 8414 discovery + RFC 7591 registration + PKCE/loopback + refresh-then-redial on 401, stored under a reserved `mcp:<server>` key (D466) |
| [`providers login/list/logout`](https://opencode.ai/docs/providers) | unified credential UI | ⚠️ `auth` covers ganja's providers only |
| anthropic / openai (both credentials) / openrouter / opencode + opencode-go / grok / copilot / cursor / fake + compat | ganja's roster; cursor is ganja-original, openrouter rides the Responses machinery with the vendor's documented reasoning/effort, tool_choice and opt-in server tools (D489), and the two zen ids share one key with per-model wire dispatch (D488) | ✅ incl. OAuth logins and credential-travel bounds |

## 12. Configuration surface

Mechanics first, then the top-level keys of `opencode.json(c)`.

| Mechanism | Notes | ganja |
|---|---|---|
| [Locations and precedence](https://opencode.ai/docs/config) | remote `.well-known/opencode` < global `~/.config/opencode/opencode.json(c)` < `OPENCODE_CONFIG` < project root and `.opencode/` (walking up to the git root) < `OPENCODE_CONFIG_CONTENT` < managed/enterprise | ⚠️ three tiers only: global home < `GANJA_CONFIG` < project files — no remote, inline or managed tiers |
| [`$schema`](https://opencode.ai/docs/config) | editor completion | ✅ accepted (and ignored) |
| [`{env:VAR}` / `{file:path}` substitution](https://opencode.ai/docs/config) | dynamic values in any string | ❌ deliberate divergence, documented in `config.rs` — `key_env` names the variable instead |
| JSONC dialect | comments, trailing commas | ✅ decoded in document order |
| Unknown top-level keys | the pin **fails** config parsing; **v1.18.16 ignores them** ([release](https://github.com/anomalyco/opencode/releases/tag/v1.18.16)) | ✅ ganja keeps the pin's posture: refused by name, deliberately |
| [`tui.json`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/config/index.tsx) (`OPENCODE_TUI_CONFIG`) | separate TUI config: `theme`, `keybinds`, `leader_timeout`, `scroll_speed`, `scroll_acceleration`, `diff_style`, `mouse`, `attention` | ❌ no second file — ganja's `theme`/`keybinds` live in `ganja.jsonc`; the scroll/diff/mouse knobs are absent |

| Top-level key | Notes | ganja |
|---|---|---|
| [`model`](https://opencode.ai/docs/config) | `provider/model` default | ✅ |
| [`small_model`](https://opencode.ai/docs/config) | cheap model for the title request | ✅ |
| [`username`](https://opencode.ai/docs/config) | display name | ❌ |
| [`autoupdate`](https://opencode.ai/docs/config) | `true` / `false` / `"notify"` | ❌ no self-updater at all |
| [`share`](https://opencode.ai/docs/share) | `manual` / `auto` / `disabled` | ❌ no share subsystem |
| [`disabled_providers`](https://opencode.ai/docs/config) | hide providers globally | ❌ |
| [`instructions`](https://opencode.ai/docs/rules) | extra rule files, globs allowed | ✅ (no `{file:}` inside, per the substitution row) |
| [`permission`](https://opencode.ai/docs/permissions) | §6 | ✅ |
| [`provider`](https://opencode.ai/docs/providers) | §11 | ✅ dialect-based, not npm-based |
| [`agent`](https://opencode.ai/docs/agents) | §9 | ✅ config table + markdown file tier (D482) |
| [`command`](https://opencode.ai/docs/commands) | §4 | ✅ config table + markdown file tier (D481) |
| [`mcp`](https://opencode.ai/docs/mcp-servers) | §10 | ✅ |
| [`formatter`](https://opencode.ai/docs/formatters) | §17 | ❌ |
| [`lsp`](https://opencode.ai/docs/lsp) | §10 | ✅ |
| [`plugin`](https://opencode.ai/docs/plugins) | npm specs + local `{plugin,plugins}/*.{ts,js}` | ❌ out of scope |
| [`snapshot`](https://opencode.ai/docs/config) | toggle undo/redo snapshots | ✅ |
| [`watcher.ignore`](https://opencode.ai/docs/config) | file-watcher exclusions | ❌ watcher not configurable |
| `layout` | deprecated at the pin | n/a |
| `enterprise` / `experimental` | managed policy, feature flags | ❌ |

## 13. Sessions and storage

*New in this revision (2026-08-12), assembled from the storage row that
lived under subsystems plus researched detail.*

| Feature | Notes | ganja |
|---|---|---|
| [Storage layout](https://opencode.ai/docs/config) | XDG data dir: `auth.json`, `log/`, `project/<slug>/storage/` with per-session and per-message JSON files (`global/` outside a git repo) | ⚠️ `auth.json` ✅ same idea; sessions are one SQLite database per project, converted from the file tree on first open |
| Resume | `--continue` / `--session <id>` | ✅ the same two flags, mutually exclusive |
| Session forking (`--fork`) | branch a conversation while continuing | ❌ |
| [Snapshots](https://opencode.ai/docs/config) | git-tree objects per step, no commits; `snapshot: false` opts out | ✅ ganja's snapshot store + the same config key |
| [`/undo` / `/redo`](https://opencode.ai/docs/config) | conversation + files restored; shell side-effects stay | ✅ same semantics and caveat |
| Log retention | timestamped logs under `log/`, ten newest kept, `--log-level` | ⚠️ ganja's own shape: daily files named by the **local** date under the data home's `log/`, seven kept and the oldest pruned on roll; `RUST_LOG` and `-v` stand in for `--log-level` |
| Managed binaries (`bin/`) | self-installed helpers land beside the data | ❌ ganja installs nothing |

## 14. CLI subcommands

| Command | Notes | ganja |
|---|---|---|
| [`export`](https://opencode.ai/docs/cli) | session → JSON, `--sanitize` | ❌ |
| [`import`](https://opencode.ai/docs/cli) | session ← file or share URL | ❌ (`config import-opencode` is config only) |
| [`stats`](https://opencode.ai/docs/cli) | `--days/--models/--tools` analytics | ❌ |
| [`github install` / `github run`](https://opencode.ai/docs/github) | Actions workflow + `/oc` mentions | ❌ |
| [`pr <number>`](https://opencode.ai/docs/cli) | checkout a PR and run | ❌ |
| [`acp`](https://opencode.ai/docs/cli) | Agent Client Protocol server for IDEs | ❌ |
| [`agent create`](https://opencode.ai/docs/agents) | interactive agent scaffolding | ❌ |
| [`upgrade` / `uninstall`](https://opencode.ai/docs/cli) | self-update, self-removal | ❌ |
| [`attach <url>`](https://opencode.ai/docs/cli) | TUI onto a running server | ⚠️ headless `run --attach` only |
| [`web`](https://opencode.ai/docs/cli) | web UI | ❌ |
| [`account` / `db` / `plug` / `generate` / `debug/*`](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/cli/cmd) | account, database, plugin and debug utilities | ❌ |
| [`run --fork`](https://opencode.ai/docs/cli) | fork while continuing | ❌ |
| [`run -f <file>`](https://opencode.ai/docs/cli) | attach files/images from the CLI | ❌ (in-prompt `@` is ✅) |
| [`run --command <name>`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/cli/cmd/run.ts) | run a slash command headless | ❌ |
| [`run --share` / `--title`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/cli/cmd/run.ts) | share the session, name it | ❌ |
| [`run --variant <v>`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/cli/cmd/run.ts) | reasoning-effort variant from the CLI | ❌ (`/effort` in the TUI ✅) |
| [`serve --cors` / `--mdns`](https://opencode.ai/docs/server) | CORS origins, mDNS discovery | ❌ (serve itself ✅) |
| [`run` / `serve` / `auth` / `models` / `sessions` / `mcp`](https://opencode.ai/docs/cli) | the working set | ✅ incl. nd-JSON output, `--continue`/`--session`, Basic-auth serve |

## 15. Server surface and SDK

| Route / behavior | Notes | ganja |
|---|---|---|
| [question reply routes](https://opencode.ai/docs/server) | answer a question over HTTP | ❌ recorded follow-up (`/question` + reply) |
| [file/find routes](https://opencode.ai/docs/server) | file read, text/symbol search | ❌ |
| [`/api/provider`, `/api/integration`, `/api/credential`](https://opencode.ai/docs/server) | provider/integration/credential APIs | ❌ |
| [`/api/mcp` family](https://opencode.ai/docs/server) | server-side MCP management + resources | ❌ |
| [`/tui` bridge](https://opencode.ai/docs/server) | TUI control channel | ❌ |
| [OpenAPI 3.1 spec at `/doc`](https://opencode.ai/docs/server) | live Swagger; `@opencode-ai/sdk` is generated from it | ❌ |
| [`/api/generate`](https://opencode.ai/docs/server) | one-shot generation | ❌ |
| WebSocket / mDNS / multi-directory routing | | ❌ single launch directory is a pinned divergence |
| [share routes](https://opencode.ai/docs/share) | publish/revoke | ❌ |
| [`@opencode-ai/sdk`](https://opencode.ai/docs/sdk) | client SDK generated from the server's OpenAPI spec | ❌ (`ganja-client` is hand-written against ganja-serve) |
| legacy `/session` REST + `/event` SSE + `/permission` | | ✅ with refuse-non-loopback-without-password posture |

## 16. Environment variables

The pin exposes ~70 `OPENCODE_*` variables; the ones that shape behavior:

| Variable | Meaning | ganja |
|---|---|---|
| `OPENCODE_CONFIG` | extra config file | ✅ `GANJA_CONFIG` |
| `OPENCODE_CONFIG_DIR` | the config home | ✅ `GANJA_CONFIG_HOME` (one home, not a merge) |
| `OPENCODE_CONFIG_CONTENT` | inline JSON config | ❌ |
| `OPENCODE_TUI_CONFIG` | alternate `tui.json` | ❌ no second config file |
| `OPENCODE_PERMISSION` | inline permission ruleset | ❌ |
| `OPENCODE_DISABLE_AUTOUPDATE` / `OPENCODE_ALWAYS_NOTIFY_UPDATE` | updater knobs | n/a — no self-updater |
| `OPENCODE_DISABLE_AUTOCOMPACT` | turn auto-compaction off | ❌ |
| `OPENCODE_DISABLE_PROJECT_CONFIG` | ignore project config files | ❌ |
| `OPENCODE_DISABLE_CLAUDE_CODE(_PROMPT/_SKILLS)` | stop reading Claude Code's files | ⚠️ ganja reads the `~/.claude/CLAUDE.md` fallback unconditionally; skills are never foreign |
| `OPENCODE_DISABLE_EXTERNAL_SKILLS` | skip `.claude`/`.agents` skill dirs | n/a — never discovered |
| `OPENCODE_SERVER_PASSWORD` / `_USERNAME` | serve Basic auth | ✅ `GANJA_SERVER_PASSWORD` / `_USERNAME` |
| `OPENCODE_WEBSEARCH_PROVIDER` | pick Exa or Parallel | ✅ `GANJA_WEBSEARCH_PROVIDER` |
| `OPENCODE_ENABLE_EXA` / `_PARALLEL` | switch search backends on | ⚠️ ganja keys off `EXA_API_KEY`/`PARALLEL_API_KEY` presence instead |
| `OPENCODE_AUTO_SHARE` / `OPENCODE_DISABLE_SHARE` | share behavior | ❌ no share |
| `OPENCODE_LOG_LEVEL` / `OPENCODE_PRINT_LOGS` | logging | ⚠️ `RUST_LOG` and `-v` instead; nothing prints to the terminal, by design |
| `OPENCODE_AUTH_CONTENT` | inline credentials | ❌ |
| `OPENCODE_DISABLE_LSP_DOWNLOAD` | keep LSP servers uninstalled | n/a — ganja never installs |
| `OPENCODE_DISABLE_PRUNE` | keep stale spill/truncation files | ❌ |
| `OPENCODE_GIT_BASH_PATH` | windows shell | ❌ |
| `OPENCODE_EXPERIMENTAL_*` (~15 flags) | background subagents, code mode, plan mode, websockets, workspaces, lsp tool, … | ❌ collectively |

ganja's own additions (`GANJA_MODEL`, `GANJA_FAKE_SCRIPT`, `GANJA_MODELS_URL`/`_PATH`,
`GANJA_DISABLE_MODELS_FETCH`, `GANJA_AUTH_ISSUER`, `GANJA_OPENCODE_DIR`,
`GANJA_LIVE_TEST`) have no upstream counterpart and are documented in the
repository root's `AGENTS.md`.

## 17. Subsystems and sibling products

| Subsystem | Notes | ganja |
|---|---|---|
| [Plugins](https://opencode.ai/docs/plugins) | JS runtime; npm specs + local `{plugin,plugins}/*.{ts,js}`; hooks (`tool.execute.before/after`, `permission.ask`, `chat.message`, event bus); `@opencode-ai/plugin` types, ctx carries an SDK client and Bun's `$` | ❌ the JS plugin runtime itself is out of scope; ganja's own `hooks` config key (P13, §7) is a **different, Claude-shaped mechanism** — nine named moments, `sh -c` command handlers, no JS runtime, no event bus — not a port of this row (D456) |
| [Share](https://opencode.ai/docs/share) | `opencode.ai/s/<id>` publishing, `/share` `/unshare`, `manual`/`auto`/`disabled` | ❌ out of scope |
| [Formatters](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/format/formatter.ts) | 26+ built-ins (gofmt, prettier, biome, ruff, rustfmt, shfmt, terraform, clang-format, …) run after edits; per-formatter disable or custom `command`+`extensions`+`environment` | ❌ |
| [Background agents](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/background) | async dispatch, summaries, notifications | ❌ |
| [Worktrees](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/worktree) | per-agent git worktree isolation | ❌ |
| [Image pipeline](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/image) | image attachments | ✅ `@`-mention attach, plus Ctrl+V clipboard PNG intake encoded in-process (D449) |
| Account / sync / control-plane | cloud account machinery | ❌ out of scope |
| Installation (self-update) | | ❌ |
| [IDE / ACP](https://opencode.ai/docs/ide) | editor extensions, sidebar chat | ❌ out of scope |
| codemode | `execute` runtime | ❌ |
| desktop / web / console / slack / enterprise / identity / containers / session-ui | sibling products — Desktop is where most 1.18.15/16 release work landed | ❌ out of scope |

## 18. What moved after the pin — v1.18.14 → v1.18.16

The whole delta, from the release notes; the pin is unchanged and none of
this is charter work.

| Change | Release | ganja |
|---|---|---|
| xAI login collapsed to a single device-code flow | [v1.18.14](https://github.com/anomalyco/opencode/releases/tag/v1.18.14) | ✅ already ganja's shape — the two converged |
| Structured mid-stream provider errors preserved so compatible providers retry | v1.18.14 | ❌ divergence created by time: ganja retries only before the first byte, which is the pin's rule |
| More transient provider/network errors retried | v1.18.14 | ⚠️ same pre-first-byte posture |
| ACP usage totals count cache writes; queued ACP updates awaited | v1.18.14 | n/a — no ACP |
| Remote-workspace fixes (host `directory` not forwarded, 5xx bodies logged) | v1.18.14 | n/a — out of scope |
| Chronological message ordering for imported/legacy ids; revert/fork on real chronology | [v1.18.15](https://github.com/anomalyco/opencode/releases/tag/v1.18.15) | ✅ moot by construction — ganja's ids sort in creation order |
| Truncation cleanup removes stale files by timestamp | v1.18.15 | ⚠️ ganja's spill discipline is its own |
| Repeated compaction keeps earlier tool-call history in summaries | v1.18.15 | ❌ ganja compacts to the pin's behavior |
| Copy over ssh with tmux `set-clipboard on` | v1.18.15 | ✅ already covered — OSC 52 is queued unconditionally before the system clipboard |
| Cursor style configuration (`tui.json`) | v1.18.15 | ❌ |
| Unknown top-level config fields ignored instead of failing | [v1.18.16](https://github.com/anomalyco/opencode/releases/tag/v1.18.16) | ❌ **deliberately** — ganja keeps the pin's refuse-by-name |
| Projects registered from Home; Desktop locale/menu/macOS-lifecycle work | v1.18.15–16 | n/a — sibling product |

Earlier revisions of this document carried a speculative post-pin table
(v2 beta, FFF, queue-vs-steer). Those rows described work beyond the 1.18
line or already-pinned features and are retired; Desktop lives in §17.
