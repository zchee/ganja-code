//! What a task *says* must not come back out of a log line.
//!
//! `tests/no_bodies_in_logs.rs` states this rule for an inbox; a task list is
//! the crate's other place where somebody's words are routinely read,
//! rewritten and complained about, so it gets the same canary rather than a
//! promise. The risk is the same one and it is not a `println!` anybody
//! forgot: it is a decoder's own message quoting the value it choked on, on
//! the way into a `tracing` field about a *damaged* document.
//!
//! So canary words go into a description, a comment, a metadata value and a
//! document that will not decode, every logging path the store has is walked,
//! and every byte the library traced is searched.
//!
//! One test, one binary, like the inbox canary beside it: the capture is
//! installed as the **global** subscriber, so the flavour cannot matter and
//! asserting that library lines *arrived* is what proves the search space was
//! not empty.

mod support;

use std::sync::{Arc, Mutex};
use std::{fs, io};

use ganja_team::task::{Comment, NewTask, Store, TaskId, TaskStatus, Update};
use serde_json::json;
use tracing_subscriber::fmt::MakeWriter;

/// A description, which is what somebody picking a task up reads — and a place
/// a credential lands the way a spawn prompt is.
const DESCRIPTION_CANARY: &str = "kaleidoscopic-otter-9174";

/// What one teammate said to another about the work.
const COMMENT_CANARY: &str = "phosphorescent-gannet-3312";

/// A metadata value, which is whatever a model decided to carry.
const METADATA_CANARY: &str = "incandescent-pangolin-5521";

/// Inside a document that will not decode, so the drop-report path is the one
/// holding it. A status is the field to damage: serde names the variant it did
/// not know, verbatim, in the message it fails with.
const DAMAGED_CANARY: &str = "vermillion-quokka-8807";

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
        self.0.lock().expect("the log is never poisoned").extend_from_slice(buffer);

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
fn what_a_task_says_never_reaches_a_log_line() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("this binary holds one test, so nothing else has installed one");

    let (_home, root, team) = support::root("session-224cbeab");
    let store = Store::new(root.tasks_dir(&team));

    // Created, changed, claimed, listed, deleted — the whole ordinary path,
    // every step of which logs.
    let task = store
        .create(NewTask {
            subject: "port the parser".to_owned(),
            description: DESCRIPTION_CANARY.to_owned(),
            active_form: None,
            metadata: [("carried".to_owned(), json!(METADATA_CANARY))].into_iter().collect(),
        })
        .expect("a task is created");
    store.claim(&task.id, "worker-1").expect("an unowned task is claimable");
    store
        .update(
            &task.id,
            Update {
                status: Some(TaskStatus::InProgress),
                add_comment: Some(Comment::new(
                    "worker-1",
                    "2026-09-02T10:00:00.000Z",
                    COMMENT_CANARY,
                )),
                ..Update::default()
            },
        )
        .expect("the update lands");
    assert_eq!(store.list().expect("the list reads").len(), 1, "the task is the search space");
    store.delete(&task.id).expect("the task deletes");

    // And the drop path, which is the one that handles a damaged document's
    // own bytes on the way to complaining about it. A `serde_json` message
    // would carry this canary verbatim.
    let damaged = store.create(NewTask::new("subject", "description")).expect("a task is created");
    fs::write(
        store.path_of(&damaged.id),
        format!(
            "{{\"id\": \"{}\", \"subject\": \"s\", \"status\": \"{DAMAGED_CANARY}\"}}",
            damaged.id
        ),
    )
    .expect("the document is writable");
    assert!(store.list().expect("a damaged document does not fail the list").is_empty());

    // The leak this canary is for is real rather than theoretical, and this is
    // where that is demonstrated: the decoder's own sentence carries the
    // document's bytes, so a listing that logged it would log them too.
    let refused = store.get(&damaged.id).expect_err("a damaged document does not decode");
    assert!(
        refused.to_string().contains(DAMAGED_CANARY),
        "a decoder quotes what it choked on: {refused}",
    );
    assert!(
        store.get(&TaskId::parse("77").expect("an id")).is_err(),
        "as does asking for one that was never filed",
    );

    let logged = capture.logged();
    for canary in [DESCRIPTION_CANARY, COMMENT_CANARY, METADATA_CANARY, DAMAGED_CANARY] {
        assert!(!logged.contains(canary), "content reached a log line:\n{logged}");
    }

    // The search space was not empty, and what is in it is addressing: a
    // directory, an id, a status, an owner and a fixed sentence per drop.
    for line in [
        "a task joined the list",
        "a task was claimed",
        "a task changed",
        "a task was deleted",
        "was left out of the list",
    ] {
        assert!(logged.contains(line), "{line} never arrived:\n{logged}");
    }
    assert!(logged.contains("worker-1"), "an owner is an address, and stays readable");
    assert!(
        logged.contains(ganja_team::task::DROPPED_UNDECODABLE),
        "a drop says which kind of failure it was:\n{logged}",
    );
}
