//! Holding an inbox while it is rewritten: npm **proper-lockfile**'s protocol,
//! reproduced.
//!
//! **Upstream opencode has no counterpart** — it has no teams and no shared
//! file for two agents to race over. The specification is Claude Code's, read
//! out of the reference document's §2.5 (**D497**), and the reason this module
//! reproduces a protocol rather than choosing one is D-1: a real `claude`
//! process may be writing the very inbox this crate is writing. So the *lock
//! protocol* is an interop contract exactly as the document's bytes are, and
//! every detail below is somebody else's decision.
//!
//! Claude holds an inbox with `eb(path, { lockfilePath, … })`, which is npm
//! proper-lockfile (`lockfilePath` and `onCompromised` are that package's own
//! option names). Its protocol, in full:
//!
//! - `realpath` the **target** first, and lock `${realpath}.lock`. A target
//!   that is not there is `ENOENT`, which is why §2.5 seeds the inbox with
//!   `[]` *before* it locks it — see [`crate::mailbox::seed`].
//! - Acquire by **`mkdir`**. It is atomic on every filesystem worth the name,
//!   and `EEXIST` means somebody else holds it.
//! - Release by `rmdir`, in the `finally` this module spells [`Guard`]'s
//!   [`Drop`].
//! - Staleness is the lock directory's **mtime**, older than [`STALE`]
//!   (proper-lockfile's `stale: 10000`). A peer that finds one removes it and
//!   takes the lock — the crash recovery, and the reason a wedged teammate is
//!   a ten-second problem rather than a permanent one.
//! - The retry ladder is §2.5's literal `{retries: 10, minTimeout: 5,
//!   maxTimeout: 100}`: [`RETRIES`] delays of 5, 10, 20, 40, 80 and then 100 ms
//!   five times, ≈655 ms of waiting over eleven attempts. The private
//!   `schedule` builds it, and says what had to be read in backon's source to
//!   be sure of the count.
//!
//! Three things this module deliberately does **not** do, each because doing
//! them would break the peer:
//!
//! *No pid file inside the directory.* Claude's own stale cleanup is `rmdir`,
//! which fails `ENOTEMPTY` on a directory holding anything at all — so a pid
//! file would turn its recovery into a failure. The same reasoning forbids a
//! lock **file**: `rmdir` on one fails `ENOTDIR`. What ganja writes has to be
//! the empty directory the peer knows how to remove.
//!
//! *No liveness probe.* Staleness is mtime and nothing else, because that is
//! the only signal both sides read. A pid ganja checked would be a pid Claude
//! ignores, and the two builds would disagree about who is wedged.
//!
//! *No heartbeat.* proper-lockfile's holder refreshes the mtime every
//! `stale / 2`; ganja does not, because a ganja hold is one sub-second
//! read-modify-write. A hold that somehow reaches ten seconds is broken by a
//! peer exactly as proper-lockfile would break it — accepted, and said here
//! rather than discovered.
//!
//! Beside the on-disk lock there is an **in-process** one, keyed by the same
//! canonical path. That half is D-9's own addition and has no counterpart in
//! the reference: one lead process holds several runners, and two of its own
//! threads racing the same `mkdir` would spend the ladder discovering what a
//! `Mutex` answers immediately. The two are taken in that order — in-process,
//! then disk — and released in the mirror of it.
//!
//! Nothing here logs a message body: a lock line carries a path, an age and a
//! count. `tests/no_bodies_in_logs.rs` is the canary that keeps it true.

use std::{
    collections::HashSet,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    sync::{Condvar, LazyLock, Mutex, PoisonError},
    time::{Duration, SystemTime},
};

use backon::{BlockingRetryable as _, ExponentialBuilder};

/// What is appended to a target's real path to name its lock (§2.5's
/// `${path}.lock`).
pub const LOCK_SUFFIX: &str = ".lock";

/// How old a lock directory must be before a peer may break it —
/// proper-lockfile's `stale: 10000`, in the unit Rust states it in.
pub const STALE: Duration = Duration::from_secs(10);

/// How many times an acquire retries after its first attempt (npm `retry`'s
/// `retries: 10`, so eleven attempts in all).
pub const RETRIES: usize = 10;

/// The first delay of the ladder (`minTimeout: 5`).
const MIN_DELAY: Duration = Duration::from_millis(5);

/// The longest delay it grows to (`maxTimeout: 100`).
const MAX_DELAY: Duration = Duration::from_millis(100);

/// Why a write was refused when a peer would not let the inbox go.
pub const REFUSED_LOCK_HELD: &str = "another writer holds this inbox";

/// Why a write was refused when what stands in the lock's place cannot be one.
pub const REFUSED_LOCK_NOT_A_DIRECTORY: &str =
    "a lock is a directory, and what stands at this one's path is not";

/// A lock that could not be taken.
///
/// Every variant names the **path** it is about and nothing else: a path is an
/// address, where a message is content.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// The ladder ran out with the lock still held by somebody.
    ///
    /// This is pre-mortem 2's shape: the holder is either alive and slow, or
    /// dead less than [`STALE`] ago. Either way the answer is to report the
    /// failure rather than to retry blind.
    #[error("{REFUSED_LOCK_HELD}: failed to acquire {} after {RETRIES} retries", path.display())]
    Held {
        /// The lock directory that would not be taken.
        path: PathBuf,
    },
    /// Something that is not a directory sits at the lock's path, and it went
    /// stale.
    ///
    /// A lock **file** is what a build that reached for `O_EXCL` instead of
    /// `mkdir` would leave, and proper-lockfile fails on it exactly as this
    /// does: its own cleanup is `fs.rmdir`, which answers `ENOTDIR`. So this is
    /// reported rather than repaired — unlinking somebody else's file because
    /// its name matched would be this crate deciding what another build meant.
    #[error("{REFUSED_LOCK_NOT_A_DIRECTORY}: {}", path.display())]
    NotADirectory {
        /// The path that is not a lock directory.
        path: PathBuf,
    },
    /// The lock could not be made, read or removed.
    ///
    /// `ENOENT` from the target's `realpath` arrives here too, which is the
    /// protocol working: §2.5 seeds an inbox before it locks it, so a missing
    /// target means the seed was skipped.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl LockError {
    /// Whether retrying could plausibly change the answer.
    ///
    /// Only contention can: a lock path that is a file will still be a file
    /// next time, and an `ENOENT` target will still be missing. Retrying those
    /// would spend the ladder to arrive at the same sentence 655 ms later.
    fn is_contention(&self) -> bool {
        matches!(self, Self::Held { .. })
    }
}

/// A held inbox. Dropping it releases the lock, on both paths.
///
/// The field order is load-bearing: Rust runs [`Drop::drop`] and *then* drops
/// the fields, so the directory is removed before the in-process hold is
/// released — the mirror of the order [`acquire`] takes them in, which is what
/// keeps a thread woken by the in-process release from finding a directory
/// still there.
#[derive(Debug)]
pub struct Guard {
    /// The lock directory this guard made and will remove.
    dir: PathBuf,
    /// The in-process half, released when this field drops.
    local: Local,
}

impl Guard {
    /// The lock directory being held.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The inbox this hold is over, as its real path — the path the lock is
    /// named after, and the key the in-process half is filed under.
    #[must_use]
    pub fn inbox(&self) -> &Path {
        &self.local.key
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        match fs::remove_dir(&self.dir) {
            Ok(()) => tracing::debug!(lock = %self.dir.display(), "released an inbox lock"),
            // Somebody broke this hold: it outlived `STALE`, or a peer decided
            // it had. Worth a line, and worth nothing more — proper-lockfile
            // hands the same case to an `onCompromised` callback, which is the
            // precedent that it is expected rather than exceptional.
            Err(error) => tracing::warn!(
                lock = %self.dir.display(),
                %error,
                "an inbox lock was already gone when its holder released it",
            ),
        }
    }
}

/// Takes the inbox at `target`, waiting out the [`RETRIES`] retries for a peer
/// that holds it and breaking one that has gone [`STALE`].
///
/// The target must exist: §2.5 locks `realpath(target) + ".lock"`, and a real
/// path is a thing a real file has. [`crate::mailbox::seed`] is what guarantees
/// it, and it is why the seed is the step *before* this one rather than the
/// step after.
///
/// # Errors
///
/// [`LockError::Held`] when the ladder runs out with a peer still holding the
/// lock, [`LockError::NotADirectory`] when something that is not a lock
/// directory has gone stale at the lock's path, and [`LockError::Io`] for
/// anything the filesystem refused — including the `ENOENT` of a target that
/// was never seeded.
pub fn acquire(target: &Path) -> Result<Guard, LockError> {
    let key = fs::canonicalize(target)?;
    let dir = lock_path_of(&key);

    // In-process first. Held across the whole disk acquire, and released by
    // this binding's own `Drop` on every error path below — including the `?`.
    let local = Local::hold(key);

    (|| take(&dir))
        .retry(schedule())
        .when(LockError::is_contention)
        .call()?;
    tracing::debug!(lock = %dir.display(), "took an inbox lock");

    Ok(Guard { dir, local })
}

/// The lock a target is held through: its real path with [`LOCK_SUFFIX`]
/// appended.
///
/// # Errors
///
/// Whatever `realpath` returned — `ENOENT` for a target that is not there.
pub fn path_of(target: &Path) -> io::Result<PathBuf> {
    Ok(lock_path_of(&fs::canonicalize(target)?))
}

/// proper-lockfile's ladder, reproduced: `{retries: 10, minTimeout: 5,
/// maxTimeout: 100}` under npm `retry`'s factor-2, no-randomize defaults — 5,
/// 10, 20, 40, 80, then 100 ms five times, ≈655 ms of waiting over eleven
/// attempts.
///
/// **`with_max_times` counts the delays, not the attempts**, which is the one
/// thing about this that had to be checked rather than assumed, since npm's
/// `retries: 10` means *eleven* calls. Read in the vendored source, not the
/// prose: `ExponentialBackoff::next` stops yielding once
/// `attempts >= max_times` (`backon-1.6.0/src/backoff/exponential.rs:205`), and
/// `BlockingRetry::call` pulls a delay only *after* a failed attempt and gives
/// up when the iterator ends (`backon-1.6.0/src/blocking_retry.rs:246`). So
/// [`RETRIES`] goes in unadjusted: one attempt plus ten retries.
///
/// The other three numbers are `ExponentialBuilder`'s own defaults, and they
/// are npm's: factor 2, jitter off, no total-delay ceiling.
///
/// One gap between this ladder and npm's is real and is recorded rather than
/// rounded away: backon doubles a delay through `f32` **seconds**
/// (`saturating_mul`, `backon-1.6.0/src/backoff/exponential.rs:257`), so the
/// fourth delay is 39.999999 ms and the fifth 79.999998 ms. Three nanoseconds
/// across a ladder whose smallest step is five milliseconds — pinned by
/// `the_ladder_is_proper_lockfiles_own_five_to_a_hundred` so a future version
/// that drifts by something that matters is caught by the same test.
fn schedule() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(MIN_DELAY)
        .with_max_delay(MAX_DELAY)
        .with_max_times(RETRIES)
}

/// One attempt at the on-disk lock: `mkdir`, and proper-lockfile's one
/// recovery when that says the lock is taken.
fn take(dir: &Path) -> Result<(), LockError> {
    if make(dir)? {
        return Ok(());
    }

    match age_of(dir) {
        // Gone between the two calls: the holder released while we were asking
        // about it. The `make` below is the same immediate retry
        // proper-lockfile makes, so a release does not cost a waiter a delay.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(age) if age >= STALE => break_stale(dir, age)?,
        Ok(_) => return Err(held(dir)),
    }

    if make(dir)? { Ok(()) } else { Err(held(dir)) }
}

/// `true` when this call is the one that made the directory.
///
/// `EEXIST` is not an error here — it is the whole answer the protocol is
/// asking for, which is why it comes back as `false` rather than as a kind a
/// caller has to remember to match.
fn make(dir: &Path) -> Result<bool, LockError> {
    match fs::create_dir(dir) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// How long ago the lock was last written — the only staleness signal in the
/// protocol.
///
/// A modification time in the future (a peer with a fast clock, or a directory
/// touched across a clock change) reads as an age of zero, i.e. fresh. Breaking
/// a lock because two machines disagree about the hour is the one outcome worth
/// ruling out here.
fn age_of(dir: &Path) -> io::Result<Duration> {
    let modified = fs::metadata(dir)?.modified()?;

    Ok(SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO))
}

/// Removes a lock whose holder is gone, and says so.
fn break_stale(dir: &Path, age: Duration) -> Result<(), LockError> {
    match fs::remove_dir(dir) {
        Ok(()) => {
            tracing::warn!(
                lock = %dir.display(),
                age_ms = age.as_millis(),
                stale_ms = STALE.as_millis(),
                "broke a stale inbox lock",
            );

            Ok(())
        }
        // A peer broke it first, which is a race with a winner and no loser.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        // Checked after the failure rather than before it, so the common path
        // pays one syscall rather than two. A *file* here is the `ENOTDIR` case
        // the module doc names; a directory that will not go is something else
        // (`ENOTEMPTY` — somebody wrote inside a lock, which is the failure a
        // pid file would cause) and travels as itself.
        Err(_) if !dir.is_dir() => Err(LockError::NotADirectory {
            path: dir.to_path_buf(),
        }),
        Err(error) => Err(error.into()),
    }
}

/// The refusal a contended attempt reports, and the one the ladder retries.
fn held(dir: &Path) -> LockError {
    LockError::Held {
        path: dir.to_path_buf(),
    }
}

/// `${path}.lock`, built on the bytes rather than through `Path::set_extension`
/// — an inbox is `worker-1.json`, and setting an extension would lock
/// `worker-1.lock`, a different file that a peer would never look at.
fn lock_path_of(real: &Path) -> PathBuf {
    let mut name = OsString::from(real.as_os_str());
    name.push(LOCK_SUFFIX);

    PathBuf::from(name)
}

/// The inboxes this process holds, and the writers parked on one.
///
/// A set behind one lock rather than the map of per-path mutexes the shape
/// suggests: a map would have to keep every mutex it ever handed out, since a
/// `MutexGuard` cannot outlive the map entry it borrows, and a process that
/// spawns teammates over a long session would accumulate them. A set drops a
/// path the moment it is released, and one condvar for all inboxes is cheaper
/// than that bookkeeping at the handful of inboxes a lead actually holds — the
/// cost is that releasing one wakes whoever waits on another, and they park
/// again.
static IN_PROCESS: LazyLock<(Mutex<HashSet<PathBuf>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(HashSet::new()), Condvar::new()));

/// The in-process half of a hold: one path this process has claimed.
#[derive(Debug)]
struct Local {
    /// The canonical inbox path, released from [`IN_PROCESS`] on drop.
    key: PathBuf,
}

impl Local {
    /// Claims `key`, waiting for whichever thread of this process holds it.
    ///
    /// Unbounded, where the disk half is bounded: a peer *process* may be dead,
    /// which is what the stale break is for, but a thread of this process
    /// holding a lock is a thread this process is running, and it is doing one
    /// sub-second read-modify-write.
    fn hold(key: PathBuf) -> Self {
        let (held, free) = &*IN_PROCESS;
        let mut held = held.lock().unwrap_or_else(PoisonError::into_inner);
        // Poisoning is ignored throughout, deliberately: a thread that panicked
        // mid-insert left a set that is still a set, and refusing every future
        // mailbox write over it would turn one panic into a wedged team.
        while !held.insert(key.clone()) {
            held = free.wait(held).unwrap_or_else(PoisonError::into_inner);
        }

        Self { key }
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        let (held, free) = &*IN_PROCESS;
        held.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
        free.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    use backon::BackoffBuilder as _;

    use super::{LOCK_SUFFIX, acquire, lock_path_of, schedule};

    /// An inbox that exists, because the protocol locks a real path.
    fn inbox(home: &Path) -> std::path::PathBuf {
        let path = home.join("worker-1.json");
        fs::write(&path, "[]").expect("the inbox is writable");

        path
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
}
