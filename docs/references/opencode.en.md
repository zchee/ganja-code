# opencode feature reference (compared against ganja)

> [!IMPORTANT]
> **This document is a reference inventory, not a roadmap. Not every feature
> listed here will be ported.** ganja's charter is behavioral parity with
> opencode **v1.18.13** — the pin, not the moving tip. A ❌ is an observation,
> never a promise, and the post-pin table at the end is out of charter
> entirely until a deliberate re-pin.

Snapshot: 2026-08-11. Source-level rows link to the pinned tag
(`anomalyco/opencode@v1.18.13`); documented features link to opencode.ai.
Legend: ✅ present in ganja (parity or a near equivalent) · ⚠️ partial · ❌ absent.

## 1. Tools

| Tool | Notes | ganja |
|---|---|---|
| [`plan_enter`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/plan.ts) | enter plan mode | ❌ a name with nothing behind it |
| [`plan_exit`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/plan.ts) | hand the wheel to build | ✅ |
| [`lsp`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/lsp.ts) | hover/symbols exposed to the model | ❌ deviation `lsp-tool-unported` |
| [`apply_patch`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/apply_patch.ts) | OpenAI-model-gated patch editing | ❌ named in the permission table only |
| [`execute` (code-mode)](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/codemode) | script over MCP tools | ❌ whole package out of scope |
| [`doom_loop`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool) | experimental | ❌ |
| [read / edit / write / glob / grep / bash / todowrite / webfetch / websearch / skill / question / task](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/tool) | the working set | ✅ incl. anchored writes, read-before-write, permission gating |

## 2. CLI subcommands

| Command | Notes | ganja |
|---|---|---|
| [`export`](https://opencode.ai/docs/cli) | session → JSON, `--sanitize` | ❌ |
| [`import`](https://opencode.ai/docs/cli) | session ← file or share URL | ❌ (`config import-opencode` is config only) |
| [`stats`](https://opencode.ai/docs/cli) | `--days/--models/--tools` analytics | ❌ |
| [`github install` / `github run`](https://opencode.ai/docs/github) | Actions workflow + `/oc` mentions | ❌ |
| [`pr <number>`](https://opencode.ai/docs/cli) | checkout a PR and run | ❌ |
| [`acp`](https://opencode.ai/docs/cli) | Agent Client Protocol server for IDEs | ❌ |
| [`agent create`](https://opencode.ai/docs/agents) | interactive agent scaffolding | ❌ |
| [`upgrade`](https://opencode.ai/docs/cli) | self-update | ❌ |
| [`attach <url>`](https://opencode.ai/docs/cli) | TUI onto a running server | ⚠️ headless `run --attach` only |
| [`web`](https://opencode.ai/docs/cli) | web UI | ❌ |
| [`account` / `db` / `debug/*`](https://opencode.ai/docs/cli) | account, database, debug utilities | ❌ |
| [`run --fork`](https://opencode.ai/docs/cli) | fork while continuing | ❌ |
| [`run -f <file>`](https://opencode.ai/docs/cli) | attach files/images from the CLI | ❌ (in-prompt `@` is ✅) |
| [`serve --cors` / `--mdns`](https://opencode.ai/docs/server) | CORS origins, mDNS discovery | ❌ (serve itself ✅) |
| [`run` / `serve` / `auth` / `models` / `sessions` / `mcp`](https://opencode.ai/docs/cli) | the working set | ✅ incl. nd-JSON output, `--continue`/`--session`, Basic-auth serve |

## 3. Server surface

| Route / behavior | Notes | ganja |
|---|---|---|
| [question reply routes](https://opencode.ai/docs/server) | answer a question over HTTP | ❌ recorded follow-up (`/question` + reply) |
| [file/find routes](https://opencode.ai/docs/server) | file read, text/symbol search | ❌ |
| [`/api/provider`, `/api/integration`, `/api/credential`](https://opencode.ai/docs/server) | provider/integration/credential APIs | ❌ |
| [`/api/mcp` family](https://opencode.ai/docs/server) | server-side MCP management + resources | ❌ |
| [`/tui` bridge](https://opencode.ai/docs/server) | TUI control channel | ❌ |
| [OpenAPI spec at `/doc`](https://opencode.ai/docs/server) | live Swagger | ❌ |
| [`/api/generate`](https://opencode.ai/docs/server) | one-shot generation | ❌ |
| WebSocket / mDNS / multi-directory routing | | ❌ single launch directory is a pinned divergence |
| [share routes](https://opencode.ai/docs/share) | publish/revoke | ❌ |
| legacy `/session` REST + `/event` SSE + `/permission` | | ✅ with refuse-non-loopback-without-password posture |

## 4. Subsystems (at the pin)

| Subsystem | Notes | ganja |
|---|---|---|
| [Plugins](https://opencode.ai/docs/plugins) | JS runtime, npm + local, lifecycle hooks | ❌ out of scope |
| [Share](https://opencode.ai/docs/share) | `opencode.ai/s/<id>` publishing, `/unshare` | ❌ out of scope |
| [Formatters](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/format) | post-edit auto-formatting per language | ❌ |
| [Background agents](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/background) | async dispatch, summaries, notifications | ❌ |
| [Worktrees](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/worktree) | per-agent git worktree isolation | ❌ |
| [Image pipeline](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/image) | image attachments | ⚠️ `@`-mention attach ✅; clipboard intake ❌ |
| Account / sync / control-plane | cloud account machinery | ❌ out of scope |
| Installation (self-update) | | ❌ |
| [IDE / ACP](https://opencode.ai/docs/ide) | editor extensions, sidebar chat | ❌ out of scope |
| codemode | `execute` runtime | ❌ |
| desktop / web / console / slack / enterprise / identity / containers / session-ui | sibling products | ❌ out of scope |

## 5. Auth and providers

| Feature | Notes | ganja |
|---|---|---|
| Anthropic subscription OAuth (Pro/Max) | | ❌ **dropped** — no spec existed at the pin |
| [models.dev provider catalog](https://opencode.ai/docs/providers) | 75+ providers | ❌ six built-ins + two compat dialects |
| MCP OAuth | remote MCP auth | ❌ config key refused loudly |
| [`providers login/list/logout`](https://opencode.ai/docs/providers) | unified credential UI | ⚠️ `auth` covers ganja's providers only |
| anthropic / openai (both credentials) / grok / copilot / cursor / fake + compat | | ✅ incl. OAuth logins and credential-travel bounds |

## 6. MCP and LSP partials

| Feature | ganja |
|---|---|
| [MCP prompts / resources](https://opencode.ai/docs/mcp-servers) | ❌ |
| MCP reconnection | ❌ dialled once |
| MCP dynamic enable/disable | ❌ |
| [`lsp` tool for the model](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/lsp.ts) | ❌ |
| [LSP server auto-install](https://opencode.ai/docs/lsp) | ❌ never installs |
| Built-in LSP breadth (pyright, tsserver, …) | ⚠️ `rust` and `gopls` only |
| Remaining diagnostics pulls | ❌ |
| MCP stdio + remote HTTP, `<mcp_instructions>`, tools/list_changed; LSP push+pull diagnostics on edits | ✅ |

## 7. TUI — larger surfaces

| Surface | Notes | ganja |
|---|---|---|
| [Session rename / tag / move / export dialogs](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component) | | ❌ |
| [Timeline + fork-from-timeline](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/dialog-timeline.tsx) | `<leader>g` | ❌ |
| [Message inspect dialog](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/dialog-message.tsx) | | ❌ |
| [Workspace UI](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component) | create/list/file-changes/destination | ❌ out of scope by design |
| [Sidebar](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/feature-plugins/sidebar) | context/files/lsp/mcp/todo panes | ❌ |
| [Diff viewer](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component/diff-viewer) | file tree, split/unified, hunk nav | ❌ inline unified diffs only |
| [Subagent transcript viewer](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/dialog-subagent.tsx) | | ❌ progress metadata only |
| [Provider / MCP / skill / status / debug pickers](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component) | | ❌ (`/effort` picker ✅) |
| Delete-failed / retry recovery dialogs | | ❌ |
| [Desktop notifications](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/notifications.ts) | | ❌ |
| Toast overlay | | ⚠️ adapted to status-bar notices, texts verbatim |
| Logo / startup animations / tips | | ❌ |
| TUI plugin runtime | | ❌ |
| Chat + streaming, permission dialog (`a`/`A`/`d` semantics), question dialog incl. free-text, palette + menus, themes, markdown, `/undo` markers | | ✅ |

## 8. TUI — the full keybind registry

Ported and rebindable (6): [`app_exit`, `command_list`, `session_list`, `theme_list`, `agent_cycle`, `input_newline`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/config/keybind.ts).
Everything below is from the same registry. *Tab completion does not exist in
the upstream composer — Tab is `agent_cycle` (ported); both completion menus
are filter-plus-Enter (ported).*

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
| `session_move` / `session_timeline` / `session_fork` / `session_rename` / `session_delete` / `session_share` / `session_unshare` | ctrl+r, ctrl+d, … | ❌ |
| `session_new` / `session_compact` / `session_interrupt` | `<leader>n` / `<leader>c` / escape | ⚠️ `/new`, `/compact`, Esc-cancel ✅; keys not rebindable |
| `session_background` | ctrl+b | ❌ |
| `session_toggle_timestamps` / `_generic_tool_output` | none | ❌ |
| `session_queued_prompts` | `<leader>q` | ❌ |
| `session_child_first/child_cycle/child_cycle_reverse/parent` | `<leader>down`, right, left, up | ❌ |
| `session_pin_toggle` / `session_quick_switch_1..9` | ctrl+f / `<leader>1-9` | ❌ |
| `stash_delete` | ctrl+d | ❌ |
| `model_provider_list` / `model_favorite_toggle` / `model_cycle_recent(_reverse)` / `model_cycle_favorite(_reverse)` | ctrl+a, ctrl+f, f2 | ❌ (`/models` list ✅) |
| `mcp_list` / `provider_connect` / `console_org_switch` | none | ❌ |
| `agent_list` / `agent_cycle_reverse` | `<leader>a` / shift+tab | ⚠️ `/agents` ✅ / ❌ |
| `variant_cycle` / `variant_list` | ctrl+t / none | ❌ cycle (`/effort` list ✅, catalog-synthesized roster) |
| `messages_page_up/…/half_page_down` (6) | pageup, … | ⚠️ scrolling ✅, not rebindable |
| `messages_first/last/next/previous/last_user` | ctrl+g, home, … | ❌ message-level navigation |
| `messages_copy` / `messages_undo` / `messages_redo` / `messages_toggle_conceal` | `<leader>y/u/r/h` | ⚠️ `/copy-message`, `/undo`, `/redo` ✅; keys + conceal ❌ |
| `tool_details` / `display_thinking` | none | ❌ |
| `prompt_submit` / `prompt_editor_context_clear` / `prompt_skills` / `prompt_stash(_pop/_list)` / `workspace_set` | none | ❌ |
| `input_clear` / `input_paste` | ctrl+c / ctrl+v | ❌ / ⚠️ bracketed paste only |
| `input_submit` / `input_move_*` / `input_backspace` / `input_delete` | return, arrows, … | ⚠️ behaviors ✅, not rebindable (Up/Down feed history ✅) |
| `input_select_*` (left/right/up/down/line/buffer/visual-line, 10 actions) | shift+… | ❌ no selection machinery |
| `input_line_home/end` / `input_visual_line_home/end` / `input_buffer_home/end` | ctrl+a/e, alt+a/e, home/end | ⚠️ partial built-ins; visual-line ❌ |
| `input_delete_line` / `input_delete_to_line_end` / `input_delete_to_line_start` | ctrl+shift+d, ctrl+k, ctrl+u | ⚠️ k/u built-in, not rebindable |
| `input_undo` / `input_redo` / `input_word_*` | ctrl+-, ctrl+., alt+f/b … | ⚠️ built-ins only |

## 9. Prompt modules

| Module | Notes | ganja |
|---|---|---|
| [`frecency.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/frecency.tsx) | frequency+recency completion ranking | ❌ |
| [`stash.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/stash.tsx) | draft stash | ❌ |
| [`move.tsx` / `workspace.tsx` / `cwd.ts`](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component/prompt) | session move, workspace, cwd context | ❌ |
| [`local-attachment.ts`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/local-attachment.ts) | mime attachments | ✅ |
| [`history.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/prompt/history.tsx) | prompt history | ✅ |
| [`autocomplete.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/autocomplete.tsx) | `@`/`/` completion + `#line-range` | ✅ |

## 10. Post-pin (after v1.18.13 — out of charter until a re-pin)

| Feature | Notes |
|---|---|
| [Queue vs Steer](https://opencode.ai/docs) | queued prompts plus mid-run course-correction injection |
| FFF search engine | in-process frecency-ranked search replacing rg spawns |
| [OpenCode Desktop](https://opencode.ai/download) | native GUI, worktree drawers, notifications |
| OpenCode v2 beta | architecture generation change |
| Worktree drawer UI / agent manager | parallel-agent worktree management |
| `opencode.jsonc` v1.0.210+ catalog format | unified variant declarations |
