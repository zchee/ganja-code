<!-- Parent: ../AGENTS.md -->
# ganja-storage

The per-project SQLite session store (`storage.rs`) and the git snapshots of the working tree that `/undo` and `/rewind` walk (`snapshot.rs`). It never depends on the engine: callers hand in plain values, the same rule `ganja-tool`'s `ToolCtx` follows. It sits beneath `ganja-core`, above the two leaves.

## Boundary

- `depgate.toml`: `[rules."ganja-storage"] internal = ["ganja-permission", "ganja-protocol"]`. The set is exact; a new `ganja-*` edge fails `cargo depgate check` in CI until that rule is edited.
- `ganja-core` depends on it through `internal = [..., "ganja-storage", ...]`; `ganja-teammate-local`'s internal set lists it too.
- `crates/ganja-core/src/lib.rs` re-exports both modules whole (`pub use ganja_storage::{snapshot, storage};`) plus `RevertState`, `Snapshots`, `SessionId`, `SessionInfo`, `Storage` and `StorageError` at its root. Callers use `ganja_core::storage` paths.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | `pub mod storage; pub mod snapshot;` only. |
| `src/storage.rs` | `Storage`, `SessionInfo`, `StorageError`, schema, migration journal, pragmas, writer startup and legacy-tree conversion. The module doc defines admission, recovery and operator requirements. |
| `src/storage_admission.rs`, `src/storage_maintenance.rs` | Shared connection-lifetime leases, bounded admission, exclusive recheck and journaled companions-first/main-last archive publication. |
| `src/snapshot.rs` | `Snapshots`, `Patch`, `RevertState`, and the transcript walk `undo_anchor`, `redo_anchor`, `patches_from`, `prompt_at` (read only by `ganja-core`'s `engine.rs`). |
| `src/storage_tests.rs`, `src/snapshot_tests.rs` | Unit tests. |

## Commands

```sh
cargo nextest run -p ganja-storage
cargo nextest run -p ganja-core --test persistence --test undo --test rewind   # the engine driving this crate
cargo nextest run -p ganja-core -E 'binary(/^storage_preuuid/) | binary(reasoning_downgrade)'   # raw-database fixtures
cargo depgate check --config depgate.toml   # the internal allowlist, from the repository root
```

## Conventions

- **SQLite layout:** one database per project, tables `session`, `message`, `part` plus the `migration` journal. Each row keeps identity and ordering columns beside an opaque JSON `data` column; parts are their own rows so a streaming fragment rewrites one small row. Reassembly is `ORDER BY id`, which is creation order because ids are UUIDv7 strings.
- **Writes go through one thread, reads through a second connection.** Both connections open synchronously before startup is cached; an open failure returns without a dead writer queue and the next operation retries. `spawn_writer` receives the ready connection and runs the `ganja-storage` thread fed by an `mpsc` queue. Reads use the separate WAL connection. Do not add a second writer connection.
- **Every record carries a `version`,** read before anything else decodes (`Decoded` in `src/storage.rs`). A row from a newer build is left alone; a row that will not decode is skipped with a warning and left in place.
- **Unreadable reasoning keeps a marker.** A part row whose `type` has the `REASONING_TAG` prefix but will not decode becomes a stateless `PartBody::Reasoning` (`Storage::lost_reasoning`), because the next request is built from it. Any future request-affecting part variant needs the same treatment.
- **Every connection retains shared admission** on the permanent `<database>.quarantine.lock` inode. Create the parent first, take the lease before pending/existence decisions or SQLite, and retain it until reader and writer close. Never unlink or move the lock file. Maintenance closes its own connections, releases shared admission, takes exclusive admission and rechecks current state; it never upgrades while retaining shared admission.
- **Set-asides rename, never delete user data.** Suffixes: `.corrupt-<millis>`, `.preuuid-<millis>` and `storage.migrated-<millis>`. The complete `.quarantine.pending` record precedes every database move and reserves one target plus exact component membership. Move WAL/SHM first, main last. Main publication ends companion replay; archive readers may have already checkpointed its WAL. Initialize the replacement without legacy reimport, then remove the record. Any error retains the intent and remaining captured set; recovery completes or refuses before a live SQLite open, never guesses or overwrites.
- **Snapshots are git trees, not commits.** Every git call runs `git --git-dir <ganja's own> --work-tree <project>`, so the checkout's index, HEAD and reflog are never touched. `git write-tree` produces the hash that is the snapshot; a revert is `checkout <hash> -- <file>` per file. The git directory is `<data_home>/snapshot/<slug>/<project::digest(worktree)>`.

## Gotchas

- **Pre-UUIDv7 stores are quarantined whole.** Every open checks rows with `ganja_protocol::is_uuidv7`. Admission uses one private five-second deadline and nonblocking polling; the deadline bounds waiting, not a maintainer holding exclusive admission through replacement migration. If exclusive admission remains blocked, a readable old-id generation stays live with a warning and maintenance is deferred. Pending/corrupt state or blocked shared admission returns a typed storage error, not unlocked access.
- **Only cooperating updated processes are coordinated.** Supported on one host, a local filesystem, macOS/Linux; no network-filesystem, multi-host or power-loss guarantee. Older binaries and other SQLite clients are not detected: stop them before maintenance, never downgrade while a pending intent exists, and expect an older binary afterwards to see the replacement. Restore archives offline with every archived companion; closing SQLite does not guarantee a self-contained main.
- **Debug builds use a different file.** The database is `sessions-dev.db` under `cfg!(debug_assertions)` and `sessions.db` otherwise. Use `Storage::database` to find it. Their lock names also differ but they share the legacy tree, so simultaneous first conversion by debug/release builds is unsupported.
- First creation and the legacy-tree decision hold exclusive admission, but interrupted first-ever legacy import is not resumed by quarantine recovery. A tree appearing after completed initialization is never imported.
- `Storage::open` takes the `storage/` directory, not the database path; the database sits beside it (`crates/ganja-cli/src/assemble.rs` passes `<project data dir>/storage`).
- `StorageError::Newer` (unknown migration id) and `StorageError::Foreign` (a non-empty database with no session table) refuse the whole database. Admission, recovery, IO and SQLite failures also return errors; the engine warns and continues the conversation in memory. That fallback is not persistent success and never permits an unsafe move.
- `Snapshots` is best-effort. A missing `git`, a failed spawn, an unresolvable data home or a worktree without `.git` (`Unavailable::NotAProject`) disables it in `Snapshots::new`, after which every method does nothing; `Snapshots::notice` says why.
- `rusqlite` uses the `bundled` feature so the compiled SQLite defaults are fixed; every pragma is still set explicitly, `busy_timeout` first. `journal_mode = WAL` has its own retry loop because `busy_timeout` does not cover it.

## Tests

Unit tests live in sibling files wired through `#[path]`; no inline test module exists.

Integration tests that drive storage directly live in this crate's `tests/`, including `child_environment.rs` for snapshot git isolation. Engine-driven tests live in `crates/ganja-core/tests/`: `persistence.rs`, `undo.rs`, `rewind.rs`, `plan_exit_undo.rs`, and the on-disk store binaries `storage_preuuid.rs`, `storage_preuuid_wal.rs`, `storage_preuuid_inode.rs`, `storage_preuuid_tree.rs`, `reasoning_downgrade.rs`.

Four of those open SQLite directly through `rusqlite`, a `ganja-core` dev-dependency. Each binary's `//!` header states its prerequisites.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-storage.md`, frozen from commit 35d1720. New decisions go in `docs/decisions/ledger.md`, not here.
