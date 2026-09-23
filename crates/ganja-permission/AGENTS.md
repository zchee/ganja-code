<!-- Parent: ../AGENTS.md -->
# ganja-permission

Decides which tool calls ganja runs without asking, and which worktree a session belongs to. A call becomes one or more patterns, the last matching rule wins, and every pattern must come back allowed for the call to run unasked. It sits at the bottom of the workspace graph: it depends on no workspace crate, so a rule is decidable without a session.

## Boundary

- `depgate.toml`: `[rules."ganja-permission"] leaf = true`. No `ganja-*` dependency may be added.
- `crates/ganja-core/src/lib.rs` re-exports it as `pub use ganja_permission::{permission, project};`, so `ganja_core::permission` must keep resolving. That is why the inner module is named `permission`; `src/lib.rs` re-exports the main types at the crate root for direct consumers.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | Module declarations and crate-root re-exports. No logic. |
| `src/permission.rs` | `Rule`, `Action`, `Decision`, `RuleSet`, `PermissionConfig`, `Permissions`, `CallDecision`, the wildcard `matches`, the shell arity table, the `permissions.json` store, and the constants `ASK_BY_DEFAULT`, `EXTERNAL_DIRECTORY`, `MCP_PREFIX`, `TASK`. Spec: upstream `packages/opencode/src/permission/`. |
| `src/project.rs` | `Project` (worktree root and slug), `data_home`, `write_new`, `digest`. |
| `src/permission_tests.rs`, `src/project_tests.rs` | Unit tests. |

## Commands

```sh
cargo nextest run -p ganja-permission
cargo nextest run -p ganja-core --test permissions   # the data-home and store behaviour, through the engine
cargo depgate check --config depgate.toml            # the leaf rule, from the repository root
```

## Conventions

- **The engine calls `Permissions::gate_with_default` once per tool call** (from `prepare` in `crates/ganja-core/src/session.rs`, with the value of `effective_default`). `gate` is the same function with no default; only tests call it. A change to how a call is judged goes into `gate_with_default`.
- **Precedence, highest first:** the last matching rule over the baseline followed by the stored rules (`decide` walks `ordered()` in reverse), then the caller's `unmatched` default, then `ASK_BY_DEFAULT`, then the `MCP_PREFIX` ask, then allow. `Decision` is ordered `Allow < Ask < Deny`, and a call takes the maximum over its patterns and directories (pinned by `src/permission_tests.rs`).
- **Order is data.** `PermissionConfig` is a list, not a map, and nothing here sorts it. Sorting would change which rule wins.
- **Baseline and stored rules layer; they do not merge.** `set_baseline` replaces the agent's rules wholesale; stored answers sit above them and survive an agent switch.
- **A subagent inherits denials and the location gate, never allows.** `derive_subagent` drops the parent's stored answers and keeps the store; `inherited_by_subagent` returns the rules passed down. `derive` is the attended variant and keeps the stored answers.
- **Two gates per call.** Tool patterns decide what a call does; `EXTERNAL_DIRECTORY` decides where. The `unmatched` default applies to the tool only, never to the location gate.
- **`MCP_PREFIX` is defined only here.** The engine's MCP module imports it; do not write the prefix a second time.
- **Keep items private unless a named caller in another crate needs them.** Current cross-crate items include `PermissionConfig::merge`, `Permissions::{derive, derive_subagent, inherited_by_subagent, baseline_mentions}`, `matches`, `project::digest` and `write_new` (called by `crates/ganja-tui/src/theme/selection.rs`). `RuleSet` is public but has no caller outside this crate today.

## Gotchas

- **The worktree is decided by `Project::resolve`** in `src/project.rs`: it canonicalises `cwd` and takes the nearest ancestor that contains `.git`, or `cwd` itself when none does. The slug is Claude Code's scheme (non-alphanumeric UTF-16 units become `-`, cut at 200 characters plus a hash).
- `project::digest` names a worktree's snapshot repository in `crates/ganja-storage/src/snapshot.rs`. Changing it orphans existing snapshots.
- An "always" answer stores a `Rule`, not a command line: for a shell call the arity table keeps the tokens that name the command and wildcards the arguments. A directory whose name contains a wildcard character is never remembered and keeps asking.
- A trailing ` *` in a pattern is optional, so `ls *` matches bare `ls` but not `lst` (pinned by `src/permission_tests.rs`).
- Nothing here may fail a turn. An unreadable store is quarantined or ignored with a warning and the session falls back to defaults; an unwritable store keeps the answer in memory only.
- `evaluate` is in `ASK_BY_DEFAULT`; removing it lets built-in agents send project content to a third party unasked.

## Tests

Unit tests live in sibling files wired through `#[path]`; no inline test module exists. Unit tests use `tempfile` directories and never set `XDG_DATA_HOME`: that variable is process-wide, so the data-home test lives in `crates/ganja-core/tests/permissions.rs`, which calls `ganja_testkit::redirect_xdg_data_home` (see `src/project_tests.rs`).

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-permission.md`, frozen from commit 35d1720. New decisions are recorded there, not here.
