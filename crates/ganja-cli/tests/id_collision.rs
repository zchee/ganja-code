//! The concurrent-writers drill, kept as a regression test (**D493**,
//! `ganja-code-76w`).
//!
//! Run live on 2026-08-16 (the Claude teammates reference, §10.13), six `ganja
//! run` processes released together into one project lost nine of 46 sessions —
//! not to an error, but to *fusion*: `ganja_protocol::ascending` minted
//! `{prefix}_{millis}{counter}` from a **process-local** counter starting at
//! zero, so two processes reaching engine construction in the same millisecond
//! did not merely risk the same session id, they were guaranteed it. Two
//! conversations then landed in one row; and with the session id shared, the
//! message and part composite keys lost their discriminating column, so an
//! upsert silently overwrote the other run's rows. Exit 0 every time, no log
//! line, nothing to see. The mint is standard UUIDv7 now, and this file is what
//! stops the old layout coming back.
//!
//! **Why a barrier and not a tight spawn loop.** What collides is the id minted
//! at engine construction, so the drill has to put two processes *there* in the
//! same millisecond — and the spread between six `fork`/`exec` pairs issued in
//! a loop is not small next to that. Each child is therefore a shell that
//! announces itself, waits for a release file to appear, and only then becomes
//! `ganja run`: the release is one `stat` call wide for every process at once.
//! The wait is a spin rather than a sleep for the same reason — a sleep cheap
//! enough to use is long enough to hide the thing being tested — and it cannot
//! outlive its test, because the loop also watches a liveness file this
//! harness's temporary directory owns and exits when it goes. It works: the
//! millisecond fields of the ids three rounds minted here span 19, 7 and 7 ms,
//! and two of the three rounds had *two pairs* of processes mint inside one
//! millisecond — the exact condition under which the old layout produced
//! identical ids rather than merely risking them.
//!
//! **Why the store is the witness.** A fused session is not a message anybody
//! prints: the process exits 0 and the listing simply shows one conversation
//! where two ran. So the assertion is a count of `session` rows read back
//! through `ganja_core::Storage` — the same reader the binary uses — which
//! cannot report a duplicate id even in principle, because `id` is that table's
//! key. Six processes that minted five ids store five rows, and that is exactly
//! the shape this counts.
//!
//! Unix only: the barrier is a POSIX shell, and this build's windows lane is
//! parked. No upstream opencode counterpart — the id layout drilled here was
//! ganja's own.

#![cfg(unix)]

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ganja_core::{SessionId, Storage};
use ganja_protocol::{Message, Part, Role, is_uuidv7};
use ganja_testkit::Homes;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;

/// How many processes are released together — the N the live drill ran at.
const PROCESSES: usize = 6;

/// How many times the drill is repeated, each round in its own project and its
/// own data home. One green round of a race proves less than three do.
const ROUNDS: usize = 3;

/// What the fake provider's one turn says. One word, appearing nowhere else.
const CLOSING: &str = "script-finished-zarquon";

/// The script file every run in a drill plays.
const SCRIPT: &str = "script.json";

/// The directory the barrier files of one release live in, under the project.
const BARRIER: &str = ".barrier";

/// What `storage.rs` renames a pre-UUIDv7 store to, ahead of the millisecond
/// stamp: `sessions.db.preuuid-<millis>`.
const PREUUID: &str = "preuuid";

/// A session id in the layout this drill exists to have abolished — the
/// `<prefix>_<11 hex millis><6 hex counter>` the old mint wrote.
const OLD_ID: &str = "ses_0193b2f0a1c2000000";

/// How long one release is given to run to completion.
///
/// Generous on the crate's own idiom: a timeout here should mean "hung", not
/// "a debug build of six agents on a loaded machine".
const RELEASE_DEADLINE: Duration = Duration::from_secs(180);

/// How long the children are given to reach the barrier before being killed.
const READY_DEADLINE: Duration = Duration::from_secs(60);

/// How often the harness looks at the filesystem or at a child.
const POLL: Duration = Duration::from_millis(20);

/// What every child runs: announce, wait, become `ganja run`.
///
/// The paths and the prompt arrive through the environment rather than through
/// this string, so nothing here has to be quoted and no temporary directory's
/// name can change what the shell parses. Exit 97 is the liveness bail-out —
/// it is never expected, and a run that produced it would fail the exit-status
/// assertion by name.
const WAIT_THEN_RUN: &str = concat!(
    r#": > "$DRILL_READY"; "#,
    r#"while [ ! -e "$DRILL_GO" ]; do [ -e "$DRILL_ALIVE" ] || exit 97; done; "#,
    r#"exec "$DRILL_BIN" run "$DRILL_PROMPT""#,
);

/// One round: a project, the data home its runs store under, and a counter so
/// that two releases in one round cannot inherit each other's barrier files.
struct Drill {
    homes: Homes,
    releases: Cell<usize>,
}

impl Drill {
    fn new() -> Self {
        let homes = Homes::new();
        homes.script(SCRIPT, json!([{"text": CLOSING}]));
        fs::create_dir(homes.project().join(BARRIER)).expect("the barrier directory is creatable");

        Self { homes, releases: Cell::new(0) }
    }

    fn path(&self) -> &Path {
        self.homes.project()
    }

    /// Pins everything that could decide what a run does onto this drill's
    /// own directories ([`Homes::pin`]): a developer's global config can
    /// choose a provider, and their cached catalog can decide what a model is
    /// sized at, so all of it moves or none of it has moved.
    fn pinned(&self, command: &mut Command) {
        self.homes.pin(command, &self.path().join(SCRIPT));
    }

    /// Spawns `count` runs, releases them together, and returns once every one
    /// of them has exited 0.
    ///
    /// A non-zero exit fails here rather than downstream, naming that child's
    /// captured standard error: a drill whose processes did not all run has
    /// nothing to say about the ids they would have minted.
    fn released_together(&self, count: usize) {
        let release = self.releases.get();
        self.releases.set(release + 1);

        let barrier = self.path().join(BARRIER).join(release.to_string());
        fs::create_dir(&barrier).expect("this release's barrier directory is creatable");

        let alive = barrier.join("alive");
        let go = barrier.join("go");
        File::create(&alive).expect("the liveness file is creatable");

        let mut children: Vec<Child> = (0..count)
            .map(|index| {
                let mut command = Command::new("sh");
                command
                    .arg("-c")
                    .arg(WAIT_THEN_RUN)
                    .env("DRILL_READY", barrier.join(format!("ready.{index}")))
                    .env("DRILL_GO", &go)
                    .env("DRILL_ALIVE", &alive)
                    .env("DRILL_BIN", env!("CARGO_BIN_EXE_ganja"))
                    .env("DRILL_PROMPT", format!("drill {index}"))
                    // A closed pipe rather than the harness's stdin: `run`
                    // reads standard input whole when it is not a terminal.
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(created(&barrier, "out", index)))
                    .stderr(Stdio::from(created(&barrier, "err", index)));
                self.pinned(&mut command);

                command.spawn().expect("the shell is spawnable")
            })
            .collect();

        wait_for(
            &mut children,
            READY_DEADLINE,
            || (0..count).all(|index| barrier.join(format!("ready.{index}")).exists()),
            "every process to reach the barrier",
        );

        // The release itself. Every child is one `stat` away from `exec`.
        File::create(&go).expect("the release file is creatable");

        let statuses = reaped(&mut children);
        for (index, status) in statuses.iter().enumerate() {
            assert!(
                status.success(),
                "process {index} exited {status}\n--- stderr ---\n{}",
                read(&barrier.join(format!("err.{index}"))),
            );
        }
    }

    /// The store this drill's runs wrote into — found rather than computed
    /// ([`Homes::store`]): runs that stored under two projects stored under
    /// the wrong one, and a count taken from either would be a count of half
    /// a drill.
    fn store(&self) -> Storage {
        self.homes.store()
    }
}

/// One child's captured stream, as a file the child may inherit.
fn created(barrier: &Path, stream: &str, index: usize) -> File {
    File::create(barrier.join(format!("{stream}.{index}"))).expect("a capture file is creatable")
}

/// Whatever is at `path`, or an explanation of why there is nothing.
fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|failure| format!("<unreadable: {failure}>"))
}

/// Polls `ready` until it holds, killing every child and failing on the
/// deadline: a harness that left six spinning shells behind would outlive the
/// `cargo test` run that started them.
fn wait_for(children: &mut [Child], deadline: Duration, ready: impl Fn() -> bool, what: &str) {
    let until = Instant::now() + deadline;

    while !ready() {
        if Instant::now() >= until {
            kill_all(children);
            panic!("waited {deadline:?} for {what}");
        }
        thread::sleep(POLL);
    }
}

/// Waits for every child to exit, under the release deadline, and returns what
/// each exited with in spawn order.
fn reaped(children: &mut [Child]) -> Vec<ExitStatus> {
    let until = Instant::now() + RELEASE_DEADLINE;
    let mut statuses: Vec<Option<ExitStatus>> = vec![None; children.len()];

    loop {
        let mut pending = 0_usize;
        for (index, child) in children.iter_mut().enumerate() {
            if statuses[index].is_some() {
                continue;
            }
            match child.try_wait().expect("a child's status is readable") {
                Some(status) => statuses[index] = Some(status),
                None => pending += 1,
            }
        }

        if pending == 0 {
            return statuses.into_iter().flatten().collect();
        }
        if Instant::now() >= until {
            kill_all(children);
            panic!("{pending} of {} processes never exited", children.len());
        }
        thread::sleep(POLL);
    }
}

fn kill_all(children: &mut [Child]) {
    for child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Every stored column, including opaque record bytes, is part of the oracle.
fn logical_rows(database: &Path) -> BTreeMap<String, Vec<Vec<rusqlite::types::Value>>> {
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("the complete set must open with ordinary WAL-aware SQLite reads");
    ["session", "message", "part"]
        .into_iter()
        .map(|table| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2, 3"))
                .expect("every retained table is readable");
            let columns = statement.column_count();
            let rows = statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|index| row.get(index))
                        .collect::<Result<Vec<rusqlite::types::Value>, _>>()
                })
                .expect("retained rows read")
                .collect::<Result<Vec<_>, _>>()
                .expect("every retained value decodes");
            (table.to_owned(), rows)
        })
        .collect()
}

fn copy_database_set(source: &Path, target: &Path) {
    let name = source.file_name().expect("source database basename").to_string_lossy();
    let target_name = target.file_name().expect("target database basename").to_string_lossy();
    for suffix in ["", "-wal", "-shm"] {
        let from = source.with_file_name(format!("{name}{suffix}"));
        let to = target.with_file_name(format!("{target_name}{suffix}"));
        match from.try_exists() {
            Ok(true) => {
                fs::copy(&from, &to).unwrap_or_else(|error| {
                    panic!("copy {} to {}: {error}", from.display(), to.display())
                });
            }
            Ok(false) if !suffix.is_empty() => {}
            Ok(false) => panic!("archive main {} is missing", from.display()),
            Err(error) => panic!("inspect archive component {}: {error}", from.display()),
        }
    }
}

#[test]
fn the_archive_oracle_rejects_a_foreign_wal_even_when_the_old_id_bytes_remain() {
    let directory = tempfile::TempDir::new().expect("oracle fixture");
    let old = directory.path().join("old.db");
    let foreign = directory.path().join("foreign.db");
    let schema = "CREATE TABLE session(id TEXT, parent_id TEXT, data TEXT); CREATE TABLE message(id TEXT, session_id TEXT, data TEXT); CREATE TABLE part(id TEXT, message_id TEXT, data TEXT);";
    let connection = Connection::open(&old).expect("old main fixture");
    connection.execute_batch(schema).expect("old schema");
    connection
        .execute("INSERT INTO session VALUES (?1, NULL, 'old logical row')", [OLD_ID])
        .expect("old row");
    drop(connection);
    let expected = logical_rows(&old);
    let connection = Connection::open(&foreign).expect("foreign generation");
    connection.execute_batch("PRAGMA journal_mode=WAL;").expect("foreign WAL mode");
    connection.execute_batch(schema).expect("foreign schema lives in WAL");
    connection
        .set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .expect("retain foreign WAL");
    drop(connection);
    fs::copy(directory.path().join("foreign.db-wal"), directory.path().join("old.db-wal"))
        .expect("stage the observed generation mismatch");
    let bytes = fs::read(&old).expect("main retained");
    assert!(
        bytes.windows(OLD_ID.len()).any(|window| window == OLD_ID.as_bytes()),
        "a byte-only witness would accept this retained main"
    );
    assert_ne!(
        logical_rows(&old),
        expected,
        "ordinary WAL-aware queries must reject the foreign-log set even though immutable or byte-only reads see old data"
    );
}

// ---------------------------------------------------------------------------
// The drill itself.
// ---------------------------------------------------------------------------

/// **AC-17.** N processes released together store N sessions.
///
/// Under the old mint this failed at the first round the machine was fast
/// enough to reach engine construction twice in one millisecond — which the
/// live drill hit in six of eleven rounds.
#[test]
fn n_processes_started_together_mint_n_sessions() {
    for round in 1..=ROUNDS {
        let drill = Drill::new();
        drill.released_together(PROCESSES);

        let sessions = drill.store().list_sessions().expect("the store lists its sessions");
        let ids: Vec<&str> = sessions.iter().map(|info| info.id.as_str()).collect();

        // A count, not a distinctness check: `id` is the `session` table's key,
        // so two processes that minted the same one leave *one* row and the
        // second conversation is inside it. Fusion is a shortfall here.
        assert_eq!(
            sessions.len(),
            PROCESSES,
            "round {round}: {PROCESSES} processes left {} sessions — {ids:?}",
            sessions.len(),
        );
        assert!(
            ids.iter().all(|id| is_uuidv7(id)),
            "round {round}: a stored session id is not the mint's spelling — {ids:?}",
        );
    }
}

/// Two processes opening the same pre-UUIDv7 store leave one readable archive
/// and persist both new transcripts without inheriting any previous ids.
///
/// The store is planted rather than found: a run creates it (so the project
/// slug stays `ganja-permission`'s to decide), and one row is then rewritten
/// under an id in the abolished layout, which is precisely what the quarantine
/// probe looks for.
#[test]
fn two_processes_racing_the_quarantine_leave_one_aside_file() {
    let drill = Drill::new();

    // One process, so nothing races yet: this exists only to create the store.
    drill.released_together(1);

    let (database, old_ids) = {
        let store = drill.store();
        let mut info = store
            .list_sessions()
            .expect("the store lists its sessions")
            .pop()
            .expect("the first run stored a session");
        info.id = SessionId::from(OLD_ID.to_owned());
        info.title = Some("distinctive retained session title".to_owned());
        store.save_info(&info).expect("the old-format row is writable");
        let mut message = Message::assistant("archive-fixture");
        message.parts.push(Part::text("distinctive retained transcript content"));
        store.save_message(&info.id, &message).expect("old message stores");
        for part in &message.parts {
            store.save_part(&info.id, &message.id, part).expect("old part stores");
        }
        assert_eq!(
            store.load_transcript(&info.id).expect("seeded transcript reads"),
            vec![message]
        );

        // Read back through the same handle rather than trusted: a race
        // against a store that never held an old id would find nothing set
        // aside and would say so in exactly the words a working quarantine
        // earns.
        assert!(
            store.load_info(&info.id).expect("the store answers about the planted row").is_some(),
            "the pre-UUIDv7 row is not in the store the race is about to open",
        );

        let old_ids: BTreeSet<_> = store
            .list_sessions()
            .expect("seeded sessions read")
            .into_iter()
            .map(|info| info.id.as_str().to_owned())
            .collect();
        (store.database().to_path_buf(), old_ids)
    };
    let baseline = tempfile::TempDir::new().expect("disposable seed query directory");
    let seed_copy = baseline.path().join("seed.db");
    copy_database_set(&database, &seed_copy);
    let expected = logical_rows(&seed_copy);

    // Two now, together, against a store both must decide about at once.
    drill.released_together(2);

    let directory = database.parent().expect("the database has a directory");
    let name =
        database.file_name().expect("the database has a name").to_string_lossy().into_owned();
    let marker = format!("{name}.{PREUUID}-");

    // Companions share the reserved archive prefix and are not additional mains.
    let mut aside: Vec<PathBuf> = fs::read_dir(directory)
        .expect("the project directory is readable")
        .map(|entry| entry.expect("each archive directory entry is readable").path())
        .filter(|path| {
            let file = path.file_name().unwrap_or_default().to_string_lossy();

            file.starts_with(&marker) && !file.ends_with("-wal") && !file.ends_with("-shm")
        })
        .collect();
    aside.sort();

    assert_eq!(
        aside.len(),
        1,
        "two processes racing one pre-UUIDv7 store left {} aside files: {aside:?}",
        aside.len(),
    );

    // Both children and every test-owned Storage have closed. Keep the captured
    // set untouched, and let ordinary SQLite consume only a second query copy.
    let retained = tempfile::TempDir::new().expect("retained archive directory");
    let evidence = retained.path().join("retained.db");
    copy_database_set(&aside[0], &evidence);
    let query = tempfile::TempDir::new().expect("disposable query directory");
    let copied = query.path().join("query.db");
    copy_database_set(&evidence, &copied);
    assert_eq!(
        logical_rows(&copied),
        expected,
        "every old session/message/part column and content must survive an ordinary WAL-aware read"
    );

    let live = drill.store();
    let sessions = live.list_sessions().expect("the fresh store lists its sessions");
    let ids: Vec<&str> = sessions.iter().map(|info| info.id.as_str()).collect();
    assert_eq!(sessions.len(), 2, "both racing sessions must persist — {ids:?}");
    assert!(
        ids.iter().all(|id| is_uuidv7(id) && !old_ids.contains(*id)),
        "the live generation must contain only new ids — {ids:?}"
    );
    let mut prompts = BTreeSet::new();
    for session in sessions {
        let transcript = live.load_transcript(&session.id).expect("new transcript reads");
        assert_eq!(transcript.len(), 2, "each child persists its prompt and reply");
        assert_eq!(transcript[0].role, Role::User);
        assert_eq!(transcript[1].role, Role::Assistant);
        let text = |message: &Message| {
            message.parts.iter().filter_map(|part| part.as_text()).collect::<String>()
        };
        prompts.insert(text(&transcript[0]));
        assert_eq!(
            text(&transcript[1]),
            CLOSING,
            "a successful child must retain its complete reply"
        );
    }
    assert_eq!(prompts, BTreeSet::from(["drill 0".to_owned(), "drill 1".to_owned()]));
}
