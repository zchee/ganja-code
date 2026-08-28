<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-28 -->

# ganja-storage

## Purpose

Where sessions live between runs, and the working-tree snapshots `/undo` and `/rewind` walk. Two modules, born together inside `ganja-core` (`storage.rs`, `snapshot.rs`) and split into a leaf of their own in **D540** (`.omc/plans/2026-08-28-teammate-seam-crate-split.md`, W3) — the `634d8dd` provider-split procedure, applied a second time on the other side of the engine. Its own crate for the reason `ganja-tool` and `ganja-provider` are ones of theirs: a session store answers to a project's worktree and to what a stored record decodes to, never to the loop that calls it, and with the engine outside its dependency graph that is the compiler's rule rather than a convention. `ganja-core` re-exports both modules whole — `ganja_core::storage`, `ganja_core::snapshot`, and the root's own `SessionId`, `SessionInfo`, `Storage`, `StorageError`, `RevertState`, `Snapshots` — so no caller outside this crate had to change a path.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest, `publish = false`. Every dependency carries the reason it is there — `rusqlite` (`bundled`, for the pragma defaults the storage layer relies on), `tokio` (the snapshot repository's own git child processes; `io-util`/`process`/`sync`/`time` only, none of it needed by `storage.rs`, which is plain `std::thread`), `ganja-permission` (`project`, for the worktree and the data home both modules are anchored on) and `ganja-protocol` (the message/part types a stored record decodes to). |
| `src/lib.rs` | Crate doc and `pub mod storage; pub mod snapshot;` — nothing else. |
| `src/storage.rs` | One SQLite database per project, `session`/`message`/`part` with opaque JSON `data` columns, upstream's migration journal and pragma set (`packages/core/src/database/{database,migration,schema.gen}.ts`; the *machinery* is ported, not upstream's 38 migrations — this tree starts at its own base schema). Writes are serialized through a dedicated thread so their order is structural rather than conventional; reads take a second connection, which is what WAL buys. Every record still carries an explicit `version`, so one a newer build wrote is left alone and one that will not decode costs its own row and nothing else — with one named exception: a part row whose `type` names reasoning is *request-affecting* state, so the message keeps a stateless `PartBody::Reasoning` marker in its place rather than silently losing continuity the next request would otherwise ask about. A `storage/` tree from the old file layout converts on first open and is renamed rather than deleted. A store whose ids predate UUIDv7 (**D493**) is a third, distinct outcome, decided before anything is carried and under its own advisory lock (`QuarantineLock`, `flock(2)` on `sessions.db.quarantine.lock`, never removed): the whole tree, or the database with its write-ahead log, is set aside as `*.preuuid-<millis>` and a fresh, empty store takes the name — because ids minted by the old `<millis hex><process-local counter hex>` scheme were guaranteed to collide across two `ganja` processes started together, and mixing such rows with UUIDv7 ones would fuse two sessions into one. |
| `src/snapshot.rs` | Git snapshots of the working tree, in a **separate git directory** of ganja's own under the data home — every command `git --git-dir <ours> --work-tree <the project>`, so nothing here can touch the checkout's own index, HEAD or reflog. No commits: `write-tree` names a tree and that hash *is* the snapshot; a revert is `checkout <hash> -- <file>` per file. **Nothing here may fail a turn** — a git that will not spawn, a missing binary, or a data directory that cannot be resolved disables the subsystem at construction, and every entry point then does nothing at all; the engine never branches on whether a snapshot succeeded. |

## Architecture

**Why one crate and not two.** `storage.rs` and `snapshot.rs` share nothing at the type level beyond one intra-crate link — a `SessionInfo` carries an `Option<RevertState>` naming how far an `/undo` walked — but they share every fact that decides the *boundary*: both are anchored on a project's worktree (`ganja_permission::project`), both decode or produce values `ganja_protocol` declares, and neither has ever needed the engine that calls it. Splitting them into two single-purpose crates would buy nothing this dependency list does not already state, at the cost of a second manifest for one intra-crate reference to cross.

**The SQLite store.** `rusqlite`'s `bundled` feature compiles the exact SQLite this workspace was validated against rather than linking whatever the platform ships, because the pragma *defaults* the storage layer relies on (`SQLITE_DEFAULT_FOREIGN_KEYS=1`, `SQLITE_DEFAULT_WAL_SYNCHRONOUS=2`) are build-time properties that differ between system libraries — pinning the library is what pins the semantics, and every pragma is still set explicitly on every connection regardless. `busy_timeout` is set **first**, ahead of upstream's own order, because a connection that meets a busy database before its timeout is set fails outright instead of waiting; `journal_mode` is not in that pragma list at all, since it is a property of the *file* rather than of the connection and gets its own retry loop (`wal`) for the brief exclusive lock a journal-mode switch needs without `busy_timeout`'s cover.

**The D493 quarantine.** A store minted before ids became UUIDv7 is not a schema question — there is no version to bump for it, and such a store is structurally identical to a current one — so it is asked of the rows themselves, cheaply and unlocked, on every open a project ever has. The one open in a project's life that answers yes takes an exclusive advisory lock and asks again, against the file the path names *at that moment* rather than the one this connection opened on, because the rename that quarantines a store is not a database operation and no SQLite transaction spans it: two processes racing this decision without the lock can each rename the other's fresh replacement, which is exactly the corruption the lock exists to prevent.

**The snapshots `/undo` walks.** `Snapshots` is deliberately best-effort at every layer: a git spawn failure, a missing binary or an unresolvable data directory disables the whole subsystem at construction rather than at the first call, and every method after that is a no-op rather than an error a caller has to handle. `undo_anchor`, `redo_anchor`, `patches_from` and `prompt_at` are the walk over a session's transcript that decides what an `/undo` or `/rewind` targets; `ganja-core`'s `engine.rs` is what drives that walk and turns its answer into an event, which is why those four functions are `pub` here and read nowhere but there.

## For AI Agents

### Working In This Directory

- **This crate may name `ganja-permission` and `ganja-protocol`, and nothing else of ours.** CI's closed allowlist (`ganja-permission ganja-protocol `) is the assertion; a change that appears to need the engine — a config value, a permission decision, a running turn — needs a plain value handed in by the caller instead, the same discipline `ganja-tool`'s `ToolCtx` and `ganja-team`'s `TeamsRoot` already keep.
- **Nothing here may fail a turn.** `snapshot.rs`'s own module doc says so for git, and `storage.rs` holds the same rule for a different reason: a write that cannot be answered comes back as a `StorageError` the caller already handles (the turn task warns and carries on, exactly as it did when the write was a bare `rename`), and a record that will not decode is skipped with a warning rather than propagated as a hard failure — the one exception is the two whole-database refusals (`StorageError::Newer`, `StorageError::Foreign`), which exist precisely because guessing at somebody else's database is worse than refusing to open it.
- **A part row that cannot be read is not always a part row that costs only itself.** `Storage::lost_reasoning` is the one place granularity moves from *the record* to *the record plus a marker*, because `PartBody::Reasoning` is request-affecting state the next request is built from; any future part variant with the same property earns the same treatment, not a silent drop.
- **The quarantine's lock is never removed.** Unlinking `sessions.db.quarantine.lock` would let a later pair of processes race exactly as if there were no lock at all; it is created once per project's life, the moment a store that actually predates UUIDv7 is found, and stays.

### Testing Requirements

```sh
cargo test -p ganja-storage            # the sibling suites travelled with the files
cargo nextest run --workspace          # the integration suites below, exercised through the engine
```

Unit tests are sibling files (`storage_tests.rs`, `snapshot_tests.rs`) declared `#[cfg(test)] #[path = "..."] mod tests;` beside the modules they cover — the pattern the whole workspace uses. The **integration** suites that exercise this crate stay in `crates/ganja-core/tests` (`persistence.rs`, `undo.rs`, `rewind.rs`, `plan_exit_undo.rs`, `tool_defer.rs`, and the four binaries that open a raw fixture database through `rusqlite` itself — `storage_preuuid.rs`, `storage_preuuid_wal.rs`, `storage_preuuid_inode.rs`, `reasoning_downgrade.rs`), because what they are really testing is the engine driving this crate through a turn, not this crate alone — moving them here would test a shape no caller uses. Tests that write a store redirect `XDG_DATA_HOME` so they cannot touch the real user's sessions.

### Common Patterns

- A record's `version` field is read before anything else decodes, so a build never mistakes "I cannot parse this" for "this is corrupt" — see `Decoded` in `storage.rs`.
- Every filesystem set-aside is a rename, never a delete: `QUARANTINE`, `PREUUID` and `MIGRATED` are three reasons a path moves, told apart by suffix, and none of them destroys what it moves.

## Dependencies

### Internal

`ganja-permission` (`project`, for the worktree and the data home both modules anchor on) and `ganja-protocol` (the message/part types a stored record decodes to, and `SessionId`, re-exported here as `storage::SessionId` for the callers that already read it at that path). Nothing else — asserted closed in CI, the same allowlist form every other member's boundary takes.

### External

`rusqlite` (`bundled`; the session store — the only member whose *normal* manifest names it since **D540**), `tokio` (`io-util`, `process`, `sync`, `time`; the snapshot repository's own git child processes — `storage.rs` needs none of it), `serde`/`serde_json` (every stored record's envelope), `thiserror` (`StorageError`), `tracing` (every quarantine, conversion and decode failure is `tracing::warn!`, never a silent drop). `tempfile` for the tests.

<!-- MANUAL: -->
