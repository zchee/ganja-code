use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{
    ENTER, EXIT, NO_BUILD_AGENT, NO_LABEL, NO_PLAN_AGENT, PlanEnterTool, PlanExitTool, Switcher,
    YES_LABEL,
};
use crate::question::{self, Answer, Asker, Choice, Prompt};
use crate::{Tool, ToolCtx, ToolError};

/// An asker that answers whatever it was built with, and records what it
/// was asked.
#[derive(Debug)]
struct Scripted {
    reply: Result<Vec<Answer>, question::Unanswered>,
    seen: Mutex<Vec<Prompt>>,
}

impl Scripted {
    fn new(reply: Result<Vec<Answer>, question::Unanswered>) -> Arc<Self> {
        Arc::new(Self { reply, seen: Mutex::new(Vec::new()) })
    }
}

#[async_trait]
impl Asker for Scripted {
    async fn ask(&self, questions: Vec<Prompt>) -> Result<Vec<Answer>, question::Unanswered> {
        *self.seen.lock().expect("the record is never poisoned") = questions;
        self.reply.clone()
    }
}

/// A switcher whose per-direction records make an omitted call, a
/// duplicate call and a call in the *wrong* direction all observable.
#[derive(Debug, Default)]
struct RecordingSwitcher {
    builds: Mutex<usize>,
    plans: Mutex<usize>,
}

impl RecordingSwitcher {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The two counts, as `(build, plan)`.
    fn calls(&self) -> (usize, usize) {
        (
            *self.builds.lock().expect("the record is never poisoned"),
            *self.plans.lock().expect("the record is never poisoned"),
        )
    }
}

impl Switcher for RecordingSwitcher {
    fn switch_to_build(&self) {
        *self.builds.lock().expect("the record is never poisoned") += 1;
    }

    fn switch_to_plan(&self) {
        *self.plans.lock().expect("the record is never poisoned") += 1;
    }
}

fn ctx(asker: Option<Arc<dyn Asker>>, switcher: Option<Arc<dyn Switcher>>) -> ToolCtx {
    let mut ctx = ToolCtx::fixture(std::env::temp_dir());
    ctx.ask = asker;
    ctx.switch = switcher;
    ctx
}

/// The prompt a door is expected to have posed.
fn expected(door: &super::Door) -> Prompt {
    Prompt {
        question: door.question.to_owned(),
        header: door.header.to_owned(),
        options: vec![
            Choice { label: YES_LABEL.to_owned(), description: door.yes_description.to_owned() },
            Choice { label: NO_LABEL.to_owned(), description: door.no_description.to_owned() },
        ],
        multiple: None,
    }
}

#[tokio::test]
async fn a_yes_answer_switches_to_build_and_tells_the_model_to_wait() {
    let asker = Scripted::new(Ok(vec![vec![YES_LABEL.to_owned()]]));
    let switcher = RecordingSwitcher::new();
    let output = PlanExitTool
        .run(serde_json::json!({}), &ctx(Some(asker.clone()), Some(switcher.clone())))
        .await
        .expect("an approved switch completes");

    assert_eq!(output.title, EXIT.title);
    assert_eq!(output.output, EXIT.output);
    assert_eq!(output.metadata, serde_json::json!({}));
    assert_eq!(switcher.calls(), (1, 0));
    assert_eq!(
        asker.seen.lock().expect("the record is never poisoned").as_slice(),
        &[expected(&EXIT)]
    );
}

#[tokio::test]
async fn a_yes_answer_switches_to_plan_and_tells_the_model_to_wait() {
    let asker = Scripted::new(Ok(vec![vec![YES_LABEL.to_owned()]]));
    let switcher = RecordingSwitcher::new();
    let output = PlanEnterTool
        .run(serde_json::json!({}), &ctx(Some(asker.clone()), Some(switcher.clone())))
        .await
        .expect("an approved switch completes");

    assert_eq!(output.title, ENTER.title);
    assert_eq!(output.output, ENTER.output);
    assert_eq!(output.metadata, serde_json::json!({}));
    assert_eq!(switcher.calls(), (0, 1), "exactly one switch, and in the enter direction");
    assert_eq!(
        asker.seen.lock().expect("the record is never poisoned").as_slice(),
        &[expected(&ENTER)]
    );
}

/// The synthesized wording, pinned where it is read rather than where it
/// is written: these are the sentences a person sees, and D477 owns them
/// precisely because upstream published none.
#[tokio::test]
async fn the_enter_door_asks_about_researching_before_implementing() {
    let asker = Scripted::new(Ok(vec![vec![NO_LABEL.to_owned()]]));
    let switcher = RecordingSwitcher::new();
    let _ =
        PlanEnterTool.run(serde_json::json!({}), &ctx(Some(asker.clone()), Some(switcher))).await;

    let seen = asker.seen.lock().expect("the record is never poisoned");
    let asked = seen.first().expect("the tool asks before it switches");
    assert_eq!(
        asked.question,
        "Would you like to switch to the plan agent to research and design before \
             implementing?"
    );
    assert_eq!(asked.header, "Plan Agent");
    assert_eq!(
        asked.options,
        vec![
            Choice {
                label: "Yes".to_owned(),
                description: "Switch to plan agent and start planning the work".to_owned(),
            },
            Choice {
                label: "No".to_owned(),
                description: "Stay with build agent to continue implementing".to_owned(),
            },
        ]
    );
}

#[tokio::test]
async fn a_no_answer_reads_as_the_dismissal_sentence_and_switches_nothing() {
    let asker = Scripted::new(Ok(vec![vec![NO_LABEL.to_owned()]]));
    let switcher = RecordingSwitcher::new();
    let error = PlanExitTool
        .run(serde_json::json!({}), &ctx(Some(asker), Some(switcher.clone())))
        .await
        .expect_err("a declined switch fails the call");

    assert!(matches!(error, ToolError::Failed(_)));
    assert_eq!(error.to_string(), question::DISMISSED);
    assert_eq!(switcher.calls(), (0, 0));
}

#[tokio::test]
async fn a_no_answer_to_the_enter_door_keeps_the_build_agent() {
    let asker = Scripted::new(Ok(vec![vec![NO_LABEL.to_owned()]]));
    let switcher = RecordingSwitcher::new();
    let error = PlanEnterTool
        .run(serde_json::json!({}), &ctx(Some(asker), Some(switcher.clone())))
        .await
        .expect_err("a declined switch fails the call");

    assert!(matches!(error, ToolError::Failed(_)));
    assert_eq!(error.to_string(), question::DISMISSED);
    assert_eq!(switcher.calls(), (0, 0));
}

#[tokio::test]
async fn a_dismissed_dialog_reads_the_same_as_a_no() {
    let asker = Scripted::new(Err(question::Unanswered::Dismissed));
    let switcher = RecordingSwitcher::new();
    let error = PlanExitTool
        .run(serde_json::json!({}), &ctx(Some(asker), Some(switcher.clone())))
        .await
        .expect_err("a dismissed switch fails the call");

    assert!(matches!(error, ToolError::Failed(_)));
    assert_eq!(error.to_string(), question::DISMISSED);
    assert_eq!(switcher.calls(), (0, 0));
}

#[tokio::test]
async fn a_dismissed_enter_dialog_reads_the_same_as_a_no() {
    let asker = Scripted::new(Err(question::Unanswered::Dismissed));
    let switcher = RecordingSwitcher::new();
    let error = PlanEnterTool
        .run(serde_json::json!({}), &ctx(Some(asker), Some(switcher.clone())))
        .await
        .expect_err("a dismissed switch fails the call");

    assert_eq!(error.to_string(), question::DISMISSED);
    assert_eq!(switcher.calls(), (0, 0));
}

#[tokio::test]
async fn an_answer_that_is_not_no_still_switches() {
    let asker = Scripted::new(Ok(vec![vec!["Sure, let's go".to_owned()]]));
    let switcher = RecordingSwitcher::new();
    PlanExitTool
        .run(serde_json::json!({}), &ctx(Some(asker), Some(switcher.clone())))
        .await
        .expect("only a literal No declines the switch");

    assert_eq!(switcher.calls(), (1, 0));
}

#[tokio::test]
async fn an_answer_that_is_not_no_still_enters_planning() {
    let asker = Scripted::new(Ok(vec![vec!["Sure, let's plan".to_owned()]]));
    let switcher = RecordingSwitcher::new();
    PlanEnterTool
        .run(serde_json::json!({}), &ctx(Some(asker), Some(switcher.clone())))
        .await
        .expect("only a literal No declines the switch");

    assert_eq!(switcher.calls(), (0, 1));
}

#[tokio::test]
async fn a_cancelled_question_is_a_cancelled_call_and_not_failure_text() {
    let asker = Scripted::new(Err(question::Unanswered::Cancelled));
    let switcher = RecordingSwitcher::new();
    let error = PlanExitTool
        .run(serde_json::json!({}), &ctx(Some(asker), Some(switcher.clone())))
        .await
        .expect_err("a cancelled question fails the call");

    assert!(matches!(error, ToolError::Cancelled), "{error:?}");
    assert_eq!(switcher.calls(), (0, 0));
}

#[tokio::test]
async fn a_cancelled_enter_question_is_a_cancelled_call_too() {
    let asker = Scripted::new(Err(question::Unanswered::Cancelled));
    let switcher = RecordingSwitcher::new();
    let error = PlanEnterTool
        .run(serde_json::json!({}), &ctx(Some(asker), Some(switcher.clone())))
        .await
        .expect_err("a cancelled question fails the call");

    assert!(matches!(error, ToolError::Cancelled), "{error:?}");
    assert_eq!(switcher.calls(), (0, 0));
}

#[tokio::test]
async fn a_build_with_nobody_to_ask_says_so_in_the_words_a_dismissal_uses() {
    let switcher = RecordingSwitcher::new();
    let error = PlanExitTool
        .run(serde_json::json!({}), &ctx(None, Some(switcher.clone())))
        .await
        .expect_err("there is nobody to answer");

    assert_eq!(error.to_string(), question::DISMISSED);
    assert_eq!(switcher.calls(), (0, 0));
}

#[tokio::test]
async fn an_enter_with_nobody_to_ask_says_so_in_the_words_a_dismissal_uses() {
    let switcher = RecordingSwitcher::new();
    let error = PlanEnterTool
        .run(serde_json::json!({}), &ctx(None, Some(switcher.clone())))
        .await
        .expect_err("there is nobody to answer");

    assert_eq!(error.to_string(), question::DISMISSED);
    assert_eq!(switcher.calls(), (0, 0));
}

#[tokio::test]
async fn a_build_with_no_switcher_fails_in_words_rather_than_panicking() {
    let asker = Scripted::new(Ok(vec![vec![YES_LABEL.to_owned()]]));
    let error = PlanExitTool
        .run(serde_json::json!({}), &ctx(Some(asker), None))
        .await
        .expect_err("there is no build agent to switch to");

    assert!(matches!(error, ToolError::Failed(_)));
    assert!(error.to_string().contains(NO_BUILD_AGENT), "{error}");
}

#[tokio::test]
async fn an_enter_with_no_switcher_names_the_missing_plan_agent() {
    let asker = Scripted::new(Ok(vec![vec![YES_LABEL.to_owned()]]));
    let error = PlanEnterTool
        .run(serde_json::json!({}), &ctx(Some(asker), None))
        .await
        .expect_err("there is no plan agent to switch to");

    assert!(matches!(error, ToolError::Failed(_)));
    assert!(error.to_string().contains(NO_PLAN_AGENT), "{error}");
}

#[test]
fn the_schema_asks_for_nothing_at_all() {
    for schema in [PlanExitTool.schema(), PlanEnterTool.schema()] {
        let schema = serde_json::to_value(schema).expect("a schema is JSON");
        let required = schema.get("required").and_then(serde_json::Value::as_array);
        let properties = schema.get("properties").and_then(serde_json::Value::as_object);

        assert!(required.is_none_or(|required| required.is_empty()));
        assert!(properties.is_none_or(|properties| properties.is_empty()));
    }
}

/// The two doors are one another's mirror, and this is where that is
/// checked rather than assumed: same ids as their permission keys, same
/// empty arguments, and descriptions that are the two upstream files
/// rather than one file twice.
#[test]
fn each_door_answers_to_the_permission_key_it_is_ruled_by() {
    assert_eq!(PlanExitTool.id(), super::EXIT_ID);
    assert_eq!(PlanEnterTool.id(), super::ENTER_ID);
    assert_ne!(PlanExitTool.description(), PlanEnterTool.description());
    assert!(
        PlanEnterTool.description().contains("switch to plan agent"),
        "the enter description is upstream's plan-enter.txt"
    );
}
