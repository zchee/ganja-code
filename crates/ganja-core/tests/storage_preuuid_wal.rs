//! Cooperative leases fence maintenance; quiescent WAL-dependent captures are
//! recovered without pairing their log with the replacement generation.
//!
//! The private storage tests observe automatic intent publication. This public
//! API fixture seeds a published record itself so no uncooperative SQLite
//! keeper or test-only production pause is needed to preserve a real WAL.

use std::fs;
use std::path::{Path, PathBuf};

use ganja_core::{SessionId, Storage};
use ganja_testkit::{
    PRE_UUID_ID, plant_preuuid_store, seeded_session_info, set_aside_of, temp_dir,
};
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};

const NEW: &str = "0198f2c4-a1b0-7000-8000-000000000001";

#[test]
fn a_cooperative_owner_fences_maintenance_and_a_quiescent_wal_survives_recovery() {
    let directory = temp_dir();
    let root = directory.path().join("storage");
    let (planted, database) = plant_preuuid_store(root.clone());
    let lease = fs::OpenOptions::new()
        .write(true)
        .open(with_suffix(&database, ".quarantine.lock"))
        .expect("stable cooperative lock");
    assert!(
        matches!(lease.try_lock(), Err(fs::TryLockError::WouldBlock)),
        "the live Storage owns shared admission"
    );
    drop(planted);
    lease.try_lock().expect("all cooperative connections closed before maintenance");

    let connection = Connection::open(&database).expect("quiescent fixture opens under EX");
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .expect("fixture uses WAL without automatic checkpoints");
    // Closing Storage guarantees quiescence, not a checkpointed baseline.
    let checkpoint: (i32, i32, i32) = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("fixture checkpoints its planted baseline under EX");
    assert_eq!(checkpoint, (0, 0, 0), "the baseline checkpoint completed and emptied WAL");
    let previous: String = connection
        .query_row("SELECT data FROM session WHERE id = ?1", [PRE_UUID_ID], |row| row.get(0))
        .expect("planted baseline row");
    connection
        .execute("UPDATE session SET data = 'distinctive WAL-only old content'", [])
        .expect("old content commits only in WAL");
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .expect("real SQLite preserves the log at close");
    drop(connection);
    for suffix in ["-wal", "-shm"] {
        assert!(with_suffix(&database, suffix).is_file(), "captured {suffix} exists");
    }
    let name = database.file_name().expect("database basename").to_string_lossy();
    let target = format!("{name}.preuuid-1");
    let temporary = with_suffix(&database, ".fixture-intent.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "kind": "preuuid",
            "target": target,
            "components": ["wal", "shm", "main"],
            "initialization": "no_legacy_import",
        }))
        .expect("versioned exact-set intent"),
    )
    .expect("complete temporary record");

    let control = directory.path().join("main-only.db");
    fs::copy(&database, &control).expect("main-only control");
    let main = Connection::open_with_flags(&control, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("main-only reads");
    let old: String = main
        .query_row("SELECT data FROM session WHERE id = ?1", [PRE_UUID_ID], |row| row.get(0))
        .expect("previous checkpointed row");
    assert_eq!(old, previous, "the main-only control retains the planted baseline");
    assert_ne!(
        old, "distinctive WAL-only old content",
        "old content still requires its WAL immediately before record publication"
    );
    drop(main);
    fs::rename(&temporary, with_suffix(&database, ".quarantine.pending"))
        .expect("publish the complete intent before any moves");
    drop(lease);

    let storage = Storage::open(root);
    assert!(
        storage.list_sessions().expect("pending recovery precedes SQLite admission").is_empty()
    );
    storage
        .save_info(&seeded_session_info(SessionId::from(NEW.to_owned()), 1))
        .expect("replacement saves");
    assert_eq!(storage.list_sessions().expect("replacement reads").len(), 1);
    drop(storage);
    let kept = set_aside_of(directory.path(), &database);
    assert_eq!(kept.len(), 1, "recovery reuses the recorded target");
    assert_eq!(kept[0], target);
    let archived_main = directory.path().join(&kept[0]);
    let copy = directory.path().join("query.db");
    for suffix in ["", "-wal", "-shm"] {
        let source = with_suffix(&archived_main, suffix);
        assert!(source.is_file(), "the captured {suffix:?} travelled with the main");
        fs::copy(&source, with_suffix(&copy, suffix)).expect("each archived component copies");
    }
    let archived = Connection::open_with_flags(copy, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("ordinary WAL-aware copy opens");
    let row: (String, String) = archived
        .query_row("SELECT id, data FROM session", [], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("archived content reads");
    assert_eq!(row, (PRE_UUID_ID.to_owned(), "distinctive WAL-only old content".to_owned()));
    assert!(!with_suffix(&database, ".quarantine.pending").exists());
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().expect("database basename").to_string_lossy();
    path.with_file_name(format!("{name}{suffix}"))
}
