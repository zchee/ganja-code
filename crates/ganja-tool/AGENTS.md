<!-- Parent: ../AGENTS.md -->
# ganja-tool

The tools the model can call: the `Tool` trait, `ToolCtx`, the `Registry`, the read-before-write log (`FileTimes`) and the stale-read watcher. It must never depend on `ganja-core`.

It also holds three non-tool modules: the session-socket scheme (`socket.rs`), the session-name registry grammar (`registry.rs`) and the refusal sentences (`permission_text.rs`). The engine, `ganja-serve`, `ganja-provider`, `ganja-tui` and `ganja-cli` need the same answers, and this is the lowest crate they share.

## Boundary

- `depgate.toml`: `[rules."ganja-tool"] internal = ["ganja-permission"]`. The internal dependency set is exactly `ganja-permission`, so no `ganja-core`, and no `ganja-protocol` either.
- `ganja-core`, `ganja-provider` and `ganja-teammate-local` list `ganja-tool` in their `internal` allowlists.
- `ganja-core` re-exports it as `ganja_core::tool` and `ganja_core::watch` (`crates/ganja-core/src/lib.rs`).

## Tool roster

"Asks" means the id is in `ASK_BY_DEFAULT` (`crates/ganja-permission/src/permission.rs`); an agent's ruleset can override it.

| Tool | Module | Registered by | Asks |
|---|---|---|---|
| `read` | `read.rs` | `Registry::with_builtins()` | no |
| `edit` | `edit.rs` | `with_builtins()` | yes |
| `write` | `write.rs` | `with_builtins()` | yes |
| `glob` | `glob.rs` | `with_builtins()` | no |
| `grep` | `grep.rs` | `with_builtins()` | no |
| `bash` | `shell.rs` | `with_builtins()` | yes |
| `todowrite` | `todo.rs` | `with_builtins()` | no |
| `webfetch` | `webfetch.rs` | `with_builtins()` | yes |
| `websearch` | `websearch.rs` | `with_builtins()` | yes |
| `skill` | `skill.rs` | `with_builtins()` holds an empty one; each frontend overlays `SkillTool::over(roots)` | no |
| `question` | `question.rs` | `with_builtins()` | no |
| `bash_output` | `bash_output.rs` | `with_builtins()` | no |
| `kill_shell` | `kill_shell.rs` | `with_builtins()` | no |
| `task` | `task.rs` | engine, `install` in `ganja-core/src/engine.rs`, when the engine holds an agent roster | yes |
| `plan_exit` | `plan.rs` | engine `install`, when the roster has the build agent | no |
| `plan_enter` | `plan.rs` | engine `install`, when the roster has the plan agent | no |
| `send_message` | `send_message.rs` | engine `team_messaging` | no |
| `task_create` | `tasklist.rs` | engine `team_tasks`, when a team task list is installed | no |
| `task_update` | `tasklist.rs` | engine `team_tasks` | no |
| `task_list` | `tasklist.rs` | engine `team_tasks` | no |
| `task_get` | `tasklist.rs` | engine `team_tasks` | no |
| `list_sessions` | `list_sessions.rs` | engine `session_listing` | no |
| `tool_search` | `deferral.rs` | engine `compose_deferral`, when MCP tools are deferred | no |
| `evaluate` | `evaluate.rs` | frontend overlay (`ganja-tui/src/lib.rs`, `ganja-tui/src/app.rs`, `ganja-cli/src/assemble.rs`) when `EvaluateTool::configured()` returns `Some` | yes |

A subagent gets the lent registry, which has no `task`, `send_message` or `task_*` tools.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | `Tool`, `ToolCtx`, `ToolOutput`, `ToolError`, `Registry`, `FileTimes`, `ToolCtx::is_credential_store`. |
| `src/anchor.rs` | Private. Directory-descriptor file I/O for `write` and `edit`. |
| `src/watch.rs` | The stale-read watcher: non-recursive watches on each read file's directory. |
| `src/truncate.rs` | Output clamping (`MAX_LINES` 2,000, `MAX_CHARS` 50 KiB), spill files, the spill sweep. |
| `src/socket.rs` | Session socket paths under `/tmp/ganja-<uid>/`, `vet_directory`, `vet_address`. |
| `src/registry.rs` | The session-name record beside a lead's socket; `vet_name`, `same_name`. |
| `src/permission_text.rs` | `REJECTED`, `DENIED_PREFIX`, `HOOK_REFUSED_PREFIX`, `is_refusal`. |
| `src/typesafe.rs` | The TypeSafe client shared by `evaluate` and `ganja evaluate`. |
| `src/job.rs`, `src/team.rs` | The `Jobs` and `Postbox` traits the engine implements. |
| `src/frontmatter.rs` | `SKILL.md` frontmatter parsing. |
| `src/*.txt` | Tool descriptions. |

## Commands

```sh
cargo nextest run -p ganja-tool
cargo nextest run -p ganja-tool -E 'binary(websearch_keys)'   # one integration binary
cargo depgate check --config depgate.toml                     # the internal-set rule, from the repo root
GANJA_LIVE_TEST=1 cargo test -p ganja-tool --test evaluate_live -- --ignored --nocapture
```

## Conventions

- Anything a tool needs from its caller goes into `ToolCtx` as a value or a trait object the caller implements (`task::Subagents`, `team::Postbox`, `tasklist::TaskList`, `question::Asker`, `plan::Switcher`, `job::Jobs`), never a handle to the engine. Doc comments that refer to `ganja-core` use prose, not intra-doc links.
- Read-before-write: `FileTimes` refuses `write`/`edit` on an existing file not read this session or that changed after the read; the error text tells the model to "read it first" or "read it again" (`src/lib.rs`). `watch.rs` marks a moved file `Stale` in the same log.
- `write` and `edit` reach the disk only through `anchor.rs`: the parent opened with `O_NOFOLLOW` one component at a time, then `openat`/`mkdirat` relative to that descriptor (pinned by `src/anchor_tests.rs`).
- Argument schemas are generated by `schemars` from each tool's argument struct. Never hand-write one. Argument names are upstream's camelCase (`filePath`, `oldString`), which `ganja-core/tests/golden.rs` compares against upstream.
- Large output goes through `truncate::clamp` or `clamp_bytes`, which spill the full text to a file the notice names.
- The tool id is the permission key: `shell.rs` registers `bash`, `todo.rs` registers `todowrite`. Renaming an id breaks stored permission rules.
- `*.txt` descriptions are upstream prompt text; `THIRD_PARTY_NOTICES.md` records each one and its adaptation. Do not reword them. New text of ganja's own is a Rust constant, as in `send_message.rs` and `evaluate.rs`.
- `edit.rs` tries its nine replacers in a fixed order (`simple` through `multi-occurrence`); order changes behavior, and `src/edit_tests.rs` exercises all nine.
- A new tool goes into `with_builtins()` only if it needs no session state; otherwise the engine or a frontend registers it. Either way, decide its default in `ASK_BY_DEFAULT`.

## Gotchas

- `socket.rs` fixes the socket path as literal `/tmp/ganja-<uid>/<hex>.sock`, never `temp_dir()`, because macOS's temp path can overflow `sun_path`. `ganja-serve`, the engine and `ganja sessions --live` all read it from here.
- `permission_text::is_refusal` is the only list of refusal sentences. Wires (cursor) use it to tell a refused call from a failed one, so a new refusal sentence is added there.
- `evaluate` is not offered at all when `TYPESAFE_API_KEY` is unset or blank or `TYPESAFE_BASE_URL` is not https or loopback: `EvaluateTool::configured()` returns `None` (pinned by `tests/evaluate_keys.rs`).
- `websearch` is always registered and refuses without `EXA_API_KEY` or `PARALLEL_API_KEY`, naming the variables, before any request (`GANJA_WEBSEARCH_PROVIDER` picks the service; pinned by `tests/websearch_keys.rs`).
- `read` and `grep` refuse ganja's credential store; the store path arrives in `ToolCtx`, and the comparison is by file identity.
- `question` is not in `ASK_BY_DEFAULT`; `ganja run` refuses it with a rule instead. `ASK_BY_DEFAULT` also names `apply_patch` and `shell`, upstream ids no tool here registers; they stay on purpose.
- A frontend-registered tool (`evaluate` today) must be added at all three overlay sites: `ganja-tui/src/lib.rs`, `ganja-tui/src/app.rs` (the `/plugin` reload) and `ganja-cli/src/assemble.rs`.
- `webfetch` follows redirects; `typesafe.rs` refuses them.

## Tests

Unit tests are sibling `<module>_tests.rs` files attached with `#[cfg(test)] #[path = "…"] mod tests;` (for example `src/read.rs` and `src/read_tests.rs`). Put a new test there unless it mutates process-wide state.

`tests/` holds four binaries with one test each. Three mutate the environment, and `evaluate_log` also installs the process-wide tracing subscriber; the `// SAFETY:` comment on each `set_var` relies on the binary holding one test, so do not add a second.

- `evaluate_keys.rs`, `evaluate_log.rs`: the `TYPESAFE_*` variables, against no endpoint or a loopback one.
- `websearch_keys.rs`: `EXA_API_KEY`, `PARALLEL_API_KEY`, `GANJA_WEBSEARCH_PROVIDER`.
- `evaluate_live.rs`: `#[ignore]` and inert without `GANJA_LIVE_TEST=1`; it sends one request to `https://api.typesafe.ai`.

Fixtures rule: a test must never write into the real data directory (`~/.local/share`). Tests that spill output pass a temp directory through `ShellTool::spilling_into` or `TaskListTool::spilling_into` (both `#[cfg(test)]`). Network fixtures bind loopback only.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-tool.md`, `docs/decisions/ganja-tool-src.md`, `docs/decisions/ganja-tool-tests.md`, frozen from commit 35d1720. New decisions are recorded there, not here.
