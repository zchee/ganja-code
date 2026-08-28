use ganja_tui::lister::{Health, Listing, LiveSession};

use super::list;

/// An empty directory lists nothing, and reads as complete: an absent
/// registry is not a partial one.
#[tokio::test]
async fn an_empty_registry_lists_as_complete_and_empty() {
    let dir = tempfile::tempdir().expect("a scratch directory");

    assert_eq!(list(dir.path()).await, Listing::Complete(Vec::new()));
}

/// A registry this listing cannot even read at all answers `Partial`
/// with no rows — the same refuse-don't-guess posture the resolver
/// holds for an unreadable directory.
#[tokio::test]
async fn an_unreadable_directory_answers_partial_with_no_rows() {
    let listing = list(std::path::Path::new("/nonexistent-ganja-registry")).await;

    assert!(matches!(&listing, Listing::Partial { rows, .. } if rows.is_empty()), "{listing:?}");
}

/// A stale record — nobody holds its stem's lock — is excluded, exactly
/// as it is from resolution; a live one is listed, its health probed
/// against a socket nothing is serving, which is neither `Answered` nor
/// silently dropped: the registry says live (the lock is held), the
/// socket says nothing back, and that combination is `Held`.
#[tokio::test]
async fn a_stale_record_is_excluded_and_a_live_one_is_listed_with_its_health_probed() {
    use ganja_core::tool::registry::{NameSource, Record, write};

    let dir = tempfile::tempdir().expect("a scratch directory");
    let record = |name: &str, id: &str| Record {
        format: ganja_core::tool::registry::FORMAT,
        session_id: id.to_owned(),
        name: name.to_owned(),
        name_source: NameSource::User,
        cwd: "/work".into(),
        root: "/work".into(),
        pid: 4242,
        started_at: 1_756_150_000_000,
    };

    write(dir.path(), "0198c1a2", &record("worker", "0198c1a2-0000-7000-8000-000000000001"))
        .expect("a record writes");
    write(dir.path(), "0299d2b3", &record("stale", "0299d2b3-0000-7000-8000-000000000002"))
        .expect("a record writes");

    // Only the first is live: its lock is held, unbound socket and all —
    // a socket the health probe then reaches nobody behind.
    let held = ganja_core::tool::socket::open_lock(&dir.path().join("0198c1a2.sock"))
        .expect("the lock file opens");
    held.try_lock().expect("nothing else holds a fresh lock");

    let listing = list(dir.path()).await;
    let Listing::Complete(rows) = listing else {
        panic!("a directory this test just wrote reads back complete: {listing:?}");
    };
    assert_eq!(
        rows,
        vec![LiveSession {
            name: "worker".to_owned(),
            name_source: NameSource::User,
            session_id: "0198c1a2-0000-7000-8000-000000000001".to_owned(),
            stem: "0198c1a2".to_owned(),
            socket: dir.path().join("0198c1a2.sock"),
            cwd: "/work".into(),
            health: Health::Held,
        }],
        "the stale record is excluded and the live one's health is probed: {rows:?}"
    );
}
