<!-- Parent: ../AGENTS.md -->
# ganja-team

File I/O over Claude Code's teams directory: team files, member records, file-backed inboxes, and ganja's own shared task list under `tasks/`. A real `claude` process may share the same directory, so the document bytes and the lock protocol are interop contracts. It knows nothing about sessions, config homes or permissions, sits beneath `ganja-core` and depends only on `ganja-protocol`.

## Boundary

- `depgate.toml` `[rules."ganja-team"]`: `internal = ["ganja-protocol"]`.
- `depgate.toml` `[rules."ganja-core"]` and `[rules."ganja-teammate-local"]` list `ganja-team` in their `internal` sets.
- `ganja-core` re-exports it as `ganja_core::team` (`pub use ganja_team as team;` in `crates/ganja-core/src/lib.rs`).

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | Crate doc (why a crate, what it does not know, why synchronous), runnable mailbox doc example, root re-exports |
| `src/team.rs` | `TeamsRoot` and its path builders (`team_dir`, `config_path`, `inbox_path`, `tasks_dir`), `TeamName`/`MemberName::parse`, `LEAD`, `resolve_unique` |
| `src/record.rs` | `TeamFile`, `MemberRecord` (two key orders: lead and teammate), `MailboxMessage`, `Surface`, `ShimCli`, `now_iso8601` (RFC 3339 with milliseconds, via `jiff`) |
| `src/mailbox.rs` | `seed`, `read`, `write`, `write_bounded`, `prune_delivered`, `identity` |
| `src/lock.rs` | `acquire`, `acquire_unseeded`, `Guard`, `STALE`, `RETRIES` |
| `src/task.rs` | `Store` (`create`, `get`, `list`, `update`, `claim`, `delete`), `Task`, `Update`, `TaskId`, `TASKS_DIR`, `COUNTER`, `MAX_COUNTERPARTS` |
| `tests/support/mod.rs` | Shared fixture module (`mod support;`), not a test binary |
| `tests/fixtures/` | Documents a real Claude Code wrote; `PROVENANCE.md` records capture and redaction |

## Commands

```sh
cargo nextest run -p ganja-team
cargo test -p ganja-team --doc      # nextest skips the lib.rs mailbox example
```

## Conventions

- Every document shape carries a `#[serde(flatten)]` `IndexMap` extra, so an unknown key survives a rewrite in its original position. Never decode Claude's documents through a `serde_json::Map`, which sorts keys (see the `serde_json` comment in `Cargo.toml`; pinned by `tests/claude_format_interop.rs`).
- Never add a ganja-only key to a Claude document. Ganja-only member data goes in `ganja-protocol`'s `MemberView`; ganja-only files go in the `tasks/` subdirectory (see `src/task.rs`).
- `MemberRecord` has two key orders, lead and teammate, as Claude writes them (see `src/record.rs` and `tests/fixtures/PROVENANCE.md`).
- The lock is npm proper-lockfile's protocol: `realpath` the target, `mkdir` `<target>.lock`, `rmdir` on `Guard` drop (see `src/lock.rs`).
- A lock directory older than `STALE` (10 s, by mtime) is broken; the retry ladder is `RETRIES` = 10. Put nothing inside the lock directory and never use a lock file: Claude's cleanup is `rmdir` (pinned by `tests/lock_break.rs`, `tests/lock_release.rs`, `tests/contention.rs`).
- An inbox is seeded with `[]` before it is locked (`mailbox::seed`); a team file or task document uses `acquire_unseeded`.
- Task ids come from the `tasks/counter` document, bumped under its own lock; a create also reads the directory and issues one past the higher of the two, so an id is never issued twice (see `src/task.rs`; pinned by `tests/task_race.rs`).
- Dependency edges are add-only through `Update` (`add_blocks`, `add_blocked_by`); each edge is written on both tasks under holds taken lowest id first, at most `MAX_COUNTERPARTS` (8) per call. `delete` removes its id from every counterpart one hold at a time (pinned by `src/task_tests.rs`).
- Nothing logs a message body or task content: log lines carry counts, paths and ids, and the hand-written `Debug` impls redact (pinned by `tests/no_bodies_in_logs.rs`, `tests/no_task_content_in_logs.rs`).

## Gotchas

- The crate is synchronous. A caller inside an async turn wraps it in `spawn_blocking` (crate doc in `src/lib.rs`).
- Task documents are opened with `O_NOFOLLOW | O_NONBLOCK` (`libc`, unix only) and judged on the descriptor, so a symlink or FIFO planted in `tasks/` is dropped instead of read or blocked on (see `src/task.rs`).
- `list` and `get` drop an edge that names a missing id, and refuse a document filed under an id that is not its own; `delete` is the only way to remove such a document (see `src/task.rs`).
- `Spawn`'s `prompt` is written to the team file verbatim, as Claude Code does: a credential in a spawn prompt lands on disk in cleartext. Its `Debug` omits it (see `src/record.rs`).
- A name is refused, never sanitized: `TeamName::parse` and `MemberName::parse` reject anything outside the grammar (see `src/team.rs`).
- Do not reformat anything under `tests/fixtures/`. Every byte is interop evidence; recapture instead of editing, and record the capture in `PROVENANCE.md` and `THIRD_PARTY_NOTICES.md` ("Claude Code").
- `tests/support/mod.rs` cannot use `ganja-testkit`: that crate depends on `ganja-core`, which depends on this one, so a dev-dependency would form a cycle.

## Tests

Unit tests are sibling `*_tests.rs` files attached with `#[path]`. Each `tests/*.rs` file is its own binary, and its `//!` header states what it proves. `claude_format_interop` reads `tests/fixtures/` and panics when the capture is missing or incomplete.

`contention` and `task_race` spawn real processes, because the lock's in-process half would serialize threads before the on-disk protocol runs; each re-executes its own binary and tells the child its role through environment variables it sets itself. Neither needs setup.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-team.md`, frozen from commit 35d1720. New decisions are recorded there, not here.
