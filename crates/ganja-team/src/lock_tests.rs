use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime},
};

use backon::BackoffBuilder as _;

use super::{LOCK_SUFFIX, LockError, acquire, lock_path_of, schedule};

/// An inbox that exists, because the protocol locks a real path.
fn inbox(home: &Path) -> PathBuf {
    let path = home.join("worker-1.json");
    fs::write(&path, "[]").expect("the inbox is writable");

    path
}

/// The lock a peer would take on `inbox`, made the way a peer makes it: a
/// bare `mkdir` on the real path.
fn peers_lock(inbox: &Path) -> PathBuf {
    let lock = lock_path_of(&fs::canonicalize(inbox).expect("the inbox is real"));
    fs::create_dir(&lock).expect("a peer takes the lock");

    lock
}

/// Dates a lock directory `to`, which is the only staleness signal the
/// protocol reads. Unix-gated because opening a *directory* to set its
/// times is a unix affordance — the same gate `tests/lock_break.rs` keeps.
#[cfg(unix)]
fn date(lock: &Path, to: SystemTime) {
    fs::File::open(lock)
        .expect("a lock is openable")
        .set_modified(to)
        .expect("a lock's modification time is settable");
}

#[test]
fn the_ladder_is_proper_lockfiles_own_five_to_a_hundred() {
    let delays: Vec<Duration> = schedule().build().collect();

    // npm `retry` with `{retries: 10, minTimeout: 5, maxTimeout: 100}`,
    // factor 2 and no randomize. Ten delays, one per retry; the eleventh
    // attempt is the call that follows the last of them.
    let npm = [5_u64, 10, 20, 40, 80, 100, 100, 100, 100, 100];
    assert_eq!(delays.len(), npm.len(), "one delay per retry");

    for (delay, expected) in delays.iter().zip(npm) {
        // Not equality, and the gap is worth naming rather than rounding
        // away: backon grows a delay through `f32` *seconds*
        // (`saturating_mul`, `backon-1.6.0/src/backoff/exponential.rs:257`),
        // so doubling 20 ms lands on 39.999999 ms and doubling that on
        // 79.999998 ms. Three nanoseconds across the whole ladder, in a
        // protocol whose smallest step is five milliseconds.
        let expected = Duration::from_millis(expected);
        assert!(
            delay.abs_diff(expected) < Duration::from_micros(1),
            "{delay:?} is not npm's {expected:?}",
        );
    }

    let total: Duration = delays.iter().sum();
    assert!(
        total.abs_diff(Duration::from_millis(655)) < Duration::from_micros(1),
        "a fully contended acquire waits ≈655 ms, not {total:?}",
    );
}

#[test]
fn a_lock_names_the_whole_inbox_and_not_its_stem() {
    assert_eq!(
        lock_path_of(Path::new("/teams/t/inboxes/worker-1.json")),
        Path::new("/teams/t/inboxes/worker-1.json.lock"),
    );
    assert!(LOCK_SUFFIX.starts_with('.'));
}

#[test]
fn an_unseeded_target_is_refused_as_not_found() {
    let home = tempfile::tempdir().expect("a temp directory");
    let missing = home.path().join("never-seeded.json");

    // §2.5 seeds before it locks because the lock is on the target's real
    // path, and a file that is not there has none. That arrives as the
    // `realpath` failure it is, and it is not contention, so the ladder is
    // not spent on it.
    let refusal = acquire(&missing).expect_err("a target with no real path has no lock");
    assert!(
        matches!(&refusal, LockError::Io(error) if error.kind() == io::ErrorKind::NotFound),
        "{refusal:?}"
    );
    assert!(
        !lock_path_of(&missing).exists(),
        "nothing was made on the way to the refusal"
    );
}

#[cfg(unix)]
#[test]
fn a_stale_lock_holding_a_file_is_reported_not_broken() {
    let home = tempfile::tempdir().expect("a temp directory");
    let path = inbox(home.path());

    // A build that put a pid file inside its lock and then went away. Its
    // `rmdir` — and a peer's — answers `ENOTEMPTY`, which is exactly the
    // failure the module doc forbids a pid file for; ganja reports it and
    // deletes nothing, because what is inside is somebody else's.
    let lock = peers_lock(&path);
    fs::write(lock.join("pid"), "1234\n").expect("the pid file is writable");
    date(&lock, SystemTime::now() - Duration::from_secs(30));

    let refusal = acquire(&path).expect_err("a directory that will not go cannot be taken");
    assert!(
        matches!(
            &refusal,
            LockError::Io(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty
        ),
        "{refusal:?}"
    );
    assert!(
        lock.join("pid").is_file(),
        "what was inside the lock is untouched"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("the inbox reads"),
        "[]",
        "and so is the inbox"
    );
}

#[cfg(unix)]
#[test]
fn a_lock_dated_in_the_future_reads_as_fresh_and_is_waited_for() {
    let home = tempfile::tempdir().expect("a temp directory");
    let path = inbox(home.path());

    // A peer with a fast clock: half a minute ahead, which a signed age
    // would read as long past stale and break.
    let lock = peers_lock(&path);
    date(&lock, SystemTime::now() + Duration::from_secs(30));
    let dated = fs::metadata(&lock)
        .expect("the lock is there")
        .modified()
        .expect("a lock has a modification time");

    let refusal = acquire(&path).expect_err("a fresh lock is waited for, then refused");
    assert!(matches!(refusal, LockError::Held { .. }), "{refusal:?}");
    assert!(lock.is_dir(), "waited for, never broken");
    assert_eq!(
        fs::metadata(&lock)
            .expect("the lock is still there")
            .modified()
            .expect("a lock has a modification time"),
        dated,
        "and not touched either",
    );
}

#[test]
fn a_second_hold_of_one_inbox_waits_for_the_first() {
    let home = tempfile::tempdir().expect("a temp directory");
    let path = inbox(home.path());

    let order = Arc::new(Mutex::new(Vec::new()));
    let held = acquire(&path).expect("an unheld inbox is takeable");

    let waiter = {
        let order = Arc::clone(&order);
        let path = path.clone();
        thread::spawn(move || {
            let _second = acquire(&path).expect("the release lets the waiter in");
            order.lock().expect("the log is never poisoned").push("in");
        })
    };

    // Long enough that a lock that did not exclude would have let the
    // waiter record "in" first, which is the whole assertion below.
    thread::sleep(Duration::from_millis(50));
    order.lock().expect("the log is never poisoned").push("out");
    drop(held);

    waiter.join().expect("the waiter finishes");
    assert_eq!(
        *order.lock().expect("the log is never poisoned"),
        ["out", "in"],
        "a second hold begins after the first ends",
    );
    assert!(
        !lock_path_of(&fs::canonicalize(&path).expect("the inbox is real")).exists(),
        "both holds released",
    );
}
