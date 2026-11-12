use super::*;
use crate::storage::StorageError;
use crate::storage::maintenance::Pending;

const SECOND: &str = "0198f2c4-a1b0-7000-8000-000000000002";
const BOUNDARIES: &[&str] = &[
    "IntentTemporary",
    "IntentPublished",
    "Renamed(\"-wal\")",
    "Renamed(\"-shm\")",
    "Renamed(\"\")",
    "ReplacementCreated",
    "Migrated",
    "IntentRemoved",
];

fn seed(root: &Path, wal: bool) -> PathBuf {
    let database = Storage::open(root.to_path_buf()).database().to_path_buf();
    let mut connection = super::super::connect(&database).expect("seed connects");
    super::super::migrate(&mut connection, &database).expect("seed schema");
    connection.execute_batch("INSERT INTO session VALUES ('ses_before_uuid', NULL, 11, 12, 'old session content'); INSERT INTO message VALUES ('msg_old', 'ses_before_uuid', 13, 14, 'old message content'); INSERT INTO part VALUES ('prt_old', 'msg_old', 'ses_before_uuid', 15, 16, 'old part content');").expect("all old content commits");
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, wal)
        .expect("fixture close policy");
    drop(connection);
    database
}

fn assert_wal_dependent(database: &Path) {
    let directory = TempDir::new().expect("main-only query");
    let main = directory.path().join("main.db");
    fs::copy(database, &main).expect("main-only control");
    let connection =
        Connection::open_with_flags(main, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("main reads");
    let content = connection
        .query_row("SELECT data FROM session WHERE id = ?1", [OLD], |row| row.get::<_, String>(0));
    assert!(
        content.is_err(),
        "old content must still require the WAL immediately before intent publication"
    );
}

fn assert_old_content(database: &Path) {
    let directory = TempDir::new().expect("ordinary query copy");
    let copy = directory.path().join("query.db");
    copy_set(database, &copy);
    let connection = Connection::open_with_flags(copy, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("ordinary WAL-aware archive read");
    let row = connection.query_row("SELECT s.id, s.parent_id, s.time_created, s.time_updated, s.data, m.id, m.time_created, m.time_updated, m.data, p.id, p.time_created, p.time_updated, p.data FROM session s JOIN message m ON m.session_id=s.id JOIN part p ON p.session_id=m.session_id AND p.message_id=m.id", [], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?, row.get::<_, i64>(6)?, row.get::<_, i64>(7)?, row.get::<_, String>(8)?, row.get::<_, String>(9)?, row.get::<_, i64>(10)?, row.get::<_, i64>(11)?, row.get::<_, String>(12)?))
    }).expect("all retained logical content reads");
    assert_eq!(row.0, OLD);
    assert_eq!((row.1, row.2, row.3, row.4), (None, 11, 12, "old session content".to_owned()));
    assert_eq!(
        (row.5, row.6, row.7, row.8),
        ("msg_old".to_owned(), 13, 14, "old message content".to_owned())
    );
    assert_eq!(
        (row.9, row.10, row.11, row.12),
        ("prt_old".to_owned(), 15, 16, "old part content".to_owned())
    );
    for table in ["session", "message", "part"] {
        assert_eq!(
            connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get::<_, i64>(0))
                .expect("exact row count"),
            1
        );
    }
}

fn archives(directory: &Path, prefix: &str) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .expect("directory reads")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| {
            let name = path.file_name().expect("filename").to_string_lossy();
            name.contains(prefix) && !name.ends_with("-wal") && !name.ends_with("-shm")
        })
        .collect()
}

fn safe_recovery(root: &Path) -> Storage {
    let database = Storage::open(root.to_path_buf()).database().to_path_buf();
    observed(root, move |phase| {
        if phase == Phase::ReaderOpening
            && let Some(pending) = Pending::read(&database).expect("intent reads before SQLite")
        {
            // Replacement initialization is allowed only after main publication.
            let json = serde_json::to_value(pending).expect("record serializes");
            let target = database.with_file_name(json["target"].as_str().expect("reserved target"));
            assert!(
                target.is_file(),
                "an unresolved split set forbids every live SQLite connection"
            );
        }
    })
}

fn kill_at(root: &Path, boundary: &str, wal: bool) {
    let database = Storage::open(root.to_path_buf()).database().to_path_buf();
    let mut worker = Worker::start(root);
    if wal {
        worker.until("IntentTemporary");
        assert_wal_dependent(&database);
        if boundary != "IntentTemporary" {
            worker.release();
            worker.until(boundary);
        }
    } else {
        worker.until(boundary);
    }
    worker.child.kill().expect("kill at acknowledged boundary");
    assert!(!worker.child.wait().expect("killed worker reaped").success());
}

#[test]
fn a_killed_move_is_completed_before_any_live_sqlite_open() {
    for wal in [true, false] {
        for boundary in BOUNDARIES {
            let directory = TempDir::new().expect("crash fixture");
            let root = directory.path().join("storage");
            let database = seed(&root, wal);
            kill_at(&root, boundary, wal);
            let store = safe_recovery(&root);
            store.save_info(&info(FRESH, 2)).expect("recovered generation persists");
            assert_eq!(store.list_sessions().expect("live rows"), vec![info(FRESH, 2)]);
            drop(store);
            let kept = archives(directory.path(), ".preuuid-");
            assert_eq!(kept.len(), 1, "one archive after death at {boundary}, wal={wal}");
            assert_old_content(&kept[0]);
            assert!(Pending::read(&database).expect("pending state").is_none());
        }
    }
}

#[test]
fn a_killed_wal_without_shm_move_recovers_the_captured_old_content() {
    for boundary in ["Renamed(\"-wal\")", "Renamed(\"\")"] {
        let directory = TempDir::new().expect("optional SHM crash fixture");
        let root = directory.path().join("storage");
        let database = seed(&root, true);
        let mut worker = Worker::start(&root);
        worker.until("ExclusiveAcquired");
        worker.release();
        worker.until("ReaderClosed");
        // The exclusive probe is closed, so removing its disposable index leaves
        // a quiescent real WAL and no SQLite handle that could recreate SHM.
        fs::remove_file(with_suffix(&database, "-shm")).expect("remove the quiescent index");
        assert!(with_suffix(&database, "-wal").is_file());
        assert_wal_dependent(&database);
        worker.release();
        worker.until("IntentTemporary");
        assert!(!with_suffix(&database, "-shm").exists());
        assert_wal_dependent(&database);
        worker.release();
        worker.until("IntentPublished");
        let record = serde_json::to_value(
            Pending::read(&database).expect("published intent").expect("intent exists"),
        )
        .expect("membership decodes");
        assert_eq!(record["components"], serde_json::json!(["wal", "main"]));
        worker.release();
        worker.until(boundary);
        worker.child.kill().expect("kill at move boundary");
        assert!(!worker.child.wait().expect("killed worker reaped").success());
        drop(worker);
        let store = safe_recovery(&root);
        store.save_info(&info(FRESH, 2)).expect("replacement persists");
        drop(store);
        let kept = archives(directory.path(), ".preuuid-");
        assert_eq!(kept.len(), 1, "one reserved archive after {boundary}");
        assert!(!with_suffix(&kept[0], "-shm").exists(), "absent SHM was not captured");
        assert_old_content(&kept[0]);
        assert!(Pending::read(&database).expect("completed intent").is_none());
    }
}

#[test]
fn a_killed_corrupt_move_preserves_its_captured_bytes_without_companions() {
    for boundary in BOUNDARIES.iter().filter(|boundary| !boundary.starts_with("Renamed(\"-")) {
        let directory = TempDir::new().expect("corrupt crash fixture");
        let root = directory.path().join("storage");
        let database = Storage::open(root.clone()).database().to_path_buf();
        fs::write(&database, b"corrupt database retained verbatim").expect("corrupt fixture");
        kill_at(&root, boundary, false);
        let store = safe_recovery(&root);
        store.save_info(&info(FRESH, 2)).expect("corrupt recovery persists");
        drop(store);
        let kept = archives(directory.path(), ".corrupt-");
        assert_eq!(kept.len(), 1, "one corrupt archive at {boundary}");
        assert_eq!(
            fs::read(&kept[0]).expect("captured main retained"),
            b"corrupt database retained verbatim"
        );
        for suffix in ["-wal", "-shm"] {
            assert!(!with_suffix(&kept[0], suffix).exists(), "absent companion is not invented");
        }
    }
}

#[test]
fn two_recoverers_finish_one_pending_move() {
    for boundary in &BOUNDARIES[1..BOUNDARIES.len() - 1] {
        let directory = TempDir::new().expect("two-recoverer fixture");
        let root = directory.path().join("storage");
        seed(&root, true);
        kill_at(&root, boundary, true);
        let mut first = Worker::configured(&root, FRESH, Duration::from_secs(10));
        let mut second = Worker::configured(&root, SECOND, Duration::from_secs(10));
        // Both see the pending state while holding shared admission. Neither
        // may wait for exclusive admission while keeping that shared lease.
        first.until("SharedAcquired");
        second.until("SharedAcquired");
        first.release();
        second.release();
        first.until("ExclusiveWanted");
        second.until("ExclusiveWanted");
        first.release();
        first.until("ExclusiveAcquired");
        second.release();
        second.until("SharedBlocked");
        first.release();
        first.until("Saved");
        second.release();
        second.until("Saved");
        first.release();
        first.finish();
        second.release();
        second.finish();
        let kept = archives(directory.path(), ".preuuid-");
        assert_eq!(kept.len(), 1, "recoverers reuse one target after {boundary}");
        assert_old_content(&kept[0]);
        let live = Storage::open(root);
        assert_eq!(live.list_sessions().expect("both saves persist").len(), 2);
        for id in [FRESH, SECOND] {
            assert_eq!(
                live.load_info(&info(id, 1).id).expect("new session reads"),
                Some(info(id, 1))
            );
        }
        assert!(live.load_info(&info(OLD, 1).id).expect("old id absent").is_none());
    }
}

#[test]
fn racing_maintenance_starters_revalidate_and_both_persist() {
    let directory = TempDir::new().expect("two-starter fixture");
    let root = directory.path().join("storage");
    seed(&root, true);
    let mut first = Worker::configured(&root, FRESH, Duration::from_secs(10));
    let mut second = Worker::configured(&root, SECOND, Duration::from_secs(10));
    first.until("OldIds");
    second.until("OldIds");
    first.release();
    first.until("ExclusiveWanted");
    first.release();
    // The second still owns SH, so the first must re-enter validation, not
    // report memory-only success or use its stale old-id verdict.
    first.until("SharedAcquired");
    second.release();
    second.until("ExclusiveWanted");
    first.release();
    first.until("ExclusiveWanted");
    first.release();
    first.until("ExclusiveAcquired");
    second.release();
    second.until("SharedBlocked");
    first.release();
    first.until("Saved");
    second.release();
    second.until("Saved");
    first.release();
    first.finish();
    second.release();
    second.finish();
    assert_eq!(archives(directory.path(), ".preuuid-").len(), 1);
    assert_old_content(&archive(directory.path()));
    let live = Storage::open(root);
    assert_eq!(live.list_sessions().expect("both racing sessions persist").len(), 2);
    for id in [FRESH, SECOND] {
        assert_eq!(live.load_info(&info(id, 1).id).expect("session reads"), Some(info(id, 1)));
    }
}

#[test]
fn a_live_owner_defers_maintenance_without_moving_data() {
    #[derive(Clone)]
    struct Warnings(Arc<Mutex<String>>);

    impl tracing::Subscriber for Warnings {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Message<'a>(&'a mut String);
            impl tracing::field::Visit for Message<'_> {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        use std::fmt::Write as _;

                        write!(self.0, "{value:?}").expect("message records");
                    }
                }
            }
            event.record(&mut Message(&mut self.0.lock().expect("captured warnings")));
        }
    }

    let warnings = Arc::new(Mutex::new(String::new()));
    let _subscriber = tracing::subscriber::set_default(Warnings(warnings.clone()));
    let directory = TempDir::new().expect("deferred fixture");
    let root = directory.path().join("storage");
    let database = seed(&root, false);
    let lease = lock_file(&database);
    lease.lock_shared().expect("cooperative established owner");
    let mut store = observed(&root, |_| {});
    Arc::get_mut(&mut store.inner).expect("private opener").admission_budget = Some(Duration::ZERO);
    store
        .save_info(&info(FRESH, 2))
        .expect("readable old generation remains writable when maintenance is deferred");
    assert!(
        warnings
            .lock()
            .expect("captured warning")
            .contains("the existing readable store remains live with its old ids"),
        "the deferral warning must explicitly say old ids remain live"
    );
    assert_eq!(old_rows(&database), vec![FRESH, OLD]);
    assert!(archives(directory.path(), ".preuuid-").is_empty(), "no data moved while SH is owned");
    drop(store);
    drop(lease);
    let next = Storage::open(root);
    assert!(next.list_sessions().expect("later exclusive maintenance").is_empty());
    assert_eq!(old_rows(&archive(directory.path())), vec![FRESH, OLD]);
}

#[test]
fn pending_and_corrupt_stores_refuse_maintenance_under_an_established_lease() {
    for pending in [true, false] {
        let directory = TempDir::new().expect("blocked unsafe fixture");
        let root = directory.path().join("storage");
        let database = Storage::open(root.clone()).database().to_path_buf();
        fs::write(&database, b"corrupt captured main").expect("corrupt input");
        if pending {
            Pending::begin(&database, super::super::QUARANTINE, &Observation::default())
                .expect("pending fixture");
        }
        let lease = lock_file(&database);
        lease.lock_shared().expect("established cooperative owner");
        let opens = Arc::new(AtomicBool::new(false));
        let seen = opens.clone();
        let mut store = observed(&root, move |phase| {
            if phase == Phase::ReaderOpening {
                seen.store(true, Ordering::SeqCst);
            }
        });
        Arc::get_mut(&mut store.inner).expect("private budget").admission_budget =
            Some(Duration::ZERO);
        assert!(
            matches!(store.list_sessions(), Err(StorageError::Maintaining { .. })),
            "unsafe maintenance cannot be deferred into SQLite"
        );
        if pending {
            assert!(!opens.load(Ordering::SeqCst), "pending intent prevents even a probe");
        }
        assert_eq!(fs::read(&database).expect("unmoved source"), b"corrupt captured main");
        assert!(archives(directory.path(), ".corrupt-").is_empty());
    }
}

#[test]
fn racing_legacy_tree_openers_set_the_tree_aside_once() {
    let directory = TempDir::new().expect("legacy race fixture");
    let root = directory.path().join("storage");
    let relative = Path::new("session/info/ses_before_uuid.json");
    fs::create_dir_all(root.join("session/info")).expect("legacy tree");
    let bytes = serde_json::to_vec(&info(OLD, 4)).expect("legacy row");
    fs::write(root.join(relative), &bytes).expect("legacy info");
    fs::write(root.join("untouched-transcript"), b"whole old tree retained")
        .expect("legacy sentinel");
    let mut first = Worker::configured(&root, FRESH, Duration::from_secs(10));
    let mut second = Worker::configured(&root, SECOND, Duration::from_secs(10));
    first.until("SharedAcquired");
    second.until("SharedAcquired");
    first.release();
    second.release();
    first.until("ExclusiveWanted");
    second.until("ExclusiveWanted");
    first.release();
    first.until("ExclusiveAcquired");
    second.release();
    second.until("SharedBlocked");
    first.release();
    first.until("Saved");
    second.release();
    second.until("Saved");
    first.release();
    first.finish();
    second.release();
    second.finish();
    let kept = archives(directory.path(), "storage.preuuid-");
    assert_eq!(kept.len(), 1, "exactly one whole legacy tree is set aside");
    assert_eq!(fs::read(kept[0].join(relative)).expect("legacy info retained"), bytes);
    assert_eq!(
        fs::read(kept[0].join("untouched-transcript")).expect("whole tree retained"),
        b"whole old tree retained"
    );
    assert!(!root.exists(), "legacy tree was not reimported by the second starter");
    let live = Storage::open(root);
    assert_eq!(live.list_sessions().expect("both new saves persist").len(), 2);
    assert!(live.load_info(&info(OLD, 4).id).expect("old session absent").is_none());
}

#[test]
fn snapshots_remain_unchanged_during_session_quarantine() {
    for corrupt in [false, true] {
        let directory = TempDir::new().expect("snapshot sentinel fixture");
        let snapshot = directory.path().join("snapshot/project/worktree");
        fs::create_dir_all(snapshot.join("objects")).expect("separate snapshot tree");
        fs::write(snapshot.join("HEAD"), b"snapshot head sentinel").expect("snapshot head");
        fs::write(snapshot.join("objects/retained"), b"snapshot tree sentinel")
            .expect("snapshot object");
        let root = directory.path().join("project/storage");
        fs::create_dir_all(root.parent().expect("project directory")).expect("project data");
        let database = if corrupt {
            let database = Storage::open(root.clone()).database().to_path_buf();
            fs::write(&database, b"corrupt store").expect("corrupt input");
            database
        } else {
            seed(&root, false)
        };
        let store = Storage::open(root);
        store.save_info(&info(FRESH, 2)).expect("session maintenance completes");
        drop(store);
        assert!(database.is_file(), "the replacement stays in project data");
        assert_eq!(
            fs::read(snapshot.join("HEAD")).expect("snapshot head unchanged"),
            b"snapshot head sentinel"
        );
        assert_eq!(
            fs::read(snapshot.join("objects/retained")).expect("snapshot object unchanged"),
            b"snapshot tree sentinel"
        );
        assert_eq!(fs::read_dir(&snapshot).expect("snapshot directory unchanged").count(), 2);
        assert_eq!(
            fs::read_dir(snapshot.join("objects")).expect("snapshot objects unchanged").count(),
            1
        );
    }
}

#[test]
fn revalidation_rounds_share_one_admission_deadline() {
    let directory = TempDir::new().expect("one-budget fixture");
    let root = directory.path().join("storage");
    let database = seed(&root, false);
    let owner = lock_file(&database);
    owner.lock_shared().expect("maintenance stays contended");
    let (blocked, rounds) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Mutex::new(released);
    let deadline = Mutex::new(None);
    let mut store = observed(&root, move |phase| match phase {
        Phase::AdmissionDeadline(current) => {
            let mut original = deadline.lock().expect("original deadline");
            if let Some(original) = *original {
                assert_eq!(
                    current, original,
                    "revalidation must not restart the admission deadline"
                );
            } else {
                *original = Some(current);
            }
        }
        Phase::ExclusiveBlocked => {
            blocked
                .send(deadline.lock().expect("deadline").expect("deadline observed"))
                .expect("contended round reports");
            released
                .lock()
                .expect("round barrier")
                .recv_timeout(Duration::from_secs(15))
                .expect("parent releases contended round");
        }
        _ => {}
    });
    Arc::get_mut(&mut store.inner).expect("private short budget").admission_budget =
        Some(Duration::from_secs(2));
    thread::scope(|scope| {
        let result = scope.spawn(move || store.list_sessions());
        for round in 0..3 {
            let deadline =
                rounds.recv_timeout(Duration::from_secs(15)).expect("three revalidation rounds");
            if round == 2 {
                // The blocked opener stays at its acknowledgement barrier until
                // the original deadline expires; no scheduling sleep orders it.
                let (_keep_open, never_sent) = mpsc::channel::<()>();
                assert!(matches!(
                    never_sent.recv_timeout(
                        deadline.saturating_duration_since(std::time::Instant::now())
                    ),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ));
                assert!(std::time::Instant::now() >= deadline);
            }
            release.send(()).expect("release refused EX");
        }
        assert!(result.join().expect("opener joins").expect("readable fallback").is_empty());
    });
    assert_eq!(old_rows(&database), vec![OLD]);
    assert!(archives(directory.path(), ".preuuid-").is_empty());
}

#[test]
fn an_expired_opener_never_waits_for_a_new_exclusive_owner() {
    let directory = TempDir::new().expect("expiry fixture");
    let root = directory.path().join("storage");
    let database = seed(&root, false);
    let held = lock_file(&database);
    held.lock_shared().expect("initial established owner");
    let owner = Arc::new(Mutex::new(Some(held)));
    let switched = owner.clone();
    let source = database.clone();
    let store = observed(&root, move |phase| {
        if phase == Phase::ExclusiveBlocked {
            drop(switched.lock().expect("owner slot").take());
            let exclusive = lock_file(&source);
            exclusive.try_lock().expect("new maintainer enters after refused EX");
            *switched.lock().expect("owner slot") = Some(exclusive);
        }
    });
    let mut store = store;
    Arc::get_mut(&mut store.inner).expect("private budget").admission_budget = Some(Duration::ZERO);
    assert!(
        matches!(store.list_sessions(), Err(StorageError::Maintaining { .. })),
        "expired fallback must try SH once and refuse, never block or bypass it"
    );
    assert!(archives(directory.path(), ".preuuid-").is_empty());
}

#[test]
fn an_expired_fallback_rechecks_pending_state_before_opening_sqlite() {
    let directory = TempDir::new().expect("stale verdict fixture");
    let root = directory.path().join("storage");
    let database = seed(&root, false);
    let held = lock_file(&database);
    held.lock_shared().expect("initial established owner");
    let owner = Arc::new(Mutex::new(Some(held)));
    let switched = owner.clone();
    let source = database.clone();
    let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = opens.clone();
    let mut store = observed(&root, move |phase| {
        if phase == Phase::ReaderOpening {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        if phase == Phase::ExclusiveBlocked {
            drop(switched.lock().expect("owner slot").take());
            let exclusive = lock_file(&source);
            exclusive.try_lock().expect("intervening maintenance");
            Pending::begin(&source, super::super::PREUUID, &Observation::default())
                .expect("intervening pending operation");
            drop(exclusive);
            let shared = lock_file(&source);
            shared.try_lock_shared().expect("intervening shared owner");
            *switched.lock().expect("owner slot") = Some(shared);
        }
    });
    Arc::get_mut(&mut store.inner).expect("private budget").admission_budget = Some(Duration::ZERO);
    assert!(matches!(store.list_sessions(), Err(StorageError::Maintaining { .. })));
    assert_eq!(
        opens.load(Ordering::SeqCst),
        1,
        "the fallback cannot reuse the readable verdict from before SH release"
    );
    assert!(Pending::read(&database).expect("pending retained").is_some());
}

#[test]
fn a_maintainer_keeps_exclusive_admission_through_replacement_migration_after_expiry() {
    let directory = TempDir::new().expect("maintainer expiry fixture");
    let root = directory.path().join("storage");
    seed(&root, true);
    let mut worker = Worker::configured(&root, FRESH, Duration::ZERO);
    worker.until("ReplacementCreated");
    let mut waiting = observed(&root, |phase| {
        assert_ne!(
            phase,
            Phase::ReaderOpening,
            "a waiting opener cannot enter SQLite during replacement migration"
        )
    });
    Arc::get_mut(&mut waiting.inner).expect("private budget").admission_budget =
        Some(Duration::ZERO);
    assert!(matches!(waiting.list_sessions(), Err(StorageError::Maintaining { .. })));
    worker.release();
    worker.until("Saved");
    worker.release();
    worker.finish();
    drop(waiting);
    let waiting = Storage::open(root);
    waiting.save_info(&info(SECOND, 2)).expect("a later starter persists after maintenance");
    assert_eq!(waiting.list_sessions().expect("both sessions persist").len(), 2);
}

#[test]
fn recovery_refuses_real_rename_and_replacement_errors_without_admitting_a_reader() {
    for replacement in [false, true] {
        let directory = TempDir::new().expect("filesystem failure fixture");
        let root = directory.path().join("storage");
        let database = seed(&root, true);
        let source = database.clone();
        let blocker = database.clone();
        let mut store = observed(&root, move |phase| {
            if !replacement && phase == Phase::Renaming("-wal") {
                let pending = Pending::read(&source).expect("published record").expect("intent");
                let json = serde_json::to_value(pending).expect("record");
                let target = source.with_file_name(json["target"].as_str().expect("target"));
                // A directory cannot be replaced by a regular-file rename.
                fs::create_dir(with_suffix(&target, "-wal")).expect("real rename obstacle");
            }
            if replacement && phase == Phase::Renamed("") {
                fs::create_dir(&blocker).expect("real replacement-open obstacle");
            }
        });
        Arc::get_mut(&mut store.inner).expect("private fixture close policy").no_checkpoint = true;
        let result = store.list_sessions();
        assert!(
            if replacement {
                matches!(result, Err(StorageError::Sql { .. }))
            } else {
                matches!(result, Err(StorageError::Io { .. }))
            },
            "the real filesystem error must propagate, got {result:?}"
        );
        assert!(store.inner.open.lock().expect("cache").is_none());
        assert!(Pending::read(&database).expect("recoverable record").is_some());
        drop(store);
        if replacement {
            fs::remove_dir(&database).expect("remove test obstacle");
        } else {
            let pending = Pending::read(&database).expect("record").expect("intent");
            let json = serde_json::to_value(pending).expect("record");
            fs::remove_dir(with_suffix(
                &database.with_file_name(json["target"].as_str().expect("target")),
                "-wal",
            ))
            .expect("remove test obstacle");
        }
        let next = safe_recovery(&root);
        next.save_info(&info(FRESH, 2))
            .expect("recovery after actual filesystem availability returns");
        drop(next);
        assert_old_content(&archive(directory.path()));
    }
}
