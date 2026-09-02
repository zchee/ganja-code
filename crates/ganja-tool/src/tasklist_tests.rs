use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use super::{
    Change, Comment, DELETE_WITH_CHANGES, Draft, EMPTY, MAX_COUNTERPARTS, NO_LIST, Owner, Record,
    Status, Summary, TaskCreateTool, TaskFailure, TaskGetTool, TaskList, TaskListTool,
    TaskUpdateTool, UNOWNED, UPDATE_DESCRIPTION,
};
use crate::{Tool as _, ToolCtx, ToolError, ToolOutput};

/// What the fake was asked to do, in the vocabulary the seam crosses in — so
/// a test asserts on the *call the tool made*, which is the whole of what this
/// layer decides.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Asked {
    Create(Draft),
    Update(String, Change),
    Delete(String),
    List,
    Get(String),
}

/// A list that records what reached it and answers whatever the test handed
/// it.
#[derive(Debug)]
struct Fake {
    asked: Mutex<Vec<Asked>>,
    record: Record,
    summaries: Vec<Summary>,
    refusal: Option<TaskFailure>,
}

impl Fake {
    fn new() -> Self {
        Self {
            asked: Mutex::new(Vec::new()),
            record: record(),
            summaries: vec![
                Summary {
                    id: "1".to_owned(),
                    subject: "port the parser".to_owned(),
                    status: Status::Completed,
                    owner: "worker-1".to_owned(),
                    blocked_by: Vec::new(),
                },
                Summary {
                    id: "2".to_owned(),
                    subject: "wire the tests".to_owned(),
                    status: Status::Pending,
                    owner: String::new(),
                    blocked_by: vec!["1".to_owned()],
                },
            ],
            refusal: None,
        }
    }

    /// A list that refuses everything with `reason` — the far side's sentence,
    /// which this layer must carry through unchanged.
    fn refusing(reason: &str) -> Self {
        Self { refusal: Some(TaskFailure { reason: reason.to_owned() }), ..Self::new() }
    }

    fn calls(&self) -> Vec<Asked> {
        self.asked.lock().expect("no test panics while holding this").clone()
    }

    fn record(&self, asked: Asked) -> Result<(), TaskFailure> {
        self.asked.lock().expect("no test panics while holding this").push(asked);
        match &self.refusal {
            Some(refusal) => Err(refusal.clone()),
            None => Ok(()),
        }
    }
}

/// One whole task, as the fake answers with it.
fn record() -> Record {
    Record {
        id: "1".to_owned(),
        subject: "port the parser".to_owned(),
        description: "start from the spec".to_owned(),
        active_form: Some("porting the parser".to_owned()),
        status: Status::InProgress,
        owner: "worker-1".to_owned(),
        blocks: vec!["2".to_owned()],
        blocked_by: Vec::new(),
        metadata: serde_json::Map::new(),
        comments: vec![Comment {
            from: "worker-1".to_owned(),
            at: "2026-09-02T00:00:00.000Z".to_owned(),
            text: "the lexer is the hard half".to_owned(),
        }],
    }
}

#[async_trait]
impl TaskList for Fake {
    async fn create(&self, draft: Draft) -> Result<Record, TaskFailure> {
        self.record(Asked::Create(draft))?;

        Ok(self.record.clone())
    }

    async fn update(&self, id: &str, change: Change) -> Result<Record, TaskFailure> {
        self.record(Asked::Update(id.to_owned(), change))?;

        Ok(self.record.clone())
    }

    async fn delete(&self, id: &str) -> Result<(), TaskFailure> {
        self.record(Asked::Delete(id.to_owned()))
    }

    async fn list(&self) -> Result<Vec<Summary>, TaskFailure> {
        self.record(Asked::List)?;

        Ok(self.summaries.clone())
    }

    async fn get(&self, id: &str) -> Result<Record, TaskFailure> {
        self.record(Asked::Get(id.to_owned()))?;

        Ok(self.record.clone())
    }
}

/// A call with `list` behind it, or with nothing behind it when [`None`].
fn ctx(list: Option<Arc<dyn TaskList>>) -> ToolCtx {
    let mut ctx = ToolCtx::fixture(std::env::temp_dir());
    ctx.tasks = list;
    ctx
}

/// Runs one call against `list` and reports what it answered.
async fn run(tool: &dyn crate::Tool, list: &Arc<Fake>, args: serde_json::Value) -> ToolOutput {
    tool.run(args, &ctx(Some(Arc::clone(list) as Arc<dyn TaskList>)))
        .await
        .expect("the call answers")
}

/// Runs one call and reports the sentence the model reads instead of an
/// answer.
async fn refusal(tool: &dyn crate::Tool, list: &Arc<Fake>, args: serde_json::Value) -> String {
    match tool.run(args, &ctx(Some(Arc::clone(list) as Arc<dyn TaskList>))).await {
        Err(ToolError::Failed(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_create_files_the_draft_the_call_described() {
    let list = Arc::new(Fake::new());
    let output = run(
        &TaskCreateTool,
        &list,
        json!({
            "subject": "port the parser",
            "description": "start from the spec",
            "active_form": "porting the parser",
            "metadata": {"wave": "W3"},
        }),
    )
    .await;

    assert_eq!(
        list.calls(),
        vec![Asked::Create(Draft {
            subject: "port the parser".to_owned(),
            description: "start from the spec".to_owned(),
            active_form: Some("porting the parser".to_owned()),
            metadata: [("wave".to_owned(), json!("W3"))].into_iter().collect(),
        })],
        "every argument reaches the list as the draft it describes"
    );
    assert!(
        output.output.contains("task 1"),
        "the answer names the id a later call uses: {output:?}"
    );
    assert_eq!(output.title, "task 1 filed");
}

#[tokio::test]
async fn a_create_without_the_optional_arguments_still_files_a_draft() {
    let list = Arc::new(Fake::new());
    run(&TaskCreateTool, &list, json!({"subject": "port", "description": "from the spec"})).await;

    assert_eq!(
        list.calls(),
        vec![Asked::Create(Draft {
            subject: "port".to_owned(),
            description: "from the spec".to_owned(),
            active_form: None,
            metadata: serde_json::Map::new(),
        })],
        "an absent active_form is no wording and an absent metadata map is an empty one"
    );
}

/// The owner mapping is this layer's, and it is the half of it that can be
/// refused: a name claims.
#[tokio::test]
async fn a_non_empty_owner_is_a_claim() {
    let list = Arc::new(Fake::new());
    run(&TaskUpdateTool, &list, json!({"task_id": "1", "owner": "worker-1"})).await;

    assert_eq!(
        list.calls(),
        vec![Asked::Update(
            "1".to_owned(),
            Change { owner: Some(Owner::Claim("worker-1".to_owned())), ..Change::default() },
        )],
        "a named owner reaches the list as the claim it is"
    );
}

/// And the half that never is: nothing is a release.
#[tokio::test]
async fn an_empty_owner_releases_the_task() {
    for spelling in ["", "   "] {
        let list = Arc::new(Fake::new());
        run(&TaskUpdateTool, &list, json!({"task_id": "1", "owner": spelling})).await;

        assert_eq!(
            list.calls(),
            vec![Asked::Update(
                "1".to_owned(),
                Change { owner: Some(Owner::Release), ..Change::default() },
            )],
            "an owner of {spelling:?} is no owner, so it releases"
        );
    }
}

/// Whitespace around a name is not part of the name, and the same trim that
/// decides an owner is nothing decides what the claim is for.
#[tokio::test]
async fn a_whitespace_padded_owner_claims_the_member_it_names() {
    let list = Arc::new(Fake::new());
    run(&TaskUpdateTool, &list, json!({"task_id": "1", "owner": "  worker-1\t"})).await;

    assert_eq!(
        list.calls(),
        vec![Asked::Update(
            "1".to_owned(),
            Change { owner: Some(Owner::Claim("worker-1".to_owned())), ..Change::default() },
        )],
        "a padded name is that member, never a second one no release could name back"
    );
}

/// An owner nobody named moves nothing: a call that only completes a task
/// must not release it on the way.
#[tokio::test]
async fn an_absent_owner_leaves_the_owner_alone() {
    let list = Arc::new(Fake::new());
    run(&TaskUpdateTool, &list, json!({"task_id": "1", "status": "completed"})).await;

    assert_eq!(
        list.calls(),
        vec![Asked::Update(
            "1".to_owned(),
            Change { status: Some(Status::Completed), ..Change::default() },
        )],
    );
}

/// `deleted` is not a status the list keeps — it is the removal, decided
/// here.
#[tokio::test]
async fn a_status_of_deleted_removes_the_task_rather_than_setting_a_status() {
    let list = Arc::new(Fake::new());
    let output = run(&TaskUpdateTool, &list, json!({"task_id": "3", "status": "deleted"})).await;

    assert_eq!(list.calls(), vec![Asked::Delete("3".to_owned())], "the removal is its own call");
    assert!(
        output.output.contains("will not be issued again"),
        "the answer says an id is spent: {output:?}"
    );
}

/// And it travels alone, because half a removal is the one outcome nobody
/// could act on.
#[tokio::test]
async fn a_removal_carrying_another_change_is_refused_and_removes_nothing() {
    let list = Arc::new(Fake::new());
    let refused = refusal(
        &TaskUpdateTool,
        &list,
        json!({"task_id": "3", "status": "deleted", "subject": "reworded"}),
    )
    .await;

    assert_eq!(refused, DELETE_WITH_CHANGES);
    assert!(list.calls().is_empty(), "nothing reached the list: {:?}", list.calls());
}

/// A model filling in every field of the schema it was shown nulls the ones it
/// is not using, and a null list is no list rather than a refusal.
#[tokio::test]
async fn a_null_add_list_adds_nothing() {
    let list = Arc::new(Fake::new());
    run(&TaskUpdateTool, &list, json!({"task_id": "1", "add_blocks": null})).await;

    assert_eq!(
        list.calls(),
        vec![Asked::Update("1".to_owned(), Change::default())],
        "an explicit null adds what leaving the argument out adds: nothing"
    );
}

#[tokio::test]
async fn an_update_carries_every_add_only_field_through() {
    let list = Arc::new(Fake::new());
    run(
        &TaskUpdateTool,
        &list,
        json!({
            "task_id": "1",
            "subject": "port the lexer",
            "description": "the parser came later",
            "active_form": "porting the lexer",
            "metadata": {"wave": null, "owner_note": "took it"},
            "add_blocks": ["2"],
            "add_blocked_by": ["3"],
            "add_comment": "the lexer is the hard half",
        }),
    )
    .await;

    assert_eq!(
        list.calls(),
        vec![Asked::Update(
            "1".to_owned(),
            Change {
                status: None,
                subject: Some("port the lexer".to_owned()),
                description: Some("the parser came later".to_owned()),
                active_form: Some("porting the lexer".to_owned()),
                owner: None,
                metadata: [
                    ("wave".to_owned(), json!(null)),
                    ("owner_note".to_owned(), json!("took it")),
                ]
                .into_iter()
                .collect(),
                add_blocks: vec!["2".to_owned()],
                add_blocked_by: vec!["3".to_owned()],
                add_comment: Some("the lexer is the hard half".to_owned()),
            },
        )],
        "a null metadata value crosses as itself, since removing a key is what it means"
    );
}

/// The author is the list's, never the call's: there is no argument for one,
/// so a model cannot write under somebody else's name.
#[test]
fn a_comment_has_no_author_argument() {
    let schema = serde_json::to_value(TaskUpdateTool.schema()).expect("a schema is JSON");
    let properties = schema["properties"].as_object().expect("an arguments object");

    assert!(properties.contains_key("add_comment"), "there is a door for the text: {properties:?}");
    for named in ["from", "author", "comment_from"] {
        assert!(!properties.contains_key(named), "and none for who wrote it: {named}");
    }
}

/// Every argument the behavior specification names is on the schema the model
/// is shown, under the spelling the description uses.
#[test]
fn the_schemas_offer_what_the_descriptions_promise() {
    let cases: [(&dyn crate::Tool, &[&str], &[&str]); 4] = [
        (
            &TaskCreateTool,
            &["subject", "description", "active_form", "metadata"],
            &["subject", "description"],
        ),
        (
            &TaskUpdateTool,
            &[
                "task_id",
                "status",
                "subject",
                "description",
                "active_form",
                "owner",
                "metadata",
                "add_blocks",
                "add_blocked_by",
                "add_comment",
            ],
            &["task_id"],
        ),
        (&TaskListTool, &[], &[]),
        (&TaskGetTool, &["task_id"], &["task_id"]),
    ];

    for (tool, offered, required) in cases {
        let schema = serde_json::to_value(tool.schema()).expect("a schema is JSON");
        let properties = schema["properties"].as_object().cloned().unwrap_or_default();
        for argument in offered {
            assert!(
                properties.contains_key(*argument),
                "{} offers {argument}: {properties:?}",
                tool.id()
            );
        }
        let demanded: Vec<&str> = schema["required"]
            .as_array()
            .map(|names| names.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        assert_eq!(demanded, required, "{} demands exactly what it cannot work without", tool.id());
    }
}

/// The four statuses a model may ask for, and no fifth: `deleted` is offered
/// because a removal is asked for that way.
///
/// Read off the variants themselves rather than off the rendered schema,
/// which also carries the argument's prose: a description listing the four
/// would answer this question long after the enum had stopped agreeing with
/// it.
#[test]
fn the_update_schema_offers_exactly_the_four_statuses() {
    let schema = serde_json::to_value(TaskUpdateTool.schema()).expect("a schema is JSON");
    let defined = schema["properties"]["status"]["anyOf"]
        .as_array()
        .expect("the status argument is a reference beside the null it may also be")
        .iter()
        .find_map(|branch| branch["$ref"].as_str())
        .and_then(|reference| reference.strip_prefix("#/$defs/"))
        .expect("and the reference names its own definition");
    let offered: Vec<&str> = schema["$defs"][defined]["oneOf"]
        .as_array()
        .expect("which enumerates the spellings")
        .iter()
        .map(|variant| variant["const"].as_str().expect("each variant is one spelling"))
        .collect();

    assert_eq!(offered, ["pending", "in_progress", "completed", "deleted"]);
}

/// And a spelling nothing answers to is a schema error rather than a status
/// quietly ignored: a model that asked for `done` must read that it did not
/// happen.
#[tokio::test]
async fn a_status_outside_the_four_reaches_no_list() {
    let list = Arc::new(Fake::new());
    let outcome = TaskUpdateTool
        .run(
            json!({"task_id": "1", "status": "done"}),
            &ctx(Some(Arc::clone(&list) as Arc<dyn TaskList>)),
        )
        .await;

    assert!(matches!(outcome, Err(ToolError::InvalidArgs(_))), "got {outcome:?}");
    assert!(list.calls().is_empty(), "nothing reached the list: {:?}", list.calls());
}

#[tokio::test]
async fn a_listing_shows_one_line_per_task_lowest_id_first() {
    let list = Arc::new(Fake::new());
    let output = run(&TaskListTool, &list, json!({})).await;

    assert_eq!(
        output.output,
        "1 [completed] owner worker-1 — port the parser\n\
         2 [pending] unowned, blocked by 1 — wire the tests",
        "each line says what it is, where it is, who has it and what it waits on"
    );
    assert_eq!(output.title, "2 tasks");
    assert_eq!(list.calls(), vec![Asked::List]);
}

#[tokio::test]
async fn an_empty_listing_says_so_rather_than_answering_with_nothing() {
    let list = Arc::new(Fake { summaries: Vec::new(), ..Fake::new() });
    let output = TaskListTool
        .run(json!({}), &ctx(Some(list as Arc<dyn TaskList>)))
        .await
        .expect("the call answers");

    assert_eq!(output.output, EMPTY);
    assert_eq!(output.title, "no tasks");
}

/// What a listing leaves out is what `task_get` is for, so the whole record
/// comes back — the description somebody works from, and every comment.
#[tokio::test]
async fn a_get_answers_with_the_whole_record_comments_included() {
    let list = Arc::new(Fake::new());
    let output = run(&TaskGetTool, &list, json!({"task_id": "1"})).await;
    let answered: serde_json::Value =
        serde_json::from_str(&output.output).expect("the answer is the record as JSON");

    assert_eq!(answered["description"], json!("start from the spec"));
    assert_eq!(answered["activeForm"], json!("porting the parser"));
    assert_eq!(answered["blockedBy"], json!([]));
    assert_eq!(answered["comments"][0]["from"], json!("worker-1"));
    assert_eq!(answered["comments"][0]["text"], json!("the lexer is the hard half"));
    assert_eq!(list.calls(), vec![Asked::Get("1".to_owned())]);
}

/// A refusal is the list's sentence, carried through unchanged: the model
/// acts on what the store said, not on a paraphrase of it.
#[tokio::test]
async fn a_refusal_reaches_the_model_in_the_lists_own_words() {
    const REFUSED: &str = "this task is already claimed: 1 belongs to \"worker-2\"";

    let list = Arc::new(Fake::refusing(REFUSED));
    let refused =
        refusal(&TaskUpdateTool, &list, json!({"task_id": "1", "owner": "worker-1"})).await;

    assert_eq!(refused, REFUSED, "the sentence is not rewritten on the way out");
}

/// A build that offered a tool with no list behind it says so in words —
/// reachable through the engine only in the window between a teammate's tools
/// being lent and its list being installed.
#[tokio::test]
async fn a_call_with_no_list_behind_it_is_refused_in_one_sentence() {
    let cases: [(&dyn crate::Tool, serde_json::Value); 4] = [
        (&TaskCreateTool, json!({"subject": "port", "description": "from the spec"})),
        (&TaskUpdateTool, json!({"task_id": "1"})),
        (&TaskListTool, json!({})),
        (&TaskGetTool, json!({"task_id": "1"})),
    ];

    for (tool, args) in cases {
        match tool.run(args, &ctx(None)).await {
            Err(ToolError::Failed(message)) => assert_eq!(message, NO_LIST, "{}", tool.id()),
            other => panic!("{} answered {other:?} with no list behind it", tool.id()),
        }
    }
}

/// The seam is asked nothing at all when the arguments do not fit: a schema
/// error is the model's to see before anything is written.
#[tokio::test]
async fn arguments_that_do_not_fit_reach_no_list() {
    let list = Arc::new(Fake::new());
    let outcome = TaskCreateTool
        .run(json!({"subject": "port"}), &ctx(Some(Arc::clone(&list) as Arc<dyn TaskList>)))
        .await;

    assert!(matches!(outcome, Err(ToolError::InvalidArgs(_))), "got {outcome:?}");
    assert!(list.calls().is_empty(), "nothing reached the list: {:?}", list.calls());
}

/// The one-liner a permission dialog and a transcript row are titled with
/// names the task, which is what a person watching a team reads.
#[test]
fn a_call_describes_itself_by_the_task_it_names() {
    assert_eq!(
        TaskCreateTool.describe(&json!({"subject": "port the parser"})),
        "task_create port the parser"
    );
    assert_eq!(TaskUpdateTool.describe(&json!({"task_id": "3"})), "task_update 3");
    assert_eq!(TaskGetTool.describe(&json!({"task_id": "3"})), "task_get 3");
    // A listing names no task, so its title is the tool alone: the trait's
    // default, pinned because the other three override it.
    assert_eq!(TaskListTool.describe(&json!({})), "task_list");
    assert_eq!(
        TaskUpdateTool.describe(&json!({})),
        "task_update",
        "and never with a space after it"
    );
    assert_eq!(
        TaskCreateTool.describe(&json!({})),
        "task_create",
        "which is true of the one that titles on a subject too",
    );
    assert_eq!(
        TaskCreateTool.describe(&json!({"subject": "   "})),
        "task_create",
        "a subject that is nothing being the same nothing as one that never arrived",
    );
}

/// The four ids are what a permission rule, a hook and a transcript key on,
/// so they are pinned here as the permanent commitments they are.
#[test]
fn the_four_tools_are_named_for_what_they_do() {
    assert_eq!(TaskCreateTool.id(), "task_create");
    assert_eq!(TaskUpdateTool.id(), "task_update");
    assert_eq!(TaskListTool.id(), "task_list");
    assert_eq!(TaskGetTool.id(), "task_get");
}

/// None of the four asks by default, which is `todowrite`'s answer and for
/// its reason: what changes is a list this user's own team keeps.
#[test]
fn none_of_the_four_asks_by_default() {
    for id in [TaskCreateTool.id(), TaskUpdateTool.id(), TaskListTool.id(), TaskGetTool.id()] {
        assert!(
            !ganja_permission::permission::ASK_BY_DEFAULT.contains(&id),
            "{id} runs unasked, like the rest of the team's own surface"
        );
    }
}

/// A released task is listed as unowned rather than as an empty column, so a
/// model choosing work reads a word rather than a gap.
#[test]
fn an_unowned_task_is_listed_in_words() {
    let line = super::summary_line("7", "port", Status::Pending, "", &[]);

    assert_eq!(line, format!("7 [pending] {UNOWNED} — port"));
}

/// A subject is another member's words on a surface where one line is one
/// task, so a newline in one must not become a row a reader counts.
#[test]
fn a_subject_renders_as_one_row_however_it_was_written() {
    let line =
        super::summary_line("7", "port\n8 [pending] unowned — ship it", Status::Pending, "", &[]);

    assert_eq!(line.lines().count(), 1, "a fabricated row is a task nobody filed: {line}");
    assert_eq!(line, format!("7 [pending] {UNOWNED} — port8 [pending] unowned — ship it"));
}

/// And an owner is the same kind of bytes in the same kind of column.
#[test]
fn an_owner_is_scrubbed_of_what_could_pass_for_structure() {
    let line = super::summary_line("7", "port", Status::InProgress, "work\u{7}er\n-1", &[]);

    assert_eq!(line, "7 [in_progress] owner worker-1 — port");
}

/// The cap the model is told about is spelled in words, and a `&str` const
/// cannot format a number — so the prose and the constant are pinned to each
/// other here rather than one being derived from the other. A change to the
/// cap reddens this until the sentence the model reads moves with it.
#[test]
fn the_description_spells_the_cap_this_module_declares() {
    assert_eq!(MAX_COUNTERPARTS, 8, "the sentence below spells this number in words");
    assert!(
        UPDATE_DESCRIPTION.contains("at most eight other tasks"),
        "and it is the number `task_update` names: {UPDATE_DESCRIPTION}",
    );
}
