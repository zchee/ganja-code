//! A store whose sessions were minted before UUIDv7 ids is set aside rather
//! than written into, and what was set aside is still there afterwards
//! (**D493**, AC-17).
//!
//! No upstream opencode counterpart: upstream never changed the shape of an
//! id, so it has nothing to survive here. One test, one binary, beside its
//! three `storage_preuuid*` siblings.

use ganja_core::{SessionId, Storage};
use ganja_testkit::{
    PRE_UUID_ID, entries, plant_preuuid_store, seeded_session_info, set_aside_of, temp_dir,
};
use rusqlite::Connection;

/// One this build would mint.
const NEW: &str = "0198f2c4-a1b0-7000-8000-000000000001";

#[test]
fn a_store_with_old_format_ids_is_set_aside_and_a_fresh_one_created() {
    let directory = temp_dir();
    let root = directory.path().join("storage");
    let old = SessionId::from(PRE_UUID_ID.to_owned());

    // The store a build from before the change left behind.
    let (planted, database) = plant_preuuid_store(root.clone());
    drop(planted);
    assert!(database.is_file(), "the planted store is on disk");

    let storage = Storage::open(root.clone());
    assert!(
        storage.list_sessions().expect("a superseded store opens rather than failing").is_empty(),
        "a store whose ids predate UUIDv7 must not be read"
    );
    assert!(
        storage.load_info(&old).expect("the fresh store reads").is_none(),
        "and none of its sessions may be reachable through the store that replaced it"
    );

    // What replaces it is a working store, not merely an empty one.
    let fresh = SessionId::from(NEW.to_owned());
    storage.save_info(&seeded_session_info(fresh.clone(), 1)).expect("the fresh store writes");
    let listed = storage.list_sessions().expect("the fresh store lists");
    assert_eq!(listed.len(), 1, "got {listed:#?}");
    assert_eq!(listed[0].id, fresh);

    // AC-17's third clause: nothing was deleted. The file is still there —
    // and it still holds the session, which is more than its name can say.
    let mut aside = set_aside_of(directory.path(), &database);
    assert_eq!(aside.len(), 1, "exactly one store is set aside, got {aside:?}");
    let aside = aside.remove(0);
    let kept = Connection::open(directory.path().join(&aside))
        .expect("the set-aside store opens like any other database");
    let held: i64 = kept
        .query_row("SELECT COUNT(*) FROM session WHERE id = ?1", [PRE_UUID_ID], |row| row.get(0))
        .expect("the set-aside store still answers");
    assert_eq!(held, 1, "the set-aside store must still hold what it held");

    // Filed as superseded rather than as unreadable: it reads perfectly, and
    // whoever finds it tomorrow has to be told which of the two happened.
    assert!(
        !entries(directory.path()).iter().any(|entry| entry.contains(".corrupt-")),
        "a store that reads perfectly must not be filed as one that does not"
    );
}
