//! An older store's `storage/` tree whose sessions predate UUIDv7 ids is set
//! aside instead of imported — once, and as a tree (**D493**).
//!
//! "Once, not twice" is the whole point: were the decision taken *after* the
//! conversion, the tree would be carried into the database and the database
//! would then be quarantined for holding what had just been carried into it —
//! the same sessions moved twice in one open, for one reason. So the tree is
//! read, judged, and moved whole, and the database it would have filled stays
//! empty and stays where it is.
//!
//! One test, one binary, beside its three `storage_preuuid*` siblings.

use std::fs;

use ganja_core::{SessionId, Storage};
use ganja_testkit::{PRE_UUID_ID, entries, seeded_session_info, temp_dir};

#[test]
fn a_legacy_file_tree_with_old_ids_is_set_aside_once_not_twice() {
    let directory = temp_dir();
    let root = directory.path().join("storage");

    // The file layout a build before the database left behind, written by
    // hand: nothing in the tree writes it any more, and the ids in it are the
    // ones that build minted.
    let info = seeded_session_info(SessionId::from(PRE_UUID_ID.to_owned()), 7);
    let file = root.join("session").join("info").join(format!("{PRE_UUID_ID}.json"));
    fs::create_dir_all(file.parent().expect("a file has a directory"))
        .expect("the directory is creatable");
    fs::write(&file, serde_json::to_vec(&info).expect("the record encodes"))
        .expect("the file is writable");

    let storage = Storage::open(root.clone());
    assert!(
        storage.list_sessions().expect("the store opens rather than failing").is_empty(),
        "a tree whose ids predate UUIDv7 must not be carried across"
    );

    let listing = entries(directory.path());
    assert!(!root.exists(), "the tree moved, got {listing:?}");
    let aside: Vec<&String> =
        listing.iter().filter(|entry| entry.starts_with("storage.preuuid-")).collect();
    assert_eq!(aside.len(), 1, "the tree moved once, got {listing:?}");
    assert!(
        directory
            .path()
            .join(aside[0])
            .join("session")
            .join("info")
            .join(format!("{PRE_UUID_ID}.json"))
            .is_file(),
        "the set-aside tree must still hold what it held"
    );

    // Filed as superseded rather than as carried across, which is the other
    // reason a tree moves and the opposite claim about where its sessions are.
    assert!(
        !listing.iter().any(|entry| entry.starts_with("storage.migrated-")),
        "a tree that was not imported must not be filed as one that was, got {listing:?}"
    );
    // And nothing was moved twice: the database was never filled, so there is
    // nothing beside it to set aside.
    let database = Storage::open(root).database().to_path_buf();
    assert!(database.is_file(), "the empty store that replaces the tree is created");
    let name =
        database.file_name().expect("the database has a name").to_string_lossy().into_owned();
    assert!(
        !listing.iter().any(|entry| entry.starts_with(&format!("{name}.preuuid-"))),
        "the database was never filled, so it must not be set aside too, got {listing:?}"
    );
}
