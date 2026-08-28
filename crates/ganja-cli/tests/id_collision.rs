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
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ganja_core::{SessionId, Storage};
use ganja_protocol::is_uuidv7;
use ganja_testkit::Homes;
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

/// Whether `haystack` holds `needle` anywhere in it.
fn carries(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
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

/// The other half of D493's drill: two processes opening the same pre-UUIDv7
/// store must leave **one** aside file, not two, and must delete nothing.
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

    let database = {
        let store = drill.store();
        let mut info = store
            .list_sessions()
            .expect("the store lists its sessions")
            .pop()
            .expect("the first run stored a session");
        info.id = SessionId::from(OLD_ID.to_owned());
        store.save_info(&info).expect("the old-format row is writable");

        // Read back through the same handle rather than trusted: a race
        // against a store that never held an old id would find nothing set
        // aside and would say so in exactly the words a working quarantine
        // earns.
        assert!(
            store.load_info(&info.id).expect("the store answers about the planted row").is_some(),
            "the pre-UUIDv7 row is not in the store the race is about to open",
        );

        store.database().to_path_buf()
    };

    // Two now, together, against a store both must decide about at once.
    drill.released_together(2);

    let directory = database.parent().expect("the database has a directory");
    let name =
        database.file_name().expect("the database has a name").to_string_lossy().into_owned();
    let marker = format!("{name}.{PREUUID}-");

    // `set_aside` stamps before it suffixes — `sessions.db.preuuid-<millis>`,
    // then `…-wal` and `…-shm` — so the companions match the same prefix and
    // are excluded by their tails rather than counted as second quarantines.
    let mut aside: Vec<PathBuf> = fs::read_dir(directory)
        .expect("the project directory is readable")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
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

    let sessions = drill.store().list_sessions().expect("the fresh store lists its sessions");
    let ids: Vec<&str> = sessions.iter().map(|info| info.id.as_str()).collect();

    assert_eq!(
        sessions.len(),
        2,
        "the store the race left behind holds the two sessions it created — {ids:?}",
    );
    assert!(
        ids.iter().all(|id| is_uuidv7(id)),
        "the store the race left behind carries a pre-UUIDv7 id — {ids:?}",
    );

    // Nothing was deleted. The row is looked for in the set-aside database and
    // in its companions, because closing a connection does not promise a
    // checkpointed file: an id that is still in the write-ahead log is an id
    // that is still there.
    let base = aside[0].to_string_lossy().into_owned();
    let scan = || {
        ["", "-wal", "-shm"].into_iter().any(|suffix| {
            fs::read(format!("{base}{suffix}"))
                .is_ok_and(|bytes| carries(&bytes, OLD_ID.as_bytes()))
        })
    };
    // Repeated for a while rather than read once: a checkpoint still in
    // flight moves the row's page from the log into the file *between* one
    // read and the next — the file read before the page landed, the log after
    // it was reset — and a single pass through the three can miss it in both
    // (seen on the three-core macOS runner, 2026-08-24; bead ganja-code-bry).
    // The row is there throughout; only where it is moves.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut kept = scan();
    while !kept && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        kept = scan();
    }

    assert!(kept, "the set-aside store no longer carries the row it was set aside for: {base}",);
}
