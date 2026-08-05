//! Storage fixtures for suites that seed a session directly on disk rather
//! than build one up through a turn.

use ganja_core::{SessionId, SessionInfo, Storage, storage};
use ganja_protocol::{Message, Usage};

/// A [`SessionInfo`] for seeding storage directly: already titled, so the
/// title machinery stays out of a test that is not about it, with `created`
/// fixed at 1 and `updated` at 2 — nothing in these suites compares either
/// against the clock.
///
/// ```
/// use ganja_core::SessionId;
///
/// let info = ganja_testkit::seeded_session_info(SessionId::ascending(), 42);
/// assert_eq!(info.context_tokens, 42);
/// assert_eq!(info.title.as_deref(), Some("seeded"));
/// ```
pub fn seeded_session_info(id: SessionId, context_tokens: u64) -> SessionInfo {
    SessionInfo {
        id,
        version: storage::VERSION,
        title: Some("seeded".to_owned()),
        created: 1,
        updated: 2,
        usage: Usage::default(),
        context_tokens,
        summary: None,
        agent: None,
        model: None,
        parent: None,
        revert: None,
    }
}

/// Writes a fresh, pre-titled session record and returns its id.
pub fn seed_session(storage: &Storage, context_tokens: u64) -> SessionId {
    let id = SessionId::ascending();
    storage
        .save_info(&seeded_session_info(id.clone(), context_tokens))
        .expect("the seeded record writes");

    id
}

/// Writes `message` the way the engine does: the envelope, then each part.
///
/// ```
/// use ganja_core::{SessionId, Storage};
/// use ganja_protocol::Message;
///
/// let dir = ganja_testkit::temp_dir();
/// let storage = Storage::open(dir.path().join("storage"));
/// let session = SessionId::ascending();
/// ganja_testkit::seed_message(&storage, &session, &Message::user("hello"));
/// ```
pub fn seed_message(storage: &Storage, session: &SessionId, message: &Message) {
    storage
        .save_message(session, message)
        .expect("the seeded envelope writes");
    for part in &message.parts {
        storage
            .save_part(session, &message.id, part)
            .expect("the seeded part writes");
    }
}
