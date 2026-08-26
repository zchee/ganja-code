use std::sync::Arc;

use ganja_tool::registry::NameSource;

use super::{Health, Listing, LiveSession, fake::Recording};
use crate::lister::Lister as _;

fn session(name: &str, stem: &str) -> LiveSession {
    LiveSession {
        name: name.to_owned(),
        name_source: NameSource::User,
        session_id: format!("{stem}-0000-7000-8000-000000000001"),
        stem: stem.to_owned(),
        socket: format!("/tmp/ganja-0/{stem}.sock").into(),
        cwd: format!("/work/{stem}").into(),
        health: Health::Answered,
    }
}

/// A fake lister answers exactly what it was told, and counts its calls
/// so a menu-open test can assert it was actually reached.
#[tokio::test]
async fn a_fake_lister_answers_what_it_was_set_to_and_counts_its_calls() {
    let recording = Arc::new(Recording::default());
    recording.set(Listing::Complete(vec![session("worker", "0198c1a2")]));

    let listing = recording.list().await;
    assert_eq!(
        listing,
        Listing::Complete(vec![session("worker", "0198c1a2")])
    );

    recording.set(Listing::Partial {
        rows: vec![],
        error: "the directory could not be read".to_owned(),
    });
    let listing = recording.list().await;
    assert_eq!(
        listing,
        Listing::Partial {
            rows: vec![],
            error: "the directory could not be read".to_owned(),
        }
    );

    assert_eq!(recording.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}
