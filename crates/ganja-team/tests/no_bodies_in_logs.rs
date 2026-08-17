//! What a teammate wrote must not come back out of a log line.
//!
//! The risk is not a `println!` somebody forgot. It is that a message body
//! travels through a `tracing` field, a `Debug` rendering, or a diagnostic
//! about a *damaged* entry — and nobody notices, because none of those look
//! like printing a conversation. A mailbox is the one place in this workspace
//! where user content is routinely read, rewritten and complained about, and
//! §2.4's own drop diagnostics are field-level for exactly this reason.
//!
//! So a canary body is written the way a real one is, the file is read back,
//! delivered, pruned, and then damaged so the corruption path runs too — and
//! every byte of what the library traced is searched.
//!
//! One test, one binary, like `ganja-core`'s `secrets_env.rs`: the capture is
//! installed as the **global** subscriber. A thread-local one only sees what
//! the calling thread traces, so it would quietly stop covering the library the
//! day anything here moved to a thread of its own — the assertions would still
//! pass, on an empty search space. Being global means the flavour cannot
//! matter, and asserting that library-internal lines *arrived* is what proves
//! the search space was not empty.

use std::{
    fs, io,
    sync::{Arc, Mutex},
};

use ganja_team::{MailboxMessage, MemberName, TeamName, TeamsRoot, mailbox, record};
use tracing_subscriber::fmt::MakeWriter;

/// The body of a message that is written, read and delivered normally.
const DELIVERED_CANARY: &str = "kaleidoscopic-otter-9174";

/// The body of a message that is damaged on disk, so the drop-report path is
/// the one carrying it.
const DROPPED_CANARY: &str = "phosphorescent-gannet-3312";

/// A `tracing` writer a test can read back.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn logged(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("the log is never poisoned")).into_owned()
    }
}

impl io::Write for Capture {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the log is never poisoned")
            .extend_from_slice(buffer);

        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn a_message_body_never_reaches_a_log_line() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("this binary holds one test, so nothing else has installed one");

    let home = tempfile::tempdir().expect("a temp directory");
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("session-224cbeab").expect("a valid team name");
    let worker = MemberName::parse("demo-worker-1").expect("a valid member name");
    let inbox = root.inbox_path(&team, &worker);

    // Written, read, delivered, pruned — the whole ordinary path.
    mailbox::write(
        &inbox,
        MailboxMessage::new("team-lead", DELIVERED_CANARY, record::now_iso8601()),
    )
    .expect("a message writes");

    let held = mailbox::read(&inbox).expect("the inbox reads");
    assert_eq!(held.valid.len(), 1, "the write is the search space");
    let delivered: Vec<_> = held.valid.iter().map(mailbox::identity).collect();

    // The one rendering that could put a body into somebody else's log line.
    let rendered = format!("{delivered:?}");
    assert!(
        rendered.contains("Identity(team-lead|"),
        "an identity still says who and when: {rendered}"
    );

    let pruned = mailbox::prune_delivered(&inbox, &delivered).expect("the prune writes");
    assert_eq!(pruned.pruned, 1);

    // And the corruption path, which is the one that handles an entry's own
    // bytes on the way to complaining about it.
    fs::write(
        &inbox,
        format!("[{{\"from\": \"w\", \"text\": \"{DROPPED_CANARY}\", \"timestamp\": 7}}]"),
    )
    .expect("the inbox is writable");
    let damaged = mailbox::read(&inbox).expect("a damaged inbox still reads");
    assert_eq!(damaged.dropped, 1);
    assert_eq!(damaged.reports.len(), 1, "the drop is the search space");
    assert!(
        !damaged.reports[0].contains(DROPPED_CANARY),
        "a drop report names the field and the type, never the body: {}",
        damaged.reports[0]
    );

    let logged = capture.logged();

    // Not "something was captured": the three lines this library traces on the
    // three paths above, each written by the library and none by this test.
    // Without them, finding no body in the log would prove nothing.
    for line in [
        "a message joined an inbox",
        "pruned delivered messages",
        "dropped an unreadable inbox entry",
    ] {
        assert!(
            logged.contains(line),
            "the capture never saw the library trace {line:?}, so finding no body \
             in it would prove nothing:\n{logged}"
        );
    }

    for body in [DELIVERED_CANARY, DROPPED_CANARY] {
        assert!(
            !logged.contains(body),
            "a message body reached the log:\n{logged}"
        );
    }
    for body in [DELIVERED_CANARY, DROPPED_CANARY] {
        assert!(
            !rendered.contains(body),
            "a message body reached an identity's rendering: {rendered}"
        );
    }

    // The file is where a body does belong, and a test proving nothing rendered
    // one because nothing ever held one would prove nothing at all.
    mailbox::write(
        &inbox,
        MailboxMessage::new("team-lead", DELIVERED_CANARY, record::now_iso8601()),
    )
    .expect("a message writes");
    assert!(
        fs::read_to_string(&inbox)
            .expect("the inbox is readable")
            .contains(DELIVERED_CANARY),
        "an inbox is exactly where a message body is supposed to be"
    );
}
