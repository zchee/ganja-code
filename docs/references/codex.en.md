# OpenAI Codex CLI feature reference (compared against ganja)

> [!IMPORTANT]
> **This document is a reference inventory, not a roadmap. Not every feature
> listed here will be ported.** ganja's charter is behavioral parity with
> opencode v1.18.13; Codex CLI is a third product, catalogued here for
> comparison only. A ❌ is an observation, never a promise.

Snapshot: 2026-08-12, against Codex CLI's main branch (Codex has no pin in
this repo — rows drift as upstream moves). Rows marked *(low confidence)*
rest on community sources rather than official documentation. The ganja-side
cells were refreshed 2026-08-15 against the post-P22 tree; the Codex-side
survey is still the 2026-08-12 pass.

Sections follow the shared outline all three references use (claude, codex,
opencode), so the same topic sits at the same section number in each.

Legend: ✅ present in ganja (parity or a near equivalent) · ⚠️ partial · ❌ absent.

## 1. TUI — composer and input

| Feature | Keys | ganja |
|---|---|---|
| [`@` fuzzy file search with Tab-accept](https://developers.openai.com/codex/cli) | `@` + Tab | ✅ Tab accepts, byte-identical to Enter; directory-descent (`@dir`→`@dir/`) not built — ganja's walker is files-only |
| [Esc-Esc backtrack](https://developers.openai.com/codex/cli) | Esc Esc | ✅ idle Esc Esc enters the backtrack walk (D467): the newest user message highlights in the transcript, each further Esc steps one older, Enter reverts to before it **and re-populates the composer with that prompt for editing**; any other key exits without reverting, mid-turn Esc still cancels, and `/rewind` keeps the two-step scope picker |
| [Queued messages](https://developers.openai.com/codex/cli) | Enter while running | ✅ steers into the running turn at its next step boundary (`Command::Steer`) — the same shape as Codex's own `input_queue`/`inject`; what can't steer (refused, unconsumed, slash commands) falls back to a replayed queue, Codex's `queued_user_messages` half |
| [Clipboard image paste](https://developers.openai.com/codex/cli) | Ctrl+V | ✅ PNG-encoded in-process (no OS shell-out) and attached through the `@`-mention pipeline |
| [Slash-command autocomplete](https://developers.openai.com/codex/cli) | `/` | ✅ |
| [Reasoning-effort hotkeys](https://github.com/openai/codex/blob/main/docs/config.md) | Alt+, / Alt+. | ❌ (`/effort` list picker ✅) |
| Prompt history | Up / Down | ✅ |
| Multiline input | Shift+Enter … | ✅ |
| External editor | — | ✅ `/editor` (ganja-side advantage) |

## 2. TUI — larger surfaces and keybinds

*New as its own section in this revision (2026-08-12); the transcript and
status-line rows moved here from §1, the rest is researched.*

| Feature | Notes | ganja |
|---|---|---|
| [Transcript overlay](https://developers.openai.com/codex/cli) | Ctrl+T | ✅ same chord, three tabs (expanded transcript incl. full tool/MCP input+output, raw event log, per-turn token table); full-terminal takeover and the banner are this overlay's own presentation, Claude Code's Ctrl+O supplies the one-line footer wording — and since 2026-08-15 the paint is Codex's own monochrome (the theme's text color on the terminal's background, under every theme) with every tab opening pinned to its tail and following the stream |
| [Status-line composition](https://github.com/openai/codex/blob/main/docs/config.md) | `[tui] status_line = […]` | ✅ `tui.statusline` element roster (D469): user-ordered named elements, width-aware, rendered in the OMC HUD's shape (meters, git line, optional detail lines); the element vocabulary is ganja's own, not Codex's id list, and an unknown name is refused at load |
| [Onboarding flow](https://developers.openai.com/codex/cli) | first-run auth choice (ChatGPT OAuth / API key), config bootstrap | ❌ ganja boots into the fake provider with a status-bar notice; `auth login` is a separate CLI step |
| [Approval dialog](https://github.com/openai/codex/blob/main/docs/getting-started.md) | pending command/patch preview; approve, approve-for-session, deny with feedback | ⚠️ ganja's permission dialog (allow / always / deny) — "always" persists to the per-project store instead of dying with the session |
| [Native diff rendering](https://developers.openai.com/codex/cli) | `apply_patch` changes shown as colored unified diffs before applying | ⚠️ inline unified diffs per edit; no pre-apply preview step (permission dialog carries the call instead) |
| [Desktop/terminal notifications](https://github.com/openai/codex/blob/main/docs/config.md) | `[tui] notifications` (turn-complete, approval-requested), `notification_method` osc9/bel | ✅ `tui.notifications` as a bool or the same event filter, `notification_method` osc9/bel, focus-gated off the terminal's own focus events so a watched terminal never rings (D468) |
| Keybinding customization *(low confidence)* | limited remapping via config | ⚠️ ganja's `keybinds` map covers six actions, comma-separated alternates, empty unbinds |

## 3. Modes and execution

| Feature | Notes | ganja |
|---|---|---|
| [OS-kernel sandboxing](https://github.com/openai/codex/blob/main/docs/sandbox.md) | macOS Seatbelt; Linux Landlock + seccomp | ❌ permission engine only, no isolation |
| [Approval policies](https://github.com/openai/codex/blob/main/docs/getting-started.md) | read-only / workspace-write / full-access; on-request/untrusted/never | ⚠️ rule-based allow/ask/deny + single-tier `--auto` |
| [Write-mode network cutoff](https://github.com/openai/codex/blob/main/docs/sandbox.md) | `network_access = false` under workspace-write | ❌ no such concept |
| [Project trust levels](https://github.com/openai/codex/blob/main/docs/config.md) | `[projects."path"] trust_level`, prompt on untrusted dirs | ❌ |
| [`shell_environment_policy`](https://github.com/openai/codex/blob/main/docs/config.md) | inherit all/core/none + include/exclude patterns for subshell env | ❌ tools inherit the process env |
| [`--yolo` bypass](https://github.com/openai/codex/blob/main/docs/sandbox.md) | skip sandbox + approvals | ✅ both the interactive TUI and `run` carry `--auto` + hidden `--yolo`/`--dangerously-skip-permissions` (D479): Ask-raised dialogs answered "allow once", deny unchanged — still no sandbox to bypass |
| [Container posture](https://github.com/openai/codex/blob/main/docs/sandbox.md) | degraded-sandbox flags for Docker/devcontainers | ❌ |

## 4. Slash commands

| Command | Notes | ganja |
|---|---|---|
| [`/model`](https://developers.openai.com/codex/cli) | model **and** reasoning effort in one menu | ⚠️ `/model` ✅ + separate `/effort`; no combined menu |
| [`/review`](https://developers.openai.com/codex/cli) | presets: uncommitted / commit / base-branch diff, custom focus | ❌ |
| [`/diff`](https://developers.openai.com/codex/cli) | session-wide change viewer | ❌ (per-edit inline diffs ✅) |
| [`/compact`](https://developers.openai.com/codex/cli) | summarize the conversation | ✅ plus auto-compaction |
| [`/prompts` → Agent Skills](https://developers.openai.com/codex/cli) *(medium confidence)* | prompt templates deprecated toward SKILL.md | ⚠️ skills ✅ (SKILL.md-compatible); no template list UI |
| [`/status`](https://developers.openai.com/codex/cli) | model/tokens/context/cost dashboard | ⚠️ split across `/usage` (session totals, cache/reasoning splits, vendor rate windows, plan-limit meters) and `/context` (per-category grid) beside the status bar; no single dashboard command |
| [`/init`](https://developers.openai.com/codex/cli) | generate AGENTS.md | ✅ |
| [`/resume`](https://developers.openai.com/codex/cli) | in-TUI session picker | ✅ `/sessions` |
| [`/feedback`](https://developers.openai.com/codex/cli) | sanitized diagnostics report to OpenAI | ❌ (no telemetry channel at all) |
| `/new` / `/quit` | session control | ✅ equivalents |
| [`/mcp`](https://github.com/openai/codex/blob/main/docs/config.md) | MCP connection status | ✅ `/mcp` dialog (status, tool counts, Reconnect/Login actions) + `ganja mcp` CLI listing |
| `/login` / `/logout` | credential switching in-TUI | ⚠️ `auth` CLI only |

## 5. Built-in tools

| Feature | Notes | ganja |
|---|---|---|
| [`apply_patch`](https://github.com/openai/codex/blob/main/docs/getting-started.md) | structured unified-diff editing as the primary tool, intercepted at the harness (`unified_exec`) | ❌ ganja follows upstream's `edit`/`write`; the name exists in the permission table only |
| [`unified_exec`](https://developers.openai.com/codex/cli) *(low confidence)* | consolidated exec subsystem, byte-capped streaming output | ⚠️ ganja's shell has its own spill/truncation discipline |
| [`update_plan` (plan mode)](https://developers.openai.com/codex/cli) | live checklist rendering and updates | ⚠️ `todowrite` is the nearest; no plan-specific tool |
| [`web_search` tool](https://github.com/openai/codex/blob/main/docs/config.md) | live search opt-in | ✅ `websearch` (Exa/Parallel) |
| [`view_image` tool](https://github.com/openai/codex/blob/main/docs/config.md) | the model reads local images by path, self-directed | ❌ image context is user-attached only |
| Shell execution | | ✅ `bash` |
| Best-of-N *(low confidence)* | parallel candidate generation | ❌ |

## 6. Permissions

*New as its own section in this revision (2026-08-12), researched; the
mode-level posture lives in §3.*

| Feature | Notes | ganja |
|---|---|---|
| [Interactive approval choices](https://github.com/openai/codex/blob/main/docs/getting-started.md) | allow once / allow for this session / deny with feedback to the model | ⚠️ ganja: allow / always / deny — a denial becomes error text the model reads, same loop posture |
| [Session-scoped approval memory](https://github.com/openai/codex/blob/main/docs/getting-started.md) | "don't ask again" lives in memory and dies with the session | ⚠️ ganja's "always" answers persist per project (arity-aware for shell) — a deliberate, pinned difference |
| [Network-access escalation](https://github.com/openai/codex/blob/main/docs/sandbox.md) | a command needing the network under `network_access = false` raises its own approval | ❌ no network gating concept |
| [Untrusted-project config quarantine](https://github.com/openai/codex/blob/main/docs/config.md) | project `.codex/config.toml` is parsed only once the directory is trusted | ❌ ganja reads project config unconditionally; its curated key refusal is a different kind of guard |
| [Granular `approval_policy` table](https://github.com/openai/codex/blob/main/docs/config.md) *(low confidence)* | per-category prompt rules (sandbox, MCP elicitations, …) | ❌ |
| [`/permissions` in-TUI editor](https://developers.openai.com/codex/cli) *(low confidence)* | inspect/adjust the active policy | ❌ stored rules, no UI |

## 7. Hooks and automation

*New in this revision (2026-08-12), researched.*

| Feature | Notes | ganja |
|---|---|---|
| [`notify` hook](https://github.com/openai/codex/blob/main/docs/config.md) | external program invoked with a JSON payload on `agent-turn-complete` | ⚠️ ganja's `hooks` run commands at `Stop`/`Notification` with a JSON envelope on stdin — the same job, Claude's shape (D456) |
| [Lifecycle hook system](https://github.com/openai/codex/blob/main/docs/config.md) *(medium confidence — experimental)* | `[features] hooks = true` + `hooks.json`: Claude-Code-shaped events (PreToolUse blocking, PostToolUse, SessionStart/End, …) | ⚠️ ganja ships that shape as a stable config key: nine events, PreToolUse/UserPromptSubmit blocking, regex matchers |
| `PermissionRequest` / `PostCompact` events *(low confidence)* | extra Codex-only lifecycle points | ❌ nearest is ganja's `Notification` bracket around a permission wait |

## 8. Rules, custom commands and memory

| Feature | Notes | ganja |
|---|---|---|
| [AGENTS.md, project + global](https://agents.md) | `~/.codex/AGENTS.md` + repo root | ✅ ganja reads the family plus its global tier |
| [Nested AGENTS.md](https://agents.md) | per-subdirectory instruction files, scoped recursively | ✅ lazy walk-in (D480): the AGENTS.md family on a touched file's parent chain joins the next request, closest-last; a listing (glob/grep) is not a touch |
| [Custom prompts](https://developers.openai.com/codex/cli) | `~/.codex/prompts/*.md` + project scope, argument interpolation | ⚠️ config-declared commands with `$ARGUMENTS`/`!`/`@` expansion |

## 9. Agents and skills

*New in this revision (2026-08-12), researched; the multi-agent surface is
experimental upstream and moves fast.*

| Feature | Notes | ganja |
|---|---|---|
| [Multi-agent orchestration](https://developers.openai.com/codex/cli) *(medium confidence)* | `multi_agent` feature: parallel subagent threads (`[agents] max_threads`, `max_depth`), `/agent` inspector | ⚠️ ganja's `task` tool + parallel fan-out of consecutive calls (capped by `agents.concurrency`, default 4); no recursive depth, no thread inspector |
| [Agent definition files](https://developers.openai.com/codex/cli) *(low confidence)* | `~/.codex/agents/*.toml`, per-agent model/reasoning/sandbox | ⚠️ config-declared agents (model, prompt, permission rules); no per-agent sandbox |
| [Skills (SKILL.md)](https://developers.openai.com/codex/cli) | cross-tool standard, progressive disclosure | ✅ ganja's two homes + `skills.paths` |
| [Skill discovery paths](https://developers.openai.com/codex/cli) | `$CODEX_HOME/skills` + repo `.codex/skills` + `.agents/skills` | ⚠️ ganja scans its config home + `.ganja/skills` + configured paths; nothing foreign discovered by default |
| `/skills` list, `$skill-name` invocation *(low confidence)* | in-TUI listing and explicit invocation | ⚠️ ganja's prompt carries `<available_skills>` and the `skill` tool loads on request; no list UI |

## 10. MCP and LSP

*The MCP rows moved here from the tools section; the CLI rows are researched
in this revision (2026-08-12).*

| Feature | Notes | ganja |
|---|---|---|
| [MCP client](https://github.com/openai/codex/blob/main/docs/config.md) | stdio (`command`/`args`/`env`) + streamable HTTP (`url`/`bearer_token_env_var`), per-server enable + timeouts, OAuth credential store (keyring/file) | ✅ stdio + HTTP, per-server `enabled`/`timeout`/`output_limit`, static `headers` (a bearer goes there); OAuth now too (RFC 8414 discovery + RFC 7591 registration + PKCE, stored under a reserved `mcp:<server>` key, D466) |
| [`codex mcp add/list/get/remove`](https://developers.openai.com/codex/cli) | CLI management writing `config.toml` | ✅ `ganja mcp add/list/get/remove` (D483): validated staged writes to `ganja.json`, `ganja.jsonc` edited comment-preservingly (CST), `get` reports the origin tier |
| [`codex mcp login`](https://developers.openai.com/codex/cli) | OAuth flow for a remote server | ✅ `ganja mcp login <server>` |
| [Codex as an MCP server](https://developers.openai.com/codex/cli) | expose the engine over MCP | ❌ |
| Language servers | none — Codex has no LSP subsystem | n/a — ganja-side advantage: config-declared LSP (rust/gopls builtins + custom entries), diagnostics appended to edit/write results |

## 11. Models, providers and auth

*Consolidated here in this revision (2026-08-12): the provider row from the
config section, the login rows from the CLI section, plus researched rows.*

| Feature | Notes | ganja |
|---|---|---|
| [Custom `model_providers`](https://github.com/openai/codex/blob/main/docs/config.md) | `base_url` + `env_key` + `http_headers` + model list; `wire_api = "responses"` only | ✅ strong parity: ganja's `provider` table (dialect/base_url/key_env/headers) — and ganja speaks **two** dialects where Codex kept one |
| [Model selection](https://github.com/openai/codex/blob/main/docs/config.md) | `model`, `model_reasoning_effort`, `model_reasoning_summary` | ⚠️ `model` and `effort` config keys (effort seeds a fresh session, catalog-validated at adoption; a stored session's own choice outranks it — P17) beside `/model` + `/effort`; no summary knob |
| [ChatGPT OAuth or API key](https://github.com/openai/codex/blob/main/docs/authentication.md) | dual credentials | ✅ the same shape (ganja's `openai`) |
| [`codex login --device-auth`](https://github.com/openai/codex/blob/main/docs/authentication.md) | headless device-code auth | ✅ ganja's grok and ChatGPT logins both carry device flows |
| [Credential precedence](https://github.com/openai/codex/blob/main/docs/authentication.md) | `CODEX_API_KEY` > `OPENAI_API_KEY` > `auth.json` | ✅ same shape: env key outranks the stored login |
| [`forced_login_method` / `forced_chatgpt_workspace_id`](https://github.com/openai/codex/blob/main/docs/config.md) *(low confidence)* | enterprise login lockdown | ❌ |

## 12. Configuration surface (`config.toml`)

| Feature | Notes | ganja |
|---|---|---|
| [Config locations + precedence](https://github.com/openai/codex/blob/main/docs/config.md) | `$CODEX_HOME/config.toml` + trusted project `.codex/config.toml`; project files cannot override security keys | ⚠️ three-tier jsonc merge ✅; no security-key carve-out |
| [Named `[profiles]`](https://github.com/openai/codex/blob/main/docs/config.md) | posture presets via `--profile` | ❌ |
| [History persistence knobs](https://github.com/openai/codex/blob/main/docs/config.md) *(low confidence)* | sqlite/file/disabled, custom path, max entries | ⚠️ SQLite per project, fixed — no disable/relocate knobs |
| [`personality`](https://github.com/openai/codex/blob/main/docs/config.md) | pragmatic / friendly / none tone | ❌ |
| [Display knobs](https://github.com/openai/codex/blob/main/docs/config.md) | `hide_agent_reasoning`, `model_verbosity`, TUI theme/mouse/line numbers | ⚠️ themes ✅; the rest ❌ |
| [Context-window overrides](https://github.com/openai/codex/blob/main/docs/config.md) | `model_context_window`, `model_auto_compact_token_limit` | ❌ catalog-driven sizing, fixed thresholds |
| [Feature flags](https://github.com/openai/codex/blob/main/docs/config.md) *(low confidence)* | experimental `[features]`: multi_agent, memories, goals, hooks, shell_snapshot, unified_exec, … | ❌ |
| [Shell completions](https://developers.openai.com/codex/cli) | bash/zsh/fish/powershell | ❌ (clap could; not wired) |

## 13. Sessions and storage

| Feature | Notes | ganja |
|---|---|---|
| [Rollout files](https://developers.openai.com/codex/cli) | append-only JSONL per session under `~/.codex/sessions/…` + SQLite index | ✅ different shape, same guarantee: write-through SQLite per project |
| [`codex resume` / `--last` / inline prompt](https://developers.openai.com/codex/cli) | resume and continue in one line | ✅ `--continue` / `--session` + `run --continue "…"` |
| Session forking | branch a conversation | ❌ |
| [`codex doctor`](https://developers.openai.com/codex/cli) *(medium confidence)* | config/auth/connectivity diagnostics | ❌ |
| [`/feedback` diagnostics](https://developers.openai.com/codex/cli) | sanitized log bundle to the vendor | ❌ by design — ganja phones nobody |

## 14. CLI and headless

| Feature | Notes | ganja |
|---|---|---|
| [`codex exec`](https://github.com/openai/codex/blob/main/docs/exec.md) | headless runs | ✅ `ganja run` |
| [`exec --json`](https://github.com/openai/codex/blob/main/docs/exec.md) | JSONL event stream | ✅ `--format json` |
| [`exec --output-schema`](https://github.com/openai/codex/blob/main/docs/exec.md) | JSON-Schema-constrained final message | ❌ |
| [`exec --output-last-message <file>`](https://github.com/openai/codex/blob/main/docs/exec.md) | final text to a file | ❌ (stdout redirection instead) |
| [`--image <path>`](https://developers.openai.com/codex/cli) | attach images from the CLI | ❌ |
| Update notifications | | ❌ |

## 15. Server surface and SDK

| Feature | Notes | ganja |
|---|---|---|
| [IDE extension via `app-server`](https://developers.openai.com/codex/ide) | one RPC protocol for TUI, VS Code, desktop app | ❌ (ganja-serve is REST+SSE for its own client, not an IDE protocol) |
| [TypeScript SDK](https://developers.openai.com/codex/cli) *(low confidence)* | `@openai/codex-sdk` embedding the agent | ❌ (`ganja-client` is a hand-written client for ganja-serve, not an embedding SDK) |
| HTTP server surface | none — the app-server protocol is not a public REST API | n/a — ganja-side advantage: `ganja serve` (REST + SSE, Basic auth) + typed `ganja-client` |

## 16. Environment variables

*New in this revision (2026-08-12), researched.*

| Variable | Meaning | ganja |
|---|---|---|
| [`CODEX_HOME`](https://github.com/openai/codex/blob/main/docs/config.md) | the state/config home (`~/.codex`) | ✅ `GANJA_CONFIG_HOME` — one home, not a merge |
| [`OPENAI_API_KEY`](https://github.com/openai/codex/blob/main/docs/authentication.md) | API-key credential | ✅ the same variable |
| [`CODEX_API_KEY`](https://github.com/openai/codex/blob/main/docs/authentication.md) *(medium confidence)* | Codex-specific key override | ❌ no ganja-specific key variable on purpose |
| [`RUST_LOG` / `LOG_FORMAT`](https://github.com/openai/codex/blob/main/docs/config.md) *(medium confidence)* | tracing filter + format, logs under `$CODEX_HOME/log/` | ⚠️ `RUST_LOG` honored (it outranks `-v`'s default filter), daily files named by the **local** date under the data home's `log/`, seven kept; no `LOG_FORMAT` knob |
| `OPENAI_BASE_URL` | endpoint override | ✅ the same variable — but ganja points a *Responses* client at it, refused unless https or loopback |

ganja's own `GANJA_*` surface is documented in the repository root's
`AGENTS.md`; Codex's `shell_environment_policy` (the subshell-env filter)
is catalogued in §3.

## 17. Enterprise, platform and integrations

*Consolidated here in this revision (2026-08-12): the cloud and CI rows from
the old CLI section, plus researched enterprise rows.*

| Feature | Notes | ganja |
|---|---|---|
| [`codex cloud` + `codex apply`](https://developers.openai.com/codex/cloud) | delegate to cloud, pull the diff back | ❌ out-of-scope territory |
| [GitHub Action](https://github.com/openai/codex-action) | CI review and fixes | ❌ |
| [Admin console](https://developers.openai.com/codex/cli) *(medium confidence)* | org model defaults, login lockdown, credit/usage analytics | ❌ |
| [Analytics / compliance APIs](https://developers.openai.com/codex/cli) *(low confidence)* | usage export, SIEM integration | ❌ — ganja emits nothing to integrate |

Where ganja holds its own against Codex (for perspective, not scorekeeping):
loadable TUI themes, `/editor`, the `!` shell passthrough, arity-aware
"always" permission answers, dual-dialect custom providers, the
serve/attach HTTP+SSE surface, first-party LSP diagnostics, and the
golden-differential test discipline.
