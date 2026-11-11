//! Six processes contend for one inbox without losing or damaging accepted messages.
//!
//! Each child makes one attempt per message and reports either acceptance or the
//! lock's typed refusal. The parent recovers refused messages only after every
//! child exits, so this proves write safety without demanding admission within
//! the lock's finite retry ladder. A ready/go barrier ensures all six processes
//! reach the contended phase before any of them writes.

mod support;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use ganja_team::lock::LockError;
use ganja_team::{MailboxError, MailboxMessage, mailbox, record};

/// How many processes write at once.
const WRITERS: usize = 6;

/// Each message takes its own lock; refusals are recovered without contention.
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
    // Seed before spawning so the children race the lock, not file creation.
    mailbox::seed(&inbox).expect("the inbox seeds");
    let barrier = inbox.with_extension("barrier");
    fs::create_dir(&barrier).expect("the barrier directory is created");

    let binary = env::current_exe().expect("a test binary knows its own path");
    let mut writers = Writers(Vec::with_capacity(WRITERS));
    for nth in 0..WRITERS {
        writers.0.push(
            Command::new(&binary)
                .args([
                    "n_processes_writing_one_inbox_lose_no_message",
                    "--exact",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(WRITER, format!("writer-{nth}"))
                .env(INBOX, &inbox)
                .stdout(Stdio::piped())
                .spawn()
                .expect("a writer process starts"),
        );
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    let ready_before_go = loop {
        let ready =
            (0..WRITERS).filter(|nth| barrier.join(format!("writer-{nth}")).exists()).count();
        if ready == WRITERS {
            break ready;
        }
        assert!(Instant::now() < deadline, "only {ready} writers ready; accepted=0 refused=0");
        thread::sleep(Duration::from_millis(5));
    };
    fs::write(barrier.join("go"), []).expect("all ready writers are released");

    let mut accepted = 0;
    let mut refused = Vec::new();
    for (nth, writer) in writers.0.iter_mut().enumerate() {
        while writer.try_wait().expect("a writer's status is readable").is_none() {
            assert!(
                Instant::now() < deadline,
                "writer-{nth} did not exit; accepted={accepted} refused={}",
                refused.len()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    for (nth, writer) in writers.0.drain(..).enumerate() {
        let output = writer.wait_with_output().expect("a writer's output is readable");
        let stdout = String::from_utf8(output.stdout).expect("writer output is UTF-8");
        let outcomes: Vec<_> =
            stdout.lines().filter_map(|line| line.strip_prefix("outcome ")).collect();
        for (round, outcome) in outcomes.iter().enumerate() {
            if *outcome == format!("{round} accepted") {
                accepted += 1;
            } else if *outcome == format!("{round} refused") {
                refused.push((nth, round));
            } else {
                panic!(
                    "invalid writer-{nth} outcome {outcome:?}; accepted={accepted} refused={}",
                    refused.len()
                );
            }
        }
        assert!(
            output.status.success(),
            "writer-{nth} failed: {}; accepted={accepted} refused={}\n{stdout}",
            output.status,
            refused.len()
        );
        assert_eq!(
            outcomes.len(),
            EACH,
            "writer-{nth} outcomes; accepted={accepted} refused={}",
            refused.len()
        );
    }
    let counts = format!("accepted={accepted} refused={}", refused.len());
    assert_eq!(ready_before_go, WRITERS, "all writers ready before go; {counts}");
    assert!(accepted > 0, "the contended phase accepted no message; {counts}");
    for (nth, round) in refused {
        let writer = format!("writer-{nth}");
        mailbox::write(
            &inbox,
            MailboxMessage::new(&writer, format!("{writer}-{round}"), record::now_iso8601()),
        )
        .unwrap_or_else(|error| {
            panic!("uncontended recovery failed for {writer}-{round}: {error}; {counts}")
        });
    }

    let held = mailbox::read(&inbox).expect("the inbox reads");
    assert_eq!(
        held.dropped, 0,
        "an interleaved write left something unreadable: {:?}; {counts}",
        held.reports
    );
    assert_eq!(
        held.valid.len(),
        WRITERS * EACH,
        "every accepted or recovered message survived; {counts}"
    );
    for nth in 0..WRITERS {
        let writer = format!("writer-{nth}");
        let mine: Vec<&str> = held
            .valid
            .iter()
            .filter(|message| message.from == writer)
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(mine.len(), EACH, "{writer} lost a message: {mine:?}; {counts}");
        for round in 0..EACH {
            let expected = format!("{writer}-{round}");
            assert!(mine.contains(&expected.as_str()), "{expected} is missing; {counts}");
        }
    }

    // A peer must still be able to parse the file as an array of objects.
    let text = fs::read_to_string(&inbox).expect("the inbox is readable");
    let entries: Vec<serde_json::Value> =
        serde_json::from_str(&text).expect("the inbox is still a JSON array");
    assert_eq!(entries.len(), WRITERS * EACH, "the raw message count is intact; {counts}");
    assert!(
        entries.iter().all(serde_json::Value::is_object),
        "every entry is still an object; {counts}"
    );
    assert!(
        !support::naive_lock_of(&inbox).exists(),
        "the last writer released the lock; {counts}"
    );
}

/// Attempt each message once and report only the lock's typed refusal as expected.
fn write_as(writer: &str) {
    let inbox = PathBuf::from(env::var_os(INBOX).expect("a writer is told which inbox"));
    let barrier = inbox.with_extension("barrier");
    let go = barrier.join("go");
    assert!(!go.exists(), "{writer} started after go; accepted=0 refused=0");
    fs::write(barrier.join(writer), []).expect("a writer announces readiness");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !go.exists() {
        assert!(Instant::now() < deadline, "{writer} was not released; accepted=0 refused=0");
        thread::sleep(Duration::from_millis(5));
    }

    for round in 0..EACH {
        let outcome = match mailbox::write(
            &inbox,
            MailboxMessage::new(writer, format!("{writer}-{round}"), record::now_iso8601()),
        ) {
            Ok(_) => "accepted",
            Err(MailboxError::Lock(LockError::Held { .. })) => "refused",
            Err(error) => panic!("{writer} could not write round {round}: {error}"),
        };
        println!("\noutcome {round} {outcome}");
    }
}

// std's Child does not reap on drop. Keep cleanup local to this test so a failed
// barrier or assertion cannot leave writers waiting or writing in the background.
struct Writers(Vec<Child>);

impl Drop for Writers {
    fn drop(&mut self) {
        for writer in &mut self.0 {
            let _ = writer.kill();
            let _ = writer.wait();
        }
    }
}
