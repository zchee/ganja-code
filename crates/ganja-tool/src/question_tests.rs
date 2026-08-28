use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{Answer, Asker, Choice, DISMISSED, Prompt, QuestionTool, Unanswered};
use crate::{Tool, ToolCtx, ToolError};

/// An asker that answers whatever it was built with, and records what it
/// was asked.
#[derive(Debug)]
struct Scripted {
    reply: Result<Vec<Answer>, Unanswered>,
    seen: Mutex<Vec<Prompt>>,
}

impl Scripted {
    fn new(reply: Result<Vec<Answer>, Unanswered>) -> Arc<Self> {
        Arc::new(Self { reply, seen: Mutex::new(Vec::new()) })
    }
}

#[async_trait]
impl Asker for Scripted {
    async fn ask(&self, questions: Vec<Prompt>) -> Result<Vec<Answer>, Unanswered> {
        *self.seen.lock().expect("the record is never poisoned") = questions;
        self.reply.clone()
    }
}

fn ctx(asker: Option<Arc<dyn Asker>>) -> ToolCtx {
    let mut ctx = ToolCtx::fixture(std::env::temp_dir());
    ctx.ask = asker;
    ctx
}

fn one() -> serde_json::Value {
    serde_json::json!({
        "questions": [{
            "question": "Which database?",
            "header": "Database",
            "options": [
                {"label": "Postgres", "description": "Relational"},
                {"label": "SQLite", "description": "A file"},
            ],
        }],
    })
}

#[tokio::test]
async fn an_answered_question_reaches_the_model_as_the_labels_that_were_picked() {
    let asker = Scripted::new(Ok(vec![vec!["Postgres".to_owned()]]));
    let output = QuestionTool
        .run(one(), &ctx(Some(asker.clone())))
        .await
        .expect("an answered question completes");

    assert_eq!(output.title, "Asked 1 question");
    assert_eq!(
        output.output,
        "User has answered your questions: \"Which database?\"=\"Postgres\". \
             You can now continue with the user's answers in mind."
    );
    assert_eq!(output.metadata, serde_json::json!({"answers": [["Postgres"]]}));
    assert_eq!(asker.seen.lock().expect("the record is never poisoned").len(), 1);
}

#[tokio::test]
async fn a_multiple_choice_answer_is_joined_the_way_upstream_joins_it() {
    let asker = Scripted::new(Ok(vec![vec!["Postgres".to_owned(), "SQLite".to_owned()]]));
    let output =
        QuestionTool.run(one(), &ctx(Some(asker))).await.expect("an answered question completes");

    assert!(
        output.output.contains("\"Which database?\"=\"Postgres, SQLite\""),
        "{}",
        output.output
    );
}

#[tokio::test]
async fn a_question_nobody_picked_anything_for_reads_as_unanswered() {
    let asker = Scripted::new(Ok(vec![Vec::new()]));
    let output = QuestionTool
        .run(one(), &ctx(Some(asker)))
        .await
        .expect("a skipped question still completes");

    assert!(output.output.contains("\"Which database?\"=\"Unanswered\""), "{}", output.output);
}

/// A short answer list is upstream's `answers[i]?.length` case: the model
/// is told the question went unanswered rather than the call failing.
#[tokio::test]
async fn a_question_the_reply_never_reached_reads_as_unanswered_too() {
    let asker = Scripted::new(Ok(Vec::new()));
    let output =
        QuestionTool.run(one(), &ctx(Some(asker))).await.expect("a short reply still completes");

    assert!(output.output.contains("\"Which database?\"=\"Unanswered\""), "{}", output.output);
}

#[tokio::test]
async fn a_dismissed_question_becomes_error_text_the_model_reads() {
    let asker = Scripted::new(Err(Unanswered::Dismissed));
    let error = QuestionTool
        .run(one(), &ctx(Some(asker)))
        .await
        .expect_err("a dismissed question fails the call");

    assert!(matches!(error, ToolError::Failed(_)));
    assert_eq!(error.to_string(), DISMISSED);
}

/// A cancel ends the turn, so it must not arrive as ordinary failure text
/// the loop would carry on from.
#[tokio::test]
async fn a_cancelled_question_is_a_cancelled_call_and_not_failure_text() {
    let asker = Scripted::new(Err(Unanswered::Cancelled));
    let error = QuestionTool
        .run(one(), &ctx(Some(asker)))
        .await
        .expect_err("a cancelled question fails the call");

    assert!(matches!(error, ToolError::Cancelled), "{error:?}");
}

/// A build that registered the tool and wired no asker answers the model
/// in words rather than panicking.
#[tokio::test]
async fn a_build_with_nobody_to_ask_says_so_in_the_words_a_dismissal_uses() {
    let error = QuestionTool.run(one(), &ctx(None)).await.expect_err("there is nobody to answer");

    assert_eq!(error.to_string(), DISMISSED);
}

#[tokio::test]
async fn a_call_that_asks_nothing_is_refused_rather_than_opening_an_empty_dialog() {
    let asker = Scripted::new(Ok(Vec::new()));
    let error = QuestionTool
        .run(serde_json::json!({"questions": []}), &ctx(Some(asker)))
        .await
        .expect_err("a call that asks nothing is refused");

    assert!(matches!(error, ToolError::Failed(_)));
    assert!(error.to_string().contains("at least one"), "{error}");
}

#[tokio::test]
async fn arguments_that_do_not_fit_the_schema_are_refused_before_anyone_is_asked() {
    let asker = Scripted::new(Ok(Vec::new()));
    let error = QuestionTool
        .run(serde_json::json!({"questions": "one"}), &ctx(Some(asker)))
        .await
        .expect_err("a malformed call is refused");

    assert!(matches!(error, ToolError::InvalidArgs(_)), "{error:?}");
}

#[test]
fn a_dialog_is_titled_by_the_first_headers_and_how_many_follow() {
    assert_eq!(QuestionTool.describe(&one()), "ask Database");

    let two = serde_json::json!({
        "questions": [
            {"question": "Which database?", "header": "Database", "options": []},
            {"question": "Which runtime?", "header": "Runtime", "options": []},
        ],
    });
    assert_eq!(QuestionTool.describe(&two), "ask Database (+1 more)");

    // Nothing to describe falls back to the tool's own name, which is what
    // a permission dialog for an unparseable call has to show.
    assert_eq!(QuestionTool.describe(&serde_json::json!({})), "question");
}

/// The generated schema is what the model is told, so the two optional
/// halves of the shape have to reach it: `multiple` optional, everything
/// else required.
#[test]
fn the_schema_asks_for_everything_but_the_multiple_flag() {
    let schema = serde_json::to_value(QuestionTool.schema()).expect("a schema is JSON");
    let prompt = &schema["$defs"]["Prompt"];
    let required =
        prompt["required"].as_array().expect("the question shape names its required fields");

    assert!(required.contains(&serde_json::json!("question")));
    assert!(required.contains(&serde_json::json!("header")));
    assert!(required.contains(&serde_json::json!("options")));
    assert!(!required.contains(&serde_json::json!("multiple")));
}

/// `Choice` is a shape the model fills in, so both halves are required —
/// a label with no explanation is not a choice anybody can weigh.
#[test]
fn a_choice_offers_both_a_label_and_the_reason_for_it() {
    let choice = Choice { label: "Postgres".to_owned(), description: "Relational".to_owned() };

    assert_eq!(
        serde_json::to_value(&choice).expect("a choice is JSON"),
        serde_json::json!({"label": "Postgres", "description": "Relational"})
    );
}
