//! The task tool's own half, with nothing behind the seam.
//!
//! `tests/task.rs` drives the whole thing — a parent turn, a real child loop,
//! one ordered script feeding both — which is what proves a subagent is a
//! conversation rather than a canned string. This file is the other half of
//! that story: what the *tool* does with an answer, whatever produced it.
//!
//! Everything here runs through a second implementation of the seam
//! (`ganja_testkit::ScriptedSubagents`), which is what makes it a seam at all:
//! no provider, no agents, no permissions, no turn — and the tool cannot tell.

use std::sync::Arc;

use ganja_core::tool::{
    Credentials, FileTimes, Tool as _, ToolCtx, ToolError,
    task::{Delegated, Delegation, Offered, TaskTool, Unanswered},
};
use ganja_testkit::ScriptedSubagents;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// What the model calls the tool with, which every case below varies only the
/// named subagent of.
fn args(subagent: &str) -> serde_json::Value {
    json!({
        "description": "find the thing",
        "prompt": "go and find the thing",
        "subagent_type": subagent,
    })
}

/// A call whose delegations are answered by `answer`, alongside the log of what
/// was asked.
fn ctx(answer: Result<Delegated, Unanswered>) -> (ToolCtx, Arc<std::sync::Mutex<Vec<Delegation>>>) {
    let (subagents, asked) = ScriptedSubagents::new(vec![answer]);

    (
        ToolCtx {
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            call_id: "call_1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: Credentials::Unguarded,
            spawn: Some(subagents),
            postbox: None,
            ask: None,
            switch: None,
            jobs: None,
        },
        asked,
    )
}

/// A finished delegation, as an implementation reports one.
fn finished() -> Delegated {
    Delegated {
        task_id: "ses_child".to_owned(),
        agent: "general".to_owned(),
        model: "some-model".to_owned(),
        text: "the thing is in src/main.rs".to_owned(),
        toolcalls: 2,
        calls: vec!["grep the thing".to_owned(), "read src/main.rs".to_owned()],
    }
}

/// The arguments the model wrote reach the seam as they were written: nothing
/// in between renames, defaults or reinterprets them.
#[tokio::test]
async fn the_models_arguments_arrive_at_the_seam_as_it_wrote_them() {
    let (ctx, asked) = ctx(Ok(finished()));

    TaskTool::new(&[])
        .run(
            json!({
                "description": "find the thing",
                "prompt": "go and find the thing",
                "subagent_type": "general",
                "task_id": "ses_earlier",
            }),
            &ctx,
        )
        .await
        .expect("a finished delegation is an answer");

    let asked = asked.lock().expect("the delegation log is never poisoned");
    let [delegation] = asked.as_slice() else {
        panic!("one call is one delegation: {asked:?}");
    };
    assert_eq!(delegation.subagent_type, "general");
    assert_eq!(delegation.prompt, "go and find the thing");
    assert_eq!(delegation.description, "find the thing");
    assert_eq!(delegation.task_id.as_deref(), Some("ses_earlier"));
}

/// The whole of what the tool does with a finished delegation: upstream's XML
/// for the model, and the metadata a frontend renders the row from.
#[tokio::test]
async fn a_finished_delegation_becomes_upstreams_xml_and_the_parts_metadata() {
    let (ctx, _asked) = ctx(Ok(finished()));

    let output = TaskTool::new(&[])
        .run(args("general"), &ctx)
        .await
        .expect("a finished delegation is an answer");

    assert_eq!(output.title, "find the thing");
    assert_eq!(
        output.output,
        "<task id=\"ses_child\" state=\"completed\">\n<task_result>\n\
         the thing is in src/main.rs\n</task_result>\n</task>"
    );
    assert_eq!(
        output.metadata,
        json!({
            "session": "ses_child",
            "agent": "general",
            "model": "some-model",
            "toolcalls": 2,
            "calls": ["grep the thing", "read src/main.rs"],
        })
    );
}

/// A name nothing answers to is information the model can act on, in upstream's
/// own words — which is the sentence a model reads and retries against.
#[tokio::test]
async fn an_unknown_subagent_is_named_back_in_upstreams_words() {
    let (ctx, _asked) = ctx(Err(Unanswered::Unknown));

    let refused = TaskTool::new(&[])
        .run(args("nonesuch"), &ctx)
        .await
        .expect_err("nothing goes by that name");

    let ToolError::Failed(message) = &refused else {
        panic!("an unknown agent is information, not an argument error: {refused:?}");
    };
    assert_eq!(
        message,
        "Unknown agent type: nonesuch is not a valid agent type"
    );
}

/// A child that could not answer is an error the parent model reads, in the
/// same XML a result arrives in so that it can tell which call it was.
#[tokio::test]
async fn a_child_that_failed_reports_why_in_upstreams_error_xml() {
    let (ctx, _asked) = ctx(Err(Unanswered::Failed {
        task_id: "ses_child".to_owned(),
        message: "no credentials".to_owned(),
    }));

    let refused = TaskTool::new(&[])
        .run(args("general"), &ctx)
        .await
        .expect_err("a child that failed produced no answer");

    let ToolError::Failed(message) = &refused else {
        panic!("a failed child is information, not an argument error: {refused:?}");
    };
    assert_eq!(
        message,
        "<task id=\"ses_child\" state=\"error\">\n<task_error>\n\
         no credentials\n</task_error>\n</task>"
    );
}

/// A cancel is the turn's outcome rather than a result to be rendered: the
/// parent's loop stops on it, and no XML is written about it.
#[tokio::test]
async fn a_cancelled_delegation_cancels_the_call() {
    let (ctx, _asked) = ctx(Err(Unanswered::Cancelled));

    let refused = TaskTool::new(&[])
        .run(args("general"), &ctx)
        .await
        .expect_err("a cancelled child answers nothing");

    assert!(matches!(refused, ToolError::Cancelled), "got {refused:?}");
}

/// A build that offered the tool with nothing behind it says so rather than
/// pretending to delegate. Not reachable through the engine, which registers
/// the tool only when it has agents to spawn.
#[tokio::test]
async fn a_call_with_nothing_to_delegate_through_says_so() {
    let ctx = ToolCtx {
        cwd: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        call_id: "call_1".to_owned(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: None,
        postbox: None,
        ask: None,
        switch: None,
        jobs: None,
    };

    let refused = TaskTool::new(&[Offered {
        name: "general".to_owned(),
        description: None,
    }])
    .run(args("general"), &ctx)
    .await
    .expect_err("there is nothing to run a subagent with");

    let ToolError::Failed(message) = &refused else {
        panic!("having nothing to delegate through is information: {refused:?}");
    };
    assert_eq!(message, "This session has no subagents to delegate to.");
}
