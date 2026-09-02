use ganja_team::task::{REFUSED_ALREADY_OWNED, REFUSED_ID_SHAPE, REFUSED_NO_SUCH_TASK, Store};
use ganja_tool::tasklist::{Change, Draft, Owner, Status, TaskList as _};

use super::TeamTasks;

/// A list in a directory that goes away with the test, acted on as `identity`.
fn list(identity: &str) -> (tempfile::TempDir, TeamTasks) {
    let home = tempfile::tempdir().expect("a temporary directory");
    let tasks = TeamTasks::new(home.path().join("tasks"), identity);

    (home, tasks)
}

/// A filed task to act on, with the subject the test names.
async fn filed(tasks: &TeamTasks, subject: &str) -> String {
    tasks
        .create(Draft {
            subject: subject.to_owned(),
            description: "start from the spec".to_owned(),
            ..Draft::default()
        })
        .await
        .expect("a create files a task")
        .id
}

#[tokio::test]
async fn a_create_files_a_pending_ownerless_task_with_a_fresh_id() {
    let (_home, tasks) = list("team-lead");

    let first = tasks
        .create(Draft {
            subject: "port the parser".to_owned(),
            description: "start from the spec".to_owned(),
            active_form: Some("porting the parser".to_owned()),
            metadata: [("wave".to_owned(), serde_json::json!("W3"))].into_iter().collect(),
        })
        .await
        .expect("a create files a task");

    assert_eq!(first.id, "1", "ids are sequential and start at one");
    assert_eq!(first.status, Status::Pending);
    assert!(first.owner.is_empty(), "and nobody owns it yet");
    assert_eq!(first.active_form.as_deref(), Some("porting the parser"));
    assert_eq!(first.metadata["wave"], serde_json::json!("W3"));

    let second = filed(&tasks, "wire the tests").await;
    assert_eq!(second, "2", "the next id follows the counter");
}

/// The claim is what two teammates race for, and only one of them can win it.
#[tokio::test]
async fn a_claim_is_refused_when_somebody_already_holds_the_task() {
    let (_home, tasks) = list("worker-1");
    let id = filed(&tasks, "port the parser").await;

    let claimed = tasks
        .update(
            &id,
            Change { owner: Some(Owner::Claim("worker-1".to_owned())), ..Change::default() },
        )
        .await
        .expect("an unowned task can be claimed");
    assert_eq!(claimed.owner, "worker-1");

    let refused = tasks
        .update(
            &id,
            Change { owner: Some(Owner::Claim("worker-2".to_owned())), ..Change::default() },
        )
        .await
        .expect_err("a claimed task cannot be claimed again");
    assert!(refused.reason.contains(REFUSED_ALREADY_OWNED), "{}", refused.reason);
    assert!(refused.reason.contains("worker-1"), "the loser is told who won: {}", refused.reason);
}

/// A lost claim leaves the task exactly as it was — a teammate that had
/// already marked it in progress would have told the team a lie about who is
/// doing the work.
#[tokio::test]
async fn a_refused_claim_applies_nothing_else_in_the_same_call() {
    let (_home, tasks) = list("worker-2");
    let id = filed(&tasks, "port the parser").await;
    tasks
        .update(
            &id,
            Change { owner: Some(Owner::Claim("worker-1".to_owned())), ..Change::default() },
        )
        .await
        .expect("the first claim wins");

    tasks
        .update(
            &id,
            Change {
                owner: Some(Owner::Claim("worker-2".to_owned())),
                status: Some(Status::InProgress),
                subject: Some("reworded".to_owned()),
                ..Change::default()
            },
        )
        .await
        .expect_err("the second claim loses");

    let after = tasks.get(&id).await.expect("the task is still there");
    assert_eq!(after.owner, "worker-1", "the winner still holds it");
    assert_eq!(after.status, Status::Pending, "and nothing else moved");
    assert_eq!(after.subject, "port the parser");
}

/// Releasing is the door that never refuses: it is how a lead takes work back
/// from a member that stopped, and what makes reassigning two calls.
#[tokio::test]
async fn a_release_frees_the_task_for_the_next_claim() {
    let (_home, tasks) = list("team-lead");
    let id = filed(&tasks, "port the parser").await;
    tasks
        .update(
            &id,
            Change { owner: Some(Owner::Claim("worker-1".to_owned())), ..Change::default() },
        )
        .await
        .expect("the claim lands");

    let released = tasks
        .update(&id, Change { owner: Some(Owner::Release), ..Change::default() })
        .await
        .expect("a release never refuses");
    assert!(released.owner.is_empty());

    let reclaimed = tasks
        .update(
            &id,
            Change { owner: Some(Owner::Claim("worker-2".to_owned())), ..Change::default() },
        )
        .await
        .expect("a released task is free");
    assert_eq!(reclaimed.owner, "worker-2");
}

/// A claim and the rest of a change in one call: the claim lands first, and
/// the rest lands on top of it.
#[tokio::test]
async fn a_claim_beside_other_changes_applies_both() {
    let (_home, tasks) = list("worker-1");
    let id = filed(&tasks, "port the parser").await;

    let taken = tasks
        .update(
            &id,
            Change {
                owner: Some(Owner::Claim("worker-1".to_owned())),
                status: Some(Status::InProgress),
                ..Change::default()
            },
        )
        .await
        .expect("an unowned task takes both");

    assert_eq!(taken.owner, "worker-1");
    assert_eq!(taken.status, Status::InProgress);
}

/// Who wrote a comment is the list's to say. There is no argument for it, and
/// the name that lands is the one this list was built with.
#[tokio::test]
async fn a_comment_is_written_under_the_identity_the_list_was_built_with() {
    let (_home, tasks) = list("worker-7");
    let id = filed(&tasks, "port the parser").await;

    let commented = tasks
        .update(
            &id,
            Change {
                add_comment: Some("the lexer is the hard half".to_owned()),
                ..Change::default()
            },
        )
        .await
        .expect("a comment appends");

    assert_eq!(commented.comments.len(), 1);
    assert_eq!(commented.comments[0].from, "worker-7");
    assert_eq!(commented.comments[0].text, "the lexer is the hard half");
    assert!(!commented.comments[0].at.is_empty(), "and it is stamped with a time");
}

#[tokio::test]
async fn metadata_merges_and_a_null_removes_its_key() {
    let (_home, tasks) = list("team-lead");
    let id = tasks
        .create(Draft {
            subject: "port".to_owned(),
            description: "from the spec".to_owned(),
            metadata: [
                ("wave".to_owned(), serde_json::json!("W3")),
                ("owner_note".to_owned(), serde_json::json!("mine")),
            ]
            .into_iter()
            .collect(),
            ..Draft::default()
        })
        .await
        .expect("a create files a task")
        .id;

    let merged = tasks
        .update(
            &id,
            Change {
                metadata: [
                    ("wave".to_owned(), serde_json::json!("W4")),
                    ("owner_note".to_owned(), serde_json::json!(null)),
                ]
                .into_iter()
                .collect(),
                ..Change::default()
            },
        )
        .await
        .expect("a merge lands");

    assert_eq!(merged.metadata["wave"], serde_json::json!("W4"), "a value replaces its key");
    assert!(!merged.metadata.contains_key("owner_note"), "and a null removes one");
}

#[tokio::test]
async fn blockers_are_added_and_listed_as_the_ids_they_are() {
    let (_home, tasks) = list("team-lead");
    let first = filed(&tasks, "port the parser").await;
    let second = filed(&tasks, "wire the tests").await;

    tasks
        .update(&second, Change { add_blocked_by: vec![first.clone()], ..Change::default() })
        .await
        .expect("a blocker is added");

    let listed = tasks.list().await.expect("the list reads");
    let ids: Vec<&str> = listed.iter().map(|summary| summary.id.as_str()).collect();
    assert_eq!(ids, ["1", "2"], "lowest id first");
    assert_eq!(listed[1].blocked_by, vec![first], "and what holds it up is named");
}

#[tokio::test]
async fn a_delete_removes_the_task_and_leaves_its_id_spent() {
    let (_home, tasks) = list("team-lead");
    let id = filed(&tasks, "port the parser").await;

    tasks.delete(&id).await.expect("a filed task can be removed");

    let gone = tasks.get(&id).await.expect_err("nothing answers to it now");
    assert!(gone.reason.contains(REFUSED_NO_SUCH_TASK), "{}", gone.reason);
    assert_eq!(filed(&tasks, "wire the tests").await, "2", "and the id is not issued again");
}

/// An id that is not an id is refused before anything is touched, in the
/// store's own words.
#[tokio::test]
async fn an_id_outside_the_grammar_is_refused_in_the_stores_own_words() {
    let (_home, tasks) = list("team-lead");

    for offered in ["", "0", "01", "one", "../1", "1.json"] {
        let refused = tasks.get(offered).await.expect_err("{offered:?} is no id");
        assert!(refused.reason.contains(REFUSED_ID_SHAPE), "{offered:?}: {}", refused.reason);
    }
}

/// A blocker id that is not an id refuses the whole call rather than being
/// half-applied.
#[tokio::test]
async fn a_blocker_that_is_not_an_id_refuses_before_anything_is_written() {
    let (_home, tasks) = list("team-lead");
    let id = filed(&tasks, "port the parser").await;

    let refused = tasks
        .update(
            &id,
            Change {
                subject: Some("reworded".to_owned()),
                add_blocked_by: vec!["not-an-id".to_owned()],
                ..Change::default()
            },
        )
        .await
        .expect_err("a blocker outside the grammar is refused");
    assert!(refused.reason.contains(REFUSED_ID_SHAPE), "{}", refused.reason);
    assert_eq!(
        tasks.get(&id).await.expect("the task is still there").subject,
        "port the parser",
        "and the reword that travelled with it landed nowhere"
    );
}

/// Including one that travels with a claim nobody else is racing for: the ids
/// are read before the claim is written, so a call refused for a malformed
/// blocker leaves the task unowned rather than taken by a call that did not
/// land.
#[tokio::test]
async fn a_blocker_that_is_not_an_id_refuses_before_a_claim_that_would_have_won() {
    let (_home, tasks) = list("team-lead");
    let id = filed(&tasks, "port the parser").await;

    let refused = tasks
        .update(
            &id,
            Change {
                owner: Some(Owner::Claim("worker-1".to_owned())),
                add_blocked_by: vec!["not-an-id".to_owned()],
                ..Change::default()
            },
        )
        .await
        .expect_err("a blocker outside the grammar is refused");
    assert!(refused.reason.contains(REFUSED_ID_SHAPE), "{}", refused.reason);
    assert!(
        tasks.get(&id).await.expect("the task is still there").owner.is_empty(),
        "and the claim that travelled with it took nothing"
    );
}

/// A whole record comes back with everything a listing leaves out.
#[tokio::test]
async fn a_get_answers_with_what_a_listing_leaves_out() {
    let (_home, tasks) = list("worker-1");
    let id = filed(&tasks, "port the parser").await;
    tasks
        .update(&id, Change { add_comment: Some("started".to_owned()), ..Change::default() })
        .await
        .expect("a comment appends");

    let whole = tasks.get(&id).await.expect("the task reads");
    assert_eq!(whole.description, "start from the spec");
    assert_eq!(whole.comments.len(), 1);

    let summarized = tasks.list().await.expect("the list reads");
    assert_eq!(summarized[0].subject, "port the parser");
}

/// A list whose directory has never been made is an empty list, not a
/// failure: a team that has filed nothing is exactly that case.
#[tokio::test]
async fn a_list_that_was_never_created_reads_as_empty() {
    let (_home, tasks) = list("team-lead");

    assert!(tasks.list().await.expect("an absent directory is an empty list").is_empty());
}

/// The identity is a field, never a parameter — asserted on the type's own
/// rendering, which names the list and the member and nothing a task says.
#[test]
fn the_rendering_names_the_list_and_the_member_and_no_content() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let tasks = TeamTasks::new(home.path().join("tasks"), "worker-1");
    let rendered = format!("{tasks:?}");

    assert!(rendered.contains("worker-1"), "{rendered}");
    assert!(rendered.contains("tasks"), "{rendered}");
}

/// What this seam is built on is the store, not a second implementation of
/// it: a task filed through the seam is the document the store reads back.
#[tokio::test]
async fn what_the_seam_writes_is_what_the_store_reads() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let directory = home.path().join("tasks");
    let tasks = TeamTasks::new(&directory, "team-lead");
    let id = filed(&tasks, "port the parser").await;

    let store = Store::new(&directory);
    let listed = store.list().expect("the store reads the same directory");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id.to_string(), id);
    assert_eq!(listed[0].subject, "port the parser");
}
