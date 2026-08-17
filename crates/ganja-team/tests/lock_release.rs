//! A hold that is taken is a hold that is given back — including on the paths
//! nobody plans for (AC-2).
//!
//! This is pre-mortem 2 approached from the inside. A lock directory left
//! behind by ganja's own failure path wedges the inbox for ten seconds and then
//! gets broken by whoever comes next: recoverable, and still a bug, because the
//! failure that left it was one this process saw and returned from. The `finally`
//! §2.5 names is `Guard`'s `Drop` here, and `Drop` runs on the way out of a
//! function whether the function is returning a value or an error — that is the
//! property, and this is the test of it.
//!
//! The failure is injected rather than simulated: the inbox path is made a
//! *directory*, so the read inside the hold fails `EISDIR`. That is a real io
//! error arriving where a real one would, after the lock has been taken and
//! before anything has been written, which is exactly the window that matters.

use std::{fs, path::PathBuf};

use ganja_team::{MailboxMessage, MemberName, TeamName, TeamsRoot, mailbox, record};
use serde_json::json;

#[test]
fn every_lock_is_released_even_when_the_write_fails() {
    let home = tempfile::tempdir().expect("a temp directory");
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("session-224cbeab").expect("a valid team name");

    let wedged = root.inbox_path(
        &team,
        &MemberName::parse("wedged").expect("a valid member name"),
    );
    fs::create_dir_all(&wedged).expect("the inbox path is takeable");

    let refusal = mailbox::write(
        &wedged,
        MailboxMessage::new("team-lead", "never lands", record::now_iso8601()),
    )
    .expect_err("a write into a directory fails");
    // The point of naming the variant: an `Io` refusal is one that happened
    // *past* the lock. A `Lock` refusal would mean the hold was never taken and
    // this test would be asserting nothing.
    assert!(
        matches!(refusal, mailbox::MailboxError::Io(_)),
        "the failure lands inside the hold, not before it: {refusal:?}",
    );
    assert!(
        !lock_of(&wedged).exists(),
        "a failed write gave its lock back",
    );

    // The same claim for the refusal that never reaches the disk at all: a
    // message refused by validation must not have cost a hold either.
    let refused = root.inbox_path(
        &team,
        &MemberName::parse("refused").expect("a valid member name"),
    );
    mailbox::write(
        &refused,
        MailboxMessage::new("team-lead", "kept", record::now_iso8601()),
    )
    .expect("a message writes");
    mailbox::write_value(&refused, json!({"from": "w", "text": 42, "timestamp": "t"}))
        .expect_err("a number is not a message body");
    assert!(
        !lock_of(&refused).exists(),
        "a refused write left no hold behind",
    );

    // And the ordinary path, which is what proves the assertions above are not
    // passing because nothing ever took a lock in this process.
    mailbox::write(
        &refused,
        MailboxMessage::new("team-lead", "also kept", record::now_iso8601()),
    )
    .expect("the inbox was never wedged by the refusals above");
    assert_eq!(
        mailbox::read(&refused)
            .expect("the inbox reads")
            .valid
            .len(),
        2
    );
    assert!(
        !lock_of(&refused).exists(),
        "a successful write gave its lock back too",
    );
}

/// `${path}.lock`, spelled the way a peer that never canonicalized anything
/// would spell it — through the same symlinks, at the same file.
fn lock_of(inbox: &std::path::Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", inbox.display()))
}
