use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde_json::{Value, json};

use super::{
    COUNTER, Comment, ID_MAX, NewTask, Store, TASK_KEYS, Task, TaskError, TaskId, TaskStatus,
    Update, write,
};
use crate::lock::LockError;
use crate::record::document;

/// A store nothing else can reach. The temporary home comes back for the
/// caller to keep alive: a `TempDir` removes its tree on drop.
fn store() -> (tempfile::TempDir, Store) {
    let home = tempfile::tempdir().expect("a temp directory");
    let store = Store::new(home.path().join("teams").join("session-224cbeab").join("tasks"));

    (home, store)
}

/// A task with something in every field, for the tests that are about the
/// shape rather than about one operation.
fn filled() -> Task {
    Task {
        id: TaskId::parse("1").expect("a valid id"),
        subject: "port the parser".to_owned(),
        description: "start from the spec".to_owned(),
        active_form: Some("porting the parser".to_owned()),
        status: TaskStatus::InProgress,
        owner: "worker-1".to_owned(),
        blocks: vec![TaskId::parse("2").expect("a valid id")],
        blocked_by: vec![TaskId::parse("3").expect("a valid id")],
        metadata: IndexMap::from([("lane".to_owned(), json!("w2"))]),
        comments: vec![Comment::new("team-lead", "started", "2026-09-02T10:00:00.000Z")],
        extra: IndexMap::new(),
    }
}

#[test]
fn an_id_is_one_to_nineteen_decimal_digits_starting_at_one() {
    for accepted in ["1", "9", "10", "42", "9999999999999999999"] {
        assert_eq!(
            TaskId::parse(accepted).expect("a valid id").to_string(),
            accepted,
            "{accepted} round-trips through its own spelling",
        );
    }

    // A leading zero is refused rather than trimmed, so `01.json` can never
    // sit beside `1.json` holding a different task.
    for refused in ["", "0", "01", "1a", "-1", "1.5", " 1", "1 ", "../1", "1/2", "١"] {
        assert!(
            matches!(TaskId::parse(refused), Err(TaskError::Shape { .. })),
            "{refused:?} is not an id",
        );
    }
    assert!(
        matches!(TaskId::parse("10000000000000000000"), Err(TaskError::Shape { .. })),
        "twenty digits is past the grammar",
    );
}

#[test]
fn ids_are_sequential_and_a_deleted_one_leaves_a_gap() {
    let (_home, store) = store();

    for expected in ["1", "2", "3"] {
        let task = store.create(NewTask::new("subject", "description")).expect("a task is created");
        assert_eq!(task.id.to_string(), expected);
    }

    let second = TaskId::parse("2").expect("a valid id");
    store.delete(&second).expect("the task is deleted");

    let fourth = store.create(NewTask::new("subject", "description")).expect("a task is created");
    assert_eq!(fourth.id.to_string(), "4", "an id a deletion freed is never handed out again");
}

#[test]
fn a_counter_somebody_removed_is_rebuilt_from_the_documents_on_disk() {
    let (_home, store) = store();
    for _ in 0..3 {
        store.create(NewTask::new("subject", "description")).expect("a task is created");
    }
    fs::remove_file(store.counter_path()).expect("the counter is removable");

    let next = store.create(NewTask::new("subject", "description")).expect("a task is created");
    assert_eq!(next.id.to_string(), "4", "starting over at 1 would merge two pieces of work");
}

#[test]
fn a_counter_that_will_not_parse_is_repaired_rather_than_fatal() {
    let (_home, store) = store();
    store.create(NewTask::new("subject", "description")).expect("a task is created");
    fs::write(store.counter_path(), "banana").expect("the counter is writable");

    let next = store.create(NewTask::new("subject", "description")).expect("a task is created");
    assert_eq!(next.id.to_string(), "2", "a list stays addable-to after somebody breaks a counter");
}

#[test]
fn a_counter_past_the_grammar_is_repaired_like_one_that_will_not_parse() {
    let (_home, store) = store();
    store.create(NewTask::new("subject", "description")).expect("a task is created");
    fs::write(store.counter_path(), u64::MAX.to_string()).expect("the counter is writable");

    let next = store.create(NewTask::new("subject", "description")).expect("a task is created");
    assert_eq!(
        next.id.to_string(),
        "2",
        "a counter too large for the grammar is repaired like one that will not parse"
    );
}

#[test]
fn the_counter_refuses_rather_than_wraps_at_the_last_id_the_grammar_admits() {
    let (_home, store) = store();
    store.create(NewTask::new("subject", "description")).expect("a task is created");
    fs::write(store.counter_path(), ID_MAX.to_string()).expect("the counter is writable");

    let refused =
        store.create(NewTask::new("subject", "description")).expect_err("the id space is spent");
    assert!(matches!(refused, TaskError::CounterExhausted), "{refused}");
    assert_eq!(store.list().expect("the list reads").len(), 1, "a refused create files nothing");
}

#[test]
fn a_rebuilt_counter_reissues_the_ids_deleted_above_the_highest_survivor() {
    let (_home, store) = store();
    let ids: Vec<TaskId> = (0..3)
        .map(|_| {
            store.create(NewTask::new("subject", "description")).expect("a task is created").id
        })
        .collect();
    store.delete(&ids[2]).expect("the task deletes");
    fs::remove_file(store.counter_path()).expect("the counter is removable");

    // The counter was the only record that 3 had ever been issued. This is the
    // documented limit of rebuilding from the documents on disk, pinned so a
    // reader of the module doc can see it is a measured cost and not a claim
    // nobody checked.
    let next = store.create(NewTask::new("subject", "description")).expect("a task is created");
    assert_eq!(next.id, ids[2], "without the counter, the highest survivor decides");
}

#[test]
fn a_created_task_is_pending_unowned_and_unblocked() {
    let (_home, store) = store();

    let task = store
        .create(NewTask {
            subject: "port the parser".to_owned(),
            description: "start from the spec".to_owned(),
            active_form: Some("porting the parser".to_owned()),
            metadata: IndexMap::from([("lane".to_owned(), json!("w2"))]),
        })
        .expect("a task is created");

    assert_eq!(task.status, TaskStatus::Pending);
    assert!(!task.is_owned(), "a create leaves the owner empty for a claim to fill");
    assert!(task.blocked_by.is_empty());
    assert!(task.comments.is_empty());
    assert_eq!(task.metadata["lane"], json!("w2"));
    assert_eq!(store.get(&task.id).expect("the task reads back"), task);
}

#[test]
fn a_claim_takes_an_unowned_task_and_a_second_claim_is_refused() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");

    let claimed = store.claim(&task.id, "worker-1").expect("an unowned task is claimable");
    assert_eq!(claimed.owner, "worker-1");

    match store.claim(&task.id, "worker-2") {
        Err(TaskError::AlreadyOwned { id, owner }) => {
            assert_eq!(id, task.id);
            assert_eq!(owner, "worker-1", "the loser of a race learns who won it");
        }
        other => panic!("a claimed task is not claimable again: {other:?}"),
    }
    assert_eq!(store.get(&task.id).expect("the task reads").owner, "worker-1");
}

#[test]
fn a_claimant_re_claiming_its_own_task_is_told_it_already_holds_it() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");
    store.claim(&task.id, "worker-1").expect("an unowned task is claimable");

    match store.claim(&task.id, "worker-1") {
        Err(TaskError::AlreadyOwned { owner, .. }) => assert_eq!(owner, "worker-1"),
        other => panic!("a claim tests the owner, not who is asking: {other:?}"),
    }
}

#[test]
fn a_released_task_is_claimable_again() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");
    store.claim(&task.id, "worker-1").expect("an unowned task is claimable");

    // The door a lead reassigning a dead member's work goes through.
    store
        .update(&task.id, Update { owner: Some(String::new()), ..Update::default() })
        .expect("an owner is clearable");

    let claimed = store.claim(&task.id, "worker-2").expect("a released task is claimable");
    assert_eq!(claimed.owner, "worker-2");
}

#[test]
fn metadata_merges_in_place_and_a_null_deletes_a_key() {
    let (_home, store) = store();
    let task = store
        .create(NewTask {
            subject: "subject".to_owned(),
            description: "description".to_owned(),
            active_form: None,
            metadata: IndexMap::from([
                ("lane".to_owned(), json!("w2")),
                ("attempts".to_owned(), json!(1)),
                ("note".to_owned(), json!("keep me")),
            ]),
        })
        .expect("a task is created");

    let merged = store
        .update(
            &task.id,
            Update {
                metadata: IndexMap::from([
                    ("attempts".to_owned(), json!(2)),
                    ("lane".to_owned(), Value::Null),
                    ("added".to_owned(), json!(true)),
                ]),
                ..Update::default()
            },
        )
        .expect("the merge lands");

    let keys: Vec<&str> = merged.metadata.keys().map(String::as_str).collect();
    assert_eq!(keys, ["attempts", "note", "added"], "a merge keeps positions and appends new keys");
    assert_eq!(merged.metadata["attempts"], json!(2), "a value is replaced, not merged into");
    assert!(!merged.metadata.contains_key("lane"), "a null deletes its key");

    // An empty map is a no-op rather than a clear.
    let untouched = store.update(&task.id, Update::default()).expect("an empty update lands");
    assert_eq!(untouched.metadata, merged.metadata);
}

#[test]
fn comments_only_ever_grow() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");

    for (from, text) in [("team-lead", "picked up"), ("worker-1", "blocked on the wire")] {
        store
            .update(
                &task.id,
                Update {
                    add_comment: Some(Comment::new(from, text, "2026-09-02T10:00:00.000Z")),
                    ..Update::default()
                },
            )
            .expect("a comment appends");
    }

    let held = store.get(&task.id).expect("the task reads");
    assert_eq!(held.comments.len(), 2);
    assert_eq!(held.comments[0].from, "team-lead", "oldest first");
    assert_eq!(held.comments[1].text, "blocked on the wire");
    assert_eq!(held.comments[1].at, "2026-09-02T10:00:00.000Z");
}

#[test]
fn a_blocker_added_twice_is_held_once() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");
    let second = TaskId::parse("2").expect("a valid id");
    let third = TaskId::parse("3").expect("a valid id");

    store
        .update(
            &task.id,
            Update {
                add_blocked_by: vec![second, third],
                add_blocks: vec![third],
                ..Update::default()
            },
        )
        .expect("blockers wire up");
    let held = store
        .update(&task.id, Update { add_blocked_by: vec![second], ..Update::default() })
        .expect("a repeat wires up");

    assert_eq!(held.blocked_by, [second, third], "the order ids arrived in is the order kept");
    assert_eq!(held.blocks, [third]);
}

#[test]
fn a_status_walks_pending_to_in_progress_to_completed() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");

    for status in [TaskStatus::InProgress, TaskStatus::Completed] {
        let moved = store
            .update(&task.id, Update { status: Some(status), ..Update::default() })
            .expect("the status moves");
        assert_eq!(moved.status, status);
    }

    // The wire spelling is the one a model was trained on, not Rust's.
    let text = fs::read_to_string(store.path_of(&task.id)).expect("the document reads");
    assert!(text.contains("\"status\": \"completed\""), "{text}");
}

#[test]
fn a_deleted_task_takes_its_document_and_its_lock_with_it() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");
    let path = store.path_of(&task.id);

    store.delete(&task.id).expect("the task deletes");

    assert!(!path.exists(), "a delete is permanent, never a tombstone");
    assert!(
        !path.with_extension("json.lock").exists(),
        "the hold's own directory goes with the document",
    );
    assert!(matches!(store.get(&task.id), Err(TaskError::NoSuchTask { .. })));
    assert!(matches!(store.delete(&task.id), Err(TaskError::NoSuchTask { .. })));
    assert!(store.list().expect("the list reads").is_empty());
}

#[test]
fn nothing_answers_for_an_id_that_was_never_filed() {
    let (_home, store) = store();
    let absent = TaskId::parse("7").expect("a valid id");

    // Before the directory exists at all, which is the case a store answers
    // for a team nobody has created a task in yet.
    assert!(matches!(store.get(&absent), Err(TaskError::NoSuchTask { .. })));
    assert!(matches!(store.claim(&absent, "worker-1"), Err(TaskError::NoSuchTask { .. })));
    assert!(matches!(store.update(&absent, Update::default()), Err(TaskError::NoSuchTask { .. })));
    assert!(matches!(store.delete(&absent), Err(TaskError::NoSuchTask { .. })));
    assert!(store.list().expect("an absent directory is an empty list").is_empty());

    // And once it does, beside a task that is filed.
    store.create(NewTask::new("subject", "description")).expect("a task is created");
    assert!(matches!(store.get(&absent), Err(TaskError::NoSuchTask { .. })));
    assert!(matches!(store.claim(&absent, "worker-1"), Err(TaskError::NoSuchTask { .. })));
}

#[test]
fn a_listing_is_lowest_id_first_and_never_alphabetical() {
    let (_home, store) = store();
    for nth in 1..=11 {
        store
            .create(NewTask::new(format!("task {nth}"), "description"))
            .expect("a task is created");
    }
    let second = TaskId::parse("2").expect("a valid id");
    store.claim(&second, "worker-1").expect("a task is claimable");
    store
        .update(&second, Update { status: Some(TaskStatus::InProgress), ..Update::default() })
        .expect("the status moves");

    let listed = store.list().expect("the list reads");
    let ids: Vec<u64> = listed.iter().map(|summary| summary.id.number()).collect();
    assert_eq!(ids, (1..=11).collect::<Vec<_>>(), "\"10\" sorts before \"9\" only as text");

    let claimed = &listed[1];
    assert_eq!(claimed.subject, "task 2");
    assert_eq!(claimed.status, TaskStatus::InProgress);
    assert_eq!(claimed.owner, "worker-1");
    assert!(listed[0].owner.is_empty(), "an unowned task's owner is the empty string");
}

#[test]
fn a_damaged_document_is_left_out_of_the_list_rather_than_taking_it_down() {
    let (_home, store) = store();
    for _ in 0..3 {
        store.create(NewTask::new("subject", "description")).expect("a task is created");
    }
    let damaged = TaskId::parse("2").expect("a valid id");
    fs::write(store.path_of(&damaged), "{not json at all").expect("the document is writable");

    let listed = store.list().expect("a damaged document does not fail the list");
    let ids: Vec<u64> = listed.iter().map(|summary| summary.id.number()).collect();
    assert_eq!(ids, [1, 3], "one broken file must not cost a team its whole list");
    assert!(store.get(&damaged).is_err(), "asking for it directly still says so");
}

#[test]
fn a_key_this_build_never_heard_of_survives_an_update_in_position() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");
    let path = store.path_of(&task.id);

    // What a newer build — or a peer sharing this directory — might write.
    let mut parsed: Value = serde_json::from_str(&fs::read_to_string(&path).expect("it reads"))
        .expect("the document is JSON");
    parsed.as_object_mut().expect("a task is an object").insert("effort".to_owned(), json!(3));
    fs::write(&path, serde_json::to_string_pretty(&parsed).expect("it encodes"))
        .expect("the document is writable");

    let updated = store
        .update(&task.id, Update { status: Some(TaskStatus::Completed), ..Update::default() })
        .expect("the update lands");
    assert_eq!(updated.extra["effort"], json!(3), "an unknown key is read and kept");

    let rewritten = fs::read_to_string(&path).expect("the document reads");
    assert!(rewritten.contains("\"effort\": 3"), "and written back: {rewritten}");
    assert!(
        rewritten.find("\"effort\"") > rewritten.find("\"comments\""),
        "in position, which here means after the keys the shape declares: {rewritten}",
    );
}

#[test]
fn a_passthrough_key_the_shape_declares_is_refused_before_a_byte_is_written() {
    let (_home, store) = store();
    fs::create_dir_all(store.dir()).expect("the directory is creatable");
    let path = store.path_of(&TaskId::parse("1").expect("a valid id"));

    let mut task = filled();
    task.extra.insert("status".to_owned(), json!("completed"));
    let issues = match write(&path, &task) {
        Err(TaskError::SchemaInvalid { issues }) => issues,
        other => panic!("a shadowing key is refused: {other:?}"),
    };
    assert_eq!(issues.len(), 1);
    assert!(issues[0].starts_with("status: "), "{}", issues[0]);
    assert!(!path.exists(), "and nothing was written");

    // The same rule one level down, where a comment carries the map.
    let mut task = filled();
    task.comments[0].extra.insert("text".to_owned(), json!("shadowed"));
    assert!(matches!(write(&path, &task), Err(TaskError::SchemaInvalid { .. })));
}

#[test]
fn the_task_key_list_is_exactly_what_a_task_serializes() {
    let serialized = serde_json::to_value(filled()).expect("a task encodes");
    let emitted: BTreeSet<&str> =
        serialized.as_object().expect("a task is an object").keys().map(String::as_str).collect();

    assert_eq!(
        emitted,
        TASK_KEYS.into_iter().collect::<BTreeSet<_>>(),
        "a field added to the shape has to join the list the shadow check reads",
    );
}

#[test]
fn a_task_document_is_the_bytes_every_other_document_here_is() {
    let encoded = document(&filled()).expect("a task encodes");

    assert!(encoded.starts_with("{\n  \"id\": \"1\",\n  \"subject\": "), "{encoded}");
    assert!(!encoded.ends_with('\n'), "no trailing newline, like every other document here");
    assert!(encoded.contains("\"status\": \"in_progress\""), "{encoded}");
    assert!(encoded.contains("\"blockedBy\": [\n    \"3\"\n  ]"), "ids are strings: {encoded}");
    assert!(encoded.contains("\"activeForm\": \"porting the parser\""), "{encoded}");

    let decoded: Task = serde_json::from_str(&encoded).expect("a task decodes");
    assert_eq!(decoded, filled(), "and the round trip is lossless");
}

#[test]
fn an_active_form_nobody_set_is_absent_rather_than_null() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");

    let text = fs::read_to_string(store.path_of(&task.id)).expect("the document reads");
    assert!(!text.contains("activeForm"), "an absent optional writes no key: {text}");

    let set = store
        .update(&task.id, Update { active_form: Some("porting".to_owned()), ..Update::default() })
        .expect("the update lands");
    assert_eq!(set.active_form.as_deref(), Some("porting"));
}

#[test]
fn a_debug_rendering_carries_no_description_no_comment_and_no_metadata_value() {
    let mut task = filled();
    task.description = "a credential could be in here".to_owned();
    task.comments[0].text = "so could a conversation".to_owned();
    task.metadata.insert("secret".to_owned(), json!("and here"));
    let rendered = format!("{task:?}");

    for content in ["a credential could be in here", "so could a conversation", "and here"] {
        assert!(!rendered.contains(content), "content reached a rendering: {rendered}");
    }
    // What is left is addressing, which is what makes the rendering worth
    // having at all.
    for structure in
        ["TaskId(1)", "subject: \"port the parser\"", "owner: \"worker-1\"", "\"secret\""]
    {
        assert!(rendered.contains(structure), "{structure} is missing from {rendered}");
    }

    let update = Update { description: Some("also content".to_owned()), ..Update::default() };
    assert!(!format!("{update:?}").contains("also content"));
    let draft = NewTask::new("subject", "also content");
    assert!(!format!("{draft:?}").contains("also content"));
}

#[test]
fn the_counter_and_the_locks_are_not_mistaken_for_tasks() {
    let (_home, store) = store();
    store.create(NewTask::new("subject", "description")).expect("a task is created");

    // A directory that looks like a document, and the counter itself: neither
    // is a task, and a listing that thought otherwise would report a task
    // nobody created.
    fs::create_dir(store.dir().join("2.json.lock")).expect("a lock directory is creatable");
    fs::write(store.dir().join("notes.txt"), "scratch").expect("a stray file is writable");
    assert!(store.dir().join(COUNTER).exists(), "the counter is where the store says it is");

    let listed = store.list().expect("the list reads");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id.number(), 1);
}

#[test]
fn a_peers_hold_stops_every_write_and_none_of_the_reads() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");
    let path = store.path_of(&task.id);

    // A peer takes the lock the way a peer takes it: a bare `mkdir` on the
    // document's real path, through no code of ours. Nothing here goes through
    // `lock::acquire`, so the in-process half is not held and what the store
    // spends below is the on-disk ladder — which is the half that matters,
    // because the other process in a real race is not this one.
    let real = fs::canonicalize(&path).expect("the document is real");
    let lock = PathBuf::from(format!("{}.lock", real.display()));
    fs::create_dir(&lock).expect("a peer takes the lock");

    for refused in [
        store.claim(&task.id, "worker-1").err(),
        store.delete(&task.id).err(),
        store.update(&task.id, Update::default()).err(),
    ] {
        assert!(
            matches!(refused, Some(TaskError::Lock(LockError::Held { .. }))),
            "a write waits out the ladder and then reports the hold: {refused:?}",
        );
    }

    // A read is deliberately not a write: a document lands through a rename,
    // so a reader sees one whole version or the one before it and has nothing
    // to wait for.
    let read = store.get(&task.id).expect("a read does not queue behind a writer");
    assert!(!read.is_owned(), "and none of the refused writes landed");
    assert_eq!(store.list().expect("a listing reads too").len(), 1);

    fs::remove_dir(&lock).expect("the peer releases");
    assert!(path.exists(), "the document the delete was refused is still there");
    assert_eq!(
        store.claim(&task.id, "worker-1").expect("the released task claims").owner,
        "worker-1"
    );
}
