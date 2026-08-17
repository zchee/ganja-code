//! Six processes, one inbox, and not one message lost (AC-2).
//!
//! Threads would prove nothing here. The lock this exercises is a *directory on
//! disk*, taken so that a real `claude` process racing ganja for the same inbox
//! agrees about who holds it (D-1, §2.5); an in-process `Mutex` would serialize
//! the writers before the protocol under test ever ran. So the writers are real
//! processes.
//!
//! They are this very binary, re-executed. A test binary knows its own path and
//! libtest already accepts a filter, so `current_exe` plus `--exact` runs one
//! named test in a child — which is why the one test below opens by asking
//! whether it is the parent or one of the six. That keeps the whole thing in one
//! file, with no helper binary to declare.
//!
//! The child's env is set on the `Command`, never on this process, so nothing
//! here mutates process-wide state and the file stays honest about that too.

mod support;

use std::{env, path::PathBuf, process::Command};

use ganja_team::{MailboxMessage, mailbox, record};

/// How many processes write at once.
const WRITERS: usize = 6;

/// How many messages each of them writes. Every one is a whole read-modify-write
/// under its own acquire, so this is thirty contended holds, not six.
///
/// **These numbers are a flakiness budget, and the budget is the retry ladder.**
/// A writer that cannot take the lock within proper-lockfile's ten delays
/// (≈655 ms — `lock.rs`'s `schedule`) gets `LockError::Held`, panics in
/// [`write_as`], and fails this test. Six writers × five rounds is comfortable
/// on an idle machine, where a hold is a sub-millisecond rewrite of a file with
/// at most thirty short entries. It is *not* unconditionally safe: on a CI box
/// loaded enough that a single hold — seed, canonicalize, mkdir, read, encode,
/// temp-write, fsync, rename, rmdir — is descheduled past 655 ms while five
/// peers queue behind it, the ladder can genuinely run out and this test fails
/// for a reason that is not a bug in the protocol.
///
/// Left as it is deliberately: raising the ladder would mean diverging from the
/// peer's own schedule, which is the one thing this module may not do, and
/// lowering these counts would weaken the very contention the test exists to
/// create. So the note is the mitigation — a failure here reading
/// `another writer holds this inbox` is the first thing to blame on the machine
/// rather than on the change under review, and the second thing to confirm by
/// re-running it alone.
const EACH: usize = 5;

/// Set on a child, absent in the parent: which writer this process is.
const WRITER: &str = "GANJA_TEAM_CONTENTION_WRITER";

/// The inbox every child writes into.
const INBOX: &str = "GANJA_TEAM_CONTENTION_INBOX";

#[test]
fn n_processes_writing_one_inbox_lose_no_message() {
    if let Ok(writer) = env::var(WRITER) {
        return write_as(&writer);
    }

    let (_home, root, team) = support::root("session-224cbeab");
    let inbox = support::inbox_of(&root, &team, "shared-inbox");
    // §2.5's step 1, and the reason it is step 1: the lock is on the target's
    // *real* path, so the target has to be there before anybody locks it. Every
    // child would seed it anyway; doing it here means all six race the lock
    // rather than racing the seed.
    mailbox::seed(&inbox).expect("the inbox seeds");

    let binary = env::current_exe().expect("a test binary knows its own path");
    let writers: Vec<_> = (0..WRITERS)
        .map(|nth| {
            Command::new(&binary)
                .args([
                    "n_processes_writing_one_inbox_lose_no_message",
                    "--exact",
                    "--test-threads=1",
                ])
                .env(WRITER, format!("writer-{nth}"))
                .env(INBOX, &inbox)
                .spawn()
                .expect("a writer process starts")
        })
        .collect();
    for (nth, mut writer) in writers.into_iter().enumerate() {
        let status = writer.wait().expect("a writer process is waitable");
        assert!(status.success(), "writer-{nth} failed: {status}");
    }

    let held = mailbox::read(&inbox).expect("the inbox reads");
    assert_eq!(
        held.dropped, 0,
        "an interleaved write left something unreadable: {:?}",
        held.reports
    );
    assert_eq!(
        held.valid.len(),
        WRITERS * EACH,
        "every message a writer was told landed survived",
    );
    for nth in 0..WRITERS {
        let writer = format!("writer-{nth}");
        let mine: Vec<&str> = held
            .valid
            .iter()
            .filter(|message| message.from == writer)
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(mine.len(), EACH, "{writer} lost a message: {mine:?}");
        for round in 0..EACH {
            let expected = format!("{writer}-{round}");
            assert!(mine.contains(&expected.as_str()), "{expected} is missing");
        }
    }

    // Not "it parses as messages" — that is what `read` just said. This is the
    // file itself still being one JSON array, which is what a peer that has
    // never heard of ganja is going to try to parse.
    let text = std::fs::read_to_string(&inbox).expect("the inbox is readable");
    let entries: Vec<serde_json::Value> =
        serde_json::from_str(&text).expect("the inbox is still a JSON array");
    assert_eq!(entries.len(), WRITERS * EACH);
    assert!(
        entries.iter().all(serde_json::Value::is_object),
        "every entry is still an object",
    );

    // And nobody left a hold behind.
    assert!(
        !support::naive_lock_of(&inbox).exists(),
        "the last writer released the lock",
    );
}

/// One writer's whole job: [`EACH`] messages, each through the public door, each
/// taking and releasing the lock on its own.
fn write_as(writer: &str) {
    let inbox = PathBuf::from(env::var_os(INBOX).expect("a writer is told which inbox"));

    for round in 0..EACH {
        mailbox::write(
            &inbox,
            MailboxMessage::new(writer, format!("{writer}-{round}"), record::now_iso8601()),
        )
        .unwrap_or_else(|error| panic!("{writer} could not write round {round}: {error}"));
    }
}
