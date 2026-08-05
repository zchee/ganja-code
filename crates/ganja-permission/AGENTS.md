<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-05 | Updated: 2026-08-05 -->

# ganja-permission

## Purpose

What ganja is allowed to do, and where. A call becomes one or more patterns, the last matching rule wins, and every pattern has to come back allowed for the call to run without asking. Its own crate because the answer must not depend on the loop that asks: a tool checks a write for containment, the engine raises a dialog, and a stored answer outlives both — three readers, one authority, and nothing in here reaching back for a session to consult.

**On the name.** `project.rs` is bundled here and that makes the crate name a small lie, which is accepted rather than fixed. Project resolution is what the rules are *keyed by* — which worktree this is decides where the stored answers live and what counts as outside the project — and a few hundred settled lines whose only readers are this crate and the engine do not earn a manifest of their own. The alternative buys a truer name with a micro-crate that has one consumer, which is a worse trade.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest, `publish = false`. |
| `src/lib.rs` | Crate doc, the two module declarations, and the headline re-exports (`Action`, `Permissions`, `Decision`, `CallDecision`, `PermissionConfig`, `Project`, `ProjectError`) — how a direct consumer avoids the `ganja_permission::permission` stutter, which exists because the inner module's name is load-bearing for `ganja-core`'s facade. No logic. |
| `src/permission.rs` | The engine: `Rule`, `Action`, `RuleSet`, `PermissionConfig` (a config file's `permission` block with its key order intact), `Permissions` (the layered set and the `decide` that walks it backwards), the wildcard `matches`, the arity table behind what an "always" answer remembers about a shell command, and the `permissions.json` store. Spec: upstream `packages/opencode/src/permission/`. |
| `src/project.rs` | `Project` — the worktree a session runs in, resolved by walking up to a `.git`. Also `data_home` and `digest`, the stable per-project directory name. |

## For AI Agents

### Working In This Directory

- **Last-match-wins is the whole evaluation model, so order is data.** `PermissionConfig` is a list rather than a map and nothing here ever sorts; a reader that sorted the keys would change which rule decides. The config layer parses documents in order for exactly this reason.
- **Rules layer, they do not merge.** The *baseline* is what a build decided (the agent's ruleset, which already carries the config's own `permission` block) and it is replaced wholesale when the agent changes; the *stored* rules are the answers a person gave and sit on top, so an "always allow" survives an agent switch.
- **A subagent inherits the refusals and never the allows.** Nobody is watching an unattended turn, so `derive_subagent` drops the stored tier entirely rather than carrying it at the top of the order, and `inherited_by_subagent` passes down only denials and the location gate.
- **Two gates, not one.** Patterns say *what* a call does; `EXTERNAL_DIRECTORY` is raised alongside them for *where*. A rule naming a tool cannot answer the location gate — `write` is not `external_directory` — which is what keeps an "always" given before that gate existed meaning what its user meant.
- **The per-call read is `gate`.** Earlier trees spelled it `check` and answered the refusal text, the dialog's directories and the stored rules from three separate derivations; `gate` answers all of them from one look, and `remember` consumes what it precomputed. A reader hunting for `check` is looking at history.
- **`MCP_PREFIX` lives here and is the one owner.** A tool whose id starts with it asks by default, below the rules, so a config that answered for it still wins. The engine's MCP module imports the constant from this crate rather than spelling the prefix a second time.
- **Nothing here may fail a turn.** A store that cannot be read is quarantined or ignored with a warning and the session falls back to the defaults; a store that cannot be written costs the answer its persistence and nothing else.
- **A widened item is a claim about a reader.** Several items are `pub` only because the config layer, the agent layer or the engine sit in another crate now (`PermissionConfig::merge` and `.entries`, `RuleSet`, `Permissions::{derive, derive_subagent, inherited_by_subagent, baseline_mentions}`, `matches`, `project::digest`). Do not widen anything else without a named caller.

### Testing Requirements

```sh
cargo test -p ganja-permission        # the in-module suites travelled with the files
cargo nextest run --workspace         # and the engine's, which exercise them through a turn
```

Tests that write a store redirect `XDG_DATA_HOME` so they cannot touch the real user's answers.

### Common Patterns

- A stored answer is a `Rule`, never a remembered command line: for a shell call, "always" keeps the tokens that *name* the command and wildcards the arguments, from upstream's arity table.
- The wildcard matcher normalises separators and treats a trailing ` *` as optional, so `ls *` covers a bare `ls` without covering `lst`.

## Dependencies

### Internal

None. Both directions matter: the engine depends on this, and this depends on nothing in the workspace, which is what makes a rule decidable without a session.

### External

`etcetera` (the data directory the store lands in), `serde` (rules decode from a config file), `serde_json` (a gated call is described by the arguments the model sent, which arrive as a value), `thiserror`, `tracing` (a dropped store names itself). `tempfile` for the tests.

<!-- MANUAL: -->
