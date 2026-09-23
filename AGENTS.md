<!-- Operational context for agents. Decision history (D-numbers) is in docs/decisions/. Sections are ordered by what an agent needs first: the SessionStart hook shows only the first ~5 KB. -->
# ganja-code

`ganja` is a terminal-first AI coding agent in Rust: a behavioral port of [opencode](https://github.com/anomalyco/opencode) v1.18.22 with a ratatui TUI, an HTTP + SSE server, and teammate backends that drive Claude Code and foreign CLIs in tmux panes.
Upstream's TypeScript is the specification, not source to translate: write idiomatic Rust and match observable behavior. Toolchain: the non-dated `nightly` channel (`rust-toolchain.toml`), edition 2024.

## Workspace map

Fourteen members under `crates/`, each with its own `AGENTS.md`. Dependency direction is one-way and gated by `depgate.toml`; `crates/AGENTS.md` states the graph.

| Crate | Role |
|---|---|
| `ganja-protocol` | The types every side speaks (`Command`, `Event`, `Message`, `Part`). Leaf. |
| `ganja-permission` | Which tool calls run unasked, and which worktree a session is in. Leaf. |
| `ganja-team` | Claude Code's teams directory: member records, file-backed mailboxes, the shared task list. |
| `ganja-tool` | The tools the model can call, the read log, the session-socket scheme. |
| `ganja-provider` | Vendor wires, credentials and logins, the model catalog. |
| `ganja-storage` | The SQLite session store and working-tree snapshots for `/undo` and `/rewind`. |
| `ganja-core` | The engine: sessions, the agent loop, config, hooks, MCP, LSP, plugins, teammates. No terminal or HTTP-server dependency. |
| `ganja-teammate-local` | tmux-pane and foreign-CLI teammate backends. Above the engine; `ganja-serve` never links it. |
| `ganja-tui` | The ratatui frontend. Every pixel, no engine logic. |
| `ganja-serve` | The engine over HTTP + SSE, plus the per-session Unix socket. |
| `ganja-client` | The typed client for `ganja-serve`. |
| `ganja-cli` | The `ganja` binary. |
| `ganja-testkit` | Dev-only test scaffolding: scripted providers, recorder tools, storage seeding. |
| `tmux` | A tmux control-mode client. Sealed leaf: consumes nothing here and nothing here consumes it. |

## Gates

CI runs these (`.github/workflows/ci.yaml`); all must be green before work is called done.

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps   # intra-doc links must resolve
cargo nextest run --locked --workspace --profile ci           # one process per test; profiles in .config/nextest.toml
cargo test --workspace --doc                                  # nextest skips doctests
cargo depgate check --config depgate.toml                     # 19 dependency-boundary rules
cargo deny check                                              # advisories, licenses, bans, sources (deny.toml)
```

Single tests: `cargo nextest run -p ganja-permission`, `cargo nextest run -E 'binary(golden)'`, `cargo nextest run --workspace <name-substring>`. TUI snapshots: `cargo insta review`.

Suites that need setup hard-fail instead of skipping:

- `golden` and `mcp` (ganja-core): `bun` on PATH and an upstream checkout with `bun install` done, at `GANJA_OPENCODE_DIR` (CI: `upstream/opencode-v1.18.22`) or `.omc/reference/opencode-v1.18.22`.
- `lsp` (ganja-core): `rust-analyzer` on PATH (`rustup component add rust-analyzer`); `GANJA_LSP_EDIT_BUDGET_MS` widens its timing budget (CI: 6000).
- tmux-driving suites (ganja-teammate-local `teammate_pane_*` and `shim_tui`; ganja-cli `teammate_pane`, `teammate_env`, `teammate_permission` and the `*_pane` binaries; the `tmux` crate's `live` and `inventory`): a real tmux server, 3.2 or newer for the panes and 3.7c for the `tmux` crate; CI installs Homebrew's.
- Live provider tests are `#[ignore]` and inert without `GANJA_LIVE_TEST=1` plus a key: `GANJA_LIVE_TEST=1 ANTHROPIC_API_KEY=… cargo test -p ganja-core --test live -- --ignored`.

## Commands

```sh
cargo build --workspace
cargo run                                   # TUI; provider from --model, GANJA_PROVIDER, config, the oldest stored login, else fake
cargo run -- --model anthropic/claude-sonnet-4-5 --agent plan    # also --config <file>, --name <self-name>
cargo run -- --continue                     # or --session <id>; mutually exclusive
cargo run -- --auto                         # answer permission dialogs "allow once"; deny rules still deny
cargo run -- run "what does this crate do"  # one headless turn; --format json; --continue; --attach <url>
cargo run -- run --auto --command team "port the config loader"   # the /team pipeline headless; needs --auto
cargo run -- run --json-schema <file|json> "…"                    # structured output; chatgpt and openai only
cargo run -- serve --port 4096              # binds localhost; a non-loopback --hostname requires GANJA_SERVER_PASSWORD
cargo run -- auth login                     # also auth list, auth logout
cargo run -- sessions                       # stored conversations; --live lists sessions answering on a socket now
cargo run -- models anthropic --refresh     # the catalog for one provider; `models cursor` reads the wire's roster
cargo run -- mcp                            # configured MCP servers; also mcp list, add, get, remove, login
cargo run -- plugin list                    # also details, install, enable, disable, remove, marketplace add/list/remove/update
cargo run -- skills                         # the discovered skill roster
cargo run -- evaluate --questions @q.json   # TypeSafe's judge, no session; exit 0 answered, 3 unconfigured, 4 refused, 5 unavailable, 64 usage
cargo run -- config migrate                 # also config import-opencode, import-claude-hooks; --dry-run, --global, --file
dist build                                  # local archive under target/distrib/; never let dist write a workflow file
```

## Environment

| Variable | Meaning |
|---|---|
| `GANJA_PROVIDER`, `GANJA_MODEL` | One of the eleven builtin ids (listed in `crates/ganja-provider/AGENTS.md`) or an id from a config `[provider.<id>]` table, and the model override. Selection order: `--model`, these variables, the config's `model` and `default_provider`, the oldest stored login, then `fake`. |
| `GANJA_CONFIG` | An extra config file merged between the global and project tiers; it must exist. |
| `GANJA_CONFIG_HOME` | ganja's own home: the global `ganja.toml`, `AGENTS.md`, `skills/`, `themes/`. Default `$XDG_CONFIG_HOME/ganja`, else `~/.ganja`. |
| `GANJA_FAKE_SCRIPT`, `GANJA_FAKE_TITLE` | Fake provider: a JSON turn script; `1` opts into a real title request. |
| `GANJA_DISABLE_TERM_PROBE` | Truthy skips the kitty keyboard probe. Every pty test sets it. |
| `GANJA_DISABLE_MODELS_FETCH`, `GANJA_MODELS_URL`, `GANJA_MODELS_PATH` | Catalog fetching off; fetch base URL; read-only cache path override. |
| `GANJA_AUTH_ISSUER` | Login endpoint origin for tests. Loopback only; anything else is refused. |
| `GANJA_SERVER_PASSWORD`, `GANJA_SERVER_USERNAME` | `serve` Basic auth. Without the password `serve` refuses a non-loopback `--hostname`. |
| `GANJA_CLAUDE_BIN` | Absolute path of the `claude` binary the `claude-code` provider spawns. |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `OPENCODE_API_KEY` | Credentials; they outrank the stored `auth.json`. |
| `ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL` | Endpoint overrides; https or loopback only. |
| `EXA_API_KEY`, `PARALLEL_API_KEY`, `GANJA_WEBSEARCH_PROVIDER` | `websearch` credentials and which service (`exa` or `parallel`). Without a key the search is refused, not sent. |
| `TYPESAFE_API_KEY`, `TYPESAFE_BASE_URL`, `TYPESAFE_DEFAULT_MODEL` | The `evaluate` tool. Without the key the tool is not registered at all. |
| `EDITOR` | What `/editor` opens; default `vi`. |
| `GANJA_LIVE_TEST`, `GANJA_OPENCODE_DIR`, `GANJA_MCP_SDK_DIR`, `GANJA_LSP_EDIT_BUDGET_MS` | Test opt-ins and paths; see Gates. |

## Conventions

- Port behavior, not code. Module docs cite the upstream file (`//! Spec: upstream packages/opencode/src/tool/edit.ts`); a divergence is documented where it occurs, with its reason.
- Comments explain why, not what, including in `Cargo.toml`. Every dependency version lives in the root manifest with a comment; members opt in with `x.workspace = true` (`depgate.toml` gates it).
- Unit tests are sibling files: `foo.rs` declares `#[cfg(test)] #[path = "foo_tests.rs"] mod tests;` and the tests live in `foo_tests.rs`. Anything needing a real socket, a filesystem layout or process-wide environment goes in the crate's `tests/`, one binary per environment-mutating suite.
- Test names are sentences about behavior: `a_denied_edit_leaves_the_file_untouched`.
- Tests that touch stored state redirect `XDG_DATA_HOME`. Nothing renders a whole API key; `crates/ganja-core/tests/secrets_env.rs` pins that with a canary.
- `unsafe` and SIMD are chosen on merit. A performance-motivated choice needs a criterion or divan benchmark, and every unsafe block carries a `// SAFETY:` comment.
- Commit subjects are `scope: intent` (`core,tui,cli: let the model act through permission-gated tools`). Commits are GPG-signed with the message from a file.
- Check `git status` before editing. One owner per file; a dirty file belongs to another lane.
- Text ported from opencode, and anything another tool's process wrote into the tree, is recorded in `THIRD_PARTY_NOTICES.md`.
- `rustfmt.toml` enables unstable options on nightly: imports grouped std / external / crate, one `use` per module, doc-comment code formatted.

## Gotchas

- `ganja-core` may not depend on `ratatui*` or `axum*`; `ganja-provider` not on `ratatui*`, `crossterm*` or `arboard*`; `ganja-serve` not on `ratatui*` or `ganja-teammate-local`. Something the UI must draw becomes a serde type in `ganja-protocol`.
- Config is TOML only (`ganja.toml`). A discovered `ganja.jsonc` or `ganja.json` is refused with a pointer to `ganja config migrate`. An unknown key is refused with an error that names it, and `schema/ganja-config.schema.json` must match the loader: `crates/ganja-core/tests/config_schema.rs` is the drift test.
- `.omc/` is gitignored operational state on the owner's machine (plans, handoffs, the upstream reference checkout). Nothing in the tree may require it; CI checks upstream out to `upstream/`.
- `.cargo/config.toml` sets aarch64-apple-darwin rustflags; an ambient `RUSTFLAGS` replaces the whole block. Gates and CI run with it unset.
- The nextest `ci` profile runs the `tmux` crate's live suite alone on the runner and kills a wedged pty test after 4 minutes; `retries = 0` in every profile, so a flaky test is a failure.
- Claude Code loads `AGENTS.md` only when no `CLAUDE.md` is present and its `agents-md` plugin allows it; this user's settings do not, so nested files are read only when an agent opens them. Keep every `AGENTS.md` at 200 lines or fewer and well under the 45,000-byte read cap.

## Key files

| File | Purpose |
|---|---|
| `Cargo.toml`, `depgate.toml`, `deny.toml`, `.config/nextest.toml` | Every dependency version with its reason; the 19 dependency rules with theirs; cargo-deny policy; nextest profiles. |
| `schema/ganja-config.schema.json` | JSON Schema for `ganja.toml`; taplo reads it through a `#:schema` line. |
| `CONTEXT.md` | Glossary: the canonical term for each concept and the words to avoid. |
| `PRACTICE.md` | Rust exercises for the owner, mapped to the port's phases. |
| `README.md` | Public positioning; the cloud design is described there and not implemented. |
| `docs/` | `references/` (feature inventories of Claude Code, opencode and Codex; not roadmaps), `recipes/` (worked examples), `decisions/` (frozen decision ledgers), `prompts/`. |

<!-- br-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`/`bd`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

### Essential Commands

```bash
# View ready issues (open, unblocked, not deferred)
br ready              # or: bd ready

# List and search
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br search "keyword"   # Full-text search

# Create and update
br create --title="..." --description="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason="Completed"
br close <id1> <id2>  # Close multiple issues at once

# Sync with git
br sync --flush-only  # Export DB to JSONL
br sync --status      # Check sync status
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Always run `br sync --flush-only` at session end

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only open, unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers 0-4, not words)
- **Types**: task, bug, feature, epic, chore, docs, question
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads changes to JSONL
git commit --gpg-sign -F <msg-file>  # Commit everything: signed, message from a file
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress → closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always sync before ending session

<!-- end-br-agent-instructions -->

### Labels beyond the managed block

- Ideas about direction and features are GitHub issues, not beads: filed through `.github/ISSUE_TEMPLATE/idea.yml`, labelled `idea` plus `verdict:do|park|drop` (and `size:S|M|L|XL` for *do*), as sub-issues of a `Theme:` parent; issue #100 ranks them. Beads carry work only: bugs, tasks, chores.

## History

Decisions before 2026-09-23 (D-numbers, phase and wave ledgers, rationale) are in `docs/decisions/`, frozen from commit 35d1720: `root.md` for this file, one file per old nested `AGENTS.md`. New decisions are recorded there, not here.
