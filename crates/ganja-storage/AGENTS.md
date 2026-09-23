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
| `src/storage.rs` | `Storage`, `SessionInfo`, `StorageError`, the schema, migration journal, pragmas, writer thread, legacy-tree conversion and both set-aside paths (`set_aside_corrupt`, `set_aside_preuuid`, `QuarantineLock`). The module doc shows the table layout. |
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
- **Writes go through one thread, reads through a second connection.** `spawn_writer` starts the `ganja-storage` thread fed by an `mpsc` queue, so write order is the queue order. The read connection is separate and the database runs in WAL mode, so a listing never blocks the writer. Do not add a second writer connection.
- **Every record carries a `version`,** read before anything else decodes (`Decoded` in `src/storage.rs`). A row from a newer build is left alone; a row that will not decode is skipped with a warning and left in place.
- **Unreadable reasoning keeps a marker.** A part row whose `type` has the `REASONING_TAG` prefix but will not decode becomes a stateless `PartBody::Reasoning` (`Storage::lost_reasoning`), because the next request is built from it. Any future request-affecting part variant needs the same treatment.
- **Every rename of the database takes `QuarantineLock`** (`flock(2)` on `<database>.quarantine.lock` beside the database) and re-checks inside it. Never add a bare `fs::rename` of the database, and never delete the lock file.
- **Set-asides rename, never delete.** Suffixes: `.corrupt-<millis>` (will not read), `.preuuid-<millis>` (ids predate UUIDv7), and `storage.migrated-<millis>` (a legacy file tree already converted into the database).
- **Snapshots are git trees, not commits.** Every git call runs `git --git-dir <ganja's own> --work-tree <project>`, so the checkout's index, HEAD and reflog are never touched. `git write-tree` produces the hash that is the snapshot; a revert is `checkout <hash> -- <file>` per file. The git directory is `<data_home>/snapshot/<slug>/<project::digest(worktree)>`.

## Gotchas

- **Pre-UUIDv7 stores are quarantined whole.** Every open checks the rows with `ganja_protocol::is_uuidv7`; a store holding older ids is renamed to `<database>.preuuid-<millis>` (or the legacy tree to `storage.preuuid-<millis>`) under `QuarantineLock`, and a fresh empty store takes the name. Mixing old and new ids could merge two sessions into one.
- **Debug builds use a different file.** The database is `sessions-dev.db` under `cfg!(debug_assertions)` and `sessions.db` otherwise. Use `Storage::database` to find it; do not build the path by hand.
- `Storage::open` takes the `storage/` directory, not the database path; the database sits beside it (`crates/ganja-cli/src/assemble.rs` passes `<project data dir>/storage`).
- `StorageError::Newer` (unknown migration id) and `StorageError::Foreign` (a non-empty database with no session table) refuse the whole database. Every other failure is a warning; nothing here may fail a turn.
- `Snapshots` is best-effort. A missing `git`, a failed spawn, an unresolvable data home or a worktree without `.git` (`Unavailable::NotAProject`) disables it in `Snapshots::new`, after which every method does nothing; `Snapshots::notice` says why.
- `rusqlite` uses the `bundled` feature so the compiled SQLite defaults are fixed; every pragma is still set explicitly, `busy_timeout` first. `journal_mode = WAL` has its own retry loop because `busy_timeout` does not cover it.

## Tests

Unit tests live in sibling files wired through `#[path]`; no inline test module exists.

Integration tests for this crate live in `crates/ganja-core/tests/` because they test the engine driving the store: `persistence.rs`, `undo.rs`, `rewind.rs`, `plan_exit_undo.rs`, and the on-disk store binaries `storage_preuuid.rs`, `storage_preuuid_wal.rs`, `storage_preuuid_inode.rs`, `storage_preuuid_tree.rs`, `reasoning_downgrade.rs`.

Four of those open SQLite directly through `rusqlite`, a `ganja-core` dev-dependency. Each binary's `//!` header states its prerequisites.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-storage.md`, frozen from commit 35d1720. New decisions go in `docs/decisions/ledger.md`, not here.
