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
        effort: None,
        activated_tools: std::collections::BTreeSet::new(),
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

/// The spelling ids had before **D493**: `<prefix>_<millis hex><counter hex>`,
/// where the counter started at zero in every process.
pub const PRE_UUID_ID: &str = "ses_0193b2f0a1c2000000";

/// Plants the store a build from before **D493** left behind: one session
/// under [`PRE_UUID_ID`], at `root`.
///
/// Answers the still-open handle beside the database path, because *when*
/// the planting connection closes is part of each quarantine drill: SQLite
/// folds the write-ahead log back in and deletes it when the last connection
/// goes, so a drill about the `-wal` travelling opens its keeper before it
/// drops this, and the others drop it at once.
pub fn plant_preuuid_store(root: std::path::PathBuf) -> (Storage, std::path::PathBuf) {
    let planted = Storage::open(root);
    planted
        .save_info(&seeded_session_info(
            SessionId::from(PRE_UUID_ID.to_owned()),
            7,
        ))
        .expect("the old-format record writes");
    let database = planted.database().to_path_buf();

    (planted, database)
}

/// Everything directly inside `directory`, by name.
pub fn entries(directory: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(directory)
        .expect("the directory lists")
        .map(|entry| {
            entry
                .expect("the entry reads")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// The `<database>.preuuid-<millis>` entries in `directory`, without their
/// `-wal`/`-shm` log companions — what a quarantine set aside, however many
/// times it fired.
pub fn set_aside_of(directory: &std::path::Path, database: &std::path::Path) -> Vec<String> {
    let name = database
        .file_name()
        .expect("the database has a name")
        .to_string_lossy()
        .into_owned();
    let prefix = format!("{name}.preuuid-");

    entries(directory)
        .into_iter()
        .filter(|entry| {
            entry.starts_with(&prefix) && !entry.ends_with("-wal") && !entry.ends_with("-shm")
        })
        .collect()
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
