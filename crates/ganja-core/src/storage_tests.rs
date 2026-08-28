use std::fs;
use std::path::Path;

use rusqlite::Connection;
use tempfile::TempDir;

use super::{
    DATABASE, MIGRATIONS, PRAGMAS, SessionId, SessionInfo, Storage, StorageError, VERSION, connect,
};
use crate::protocol::{
    Message, MessageId, MessageTime, Part, PartBody, PartId, REASONING_TAG, Role, ToolState, Usage,
};

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// A store under a directory that does not exist yet, which is what every
/// first run opens.
fn storage(directory: &TempDir) -> Storage {
    Storage::open(directory.path().join("storage"))
}

fn session(id: &str) -> SessionId {
    SessionId::from(id.to_owned())
}

/// A session id spelled the way the mint spells one, from a small ordinal.
///
/// Most fixtures below name their sessions `ses_1`, and may: `SessionId`'s
/// own doc says the prefix is a convention rather than an invariant, and
/// nothing reads such a store twice. The ones that **reopen** a store, or
/// that write an older store's tree for the conversion to read, cannot —
/// a store whose `session` rows carry that spelling is set aside rather
/// than read (**D493**), which is a different test and has four of its own
/// binaries. Those name their sessions through here instead, which keeps
/// the ordinal readable while spelling the id the way
/// [`crate::protocol::uuidv7`] spells one; the ordinal lands in the
/// trailing field, so `ORDER BY id` still orders them the way the test
/// wrote them.
fn minted(ordinal: u32) -> String {
    format!("0198f2c4-a1b0-7000-8000-{ordinal:012x}")
}

/// Info with pinned times, so a test asserts on the order it asked for
/// rather than on whatever the clock said.
fn info(id: &str, updated: u64) -> SessionInfo {
    SessionInfo {
        id: session(id),
        version: VERSION,
        title: None,
        created: 1,
        updated,
        usage: Usage::default(),
        context_tokens: 0,
        summary: None,
        agent: None,
        model: None,
        effort: None,
        activated_tools: std::collections::BTreeSet::new(),
        parent: None,
        revert: None,
    }
}

/// Every persisted shape of the effort field, pinned where the row lives:
/// a session running Default writes the exact bytes it always wrote, a
/// selected effort survives the round trip, and a row from before the
/// field existed parses through the serde default — which is also how a
/// row written under the field's old name reads, as effort-unselected.
#[test]
fn the_session_row_preserves_default_bytes_round_trips_effort_and_reads_older_rows() {
    let mut carried = info("ses_effort", 2);
    carried.model = Some("claude-opus-5".to_owned());
    carried.effort = Some("max".to_owned());

    let encoded = serde_json::to_string(&carried).expect("the row serializes");
    assert!(encoded.contains(r#""effort":"max""#), "got {encoded}");
    assert!(!encoded.contains(r#""variant""#), "got {encoded}");
    let decoded: SessionInfo = serde_json::from_str(&encoded).expect("the row parses back");
    assert_eq!(decoded, carried);

    let bare = serde_json::to_string(&info("ses_default", 2)).expect("the row serializes");
    assert_eq!(
        bare,
        r#"{"id":"ses_default","version":1,"created":1,"updated":2,"usage":{"input_tokens":0,"output_tokens":0,"reasoning_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0},"context_tokens":0}"#,
        "Default is the field's absence, so an unselected row keeps its old bytes"
    );

    let older = r#"{"id":"ses_older","version":1,"created":1,"updated":2}"#;
    let decoded: SessionInfo =
        serde_json::from_str(older).expect("the default reads a row from before the field existed");
    assert_eq!(decoded.effort, None);
}

/// A message with pinned ids and times, carrying `parts`.
fn message(id: &str, parts: Vec<Part>) -> Message {
    Message {
        id: MessageId::from(id.to_owned()),
        role: Role::Assistant,
        parts,
        time: MessageTime { created: 7, completed: Some(9) },
        model: Some("canned".to_owned()),
        usage: Some(Usage { input_tokens: 1, output_tokens: 2, ..Usage::default() }),
    }
}

fn text(id: &str, text: &str) -> Part {
    Part { id: PartId::from(id.to_owned()), body: PartBody::Text { text: text.to_owned() } }
}

/// A completed tool call, the richest shape a part takes.
fn tool(id: &str) -> Part {
    Part {
        id: PartId::from(id.to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "read".to_owned(),
            state: ToolState::Completed {
                input: serde_json::json!({"path": "a.rs"}),
                output: "fn main() {}".to_owned(),
                title: "a.rs".to_owned(),
                metadata: serde_json::json!({"lines": 1}),
                started: 7,
                completed: 9,
            },
        },
    }
}

/// Everything directly inside `directory`, by name, sorted.
fn names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .expect("the directory lists")
        .map(|entry| entry.expect("the entry reads").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    names
}

/// Stores a whole message the way the engine does — envelope first, then
/// each part — which is the order the part table's foreign key needs.
fn store_message(storage: &Storage, id: &SessionId, message: &Message) {
    storage.save_message(id, message).expect("the envelope stores");
    for part in &message.parts {
        storage.save_part(id, &message.id, part).expect("the part stores");
    }
}

/// A second connection onto the same database, for the tests that have to
/// damage a record rather than write one.
///
/// Deliberately not the store's own: what is being simulated is bytes that
/// rotted underneath it, and a test that could only reach them through the
/// writer would be testing the writer.
fn beside(storage: &Storage) -> Connection {
    let connection =
        Connection::open(storage.database()).expect("the database opens a second time");
    for pragma in PRAGMAS {
        connection.execute_batch(pragma).expect("the pragmas apply to any connection");
    }

    connection
}

#[test]
fn a_store_that_was_never_written_reads_as_empty_rather_than_failing() {
    let directory = temporary();
    let storage = storage(&directory);

    assert!(storage.list_sessions().expect("an unwritten store lists").is_empty());
    assert_eq!(storage.load_info(&session("ses_missing")).expect("an unwritten store reads"), None);
    assert!(
        storage
            .load_transcript(&session("ses_missing"))
            .expect("an unwritten store loads")
            .is_empty()
    );
}

#[test]
fn an_info_record_round_trips_with_its_optional_fields_set_and_unset() {
    let directory = temporary();
    let storage = storage(&directory);

    let bare = info("ses_bare", 5);
    let full = SessionInfo {
        title: Some("what it was about".to_owned()),
        usage: Usage { input_tokens: 11, output_tokens: 22, ..Usage::default() },
        context_tokens: 33,
        summary: Some(MessageId::from("msg_summary".to_owned())),
        agent: Some("plan".to_owned()),
        model: Some("anthropic/claude".to_owned()),
        parent: Some(session("ses_bare")),
        ..info("ses_full", 6)
    };

    storage.save_info(&bare).expect("the bare record writes");
    storage.save_info(&full).expect("the full record writes");

    assert_eq!(storage.load_info(&bare.id).expect("the bare record reads"), Some(bare));
    assert_eq!(storage.load_info(&full.id).expect("the full record reads"), Some(full));
}

#[test]
fn a_message_is_stored_without_its_parts_and_the_caller_keeps_them() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");

    let held = message("msg_1", vec![text("prt_1", "kept by the caller")]);
    storage.save_message(&id, &held).expect("the envelope stores");

    assert_eq!(held.parts.len(), 1, "the caller's message must not be emptied by storing it");
    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    assert_eq!(loaded.len(), 1);
    assert!(
        loaded[0].parts.is_empty(),
        "the envelope is stored without its parts, got {:?}",
        loaded[0].parts
    );
}

#[test]
fn a_completed_tool_part_round_trips_whole() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");

    let held = message("msg_1", vec![tool("prt_1")]);
    store_message(&storage, &id, &held);

    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    assert_eq!(loaded[0].parts, held.parts);
}

#[test]
fn a_transcript_reassembles_its_messages_and_parts_in_id_order() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");

    // Written out of order on purpose: the store is what puts them back in
    // id order, and id order is creation order.
    let second = message("msg_2", vec![text("prt_3", "c"), text("prt_4", "d")]);
    let first = message("msg_1", vec![text("prt_2", "b"), text("prt_1", "a")]);
    store_message(&storage, &id, &second);
    store_message(&storage, &id, &first);

    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    let shape: Vec<(&str, Vec<&str>)> = loaded
        .iter()
        .map(|message| {
            (
                message.id.as_str(),
                message.parts.iter().map(|part| part.id.as_str()).collect::<Vec<_>>(),
            )
        })
        .collect();

    assert_eq!(shape, vec![("msg_1", vec!["prt_1", "prt_2"]), ("msg_2", vec!["prt_3", "prt_4"]),]);
}

#[test]
fn a_deleted_message_takes_its_parts_and_leaves_the_rest_of_the_transcript() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");

    let doomed = message("msg_1", vec![text("prt_1", "a"), text("prt_2", "b")]);
    let kept = message("msg_2", vec![text("prt_3", "c")]);
    store_message(&storage, &id, &doomed);
    store_message(&storage, &id, &kept);

    storage.delete_message(&id, &doomed.id).expect("the message deletes");

    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id, kept.id);
    assert_eq!(loaded[0].parts, kept.parts);

    // Straight at the table, because the point is the cascade: without
    // `foreign_keys = ON` the delete would leave both parts behind and the
    // transcript above would look exactly the same.
    let orphans: i64 = beside(&storage)
        .query_row(
            "SELECT COUNT(*) FROM part WHERE message_id = ?1",
            rusqlite::params![doomed.id.as_str()],
            |row| row.get(0),
        )
        .expect("the part table counts");
    assert_eq!(orphans, 0, "the cascade must carry a deleted message's parts away with it");

    storage.delete_message(&id, &doomed.id).expect("deleting what is already gone is not an error");
}

#[test]
fn a_corrupt_info_row_is_skipped_and_the_rest_still_lists() {
    let directory = temporary();
    let storage = storage(&directory);
    storage.save_info(&info("ses_rotten", 5)).expect("the record writes");
    storage.save_info(&info("ses_intact", 4)).expect("the record writes");

    beside(&storage)
        .execute("UPDATE session SET data = 'not json at all' WHERE id = 'ses_rotten'", [])
        .expect("the row is damaged");

    let listed: Vec<String> = storage
        .list_sessions()
        .expect("one unreadable record must not fail the listing")
        .into_iter()
        .map(|info| info.id.as_str().to_owned())
        .collect();
    assert_eq!(listed, vec!["ses_intact".to_owned()]);
    assert_eq!(
        storage.load_info(&session("ses_rotten")).expect("the unreadable record reads as absent"),
        None
    );

    // Left where it is: a row has no name to lose, so the reversible
    // set-aside a file gets is simply not deleting it.
    let still_there: i64 = beside(&storage)
        .query_row("SELECT COUNT(*) FROM session WHERE id = 'ses_rotten'", [], |row| row.get(0))
        .expect("the session table counts");
    assert_eq!(still_there, 1, "nothing may be destroyed to skip it");
}

#[test]
fn a_corrupt_envelope_takes_its_message_and_its_parts_out_of_the_transcript() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");

    let rotten = message("msg_1", vec![text("prt_1", "gone with it")]);
    let kept = message("msg_2", vec![text("prt_2", "still here")]);
    store_message(&storage, &id, &rotten);
    store_message(&storage, &id, &kept);

    beside(&storage)
        .execute("UPDATE message SET data = '{' WHERE id = 'msg_1'", [])
        .expect("the row is damaged");

    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    assert_eq!(loaded.len(), 1, "{loaded:#?}");
    assert_eq!(loaded[0].id, kept.id);
    assert_eq!(loaded[0].parts.len(), 1, "the surviving message keeps exactly its own parts");
}

#[test]
fn a_corrupt_part_row_is_skipped_and_its_message_keeps_the_rest() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");

    let held = message("msg_1", vec![text("prt_1", "a"), text("prt_2", "b")]);
    store_message(&storage, &id, &held);

    beside(&storage)
        .execute("UPDATE part SET data = 'nonsense' WHERE id = 'prt_1'", [])
        .expect("the row is damaged");

    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        loaded[0].parts.iter().map(|part| part.id.as_str()).collect::<Vec<_>>(),
        vec!["prt_2"]
    );
}

/// The downgrade this build is written for: a session stored by a build
/// whose reasoning part is shaped differently. The message must survive
/// whole apart from that one part, the loss must be *in* the transcript
/// rather than only in a log line, and the record must still be there for
/// the build that can read it.
#[test]
fn a_reasoning_record_this_build_cannot_read_costs_continuity_and_says_so() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");
    store_message(
        &storage,
        &id,
        &message(
            "msg_1",
            vec![
                text("prt_1", "before"),
                Part {
                    id: PartId::from("prt_2".to_owned()),
                    body: PartBody::Reasoning {
                        provider: "openai".to_owned(),
                        item: "rs_1".to_owned(),
                        encrypted: Some("sealed".to_owned()),
                    },
                },
                tool("prt_3"),
            ],
        ),
    );

    // What a later build's record looks like from here: the reserved tag,
    // and a body whose required shape this one does not have.
    let ahead = serde_json::json!({
        "version": VERSION,
        "payload": {
            "id": "prt_2",
            "type": "reasoning_v2",
            "provider": "openai",
            "item": "rs_1",
            "segments": [{"sealed": "sealed", "scheme": "something-later"}],
        },
    })
    .to_string();
    let connection = beside(&storage);
    connection
        .execute("UPDATE part SET data = ?1 WHERE id = 'prt_2'", rusqlite::params![ahead])
        .expect("the row is replaced");

    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    assert_eq!(
        loaded[0].parts.iter().map(|part| part.id.as_str()).collect::<Vec<_>>(),
        vec!["prt_1", "prt_2", "prt_3"],
        "the rest of the message survives and the lost part keeps its place"
    );
    assert_eq!(
        loaded[0].parts[1].body,
        PartBody::Reasoning {
            provider: "openai".to_owned(),
            item: "rs_1".to_owned(),
            // Nothing is salvaged out of a shape this build did not
            // understand: a wrong blob is a refused request, a missing one
            // is a model that reasons again.
            encrypted: None,
        },
        "the transcript itself has to say the continuity is gone"
    );

    let stored: String = connection
        .query_row("SELECT data FROM part WHERE id = 'prt_2'", [], |row| row.get(0))
        .expect("the row reads");
    assert_eq!(stored, ahead, "reading a record this build cannot decode must not rewrite it");

    // The other way a record becomes unreadable — a whole format this
    // build predates — has to reach the same answer, or the marker would
    // depend on *how* the future arrived rather than on what was lost.
    connection
        .execute(
            "UPDATE part SET data = ?1 WHERE id = 'prt_2'",
            rusqlite::params![
                serde_json::json!({
                    "version": VERSION + 1,
                    "payload": {"id": "prt_2", "type": REASONING_TAG},
                })
                .to_string()
            ],
        )
        .expect("the row is replaced");

    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    assert_eq!(
        loaded[0].parts[1].body,
        PartBody::Reasoning { provider: String::new(), item: String::new(), encrypted: None },
        "provenance the record does not spell plainly is left unknown \
             rather than guessed"
    );
}

/// Readable thinking is a normal versioned row: it is written, it is read
/// back word for word, and a session resumed tomorrow still shows what the
/// model was working through — which is the whole of what persisting it
/// buys, since no wire ever carries it.
#[test]
fn readable_thinking_is_stored_and_reads_back_as_itself() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");

    let mut reply = Message::assistant("canned");
    reply.parts.push(Part::reasoning_text("weighing a greeting"));
    reply.parts.push(Part::text("hello"));
    store_message(&storage, &id, &reply);

    let loaded = storage.load_transcript(&id).expect("the transcript loads");

    assert_eq!(
        loaded[0].parts[0].body,
        PartBody::ReasoningText { text: "weighing a greeting".to_owned() },
        "a stored thought comes back whole, not as a marker"
    );
    assert_eq!(loaded[0].parts[1].as_text(), Some("hello"));
}

/// The other half of the ruling: only request-affecting state earns a
/// marker. A text row that will not decode is still dropped whole, because
/// a marker for it would put a reasoning part where the model never
/// reasoned.
#[test]
fn an_unreadable_part_that_is_not_reasoning_is_still_dropped_whole() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");
    store_message(&storage, &id, &message("msg_1", vec![text("prt_1", "a"), text("prt_2", "b")]));

    beside(&storage)
        .execute(
            "UPDATE part SET data = ?1 WHERE id = 'prt_1'",
            rusqlite::params![
                serde_json::json!({
                    "version": VERSION,
                    "payload": {"id": "prt_1", "type": "text"},
                })
                .to_string()
            ],
        )
        .expect("the row is replaced");

    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    assert_eq!(
        loaded[0].parts.iter().map(|part| part.id.as_str()).collect::<Vec<_>>(),
        vec!["prt_2"]
    );
}

#[test]
fn a_record_from_a_newer_build_is_skipped_and_left_exactly_where_it_is() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");
    storage.save_info(&info("ses_1", 5)).expect("the record writes");
    store_message(&storage, &id, &message("msg_1", vec![text("prt_1", "a")]));

    let ahead = serde_json::json!({
        "version": VERSION + 1,
        "id": "ses_1",
        "created": 1,
        "updated": 5,
        "shape_this_build_has_never_seen": {"nested": true},
    })
    .to_string();
    let connection = beside(&storage);
    connection
        .execute("UPDATE session SET data = ?1 WHERE id = 'ses_1'", rusqlite::params![ahead])
        .expect("the row is replaced");
    connection
        .execute(
            "UPDATE part SET data = ?1 WHERE id = 'prt_1'",
            rusqlite::params![
                serde_json::json!({"version": VERSION + 1, "payload": {"whatever": 1}}).to_string()
            ],
        )
        .expect("the row is replaced");

    assert_eq!(storage.load_info(&id).expect("a newer build's record is not an error"), None);
    assert!(storage.list_sessions().expect("a newer build's record is not an error").is_empty());
    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    assert!(loaded[0].parts.is_empty(), "a newer build's part is skipped, not decoded");

    // The bytes are still exactly the ones the newer build wrote.
    let stored: String = connection
        .query_row("SELECT data FROM session WHERE id = 'ses_1'", [], |row| row.get(0))
        .expect("the row reads");
    assert_eq!(stored, ahead, "a newer build's record must not be rewritten");
}

#[test]
fn sessions_list_newest_first_and_ignore_what_could_not_be_read() {
    let directory = temporary();
    let storage = storage(&directory);
    for (id, updated) in [("ses_a", 10), ("ses_b", 30), ("ses_c", 20), ("ses_d", 30)] {
        storage.save_info(&info(id, updated)).expect("the record writes");
    }
    storage.save_info(&info("ses_e", 40)).expect("the record writes");
    beside(&storage)
        .execute("UPDATE session SET data = '' WHERE id = 'ses_e'", [])
        .expect("the row is damaged");

    let listed: Vec<String> = storage
        .list_sessions()
        .expect("the listing survives one unreadable record")
        .into_iter()
        .map(|info| info.id.as_str().to_owned())
        .collect();

    // Newest first, and the later id first when two share an instant.
    assert_eq!(listed, vec!["ses_d", "ses_b", "ses_c", "ses_a"]);
}

#[test]
fn rewriting_a_part_replaces_it_rather_than_leaving_the_old_one() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");

    let mut held = message("msg_1", vec![text("prt_1", "half")]);
    store_message(&storage, &id, &held);

    held.parts = vec![text("prt_1", "half and then the rest")];
    store_message(&storage, &id, &held);

    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    assert_eq!(loaded[0].parts, held.parts, "one part, at its latest text");
}

#[test]
fn a_part_whose_message_was_never_stored_is_refused_rather_than_orphaned() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");

    // Nothing ever read such a part back — a transcript reaches parts only
    // through an envelope — so what used to be a file nobody opened is now
    // a write that says so.
    let orphan =
        storage.save_part(&id, &MessageId::from("msg_never".to_owned()), &text("prt_1", "a"));
    assert!(
        matches!(orphan, Err(StorageError::Sql { .. })),
        "a part with no message must be refused, got {orphan:?}"
    );
    assert!(storage.load_transcript(&id).expect("the transcript loads").is_empty());
}

#[test]
fn a_write_leaves_nothing_beside_the_database_but_what_sqlite_owns() {
    let directory = temporary();
    let storage = storage(&directory);
    let id = session("ses_1");
    storage.save_info(&info("ses_1", 5)).expect("the record writes");
    store_message(&storage, &id, &message("msg_1", vec![text("prt_1", "a")]));

    for name in names(directory.path()) {
        assert!(
            name == DATABASE
                || name == format!("{DATABASE}-wal")
                || name == format!("{DATABASE}-shm"),
            "a write left {name} behind"
        );
    }
    assert!(
        !directory.path().join("storage").exists(),
        "a store that had no files to convert must not create the directory"
    );
}

#[test]
fn a_message_id_reused_in_another_session_does_not_overwrite_it() {
    let directory = temporary();
    let storage = storage(&directory);
    let mine = session("ses_mine");
    let yours = session("ses_yours");

    // Two sessions can carry a message under the same id however it was
    // minted; under a bare `id` primary key the second write would take
    // the first one's row.
    store_message(&storage, &mine, &message("msg_same", vec![text("prt_same", "mine")]));
    store_message(&storage, &yours, &message("msg_same", vec![text("prt_same", "yours")]));

    let mine = storage.load_transcript(&mine).expect("the transcript loads");
    let yours = storage.load_transcript(&yours).expect("the transcript loads");
    assert_eq!(mine.len(), 1);
    assert_eq!(yours.len(), 1);
    assert_eq!(mine[0].parts[0].as_text(), Some("mine"));
    assert_eq!(yours[0].parts[0].as_text(), Some("yours"));
}

#[test]
fn two_stores_on_one_database_take_turns_rather_than_take_each_others_writes() {
    let directory = temporary();
    let root = directory.path().join("storage");
    let mine = Storage::open(root.clone());
    let yours = Storage::open(root);

    // Two handles is what two `ganja` processes in one project look like
    // from here: two writer threads, two connections, one file. WAL admits
    // one writer at a time and `busy_timeout` is what makes the other wait
    // instead of failing — a claim nothing else in this suite exercises.
    //
    // Both sides deliberately use the *same* message and part ids, because
    // that is the collision the composite keys exist for. UUIDv7 ids make
    // it far less likely than the old per-process counter did (**D493**),
    // but "unlikely" is not what a primary key is for: under a bare `id`
    // the second writer would take the first one's row.
    let rounds = 25;
    let write = |storage: &Storage, owner: &str, what: &str| {
        let id = session(owner);
        storage.save_info(&info(owner, 1)).expect("the record writes");
        for round in 0..rounds {
            let held = message("msg_same", vec![text("prt_same", &format!("{what} {round}"))]);
            store_message(storage, &id, &held);
        }
    };

    // The two sessions carry this build's own spelling, because the second
    // handle to open finds the first one's rows already there — and a
    // store whose rows are older than UUIDv7 is set aside at that moment
    // rather than written into (**D493**).
    let mine_id = minted(1);
    let yours_id = minted(2);
    std::thread::scope(|scope| {
        scope.spawn(|| write(&mine, &mine_id, "mine"));
        scope.spawn(|| write(&yours, &yours_id, "yours"));
    });

    // Either handle answers for both sessions: one database, two views of
    // it, and neither writer's row was overwritten by the other's.
    let read = |storage: &Storage, owner: &str, what: &str| {
        let loaded = storage.load_transcript(&session(owner)).expect("the transcript loads");
        assert_eq!(loaded.len(), 1, "{owner}: {loaded:#?}");
        assert_eq!(
            loaded[0].parts.len(),
            1,
            "{owner} rewrote one part rather than accumulating them"
        );
        assert_eq!(
            loaded[0].parts[0].as_text(),
            Some(format!("{what} {}", rounds - 1).as_str()),
            "{owner} must hold its own last write"
        );
    };
    for storage in [&mine, &yours] {
        read(storage, &mine_id, "mine");
        read(storage, &yours_id, "yours");
    }
    assert_eq!(
        mine.list_sessions().expect("the store lists").len(),
        2,
        "both writers' sessions are in the one database"
    );
}

#[test]
fn every_connection_sets_the_pragmas_the_store_depends_on() {
    let directory = temporary();
    let storage = storage(&directory);
    storage.save_info(&info("ses_1", 1)).expect("the record writes");

    let connection = connect(storage.database()).expect("a connection opens");
    let read = |pragma: &str| -> i64 {
        connection
            .query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))
            .expect("the pragma reads")
    };
    let journal: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("the pragma reads");

    assert_eq!(journal, "wal");
    assert_eq!(read("synchronous"), 1, "NORMAL, not the build's FULL default");
    assert_eq!(read("foreign_keys"), 1, "without this the cascade is a no-op");
    assert_eq!(read("busy_timeout"), 5000);
    assert_eq!(read("cache_size"), -64_000);
}

#[test]
fn a_second_open_finds_the_schema_already_there_and_the_sessions_with_it() {
    let directory = temporary();
    let name = minted(1);
    let id = session(&name);
    {
        let storage = storage(&directory);
        storage.save_info(&info(&name, 5)).expect("the record writes");
        store_message(&storage, &id, &message("msg_1", vec![text("prt_1", "a")]));
    }

    let storage = storage(&directory);
    assert!(storage.load_info(&id).expect("the record reads").is_some());
    assert_eq!(
        storage.load_transcript(&id).expect("the transcript loads")[0].parts[0].as_text(),
        Some("a")
    );

    let applied: i64 = beside(&storage)
        .query_row("SELECT COUNT(*) FROM migration", [], |row| row.get(0))
        .expect("the journal counts");
    assert_eq!(
        applied,
        MIGRATIONS.len() as i64,
        "a second open must not replay what the first one stamped"
    );
}

#[test]
fn an_unreadable_database_is_set_aside_and_the_store_starts_fresh() {
    let directory = temporary();
    {
        let storage = storage(&directory);
        storage.save_info(&info("ses_lost", 5)).expect("the record writes");
    }

    // The log goes first, and its absence is the point rather than
    // housekeeping: a write-ahead log beside a damaged database is not
    // damage — SQLite recovers the file out of it, header and all, and the
    // store reads perfectly. That is the right outcome, and it is also
    // exactly why `set_aside` has to carry all three files together.
    let database = directory.path().join(DATABASE);
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(directory.path().join(format!("{DATABASE}{suffix}")));
    }

    // Then the header, so nothing works at all — not even reading the
    // schema — which is the one failure `integrity_check` cannot report,
    // because it cannot run: the check itself errors instead of returning
    // a verdict.
    let mut bytes = fs::read(&database).expect("the database reads");
    bytes[..16].copy_from_slice(b"not a database!!");
    fs::write(&database, &bytes).expect("the database is damaged");

    let storage = storage(&directory);
    assert!(
        storage.list_sessions().expect("a damaged store opens rather than failing").is_empty(),
        "what replaces a damaged store is empty"
    );
    storage.save_info(&info("ses_new", 1)).expect("the fresh store writes");
    assert_eq!(
        storage.list_sessions().expect("the fresh store lists").len(),
        1,
        "the store that replaces a damaged one is a working one"
    );

    let aside = names(directory.path());
    assert!(
        aside.iter().any(|name| name.contains(".corrupt-")),
        "the damaged database must be kept rather than deleted, got {aside:?}"
    );
}

#[test]
fn a_database_set_aside_takes_its_write_ahead_log_with_it() {
    let directory = temporary();
    let database = directory.path().join(DATABASE);
    for suffix in ["", "-wal", "-shm"] {
        fs::write(directory.path().join(format!("{DATABASE}{suffix}")), b"whatever was there")
            .expect("the file is writable");
    }

    assert!(super::set_aside(&database, "for the test", super::QUARANTINE));

    // A log left behind is recovered into the *fresh* file that takes the
    // old name, which would pour the damaged store straight back in — so
    // the three files move together or the set-aside is worse than
    // useless.
    let left = names(directory.path());
    assert_eq!(
        left.iter().filter(|name| !name.contains(".corrupt-")).count(),
        0,
        "nothing may be left under the name a fresh database will take, got {left:?}"
    );
    for suffix in ["", "-wal", "-shm"] {
        assert!(
            left.iter().any(|name| name.contains(".corrupt-") && name.ends_with(suffix)),
            "the {suffix:?} file must travel with its database, got {left:?}"
        );
    }
}

/// The quarantine waits for the lock, and then asks again.
///
/// This is the regression drill for the bug the first version of
/// [`set_aside_preuuid`] shipped: it decided on its own connection and
/// renamed afterwards, so two processes could both decide "old ids", and
/// the second one would rename the *fresh* store the first had already put
/// in place. `ganja-cli`'s `two_processes_racing_the_quarantine…` catches
/// it with two real processes, roughly one run in six; this catches it
/// every time, by holding the lock and playing the winner underneath a
/// store that is already past the point of no return.
///
/// Both halves are asserted, because either alone would pass on a build
/// with no lock at all: that the waiting store does **not** rename while
/// the lock is held, and that once it gets in it re-reads the path and
/// finds nothing to do.
#[test]
fn a_quarantine_waits_for_the_lock_and_then_finds_nothing_left_to_do() {
    let directory = temporary();
    let root = directory.path().join("storage");
    let old = "ses_0193b2f0a1c2000000";
    {
        let planted = storage(&directory);
        planted.save_info(&info(old, 5)).expect("the record writes");
    }
    let database = Storage::open(root.clone()).database().to_path_buf();

    // Taken before anybody else can want it, and held across the whole
    // interleaving below.
    let held = super::QuarantineLock::take(&database).expect("the lock is available");

    let waiting = Storage::open(root.clone());
    std::thread::scope(|scope| {
        let parked = scope.spawn(|| waiting.list_sessions());
        // Long enough for that store to have read the old id, asked for
        // the lock, and be waiting on it.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !names(directory.path()).iter().any(|name| name.contains(".preuuid-")),
            "nothing may be set aside while another process holds the lock"
        );

        // The winner's move, played by this test: the store renamed aside
        // and a fresh one created at the name it left.
        assert!(super::set_aside(&database, "for the test", super::PREUUID));
        let fresh = minted(1);
        Storage::open(root.clone()).save_info(&info(&fresh, 1)).expect("the fresh store writes");
        drop(held);

        let listed = parked
            .join()
            .expect("the waiting store does not panic")
            .expect("the waiting store opens rather than failing");
        assert_eq!(
            listed.iter().map(|info| info.id.as_str()).collect::<Vec<_>>(),
            vec![fresh.as_str()],
            "the store that waited must go on with what the winner left"
        );
    });

    let left = names(directory.path());
    assert_eq!(
        left.iter()
            .filter(|name| name.contains(".preuuid-")
                && !name.ends_with("-wal")
                && !name.ends_with("-shm"))
            .count(),
        1,
        "the store that waited must not set the winner's fresh store aside too, got {left:?}"
    );
}

#[test]
fn a_database_from_a_newer_build_is_refused_rather_than_migrated_down() {
    let directory = temporary();
    {
        let storage = storage(&directory);
        storage.save_info(&info("ses_1", 5)).expect("the record writes");
        beside(&storage)
                .execute(
                    "INSERT INTO migration (id, time_completed) VALUES ('29991231235959_from_ahead', 1)",
                    [],
                )
                .expect("a newer build's journal row is written");
    }

    let storage = storage(&directory);
    let refused = storage.list_sessions();
    assert!(
        matches!(
            &refused,
            Err(StorageError::Newer { unknown, .. }) if unknown == "29991231235959_from_ahead"
        ),
        "a newer build's database must be refused by name, got {refused:?}"
    );

    // Refused is not quarantined: the sessions in there belong to the
    // build that can read them.
    assert!(
        !names(directory.path()).iter().any(|name| name.contains(".corrupt-")),
        "a database this build merely does not understand must be left alone"
    );
}

#[test]
fn a_database_that_is_not_a_session_store_is_refused_rather_than_guessed_at() {
    let directory = temporary();
    fs::create_dir_all(directory.path()).expect("the directory exists");
    let connection =
        Connection::open(directory.path().join(DATABASE)).expect("a database is creatable");
    connection
        .execute_batch("CREATE TABLE somebody_elses (id TEXT);")
        .expect("the foreign table is creatable");
    drop(connection);

    let storage = storage(&directory);
    let refused = storage.list_sessions();
    assert!(
        matches!(&refused, Err(StorageError::Foreign { .. })),
        "somebody else's database must be refused, got {refused:?}"
    );
}

#[test]
fn an_older_file_store_is_carried_across_on_first_open_and_set_aside_intact() {
    let directory = temporary();
    let root = directory.path().join("storage");
    let name = minted(1);
    let id = session(&name);
    let held = message("msg_1", vec![text("prt_1", "a"), tool("prt_2")]);

    // The file layout, written by hand: this is the store a build before
    // this one left, and nothing in the tree writes it any more. Its ids
    // are this build's, because a tree carrying older ones is set aside
    // rather than carried across (**D493**) — `storage_preuuid_tree.rs`
    // is where that is asserted.
    let info = SessionInfo { title: Some("carried".to_owned()), ..info(&name, 5) };
    write_json(&root.join("session").join("info").join(format!("{name}.json")), &info);
    let mut envelope = held.clone();
    envelope.parts.clear();
    write_json(
        &root.join("session").join("message").join(&name).join("msg_1.json"),
        &serde_json::json!({"version": VERSION, "payload": envelope}),
    );
    for part in &held.parts {
        write_json(
            &root
                .join("session")
                .join("part")
                .join(&name)
                .join("msg_1")
                .join(format!("{}.json", part.id.as_str())),
            &serde_json::json!({"version": VERSION, "payload": part}),
        );
    }

    let storage = Storage::open(root.clone());
    assert_eq!(storage.load_info(&id).expect("the carried record reads"), Some(info));
    let loaded = storage.load_transcript(&id).expect("the transcript loads");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].parts, held.parts);

    // Renamed rather than deleted: whoever downgrades tomorrow finds every
    // file exactly where it was.
    assert!(!root.exists(), "the converted tree is not left in place");
    let aside = names(directory.path())
        .into_iter()
        .find(|name| name.starts_with("storage.migrated-"))
        .expect("the converted tree is kept under a new name");
    assert!(
        directory
            .path()
            .join(aside)
            .join("session")
            .join("info")
            .join(format!("{name}.json"))
            .is_file(),
        "the set-aside tree must still hold what it held"
    );
}

/// A tree the conversion cannot carry whole keeps the tree.
///
/// Two info files claiming one session id is what a hand-edited or
/// half-restored store looks like. `carry` inserts plainly where the
/// running store upserts, so the second one is a UNIQUE violation: that
/// session is counted lost, the warning says so, and the tree is left
/// exactly where it is — because the only copy of what did not make it is
/// in there. An upsert here would silently keep one of the two and then
/// rename the tree away, which is the one outcome nobody could undo.
#[test]
fn a_tree_holding_one_id_twice_is_left_where_it_is() {
    let directory = temporary();
    let root = directory.path().join("storage");

    let name = minted(1);
    for (file, title) in
        [(format!("{name}.json"), "first"), (format!("also-{name}.json"), "second")]
    {
        write_json(
            &root.join("session").join("info").join(file),
            &SessionInfo { title: Some(title.to_owned()), ..info(&name, 5) },
        );
    }

    let storage = Storage::open(root.clone());
    // Whichever of the two got there first is readable; which one it is is
    // the directory's order to decide and not this test's.
    assert!(
        storage.load_info(&session(&name)).expect("the database opens").is_some(),
        "the session that did carry across is there"
    );
    assert!(
        root.join("session").join("info").join(format!("{name}.json")).is_file(),
        "and the tree that holds the one that did not is untouched"
    );
    assert!(
        !names(directory.path()).into_iter().any(|name| name.starts_with("storage.migrated-")),
        "a conversion that lost something sets nothing aside"
    );
}

#[test]
fn a_second_open_does_not_convert_a_tree_that_appeared_after_the_first() {
    let directory = temporary();
    let root = directory.path().join("storage");
    let native = minted(1);
    {
        let storage = Storage::open(root.clone());
        storage.save_info(&info(&native, 5)).expect("the record writes");
    }

    // A tree that turns up after the database exists is not this build's
    // to import: convert-on-first-open happens exactly once, and once is
    // the open that created the file.
    let late = minted(2);
    write_json(&root.join("session").join("info").join(format!("{late}.json")), &info(&late, 9));

    let storage = Storage::open(root.clone());
    let listed: Vec<String> = storage
        .list_sessions()
        .expect("the store lists")
        .into_iter()
        .map(|info| info.id.as_str().to_owned())
        .collect();
    assert_eq!(listed, vec![native]);
    assert!(root.exists(), "a tree that was not converted is not set aside");
}

/// Writes one file of the layout the conversion reads.
fn write_json<T: serde::Serialize>(path: &Path, value: &T) {
    fs::create_dir_all(path.parent().expect("a file has a directory"))
        .expect("the directory is creatable");
    fs::write(path, serde_json::to_vec(value).expect("the value encodes"))
        .expect("the file is writable");
}
