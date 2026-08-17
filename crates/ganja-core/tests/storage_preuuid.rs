//! A store whose sessions were minted before UUIDv7 ids is set aside rather
//! than written into, and what was set aside is still there afterwards
//! (**D493**, AC-17).
//!
//! No upstream opencode counterpart: upstream never changed the shape of an
//! id, so it has nothing to survive here. One test, one binary — the rule this
//! directory keeps for anything that touches stored state, and the four
//! `storage_preuuid*` binaries keep it together.

use std::{fs, path::Path};

use ganja_core::{SessionId, Storage};
use ganja_testkit::{seeded_session_info, temp_dir};
use rusqlite::Connection;

/// The spelling ids had before **D493**: `<prefix>_<millis hex><counter hex>`,
/// where the counter started at zero in every process.
const OLD: &str = "ses_0193b2f0a1c2000000";

/// One this build would mint.
const NEW: &str = "0198f2c4-a1b0-7000-8000-000000000001";

#[test]
fn a_store_with_old_format_ids_is_set_aside_and_a_fresh_one_created() {
    let directory = temp_dir();
    let root = directory.path().join("storage");
    let old = SessionId::from(OLD.to_owned());

    // The store a build from before the change left behind.
    {
        let planted = Storage::open(root.clone());
        planted
            .save_info(&seeded_session_info(old.clone(), 7))
            .expect("the old-format record writes");
    }
    // `open` does no I/O, so asking a throwaway handle where the database is
    // costs nothing and beats spelling the debug suffix out here.
    let database = Storage::open(root.clone()).database().to_path_buf();
    assert!(database.is_file(), "the planted store is on disk");

    let storage = Storage::open(root.clone());
    assert!(
        storage
            .list_sessions()
            .expect("a superseded store opens rather than failing")
            .is_empty(),
        "a store whose ids predate UUIDv7 must not be read"
    );
    assert!(
        storage
            .load_info(&old)
            .expect("the fresh store reads")
            .is_none(),
        "and none of its sessions may be reachable through the store that replaced it"
    );

    // What replaces it is a working store, not merely an empty one.
    let fresh = SessionId::from(NEW.to_owned());
    storage
        .save_info(&seeded_session_info(fresh.clone(), 1))
        .expect("the fresh store writes");
    let listed = storage.list_sessions().expect("the fresh store lists");
    assert_eq!(listed.len(), 1, "got {listed:#?}");
    assert_eq!(listed[0].id, fresh);

    // AC-17's third clause: nothing was deleted. The file is still there —
    // and it still holds the session, which is more than its name can say.
    let aside = aside_database(directory.path(), &database);
    let kept = Connection::open(directory.path().join(&aside))
        .expect("the set-aside store opens like any other database");
    let held: i64 = kept
        .query_row("SELECT COUNT(*) FROM session WHERE id = ?1", [OLD], |row| {
            row.get(0)
        })
        .expect("the set-aside store still answers");
    assert_eq!(held, 1, "the set-aside store must still hold what it held");

    // Filed as superseded rather than as unreadable: it reads perfectly, and
    // whoever finds it tomorrow has to be told which of the two happened.
    assert!(
        !entries(directory.path())
            .iter()
            .any(|entry| entry.contains(".corrupt-")),
        "a store that reads perfectly must not be filed as one that does not"
    );
}

/// The one `<database>.preuuid-<millis>` entry, without its log companions.
fn aside_database(directory: &Path, database: &Path) -> String {
    let name = database
        .file_name()
        .expect("the database has a name")
        .to_string_lossy()
        .into_owned();
    let prefix = format!("{name}.preuuid-");

    let mut found: Vec<String> = entries(directory)
        .into_iter()
        .filter(|entry| {
            entry.starts_with(&prefix) && !entry.ends_with("-wal") && !entry.ends_with("-shm")
        })
        .collect();
    assert_eq!(
        found.len(),
        1,
        "exactly one store is set aside, got {found:?}"
    );

    found.remove(0)
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
