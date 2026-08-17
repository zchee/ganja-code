//! A quarantine whose file moved underneath it reopens instead of renaming
//! (**D493**, AC-28).
//!
//! The `IMMEDIATE` transaction the probe runs in is not enough on its own,
//! because no SQLite lock survives a `rename(2)`: the loser of a two-process
//! race keeps its descriptor on the old inode, re-reads the same old ids
//! there, and would set aside whatever now holds the path — which is the fresh,
//! empty store the winner had just created. A second aside file is the visible
//! symptom; a project that looked emptied twice is the felt one.
//!
//! Reproducing that state needs no seam in the store, and deliberately adds
//! none. It needs only the two halves in the right order, and SQLite's own
//! write lock is what holds them there: a connection this test owns takes
//! `BEGIN IMMEDIATE` on the planted database, so a real [`Storage`] opening
//! the same project parks inside `migrate` **with its connection already open
//! on that file**; the test then plays the winner — renaming the file aside and
//! creating a fresh store at the name, exactly as the winner does — and only
//! then releases the lock. What the parked store does next is the whole
//! assertion.
//!
//! One test, one binary, beside its three `storage_preuuid*` siblings.

use std::{fs, path::Path, thread, time::Duration};

use ganja_core::{SessionId, Storage};
use ganja_testkit::{seeded_session_info, temp_dir};
use rusqlite::Connection;

/// The spelling ids had before **D493**.
const OLD: &str = "ses_0193b2f0a1c2000000";

/// One this build would mint.
const NEW: &str = "0198f2c4-a1b0-7000-8000-000000000001";

/// Long enough for the parked store to have connected and reached the lock,
/// and far short of the store's own five-second `busy_timeout`, which is what
/// decides whether it waits there or gives up.
const PARKED: Duration = Duration::from_millis(300);

#[test]
fn a_quarantine_refuses_when_the_inode_moved_under_it() {
    let directory = temp_dir();
    let root = directory.path().join("storage");

    {
        let planted = Storage::open(root.clone());
        planted
            .save_info(&seeded_session_info(SessionId::from(OLD.to_owned()), 7))
            .expect("the old-format record writes");
    }
    let database = Storage::open(root.clone()).database().to_path_buf();

    // The write lock, held on the file the loser is about to open.
    let blocker = Connection::open(&database).expect("the database opens a second time");
    blocker
        .execute_batch("PRAGMA busy_timeout = 5000; BEGIN IMMEDIATE;")
        .expect("the lock is taken");

    let name = database
        .file_name()
        .expect("the database has a name")
        .to_string_lossy()
        .into_owned();
    let aside = database.with_file_name(format!("{name}.preuuid-1755300000000"));

    let loser = Storage::open(root.clone());
    let listed = thread::scope(|scope| {
        let parked = scope.spawn(|| loser.list_sessions());
        thread::sleep(PARKED);

        // The winner's whole move, performed by this test while the loser is
        // parked: the old store renamed aside under the suffix and with the
        // two companions the store itself moves, and a fresh one created at
        // the name it left.
        for suffix in ["", "-wal", "-shm"] {
            let from = database.with_file_name(format!("{name}{suffix}"));
            if from.exists() {
                let to = aside.with_file_name(format!(
                    "{}{suffix}",
                    aside
                        .file_name()
                        .expect("the aside path has a name")
                        .to_string_lossy()
                ));
                fs::rename(&from, &to).expect("the winner sets the old store aside");
            }
        }
        Storage::open(root.clone())
            .save_info(&seeded_session_info(SessionId::from(NEW.to_owned()), 1))
            .expect("the winner's fresh store writes");

        blocker
            .execute_batch("COMMIT")
            .expect("the lock is released");

        parked.join().expect("the parked store does not panic")
    });
    // Dropped before the set-aside store is read back, so nothing about that
    // read depends on a connection this drill happens to be holding.
    drop(blocker);
    drop(loser);

    // The loser read the old ids off its own descriptor and still did not
    // rename. What decides that is the quarantine lock and the re-probe taken
    // inside it: the loser lets its connection go before it waits, and by the
    // time it holds the lock and asks again, the path names the winner's fresh
    // store and has nothing old in it. The inode compare is the belt behind
    // that brace — it guards the unlocked rename `Inner::start` performs on an
    // *unreadable* database — and this drill never reaches it.
    let listed = listed.expect("the parked store opens rather than failing");
    assert_eq!(
        listed
            .iter()
            .map(|info| info.id.as_str())
            .collect::<Vec<_>>(),
        vec![NEW],
        "the loser must go on with the store that replaced the one it had open"
    );

    let listing = entries(directory.path());
    let set_aside: Vec<&String> = listing
        .iter()
        .filter(|entry| {
            entry.starts_with(&format!("{name}.preuuid-"))
                && !entry.ends_with("-wal")
                && !entry.ends_with("-shm")
        })
        .collect();
    assert_eq!(
        set_aside.len(),
        1,
        "only the winner's aside file may exist; a second one is the loser \
         setting the fresh store aside, got {listing:?}"
    );

    // And the one that is there is the *old* store rather than the fresh one:
    // a guard that fired for the wrong reason would leave an empty file here.
    let kept = Connection::open(&aside).expect("the set-aside store opens like any other database");
    let held: i64 = kept
        .query_row("SELECT COUNT(*) FROM session WHERE id = ?1", [OLD], |row| {
            row.get(0)
        })
        .expect("the set-aside store still answers");
    assert_eq!(
        held, 1,
        "the set-aside store is the one that held the old ids"
    );
}

/// Everything directly inside `directory`, by name.
fn entries(directory: &Path) -> Vec<String> {
    fs::read_dir(directory)
        .expect("the directory lists")
        .map(|entry| {
            entry
                .expect("the entry reads")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}
