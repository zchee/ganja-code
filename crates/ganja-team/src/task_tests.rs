use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde_json::{Value, json};

use super::{
    COUNTER, Comment, ID_MAX, MAX_COUNTERPARTS, MAX_DOCUMENT_BYTES, NewTask, Store, TASK_KEYS,
    Task, TaskError, TaskId, TaskStatus, Update, write,
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

/// Takes a task document's lock the way a *peer* takes it: a bare `mkdir` on
/// the document's real path, through no code of ours, so the in-process half
/// of the hold is not held and what a store spends is the on-disk ladder —
/// `a_peers_hold_stops_every_write_and_none_of_the_reads` explains why that is
/// the half worth simulating. Answers with the directory, for the caller to
/// remove.
fn peer_hold(store: &Store, id: &TaskId) -> PathBuf {
    let real = fs::canonicalize(store.path_of(id)).expect("the document is real");
    let lock = PathBuf::from(format!("{}.lock", real.display()));
    fs::create_dir(&lock).expect("a peer takes the lock");

    lock
}

/// The document as it is actually written, past every reader that tidies one
/// up on the way out.
///
/// [`Store::get`] and [`Store::list`] drop an edge naming an id nothing is
/// filed under, so a test that asked either of them whether a scrub ran would
/// pass without one. Only the bytes can say.
fn on_disk(store: &Store, id: &TaskId) -> Task {
    let text = fs::read_to_string(store.path_of(id)).expect("the document reads");

    serde_json::from_str(&text).expect("the document decodes")
}

/// Three tasks, so a test about edges has both ends of one to name.
fn three(store: &Store) -> [TaskId; 3] {
    [1, 2, 3].map(|nth| {
        store
            .create(NewTask::new(format!("task {nth}"), "description"))
            .expect("a task is created")
            .id
    })
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
fn a_counter_behind_the_documents_on_disk_never_reissues_a_live_id() {
    let (_home, store) = store();
    three(&store);

    // What a power loss leaves: the rename that files a document is not
    // fsynced, so the directory can hold 3 while the counter still says 1. A
    // copy of a team directory taken mid-flight is the same shape.
    fs::write(store.counter_path(), "1").expect("the counter is writable");

    let next = store.create(NewTask::new("task 4", "description")).expect("a task is created");
    assert_eq!(
        next.id.to_string(),
        "4",
        "a create issues past the documents, not past the counter"
    );

    let listed = store.list().expect("the list reads");
    let subjects: Vec<&str> = listed.iter().map(|summary| summary.subject.as_str()).collect();
    assert_eq!(
        subjects,
        ["task 1", "task 2", "task 3", "task 4"],
        "and renames over none of them on the way",
    );
}

#[test]
fn a_dependency_is_recorded_on_both_tasks() {
    let (_home, store) = store();
    let [first, second, third] = three(&store);

    let blocking = store
        .update(&first, Update { add_blocks: vec![second], ..Update::default() })
        .expect("an edge wires up");
    assert_eq!(blocking.blocks, [second]);
    assert_eq!(
        store.get(&second).expect("the counterpart reads").blocked_by,
        [first],
        "a listing renders blockedBy, so the end that was not named decides whether it reads free",
    );

    let blocked = store
        .update(&first, Update { add_blocked_by: vec![third], ..Update::default() })
        .expect("the other direction wires up too");
    assert_eq!(blocked.blocked_by, [third]);
    assert_eq!(store.get(&third).expect("the counterpart reads").blocks, [first]);
}

#[test]
fn an_edge_naming_a_task_nobody_filed_refuses_the_whole_update() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");
    let missing = TaskId::parse("9").expect("a valid id");

    let refused = store
        .update(
            &task.id,
            Update {
                subject: Some("moved".to_owned()),
                add_blocks: vec![missing],
                ..Update::default()
            },
        )
        .expect_err("an edge has two ends and one of them is not there");
    assert!(matches!(refused, TaskError::NoSuchTask { id } if id == missing), "{refused}");
    assert_eq!(
        store.get(&task.id).expect("the task reads").subject,
        "subject",
        "and the half that could have been written was not",
    );
}

#[test]
fn an_edge_writes_neither_end_until_it_holds_both() {
    let (_home, store) = store();
    let [first, second, _third] = three(&store);

    // Both orderings of one pair: the counterpart below the task being
    // updated, then above it. Whichever end a peer holds, every hold is taken
    // before any document is read, so the refusal leaves both ends as they
    // were.
    for (held, updated) in [(first, second), (second, first)] {
        let lock = peer_hold(&store, &held);

        let refused = store
            .update(&updated, Update { add_blocks: vec![held], ..Update::default() })
            .expect_err("a peer holds one end");
        assert!(matches!(refused, TaskError::Lock(LockError::Held { .. })), "{refused:?}");
        assert!(store.get(&updated).expect("the task reads").blocks.is_empty());
        assert!(store.get(&held).expect("the counterpart reads").blocked_by.is_empty());

        fs::remove_dir(&lock).expect("the peer releases");
    }
}

#[test]
fn a_task_that_blocks_itself_takes_one_hold_rather_than_two() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");

    // Taking a hold this thread already holds never returns, so the hold set
    // is deduplicated. A regression here hangs rather than fails, which is
    // exactly why it is pinned.
    let held = store
        .update(&task.id, Update { add_blocks: vec![task.id], ..Update::default() })
        .expect("a self-edge is one document, written once");

    assert_eq!(held.blocks, [task.id]);
    assert_eq!(held.blocked_by, [task.id], "both ends of a self-edge are the same document");
}

#[test]
fn a_document_the_schema_refuses_leaves_every_other_one_as_it_was() {
    let (_home, store) = store();
    let [first, second, _third] = three(&store);

    // A comment is the reachable way to hand an update a document a write
    // would refuse: a passthrough key the shape declares cannot arrive off
    // disk, serde refusing a duplicate declared key outright. It lands on the
    // task the call names, and the counterpart *below* it is written first —
    // which is the ordering that decides whether a refusal is a refusal or
    // half an edge.
    let mut comment = Comment::new("team-lead", "started", "2026-09-02T10:00:00.000Z");
    comment.extra.insert("text".to_owned(), json!("shadowed"));

    let refused = store
        .update(
            &second,
            Update { add_blocked_by: vec![first], add_comment: Some(comment), ..Update::default() },
        )
        .expect_err("a document this call would write does not match the schema");
    assert!(matches!(refused, TaskError::SchemaInvalid { .. }), "{refused:?}");

    assert!(
        store.get(&first).expect("the counterpart reads").blocks.is_empty(),
        "the counterpart is the document written first, so it is where a half-applied call shows",
    );
    let named = store.get(&second).expect("the task reads");
    assert!(named.blocked_by.is_empty(), "and the end the call named is untouched too");
    assert!(named.comments.is_empty());
}

#[test]
fn an_update_naming_more_counterparts_than_the_cap_is_refused_before_a_hold_is_taken() {
    let (_home, store) = store();
    let filed: Vec<TaskId> = (1..=MAX_COUNTERPARTS + 2)
        .map(|nth| {
            store
                .create(NewTask::new(format!("task {nth}"), "description"))
                .expect("a task is created")
                .id
        })
        .collect();
    let (task, counterparts) = filed.split_first().expect("a task and the ids it could name");

    // A peer holds the first document, so a call that reached the holds at all
    // would spend the ladder and report the hold. The cap has to answer ahead
    // of that: what it bounds is exactly the time that ladder costs, once per
    // counterpart, with the first document held throughout.
    let lock = peer_hold(&store, task);

    let refused = store
        .update(task, Update { add_blocks: counterparts.to_vec(), ..Update::default() })
        .expect_err("one past the cap is past the cap");
    assert!(
        matches!(refused, TaskError::TooManyCounterparts { named } if named == counterparts.len()),
        "the refusal is the cap rather than the peer's hold: {refused:?}",
    );
    assert!(
        refused.to_string().contains(&MAX_COUNTERPARTS.to_string()),
        "and it says how many a call may name: {refused}",
    );

    fs::remove_dir(&lock).expect("the peer releases");
    let wired = store
        .update(
            task,
            Update { add_blocks: counterparts[..MAX_COUNTERPARTS].to_vec(), ..Update::default() },
        )
        .expect("the cap is what a call may name, not what it must stay under");
    assert_eq!(wired.blocks.len(), MAX_COUNTERPARTS);
}

#[cfg(unix)]
#[test]
fn a_counterpart_that_is_one_file_under_two_names_is_refused_rather_than_re_entered() {
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    use std::time::Duration;

    let (_home, store) = store();
    let [first, planted, _third] = three(&store);

    // Two ids, one file. `lock::acquire_unseeded` locks the target's *real*
    // path, so a hold on 2 and a hold on 1 are one key, and the second one
    // parks on a condvar with no timeout — the dedupe is on the id and cannot
    // see this, which is why the stamp runs before any hold is taken.
    fs::remove_file(store.path_of(&planted)).expect("the document is removable");
    symlink(store.path_of(&first), store.path_of(&planted)).expect("a link is plantable");

    let (answered, answer) = mpsc::channel();
    let elsewhere = store.clone();
    std::thread::spawn(move || {
        let refused =
            elsewhere.update(&first, Update { add_blocks: vec![planted], ..Update::default() });
        let _ = answered.send(refused.err());
    });

    // The bound is the assertion: a regression here does not fail, it hangs.
    let refused = answer
        .recv_timeout(Duration::from_secs(10))
        .expect("the call answers rather than waiting on a hold this thread already holds");
    assert!(matches!(refused, Some(TaskError::NotADocument { .. })), "{refused:?}");
    assert!(
        store.get(&first).expect("the task reads").blocks.is_empty(),
        "and neither name was wired",
    );
}

#[cfg(unix)]
#[test]
fn a_name_that_is_not_a_regular_file_is_skipped_rather_than_read() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::symlink;

    let (_home, store) = store();
    let [first, planted, _third] = three(&store);

    // A symlink onto a real document: followed, it would read task 1 back
    // under task 2's name.
    fs::remove_file(store.path_of(&planted)).expect("the document is removable");
    symlink(store.path_of(&first), store.path_of(&planted)).expect("a link is plantable");

    // A FIFO: opened, this read would never return — and it runs on every
    // poll of a list somebody is watching.
    let fifo = store.dir().join("4.json");
    let name = CString::new(fifo.as_os_str().as_bytes()).expect("a path with no NUL");
    // SAFETY: `mkfifo` reads the NUL-terminated name and answers with a
    // status. The `CString` outlives the call, and nothing it returns is a
    // pointer this test dereferences.
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(made, 0, "the fifo is created: {}", std::io::Error::last_os_error());

    // And a directory wearing a document's name.
    fs::create_dir(store.dir().join("5.json")).expect("a directory is plantable");

    let listed = store.list().expect("a planted name does not fail the list");
    let ids: Vec<u64> = listed.iter().map(|summary| summary.id.number()).collect();
    assert_eq!(ids, [1, 3], "what a peer planted is skipped, and the rest of the list still reads");

    for refused in [2, 4, 5] {
        let id = TaskId::parse(&refused.to_string()).expect("a valid id");
        assert!(
            matches!(store.get(&id), Err(TaskError::NotADocument { .. })),
            "asking for {refused} directly says so rather than reading it",
        );
    }

    let next = store.create(NewTask::new("task 6", "description")).expect("a task is created");
    assert_eq!(next.id.to_string(), "6", "and the list can still be added to around them");
}

#[cfg(unix)]
#[test]
fn a_symlink_is_refused_at_the_open_rather_than_followed_to_whatever_it_names() {
    use std::os::unix::fs::symlink;

    let (home, store) = store();
    let [first, planted, _third] = three(&store);

    // Two links where documents belong: one onto a real task, one onto a name
    // nothing was ever written to. Anything that consulted the *target* would
    // tell them apart — the first would answer with task 1's document under
    // task 2's name, the second with nothing filed there at all. `O_NOFOLLOW`
    // cannot tell them apart, because it never asks: the open itself fails on
    // the link.
    fs::remove_file(store.path_of(&planted)).expect("the document is removable");
    symlink(store.path_of(&first), store.path_of(&planted)).expect("a link is plantable");
    let dangling = TaskId::parse("4").expect("a valid id");
    symlink(home.path().join("nothing-was-ever-written-here"), store.path_of(&dangling))
        .expect("a link is plantable");

    for refused in [planted, dangling] {
        assert!(
            matches!(store.get(&refused), Err(TaskError::NotADocument { .. })),
            "a link is refused whatever it names, and {refused} is no exception",
        );
    }
    assert!(
        store.path_of(&first).is_file(),
        "and the document one of them named was never opened through it",
    );
}

#[cfg(unix)]
#[test]
fn a_fifo_where_a_document_belongs_answers_rather_than_parking_the_reader() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::sync::mpsc;
    use std::time::Duration;

    let (_home, store) = store();
    let planted = TaskId::parse("1").expect("a valid id");
    fs::create_dir_all(store.dir()).expect("the directory a create would have made");

    let name =
        CString::new(store.path_of(&planted).as_os_str().as_bytes()).expect("a path with no NUL");
    // SAFETY: `mkfifo` reads the NUL-terminated name and answers with a
    // status. The `CString` outlives the call, and nothing it returns is a
    // pointer this test dereferences.
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(made, 0, "the fifo is created: {}", std::io::Error::last_os_error());

    // Nothing has this open for writing, so an ordinary `open` for reading
    // parks until something does — forever, here. `O_NONBLOCK` is the only
    // thing standing between that and a listing a lead's render loop polls,
    // now that nothing judges the name before the open. The bound is the
    // assertion: a regression here does not fail, it hangs.
    let (answered, answer) = mpsc::channel();
    let elsewhere = store.clone();
    std::thread::spawn(move || {
        let _ = answered.send((elsewhere.get(&planted).err(), elsewhere.list()));
    });

    let (refused, listed) = answer
        .recv_timeout(Duration::from_secs(10))
        .expect("the open answers rather than waiting for a writer that never comes");
    assert!(matches!(refused, Some(TaskError::NotADocument { .. })), "{refused:?}");
    assert!(
        listed.expect("a planted name does not fail the list").is_empty(),
        "and the listing that would have parked on it comes back without it",
    );
}

#[test]
fn a_document_larger_than_a_task_can_be_is_skipped_rather_than_read() {
    let (_home, store) = store();
    let [_first, planted, _third] = three(&store);

    // Valid JSON either way: what is refused is the size, not the shape. The
    // padding is plain ASCII, so a byte of description is a byte of document.
    let mut task = filled();
    task.id = planted;
    task.description = String::new();
    let bare = document(&task).expect("a task encodes").len() as u64;
    task.description = "a".repeat((MAX_DOCUMENT_BYTES - bare) as usize);
    let at_the_bound = document(&task).expect("a task encodes");
    assert_eq!(at_the_bound.len() as u64, MAX_DOCUMENT_BYTES, "the padding lands on the bound");

    fs::write(store.path_of(&planted), &at_the_bound).expect("the document is writable");
    assert_eq!(
        store.get(&planted).expect("a document on the bound reads").id,
        planted,
        "the bound is what a document may weigh, not what it must stay under",
    );

    task.description.push('a');
    fs::write(store.path_of(&planted), document(&task).expect("a task encodes"))
        .expect("the document is writable");

    assert!(
        matches!(store.get(&planted), Err(TaskError::NotADocument { .. })),
        "one byte past it is not read at all",
    );
    let listed = store.list().expect("an oversized document does not fail the list");
    let ids: Vec<u64> = listed.iter().map(|summary| summary.id.number()).collect();
    assert_eq!(ids, [1, 3]);
}

#[cfg(unix)]
#[test]
fn a_counter_planted_as_a_symlink_is_repaired_rather_than_followed() {
    use std::os::unix::fs::symlink;

    let (home, store) = store();
    store.create(NewTask::new("subject", "description")).expect("a task is created");

    let elsewhere = home.path().join("elsewhere");
    fs::write(&elsewhere, "9999").expect("the target is writable");
    fs::remove_file(store.counter_path()).expect("the counter is removable");
    symlink(&elsewhere, store.counter_path()).expect("a link is plantable");

    let next = store.create(NewTask::new("subject", "description")).expect("a create still lands");
    assert_eq!(next.id.to_string(), "2", "a counter that is not a regular file is not followed");
    assert_eq!(
        fs::read_to_string(&elsewhere).expect("the target reads"),
        "9999",
        "and the repair renames over the name rather than writing through it",
    );
    assert!(
        store.counter_path().symlink_metadata().expect("the counter is there").is_file(),
        "what stands there afterwards is a counter",
    );
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
    // Both ends of an edge are written, so both ends have to be tasks.
    let [first, second, third] = three(&store);

    store
        .update(
            &first,
            Update {
                add_blocked_by: vec![second, third],
                add_blocks: vec![third],
                ..Update::default()
            },
        )
        .expect("blockers wire up");
    let held = store
        .update(&first, Update { add_blocked_by: vec![second], ..Update::default() })
        .expect("a repeat wires up");

    assert_eq!(held.blocked_by, [second, third], "the order ids arrived in is the order kept");
    assert_eq!(held.blocks, [third]);
    assert_eq!(
        store.get(&second).expect("the counterpart reads").blocks,
        [first],
        "held once on the far end too: the repeat added nothing there either",
    );
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
fn a_delete_takes_the_other_end_of_every_edge_with_it() {
    let (_home, store) = store();
    let [first, second, third] = three(&store);

    // Both directions at once: the task being deleted blocks one and is
    // blocked by another, so a scrub that walked only one of its two lists
    // would leave the other end standing.
    store
        .update(
            &first,
            Update { add_blocks: vec![second], add_blocked_by: vec![third], ..Update::default() },
        )
        .expect("the edges wire up");

    store.delete(&first).expect("the task deletes");

    assert_eq!(on_disk(&store, &second).blocked_by, [], "what it blocked is free work again");
    assert_eq!(on_disk(&store, &third).blocks, [], "and what blocked it no longer claims to");
}

#[test]
fn a_delete_scrubs_more_counterparts_than_one_update_may_name() {
    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");
    let counterparts: Vec<TaskId> = (0..=MAX_COUNTERPARTS)
        .map(|nth| {
            store
                .create(NewTask::new(format!("counterpart {nth}"), "description"))
                .expect("a task is created")
                .id
        })
        .collect();

    // Past the cap on purpose, one call at a time — which is what a task's
    // edge list accumulates like, and why a delete takes one hold at a time
    // rather than the whole set an update would have to.
    for counterpart in &counterparts {
        store
            .update(&task.id, Update { add_blocks: vec![*counterpart], ..Update::default() })
            .expect("an edge wires up");
    }
    assert!(counterparts.len() > MAX_COUNTERPARTS, "the point of the test is the cap");

    store.delete(&task.id).expect("the task deletes");

    for counterpart in &counterparts {
        assert_eq!(
            on_disk(&store, counterpart).blocked_by,
            [],
            "every counterpart is scrubbed, not the first {MAX_COUNTERPARTS} of them",
        );
    }
}

#[test]
fn a_task_that_blocks_itself_is_deletable() {
    use std::sync::mpsc;
    use std::time::Duration;

    let (_home, store) = store();
    let task = store.create(NewTask::new("subject", "description")).expect("a task is created");
    store
        .update(&task.id, Update { add_blocks: vec![task.id], ..Update::default() })
        .expect("a self-edge is one document");

    // The only counterpart a self-edge names is the document being removed.
    // Two things keep the scrub off it — the delete's own hold is released
    // before the first one runs, and a task is not a counterpart of its own —
    // and a call that lost both would park on a hold it already holds. The
    // bound is the assertion: a regression here does not fail, it hangs.
    let (answered, answer) = mpsc::channel();
    let elsewhere = store.clone();
    std::thread::spawn(move || {
        let _ = answered.send(elsewhere.delete(&task.id));
    });

    answer
        .recv_timeout(Duration::from_secs(10))
        .expect("a delete answers rather than scrubbing the document it just removed")
        .expect("the task deletes");
    assert!(store.list().expect("the list reads").is_empty());
}

#[test]
fn an_edge_naming_a_task_nobody_filed_is_read_as_absent_rather_than_as_a_blocker() {
    let (_home, store) = store();
    let [first, second, _third] = three(&store);
    let absent = TaskId::parse("99").expect("a valid id");

    // What a crash between two scrubs leaves, and what a foreign writer or a
    // build older than the scrub can leave too: an edge naming an id the
    // directory has nothing filed under.
    let mut task = store.get(&first).expect("the task reads");
    task.blocked_by = vec![absent, second];
    task.blocks = vec![absent];
    write(&store.path_of(&first), &task).expect("the document is writable");

    let read_back = store.get(&first).expect("the task reads");
    assert_eq!(read_back.blocked_by, [second], "a blocker nobody filed does not block");
    assert_eq!(read_back.blocks, [], "and the other direction is dropped the same way");
    let listed = store.list().expect("the list reads");
    let row = listed.iter().find(|summary| summary.id == first).expect("the task is listed");
    assert_eq!(row.blocked_by, [second], "the listing that offers free work answers the same");

    assert_eq!(
        on_disk(&store, &first).blocked_by,
        [absent, second],
        "and nothing was repaired: the read side tolerates what it will not answer",
    );
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

#[test]
fn a_planted_name_at_the_top_of_the_id_space_does_not_wedge_every_create() {
    let (_home, store) = store();
    three(&store);
    // The name alone and nothing behind it, twice over: an empty file and a
    // directory, each spelt like the last id there is. A name that is no
    // document must not move the issue point, or every create from here on
    // would be refused as the end of the id space.
    fs::write(store.dir().join(format!("{ID_MAX}.json")), "").expect("the name is plantable");
    fs::create_dir(store.dir().join(format!("{}.json", ID_MAX - 1)))
        .expect("the directory is plantable");

    let next = store
        .create(NewTask::new("task 4", "description"))
        .expect("a name that is no document moves nothing");
    assert_eq!(next.id.to_string(), "4", "issued past the documents, not past the names");
}
