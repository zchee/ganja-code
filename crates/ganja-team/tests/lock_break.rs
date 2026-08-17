//! The three claims proper-lockfile's staleness rule makes, and ganja's with it
//! (AC-2's second clause, pre-mortem 2).
//!
//! A lock directory older than ten seconds is a crashed holder's and is broken;
//! a fresh one is a live holder's and is waited for; and what is held is a
//! *directory*, because Claude's own cleanup is `rmdir` and would fail
//! `ENOTDIR` on anything else (§2.5, D-1).
//!
//! Two things about how this is tested are deliberate.
//!
//! *The clock is not waited on.* A ten-second sleep in a unit suite is ten
//! seconds every developer pays forever; the lock's own signal is the
//! directory's mtime, so the test writes an old mtime with `File::set_times`
//! and the protocol reads exactly what it would have read.
//!
//! *The peer never lets go.* The alternative — release the lock mid-ladder and
//! assert the writer got in — races the retry schedule: the writer is asleep for
//! between 5 and 100 ms at a time, so which side wins depends on the machine.
//! A peer that holds forever has one outcome, and the typed refusal at the end
//! of the ladder is also the witness that the ladder is proper-lockfile's: it
//! cannot arrive before the ten delays have been slept through.
//!
//! Every hold here is a bare `create_dir`, never ganja's own `acquire`, because
//! a bare `mkdir` is precisely what the peer this protocol exists for does.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use ganja_team::{
    MailboxMessage, MemberName, TeamName, TeamsRoot,
    lock::{self, LockError},
    mailbox::{self, MailboxError},
    record,
};

/// What a fully contended acquire sleeps through: 5 + 10 + 20 + 40 + 80 +
/// 100 × 5 ms — **less three nanoseconds**.
///
/// Stated exactly rather than as a round 655 ms, so this file and
/// `lock.rs`'s own `the_ladder_is_proper_lockfiles_own_five_to_a_hundred` agree
/// about one number instead of about two that differ. The three nanoseconds are
/// that test's own finding: backon doubles a delay through `f32` seconds, so the
/// fourth delay is 39.999999 ms and the fifth 79.999998 ms. Rounding up would
/// make the `waited >= LADDER` assertion below a bound the ladder provably
/// cannot meet — it would pass only because a real `sleep` overshoots, which is
/// a test resting on the scheduler rather than on the protocol.
const LADDER: Duration = Duration::from_nanos(654_999_997);

#[cfg(unix)]
#[test]
fn a_lock_directory_older_than_ten_seconds_is_broken_and_the_write_proceeds() {
    let (_home, inbox) = inbox("crashed");
    mailbox::write(&inbox, message("first")).expect("a message writes");

    let held = lock_of(&inbox);
    fs::create_dir(&held).expect("a peer takes the lock");
    backdate(&held, Duration::from_secs(30));

    let started = Instant::now();
    mailbox::write(&inbox, message("second")).expect("a stale lock does not stop a write");
    let waited = started.elapsed();

    assert!(
        waited < LADDER,
        "a stale lock is broken, not waited out: {waited:?}",
    );
    let inbox_contents = mailbox::read(&inbox).expect("the inbox reads");
    assert_eq!(inbox_contents.valid.len(), 2, "the write landed");
    assert!(!held.exists(), "the write released the lock it took");
}

#[test]
fn a_fresh_lock_directory_held_by_a_peer_is_waited_for_not_broken() {
    let (_home, inbox) = inbox("busy");
    mailbox::write(&inbox, message("first")).expect("a message writes");

    let held = lock_of(&inbox);
    fs::create_dir(&held).expect("a peer takes the lock");
    let taken = fs::metadata(&held)
        .expect("the lock is there")
        .modified()
        .expect("a lock has a modification time");

    let started = Instant::now();
    let refusal = mailbox::write(&inbox, message("second")).expect_err("a held inbox refuses");
    let waited = started.elapsed();

    assert!(
        waited >= LADDER,
        "the whole ladder ran before the refusal: {waited:?}",
    );
    assert!(
        matches!(refusal, MailboxError::Lock(LockError::Held { .. })),
        "{refusal:?}",
    );
    // The sentence a wedged team is diagnosed by, so it is asserted rather than
    // left to drift: it names the lock and how many retries went into it.
    let said = refusal.to_string();
    assert!(said.contains(&held.display().to_string()), "{said}");
    assert!(said.contains("after 10 retries"), "{said}");

    assert!(held.is_dir(), "a fresh lock is waited for, never broken");
    assert_eq!(
        fs::metadata(&held)
            .expect("the lock is still there")
            .modified()
            .expect("a lock has a modification time"),
        taken,
        "and it is not touched either — ganja refreshes nobody's mtime",
    );
    assert_eq!(
        mailbox::read(&inbox).expect("the inbox reads").valid.len(),
        1,
        "the refused write left the inbox exactly as it was",
    );
}

#[cfg(unix)]
#[test]
fn the_lock_is_a_directory_never_a_file() {
    let (home, inbox) = inbox("shaped");
    mailbox::write(&inbox, message("first")).expect("a message writes");

    // What ganja itself takes is a directory, and it is gone when the hold ends.
    let held = lock_of(&inbox);
    let hold = lock::acquire(&inbox).expect("an unheld inbox is takeable");
    assert!(held.is_dir(), "an acquire makes a directory, not a file");
    // The naive spelling above and the protocol's own realpath one name one
    // directory — which is what makes a peer that canonicalizes nothing (and a
    // `TMPDIR` that is a symlink) contend with ganja rather than beside it.
    assert_eq!(
        held.canonicalize().expect("the lock is real"),
        lock_of(&inbox.canonicalize().expect("the inbox is real")),
        "the two spellings are the same lock",
    );
    assert!(
        fs::read_dir(&held)
            .expect("a lock is a readable directory")
            .next()
            .is_none(),
        "and an empty one: anything inside would make a peer's rmdir fail ENOTEMPTY",
    );
    drop(hold);
    assert!(!held.exists(), "the hold released");

    // A *file* at the lock's path is what a build that reached for `O_EXCL`
    // would leave. Fresh, it reads as held — `mkdir` answers `EEXIST` for a file
    // exactly as it does for a directory — so the ladder runs and the refusal is
    // the ordinary one.
    let contested = inbox_in(home.path(), "filed");
    mailbox::write(&contested, message("first")).expect("a message writes");
    let file = lock_of(&contested);
    fs::write(&file, "1234\n").expect("a lock-shaped file is writable");

    let refusal = mailbox::write(&contested, message("second")).expect_err("a fresh file holds");
    assert!(
        matches!(refusal, MailboxError::Lock(LockError::Held { .. })),
        "{refusal:?}",
    );

    // Stale, it cannot be broken: `remove_dir` on a file is `ENOTDIR`, which is
    // what proper-lockfile's own cleanup would hit too — its `rmdir` fails and
    // its acquire errors out. Ganja reports the same thing rather than unlinking
    // somebody else's file because the name matched.
    backdate(&file, Duration::from_secs(30));
    let refusal = mailbox::write(&contested, message("second")).expect_err("a file is not a lock");
    assert!(
        matches!(refusal, MailboxError::Lock(LockError::NotADirectory { .. })),
        "{refusal:?}",
    );
    assert_eq!(
        fs::read_to_string(&file).expect("the file is readable"),
        "1234\n",
        "what was not a lock was not touched",
    );
    assert_eq!(
        mailbox::read(&contested)
            .expect("the inbox reads")
            .valid
            .len(),
        1,
        "and neither was the inbox",
    );
}

/// A temporary home, and one member's inbox path under it. The inbox itself is
/// made by the first write, which is what seeds it.
fn inbox(member: &str) -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("a temp directory");
    let path = inbox_in(home.path(), member);

    (home, path)
}

/// The same path, under a home somebody else is already holding.
fn inbox_in(home: &Path, member: &str) -> PathBuf {
    let root = TeamsRoot::new(home.join("teams"));
    let team = TeamName::parse("session-224cbeab").expect("a valid team name");
    let member = MemberName::parse(member).expect("a valid member name");

    root.inbox_path(&team, &member)
}

/// `${path}.lock`, as a peer that canonicalized nothing would spell it.
fn lock_of(inbox: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", inbox.display()))
}

/// Makes a lock look `by` older than it is.
///
/// Opening a directory read-only and setting its times is enough on every unix
/// this builds for — `futimens` asks for ownership, not for a writable
/// descriptor — and it is the only way to test a ten-second rule in a
/// millisecond.
///
/// `#[cfg(unix)]` for exactly that reason, and to match `mailbox.rs`'s own
/// mode-preservation test: `File::open` on a *directory* is a unix affordance,
/// so this and the two tests that call it are unix-gated rather than left to
/// discover the difference at some future port. Nothing is lost today — the
/// windows lane is parked — and the gate is what says so out loud.
#[cfg(unix)]
fn backdate(path: &Path, by: Duration) {
    let handle = File::open(path).expect("a lock is openable");
    handle
        .set_modified(SystemTime::now() - by)
        .expect("a lock's modification time is settable");
}

/// One message, timestamped now.
fn message(text: &str) -> MailboxMessage {
    MailboxMessage::new("team-lead", text, record::now_iso8601())
}
