//! The write-ahead log and its index travel with a store set aside for holding
//! pre-UUIDv7 ids (**D493**, AC-20).
//!
//! The hazard is precise: a `-wal` left behind under the old name is recovered
//! into the *fresh* database that takes that name, which would pour the store
//! that was just set aside straight back in — old ids and all. So the three
//! files move together, and the store that replaces them starts empty.
//!
//! One test, one binary, beside its three `storage_preuuid*` siblings.

use std::{
    fs,
    path::{Path, PathBuf},
};

use ganja_core::{SessionId, Storage};
use ganja_testkit::{seeded_session_info, temp_dir};
use rusqlite::Connection;

/// The spelling ids had before **D493**.
const OLD: &str = "ses_0193b2f0a1c2000000";

/// One this build would mint.
const NEW: &str = "0198f2c4-a1b0-7000-8000-000000000001";

#[test]
fn the_wal_and_shm_companions_travel_with_the_set_aside_store() {
    let directory = temp_dir();
    let root = directory.path().join("storage");

    let planted = Storage::open(root.clone());
    planted
        .save_info(&seeded_session_info(SessionId::from(OLD.to_owned()), 7))
        .expect("the old-format record writes");
    let database = planted.database().to_path_buf();

    // A second connection, opened while the store is still open: SQLite folds
    // the log back in and deletes it when the *last* connection closes, so
    // without this there would be no `-wal` to travel and the drill would
    // prove nothing. It is also what a second `ganja` process looks like from
    // here.
    let keeper = Connection::open(&database).expect("the database opens a second time");
    let planted_rows: i64 = keeper
        .query_row("SELECT COUNT(*) FROM session", [], |row| row.get(0))
        .expect("the planted store answers");
    assert_eq!(planted_rows, 1);
    drop(planted);

    for suffix in ["-wal", "-shm"] {
        assert!(
            with_suffix(&database, suffix).exists(),
            "the drill needs a {suffix} beside the store before it starts"
        );
    }

    let storage = Storage::open(root.clone());
    assert!(
        storage
            .list_sessions()
            .expect("a superseded store opens rather than failing")
            .is_empty(),
        "a fresh store must not inherit what a stale log would have replayed"
    );
    storage
        .save_info(&seeded_session_info(SessionId::from(NEW.to_owned()), 1))
        .expect("the fresh store writes");
    assert_eq!(
        storage
            .list_sessions()
            .expect("the fresh store lists")
            .len(),
        1,
        "what replaces the set-aside store is a working one"
    );

    // All three under one stamp, which is what says they travelled together
    // rather than happening to share a prefix.
    let name = database
        .file_name()
        .expect("the database has a name")
        .to_string_lossy()
        .into_owned();
    let prefix = format!("{name}.preuuid-");
    let listing = entries(directory.path());
    let stamp = listing
        .iter()
        .find_map(|entry| {
            entry
                .strip_prefix(&prefix)
                .filter(|rest| !rest.ends_with("-wal") && !rest.ends_with("-shm"))
        })
        .unwrap_or_else(|| panic!("the store is set aside, got {listing:?}"));
    for suffix in ["", "-wal", "-shm"] {
        assert!(
            listing.contains(&format!("{prefix}{stamp}{suffix}")),
            "the {suffix:?} file must travel with its database, got {listing:?}"
        );
    }

    // Held to the end deliberately: dropping it is what lets SQLite fold the
    // set-aside log back into the set-aside database, and the assertions above
    // are about the moment before that.
    drop(keeper);
}

/// `path` with `suffix` appended to its file name, the way SQLite names the
/// two files it keeps beside a database.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .expect("the database has a name")
        .to_string_lossy()
        .into_owned();

    path.with_file_name(format!("{name}{suffix}"))
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
