use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use std::{fs, thread};

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;

use super::tests::info;
use super::{DATABASE, Observation, Phase, Storage, with_suffix};

const FRESH: &str = "0198f2c4-a1b0-7000-8000-000000000001";
const OLD: &str = "ses_before_uuid";

#[path = "storage_recovery_tests.rs"]
mod recovery;

struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    events: mpsc::Receiver<String>,
    reader: Option<thread::JoinHandle<()>>,
}

impl Worker {
    fn start(root: &Path) -> Self {
        Self::configured(root, FRESH, Duration::from_secs(10))
    }

    fn configured(root: &Path, id: &str, budget: Duration) -> Self {
        let mut child = Command::new(std::env::current_exe().expect("unit-test executable"))
            .args(["--exact", "storage::maintenance_tests::maintenance_worker", "--nocapture"])
            .env("GANJA_STORAGE_TEST_WORKER", root)
            .env("GANJA_STORAGE_TEST_ID", id)
            .env("GANJA_STORAGE_TEST_BUDGET_MS", budget.as_millis().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("test worker starts");
        let input = child.stdin.take();
        let output = child.stdout.take().expect("worker stdout");
        let (send, events) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let line = line.expect("worker stdout reads");
                if let Some((_, event)) = line.split_once("STORAGE_PHASE ")
                    && send.send(event.to_owned()).is_err()
                {
                    break;
                }
            }
        });
        Self { child, input, events, reader: Some(reader) }
    }

    fn event(&self) -> String {
        self.events.recv_timeout(Duration::from_secs(15)).expect("worker phase before deadline")
    }

    fn release(&mut self) {
        self.input.as_mut().expect("worker stdin").write_all(b"continue\n").expect("release phase");
    }

    fn until(&mut self, phase: &str) {
        loop {
            let event = self.event();
            if event == phase {
                return;
            }
            self.release();
        }
    }

    fn finish(&mut self) {
        loop {
            let event = self.event();
            if event == "Done" {
                break;
            }
            self.release();
        }
        self.input.take();
        assert!(self.child.wait().expect("worker reaped").success(), "worker succeeds");
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.input.take();
        if let Some(reader) = self.reader.take() {
            reader.join().expect("stdout reader joins after child is reaped");
        }
    }
}

#[test]
fn maintenance_worker() {
    let Some(root) = std::env::var_os("GANJA_STORAGE_TEST_WORKER") else {
        return;
    };
    let mut storage = observed(Path::new(&root), |phase| {
        println!("STORAGE_PHASE {phase:?}");
        std::io::stdout().flush().expect("phase flushes");
        let mut release = String::new();
        assert_ne!(std::io::stdin().read_line(&mut release).expect("phase acknowledgement"), 0);
    });
    let inner = Arc::get_mut(&mut storage.inner).expect("worker is not shared");
    inner.no_checkpoint = true;
    inner.admission_budget = Some(Duration::from_millis(
        std::env::var("GANJA_STORAGE_TEST_BUDGET_MS")
            .expect("private worker budget")
            .parse()
            .expect("budget is milliseconds"),
    ));
    let id = std::env::var("GANJA_STORAGE_TEST_ID").expect("private worker id");
    storage.save_info(&info(&id, 1)).expect("worker persists");
    println!("STORAGE_PHASE Saved");
    std::io::stdout().flush().expect("saved flushes");
    let mut release = String::new();
    assert_ne!(std::io::stdin().read_line(&mut release).expect("save acknowledgement"), 0);
    drop(storage);
    println!("STORAGE_PHASE Done");
}

#[test]
fn test_workers_report_real_connection_phases() {
    let directory = TempDir::new().expect("fixture");
    let mut worker = Worker::start(&directory.path().join("storage"));
    worker.until("ReaderOpened");
    worker.release();
    worker.until("WriterOpened");
    worker.release();
    worker.finish();
}

#[test]
fn an_archive_keeps_its_old_rows_when_another_process_opens_the_live_name() {
    let directory = TempDir::new().expect("fixture");
    let root = directory.path().join("storage");
    let planted = Storage::open(root.clone());
    planted.save_info(&info(OLD, 1)).expect("old row seeds");
    let database = planted.database().to_path_buf();
    drop(planted);
    let other = Arc::new(Mutex::new(None));
    let keeper = other.clone();
    let observation = Observation(Some(Arc::new(move |phase| {
        if phase == Phase::Renamed("") {
            let mut newcomer = Worker::start(&root);
            newcomer.until("Admission");
            newcomer.release();
            assert_eq!(
                newcomer.event(),
                "SharedBlocked",
                "the newcomer is fenced before any SQLite open"
            );
            *keeper.lock().expect("newcomer keeper") = Some(newcomer);
        }
    })));
    move_set(&database, &observation);
    let mut newcomer = other.lock().expect("worker keeper").take().expect("newcomer exists");
    newcomer.release();
    newcomer.until("Saved");
    assert_eq!(
        old_rows(&archive(directory.path())),
        vec![OLD],
        "the retained old main must not be paired with the newcomer's WAL"
    );
    newcomer.release();
    newcomer.finish();
    drop(other);
}

fn observed(root: &Path, callback: impl Fn(Phase) + Send + Sync + 'static) -> Storage {
    let mut storage = Storage::open(root.to_path_buf());
    let inner = Arc::get_mut(&mut storage.inner).expect("not shared yet");
    inner.observation = Observation(Some(Arc::new(callback)));
    inner.admission_budget = Some(Duration::from_millis(200));
    storage
}

fn lock_file(database: &Path) -> fs::File {
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(with_suffix(database, ".quarantine.lock"))
        .expect("the stable lock file opens")
}

fn move_set(database: &Path, observation: &Observation) {
    let exclusive = lock_file(database);
    exclusive.lock().expect("raw fixture maintenance is exclusive");
    let pending = super::maintenance::Pending::begin(database, super::PREUUID, observation)
        .expect("intent publishes before moving");
    pending.move_files(database, observation).expect("captured set moves");
}

fn copy_set(from: &Path, to: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let source = with_suffix(from, suffix);
        if source.exists() {
            fs::copy(&source, with_suffix(to, suffix)).expect("each captured component copies");
        }
    }
}

fn old_rows(database: &Path) -> Vec<String> {
    let query = TempDir::new().expect("query directory");
    let copy = query.path().join("query.db");
    copy_set(database, &copy);
    let connection = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("the complete archived set opens normally");
    connection
        .prepare("SELECT id FROM session ORDER BY id")
        .expect("the archived schema is readable")
        .query_map([], |row| row.get(0))
        .expect("the archived rows read")
        .collect::<Result<_, _>>()
        .expect("every row decodes")
}

fn archive(directory: &Path) -> PathBuf {
    fs::read_dir(directory)
        .expect("archive directory reads")
        .map(|entry| entry.expect("entry reads").path())
        .find(|path| {
            let name = path.file_name().expect("filename").to_string_lossy();
            name.contains(".preuuid-") && !name.ends_with("-wal") && !name.ends_with("-shm")
        })
        .expect("one archive exists")
}

#[test]
fn a_fresh_opener_cannot_connect_during_archive_publication() {
    let directory = TempDir::new().expect("fixture");
    let root = directory.path().join("storage");
    let seed = Storage::open(root.clone());
    seed.save_info(&info(FRESH, 1)).expect("healthy store seeds");
    let database = seed.database().to_path_buf();
    drop(seed);
    let exclusive = lock_file(&database);
    exclusive.lock().expect("maintenance admission");
    let opened = Arc::new(AtomicBool::new(false));
    let seen = opened.clone();
    let candidate = observed(&root, move |phase| {
        if phase == Phase::ReaderOpened {
            seen.store(true, Ordering::SeqCst);
        }
    });
    let result = candidate.list_sessions();
    assert!(
        !opened.load(Ordering::SeqCst),
        "an exclusive maintenance owner must exclude the first SQLite connection"
    );
    assert!(result.is_err(), "exhausted admission must report unavailable persistence");
}

#[test]
fn a_store_retains_its_lease_until_both_connections_close() {
    let directory = TempDir::new().expect("fixture");
    let root = directory.path().join("storage");
    let seed = Storage::open(root.clone());
    seed.save_info(&info(FRESH, 1)).expect("initialized fixture");
    drop(seed);
    let (arrived, closing) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Mutex::new(released);
    let store = observed(&root, move |phase| {
        if matches!(phase, Phase::ReaderClosed | Phase::WriterClosing) {
            arrived.send(phase).expect("connection closure observed");
        }
        if phase == Phase::WriterClosing {
            released
                .lock()
                .expect("release lock")
                .recv_timeout(Duration::from_secs(15))
                .expect("writer released");
        }
    });
    store.save_info(&info(FRESH, 1)).expect("the writer is ready");
    let exclusive = lock_file(store.database());
    assert!(
        matches!(exclusive.try_lock(), Err(fs::TryLockError::WouldBlock)),
        "a live reader and writer must retain shared admission"
    );
    let dropped = thread::spawn(move || drop(store));
    let closed: Vec<_> = (0..2)
        .map(|_| {
            closing
                .recv_timeout(Duration::from_secs(15))
                .expect("connection closure before deadline")
        })
        .collect();
    assert!(closed.contains(&Phase::ReaderClosed) && closed.contains(&Phase::WriterClosing));
    assert!(
        matches!(exclusive.try_lock(), Err(fs::TryLockError::WouldBlock)),
        "the writer retains admission after the reader closes"
    );
    release.send(()).expect("writer may close");
    dropped.join().expect("store closes both connections and joins its writer");
    exclusive.try_lock().expect("both connections closed before admission is released");
}

#[test]
fn the_reader_retains_admission_after_the_writer_has_closed() {
    let directory = TempDir::new().expect("fixture");
    let store = Storage::open(directory.path().join("storage"));
    store.save_info(&info(FRESH, 1)).expect("store writes");
    let reader = store.handles().expect("store handles").reader.clone();
    let exclusive = lock_file(store.database());
    drop(store);
    assert!(
        matches!(exclusive.try_lock(), Err(fs::TryLockError::WouldBlock)),
        "a reader outliving the writer still owns the generation"
    );
    drop(reader);
    exclusive.try_lock().expect("final reader closed");
}

#[test]
fn four_independent_stores_share_admission_and_all_persist() {
    let directory = TempDir::new().expect("fixture");
    let stores: Vec<_> = (0..4).map(|_| Storage::open(directory.path().join("storage"))).collect();
    for (index, store) in stores.iter().enumerate() {
        let id = format!("0198f2c4-a1b0-7000-8000-{index:012x}");
        store.save_info(&info(&id, index as u64)).expect("independent shared owner persists");
    }
    for store in stores {
        assert_eq!(store.list_sessions().expect("shared store reads").len(), 4);
    }
}

#[test]
fn an_archive_is_readable_when_its_main_name_is_published() {
    let directory = TempDir::new().expect("fixture");
    let database = directory.path().join(DATABASE);
    let mut connection = super::connect(&database).expect("fixture opens");
    super::migrate(&mut connection, &database).expect("fixture migrates");
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .expect("real SQLite leaves its WAL at close");
    connection
        .execute("INSERT INTO session VALUES (?1, NULL, 1, 1, 'distinctive old row')", [OLD])
        .expect("old row commits");
    drop(connection);
    let main_only = directory.path().join("main-only.db");
    fs::copy(&database, &main_only).expect("main-only control copies");
    assert!(
        Connection::open(&main_only)
            .expect("main-only control opens")
            .prepare("SELECT id FROM session")
            .is_err(),
        "the fixture must still depend on its WAL immediately before moving"
    );
    let checked = Arc::new(AtomicBool::new(false));
    let seen = checked.clone();
    let parent = directory.path().to_path_buf();
    let observation = Observation(Some(Arc::new(move |phase| {
        if phase == Phase::Renamed("") {
            assert_eq!(
                old_rows(&archive(&parent)),
                vec![OLD],
                "publication must expose all old rows"
            );
            seen.store(true, Ordering::SeqCst);
        }
    })));
    move_set(&database, &observation);
    assert!(checked.load(Ordering::SeqCst), "the real main rename was observed");
}

#[test]
fn a_probe_that_spends_its_budget_still_tries_exclusive_admission() {
    let directory = TempDir::new().expect("fixture");
    let root = directory.path().join("storage");
    let planted = Storage::open(root.clone());
    planted.save_info(&info(OLD, 1)).expect("old fixture");
    drop(planted);
    let mut opener = observed(&root, |_| {});
    Arc::get_mut(&mut opener.inner).expect("not shared").admission_budget = Some(Duration::ZERO);
    assert!(opener.list_sessions().expect("uncontended maintenance is admitted").is_empty());
    assert_eq!(old_rows(&archive(directory.path())), vec![OLD]);
}

#[test]
fn a_failed_writer_open_can_be_retried() {
    let directory = TempDir::new().expect("fixture");
    let root = directory.path().join("storage");
    let database = Storage::open(root.clone()).database().to_path_buf();
    let moved = directory.path().join("temporarily-unavailable.db");
    let source = database.clone();
    let target = moved.clone();
    let first = Arc::new(Mutex::new(true));
    let store = observed(&root, move |phase| {
        if phase == Phase::WriterOpening
            && std::mem::take(&mut *first.lock().expect("fixture lock"))
        {
            fs::rename(&source, &target).expect("temporarily remove writer path");
            fs::create_dir(&source).expect("a directory really refuses SQLite open");
        }
    });
    let result = store.handles();
    assert!(result.is_err(), "startup must not cache a writer whose connection failed");
    assert!(
        store.inner.open.lock().expect("startup cache").is_none(),
        "failed startup is not cached"
    );
    assert!(
        store.inner.thread.lock().expect("writer thread slot").is_none(),
        "failed startup owns no writer thread"
    );
    let lease = lock_file(&database);
    lease.try_lock().expect("failed startup closes connections before releasing its lease");
    drop(lease);
    fs::remove_dir(&database).expect("restore availability");
    fs::rename(&moved, &database).expect("restore database");
    store.save_info(&info(FRESH, 1)).expect("the same Storage retries startup");
}
