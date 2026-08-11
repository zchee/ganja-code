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
| [`@` fuzzy file search with Tab-accept](https://developers.openai.com/codex/cli) | `@` + Tab | ✅ Tab accepts, byte-identical to Enter; directory-descent (`@dir`→`@dir/`) not built — ganja's walker is files-only |
| [Esc-Esc backtrack](https://developers.openai.com/codex/cli) | Esc Esc | ⚠️ idle Esc Esc now opens ganja's own rewind picker — pick a past user-message checkpoint, then Both/Conversation/Files scope; unlike Codex it doesn't re-populate the composer with the old prompt for editing, and it's gated to an idle composer (mid-turn Esc still cancels) |
| [Transcript overlay](https://developers.openai.com/codex/cli) | Ctrl+T | ✅ same chord, three tabs (expanded transcript incl. full tool/MCP input+output, raw event log, per-turn token table); full-terminal takeover and the banner are this overlay's own presentation, Claude Code's Ctrl+O supplies the one-line footer wording |
| [Queued messages](https://developers.openai.com/codex/cli) | Enter while running | ✅ steers into the running turn at its next step boundary (`Command::Steer`) — the same shape as Codex's own `input_queue`/`inject`; what can't steer (refused, unconsumed, slash commands) falls back to a replayed queue, Codex's `queued_user_messages` half |
| [Clipboard image paste](https://developers.openai.com/codex/cli) | Ctrl+V | ✅ PNG-encoded in-process (no OS shell-out) and attached through the `@`-mention pipeline |
| [Slash-command autocomplete](https://developers.openai.com/codex/cli) | `/` | ✅ |
| [Reasoning-effort hotkeys](https://github.com/openai/codex/blob/main/docs/config.md) | Alt+, / Alt+. | ❌ (`/effort` list picker ✅) |
| [Status-line composition](https://github.com/openai/codex/blob/main/docs/config.md) | `[tui] status_line = […]` | ❌ fixed status bar (themes ✅) |
| Prompt history | Up / Down | ✅ |
| Multiline input | Shift+Enter … | ✅ |
| External editor | — | ✅ `/editor` (ganja-side advantage) |

## 2. Slash commands

| Command | Notes | ganja |
|---|---|---|
| [`/model`](https://developers.openai.com/codex/cli) | model **and** reasoning effort in one menu | ⚠️ `/model` ✅ + separate `/effort`; no combined menu |
| [`/review`](https://developers.openai.com/codex/cli) | presets: uncommitted / commit / base-branch diff, custom focus | ❌ |
| [`/diff`](https://developers.openai.com/codex/cli) | session-wide change viewer | ❌ (per-edit inline diffs ✅) |
| [`/compact`](https://developers.openai.com/codex/cli) | summarize the conversation | ✅ plus auto-compaction |
| [`/prompts` → Agent Skills](https://developers.openai.com/codex/cli) *(medium confidence)* | prompt templates deprecated toward SKILL.md | ⚠️ skills ✅ (SKILL.md-compatible); no template list UI |
| [`/status`](https://developers.openai.com/codex/cli) | model/tokens/context/cost dashboard | ⚠️ status bar + totals only |
| [`/init`](https://developers.openai.com/codex/cli) | generate AGENTS.md | ✅ |
| [`/resume`](https://developers.openai.com/codex/cli) | in-TUI session picker | ✅ `/sessions` |
| [`/feedback`](https://developers.openai.com/codex/cli) | sanitized diagnostics report to OpenAI | ❌ (no telemetry channel at all) |
| `/new` / `/quit` | session control | ✅ equivalents |
| [`/mcp`](https://github.com/openai/codex/blob/main/docs/config.md) | MCP connection status | ✅ `/mcp` dialog (status, tool counts, Reconnect/Login actions) + `ganja mcp` CLI listing |
| `/login` / `/logout` | credential switching in-TUI | ⚠️ `auth` CLI only |

## 3. Security and execution modes

| Feature | Notes | ganja |
|---|---|---|
| [OS-kernel sandboxing](https://github.com/openai/codex/blob/main/docs/sandbox.md) | macOS Seatbelt; Linux Landlock + seccomp | ❌ permission engine only, no isolation |
| [Approval policies](https://github.com/openai/codex/blob/main/docs/getting-started.md) | read-only / workspace-write / full-access; on-request/untrusted/never | ⚠️ rule-based allow/ask/deny + single-tier `--auto` |
| [Write-mode network cutoff](https://github.com/openai/codex/blob/main/docs/sandbox.md) | `network_access = false` under workspace-write | ❌ no such concept |
| [Project trust levels](https://github.com/openai/codex/blob/main/docs/config.md) | `[projects."path"] trust_level`, prompt on untrusted dirs | ❌ |
| [`shell_environment_policy`](https://github.com/openai/codex/blob/main/docs/config.md) | inherit all/core/none + include/exclude patterns for subshell env | ❌ tools inherit the process env |
| [`--yolo` bypass](https://github.com/openai/codex/blob/main/docs/sandbox.md) | skip sandbox + approvals | ⚠️ `--auto` is allow-unless-denied; no sandbox to bypass |
| [Container posture](https://github.com/openai/codex/blob/main/docs/sandbox.md) | degraded-sandbox flags for Docker/devcontainers | ❌ |

## 4. Configuration surface (`config.toml`)

| Feature | Notes | ganja |
|---|---|---|
| [Config locations + precedence](https://github.com/openai/codex/blob/main/docs/config.md) | `$CODEX_HOME/config.toml` + trusted project `.codex/config.toml`; project files cannot override security keys | ⚠️ three-tier jsonc merge ✅; no security-key carve-out |
| [Named `[profiles]`](https://github.com/openai/codex/blob/main/docs/config.md) | posture presets via `--profile` | ❌ |
| [Custom `model_providers`](https://github.com/openai/codex/blob/main/docs/config.md) | `base_url` + `env_key` + `http_headers` + model list; `wire_api = "responses"` only | ✅ strong parity: ganja's `provider` table (dialect/base_url/key_env/headers) — and ganja speaks **two** dialects where Codex kept one |
| [`notify` hooks](https://github.com/openai/codex/blob/main/docs/config.md) | run a command on completion/approval-needed | ❌ |
| [History persistence knobs](https://github.com/openai/codex/blob/main/docs/config.md) *(low confidence)* | sqlite/file/disabled, custom path, max entries | ⚠️ SQLite per project, fixed — no disable/relocate knobs |
| [`personality`](https://github.com/openai/codex/blob/main/docs/config.md) | pragmatic / friendly / none tone | ❌ |
| [Display knobs](https://github.com/openai/codex/blob/main/docs/config.md) | `hide_agent_reasoning`, `model_verbosity`, TUI theme/mouse/line numbers | ⚠️ themes ✅; the rest ❌ |
| [Context-window overrides](https://github.com/openai/codex/blob/main/docs/config.md) | `model_context_window`, `model_auto_compact_token_limit` | ❌ catalog-driven sizing, fixed thresholds |
| [Feature flags](https://github.com/openai/codex/blob/main/docs/config.md) *(low confidence)* | experimental `[features]`: multi_agent, memories, goals, hooks, shell_snapshot, unified_exec, … | ❌ |
| [Shell completions](https://developers.openai.com/codex/cli) | bash/zsh/fish/powershell | ❌ (clap could; not wired) |

## 5. Context files and prompts

| Feature | Notes | ganja |
|---|---|---|
| [AGENTS.md, project + global](https://agents.md) | `~/.codex/AGENTS.md` + repo root | ✅ ganja reads the family plus its global tier |
| [Nested AGENTS.md](https://agents.md) | per-subdirectory instruction files, scoped recursively | ❌ no subdirectory walk-in |
| [Custom prompts](https://developers.openai.com/codex/cli) | `~/.codex/prompts/*.md` + project scope, argument interpolation | ⚠️ config-declared commands with `$ARGUMENTS`/`!`/`@` expansion |
| [Skills (SKILL.md)](https://developers.openai.com/codex/cli) | cross-tool standard | ✅ ganja's two homes + `skills.paths` |

## 6. Tools and agent machinery

| Feature | Notes | ganja |
|---|---|---|
| [`apply_patch`](https://github.com/openai/codex/blob/main/docs/getting-started.md) | structured unified-diff editing as the primary tool, intercepted at the harness (`unified_exec`) | ❌ ganja follows upstream's `edit`/`write`; the name exists in the permission table only |
| [`unified_exec`](https://developers.openai.com/codex/cli) *(low confidence)* | consolidated exec subsystem, byte-capped streaming output | ⚠️ ganja's shell has its own spill/truncation discipline |
| [`update_plan` (plan mode)](https://developers.openai.com/codex/cli) | live checklist rendering and updates | ⚠️ `todowrite` is the nearest; no plan-specific tool |
| [`web_search` tool](https://github.com/openai/codex/blob/main/docs/config.md) | live search opt-in | ✅ `websearch` (Exa/Parallel) |
| [`view_image` tool](https://github.com/openai/codex/blob/main/docs/config.md) | the model reads local images by path, self-directed | ❌ image context is user-attached only |
| Shell execution | | ✅ `bash` |
| Best-of-N *(low confidence)* | parallel candidate generation | ❌ |
| [MCP client](https://github.com/openai/codex/blob/main/docs/config.md) | stdio (`command`/`args`/`env`) + streamable HTTP (`url`/`bearer_token_env_var`), per-server enable + timeouts, OAuth credential store (keyring/file) | ✅ stdio + HTTP, per-server `enabled`/`timeout`/`output_limit`, static `headers` (a bearer goes there); OAuth now too (RFC 8414 discovery + RFC 7591 registration + PKCE, stored under a reserved `mcp:<server>` key, D466) |
| [Codex as an MCP server](https://developers.openai.com/codex/cli) | expose the engine over MCP | ❌ |

## 7. Sessions, storage, diagnostics

| Feature | Notes | ganja |
|---|---|---|
| [Rollout files](https://developers.openai.com/codex/cli) | append-only JSONL per session under `~/.codex/sessions/…` + SQLite index | ✅ different shape, same guarantee: write-through SQLite per project |
| [`codex resume` / `--last` / inline prompt](https://developers.openai.com/codex/cli) | resume and continue in one line | ✅ `--continue` / `--session` + `run --continue "…"` |
| Session forking | branch a conversation | ❌ |
| [`/feedback` diagnostics](https://developers.openai.com/codex/cli) | sanitized log bundle to the vendor | ❌ by design — ganja phones nobody |

## 8. CLI, headless, cloud, integrations

| Feature | Notes | ganja |
|---|---|---|
| [`codex exec`](https://github.com/openai/codex/blob/main/docs/exec.md) | headless runs | ✅ `ganja run` |
| [`exec --json`](https://github.com/openai/codex/blob/main/docs/exec.md) | JSONL event stream | ✅ `--format json` |
| [`exec --output-schema`](https://github.com/openai/codex/blob/main/docs/exec.md) | JSON-Schema-constrained final message | ❌ |
| [`exec --output-last-message <file>`](https://github.com/openai/codex/blob/main/docs/exec.md) | final text to a file | ❌ (stdout redirection instead) |
| [`codex login --device-auth`](https://github.com/openai/codex/blob/main/docs/authentication.md) | headless device-code auth | ✅ ganja's grok and ChatGPT logins both carry device flows |
| [ChatGPT OAuth or API key](https://github.com/openai/codex/blob/main/docs/authentication.md) | dual credentials | ✅ the same shape (ganja's `openai`) |
| [`codex cloud` + `codex apply`](https://developers.openai.com/codex/cloud) | delegate to cloud, pull the diff back | ❌ out-of-scope territory |
| [IDE extension via `app-server`](https://developers.openai.com/codex/ide) | one RPC protocol for TUI, VS Code, desktop app | ❌ (ganja-serve is REST+SSE for its own client, not an IDE protocol) |
| [GitHub Action](https://github.com/openai/codex-action) | CI review and fixes | ❌ |
| [`--image <path>`](https://developers.openai.com/codex/cli) | attach images from the CLI | ❌ |
| Update notifications | | ❌ |

Where ganja holds its own against Codex (for perspective, not scorekeeping):
loadable TUI themes, `/editor`, the `!` shell passthrough, arity-aware
"always" permission answers, dual-dialect custom providers, the
serve/attach HTTP+SSE surface, and the golden-differential test discipline.
