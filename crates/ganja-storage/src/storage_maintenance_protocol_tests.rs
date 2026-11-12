use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;
use rusqlite::config::DbConfig;
use tempfile::TempDir;

use super::*;
use crate::storage::{DATABASE, Observation};

fn raw_set() -> (TempDir, PathBuf) {
    let directory = TempDir::new().expect("fixture");
    let database = directory.path().join(DATABASE);
    for (suffix, bytes) in
        [("", b"old main".as_slice()), ("-wal", b"old wal"), ("-shm", b"old shm")]
    {
        fs::write(with_suffix(&database, suffix), bytes).expect("fixture component");
    }
    (directory, database)
}

#[test]
fn destination_conflicts_preserve_both_sets() {
    let (_directory, database) = raw_set();
    let observation = Observation::default();
    let pending = Pending::begin(&database, PREUUID, &observation).expect("intent publishes");
    let archived = database.with_file_name(&pending.target);
    fs::write(with_suffix(&archived, "-wal"), b"unrelated wal").expect("destination conflict");
    assert!(
        pending.move_files(&database, &observation).is_err(),
        "collision must refuse persistence"
    );
    assert_eq!(fs::read(&database).expect("source main retained"), b"old main");
    assert_eq!(fs::read(with_suffix(&database, "-wal")).expect("source log retained"), b"old wal");
    assert_eq!(
        fs::read(with_suffix(&archived, "-wal")).expect("destination retained"),
        b"unrelated wal"
    );
    assert!(!archived.exists(), "an incomplete set must not be published");
    assert!(Pending::read(&database).expect("pending reads").is_some());
}

#[test]
fn missing_components_refuse_persistence() {
    let (_directory, database) = raw_set();
    let observation = Observation::default();
    let pending = Pending::begin(&database, PREUUID, &observation).expect("intent publishes");
    fs::remove_file(with_suffix(&database, "-wal")).expect("simulate lost captured log");
    assert!(
        pending.move_files(&database, &observation).is_err(),
        "missing data must not be guessed"
    );
    assert_eq!(fs::read(&database).expect("main retained"), b"old main");
    assert_eq!(fs::read(with_suffix(&database, "-shm")).expect("index retained"), b"old shm");
    assert!(Pending::read(&database).expect("pending reads").is_some());
}

#[test]
fn recovery_uses_the_reserved_name_after_a_companion_move() {
    let (_directory, database) = raw_set();
    let observation = Observation::default();
    let pending = Pending::begin(&database, PREUUID, &observation).expect("intent publishes");
    let archived = database.with_file_name(&pending.target);
    fs::rename(with_suffix(&database, "-wal"), with_suffix(&archived, "-wal"))
        .expect("interrupted move");
    let recovered = Pending::read(&database).expect("pending reads").expect("intent remains");
    assert_eq!(recovered.target, pending.target, "recovery cannot choose another archive name");
    recovered.move_files(&database, &observation).expect("remaining moves finish");
    for (suffix, expected) in
        [("", b"old main".as_slice()), ("-wal", b"old wal"), ("-shm", b"old shm")]
    {
        assert_eq!(
            fs::read(with_suffix(&archived, suffix)).expect("captured file retained"),
            expected
        );
        assert!(!with_suffix(&database, suffix).exists(), "captured live name moved");
    }
}

#[test]
fn unexpected_companions_refuse_to_mix_generations() {
    let directory = TempDir::new().expect("fixture");
    let database = directory.path().join(DATABASE);
    fs::write(&database, b"old main").expect("main fixture");
    let observation = Observation::default();
    let pending = Pending::begin(&database, PREUUID, &observation).expect("intent publishes");
    fs::write(with_suffix(&database, "-wal"), b"unexpected generation").expect("unexpected log");
    assert!(pending.move_files(&database, &observation).is_err());
    assert!(database.exists(), "no files move after ambiguity is found");
    assert!(!database.with_file_name(&pending.target).exists());
}

#[test]
fn a_main_without_companions_is_a_complete_captured_set() {
    let directory = TempDir::new().expect("fixture");
    let database = directory.path().join(DATABASE);
    fs::write(&database, b"main only").expect("main fixture");
    let observation = Observation::default();
    let pending = Pending::begin(&database, QUARANTINE, &observation).expect("intent publishes");
    assert_eq!(pending.components, vec![Component::Main]);
    pending.move_files(&database, &observation).expect("no optional companion is invented");
    assert_eq!(
        fs::read(database.with_file_name(&pending.target)).expect("archive reads"),
        b"main only"
    );
}

#[test]
fn published_archives_may_have_already_checkpointed_their_wal() {
    let directory = TempDir::new().expect("fixture");
    let database = directory.path().join(DATABASE);
    let connection = Connection::open(&database).expect("SQLite fixture");
    connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE old_rows(value TEXT); INSERT INTO old_rows VALUES ('retained old content');").expect("WAL rows commit");
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .expect("keep real WAL");
    drop(connection);
    let observation = Observation::default();
    let pending = Pending::begin(&database, PREUUID, &observation).expect("intent publishes");
    pending.move_files(&database, &observation).expect("archive publishes");
    let archived = database.with_file_name(&pending.target);
    {
        let reader = Connection::open(&archived).expect("ordinary archive reader");
        assert_eq!(
            reader
                .query_row("SELECT value FROM old_rows", [], |row| row.get::<_, String>(0))
                .expect("WAL-aware archived row"),
            "retained old content"
        );
    }
    assert!(
        !with_suffix(&archived, "-wal").exists(),
        "the ordinary archive reader checkpointed its log"
    );
    fs::write(&database, b"replacement main").expect("replacement marker");
    fs::write(with_suffix(&database, "-wal"), b"replacement wal").expect("replacement log marker");
    pending
        .move_files(&database, &observation)
        .expect("published sets never replay companion moves");
    assert_eq!(
        fs::read(with_suffix(&database, "-wal")).expect("replacement retained"),
        b"replacement wal"
    );
    assert_eq!(fs::read(&database).expect("replacement retained"), b"replacement main");
}

#[test]
fn malformed_and_unsupported_intents_are_refused_without_opening_sqlite() {
    for bytes in [b"not json".as_slice(), br#"{"version":2,"kind":"preuuid","target":"sessions-dev.db.preuuid-1","components":["main"],"initialization":"no_legacy_import"}"#] {
        let directory = TempDir::new().expect("fixture");
        let storage = crate::storage::Storage::open(directory.path().join("storage"));
        let database = storage.database();
        fs::write(database, b"untouched main").expect("main marker");
        fs::write(with_suffix(database, PENDING), bytes).expect("invalid intent");
        assert!(matches!(storage.list_sessions(), Err(StorageError::Maintenance { .. })), "invalid intent refuses persistence");
        assert_eq!(fs::read(database).expect("untouched main"), b"untouched main");
        assert_eq!(fs::read(with_suffix(database, PENDING)).expect("intent retained"), bytes);
    }
}
