//! Spec: upstream packages/opencode/src/tool/plan.ts
//!
//! The plan-exit tool: the plan agent asks whether its finished plan should
//! pass to the build agent.
//!
//! # What is here and what is not
//!
//! Switching agents is the engine's vocabulary, not a tool's, so the request
//! crosses [`Switcher`] as intent recorded now and applied at a later engine
//! entry. What stays here is the tool's own half: the empty argument shape, the
//! fixed question the person reads, and the approval sentence the model reads
//! back. The engine wires the seam only when its registry holds a build agent,
//! so [`ToolCtx::switch`] being present is the whole statement that switching
//! is possible, just as [`ToolCtx::spawn`] being present says delegation is.
//!
//! Ganja does not copy the model onto a synthetic message as upstream does;
//! the engine's adoption rule keeps the active model when build prefers none.
//!
//! [`ToolCtx::spawn`]: crate::ToolCtx::spawn
//! [`ToolCtx::switch`]: crate::ToolCtx::switch

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    Tool, ToolCtx, ToolError, ToolOutput,
    question::{self, Choice, Prompt},
};

/// The tool id, which is also the permission key. Both are a permanent
/// commitment: a rule stored under this name has to keep meaning what it meant.
pub const ID: &str = "plan_exit";

/// What the model is told the tool is for, carrying upstream
/// `packages/opencode/src/tool/plan-exit.txt` with one plan-is-prose deviation:
/// `After you have written a complete plan to the plan file` becomes `After
/// you have presented the complete plan`, because ganja has no plan file — a
/// plan is prose in the transcript, extending the standing `agent.rs`
/// deviation here.
pub const DESCRIPTION: &str = include_str!("plan_exit.txt");

/// What the person is asked, with the plan-is-prose deviation at the sentence
/// itself: upstream interpolates the plan file path before `is complete`, while
/// ganja's completed plan is the prose already present in the transcript.
const QUESTION: &str =
    "The plan is complete. Would you like to switch to the build agent and start implementing?";
const HEADER: &str = "Build Agent";
const YES_LABEL: &str = "Yes";
const YES_DESCRIPTION: &str = "Switch to build agent and start implementing the plan";
const NO_LABEL: &str = "No";
const NO_DESCRIPTION: &str = "Stay with plan agent to continue refining the plan";

/// What a call reads when the engine offered the tool without a build agent
/// behind it. Not reachable through the engine, which wires the seam only when
/// the registry holds that agent.
const NO_BUILD_AGENT: &str = "this session has no build agent to switch to";

const TITLE: &str = "Switching to build agent";
const OUTPUT: &str = "User approved switching to build agent. Wait for further instructions.";

/// What the model passes to `plan_exit`: upstream's empty `Schema.Struct({})`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {}

/// Records that this session should enter the build agent.
///
/// A seam like [`question::Asker`] and [`crate::task::Subagents`], because
/// switching agents is engine vocabulary a tool may not name. It is synchronous
/// and infallible because it only records intent and waits for nothing: the
/// engine announces the switch at the turn boundary and applies it at its next
/// entry. The engine wires one only when its registry holds a build agent, so
/// presence is ability.
pub trait Switcher: Send + Sync + std::fmt::Debug {
    /// Records the intent that the next engine entry should use the build
    /// agent.
    fn switch_to_build(&self);
}

/// Assembles the one fixed question upstream presents.
fn prompt() -> Prompt {
    Prompt {
        question: QUESTION.to_owned(),
        header: HEADER.to_owned(),
        options: vec![
            Choice {
                label: YES_LABEL.to_owned(),
                description: YES_DESCRIPTION.to_owned(),
            },
            Choice {
                label: NO_LABEL.to_owned(),
                description: NO_DESCRIPTION.to_owned(),
            },
        ],
        multiple: None,
    }
}

/// Asks whether a completed plan should pass to the build agent.
#[derive(Debug, Default)]
pub struct PlanExitTool;

#[async_trait]
impl Tool for PlanExitTool {
    fn id(&self) -> &str {
        ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let _: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

        let answers = match ctx.ask.as_ref() {
            None => return Err(ToolError::Failed(question::DISMISSED.to_owned())),
            Some(asker) => asker.ask(vec![prompt()]).await,
        };
        let answers = match answers {
            Ok(answers) => answers,
            Err(question::Unanswered::Dismissed) => {
                return Err(ToolError::Failed(question::DISMISSED.to_owned()));
            }
            Err(question::Unanswered::Cancelled) => return Err(ToolError::Cancelled),
        };

        if answers
            .first()
            .and_then(|answer| answer.first())
            .is_some_and(|answer| answer == NO_LABEL)
        {
            return Err(ToolError::Failed(question::DISMISSED.to_owned()));
        }

        let Some(switcher) = ctx.switch.as_ref() else {
            return Err(ToolError::Failed(NO_BUILD_AGENT.to_owned()));
        };
        switcher.switch_to_build();

        Ok(ToolOutput {
            title: TITLE.to_owned(),
            output: OUTPUT.to_owned(),
            metadata: serde_json::json!({}),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;

    use super::{
        HEADER, NO_BUILD_AGENT, NO_DESCRIPTION, NO_LABEL, OUTPUT, PlanExitTool, QUESTION, Switcher,
        TITLE, YES_DESCRIPTION, YES_LABEL,
    };
    use crate::{
        Credentials, FileTimes, Tool, ToolCtx, ToolError,
        question::{self, Answer, Asker, Choice, Prompt},
    };

    /// An asker that answers whatever it was built with, and records what it
    /// was asked.
    #[derive(Debug)]
    struct Scripted {
        reply: Result<Vec<Answer>, question::Unanswered>,
        seen: Mutex<Vec<Prompt>>,
    }

    impl Scripted {
        fn new(reply: Result<Vec<Answer>, question::Unanswered>) -> Arc<Self> {
            Arc::new(Self {
                reply,
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Asker for Scripted {
        async fn ask(&self, questions: Vec<Prompt>) -> Result<Vec<Answer>, question::Unanswered> {
            *self.seen.lock().expect("the record is never poisoned") = questions;
            self.reply.clone()
        }
    }

    /// A switcher whose count makes both an omitted call and a duplicate call
    /// observable.
    #[derive(Debug, Default)]
    struct RecordingSwitcher {
        calls: AtomicUsize,
    }

    impl RecordingSwitcher {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Switcher for RecordingSwitcher {
        fn switch_to_build(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn ctx(asker: Option<Arc<dyn Asker>>, switcher: Option<Arc<dyn Switcher>>) -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            cancel: tokio_util::sync::CancellationToken::new(),
            call_id: "call_1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: Credentials::Unguarded,
            spawn: None,
            ask: asker,
            switch: switcher,
        }
    }

    #[tokio::test]
    async fn a_yes_answer_switches_to_build_and_tells_the_model_to_wait() {
        let asker = Scripted::new(Ok(vec![vec![YES_LABEL.to_owned()]]));
        let switcher = RecordingSwitcher::new();
        let output = PlanExitTool
            .run(
                serde_json::json!({}),
                &ctx(Some(asker.clone()), Some(switcher.clone())),
            )
            .await
            .expect("an approved switch completes");

        assert_eq!(output.title, TITLE);
        assert_eq!(output.output, OUTPUT);
        assert_eq!(output.metadata, serde_json::json!({}));
        assert_eq!(switcher.calls(), 1);
        assert_eq!(
            asker
                .seen
                .lock()
                .expect("the record is never poisoned")
                .as_slice(),
            &[Prompt {
                question: QUESTION.to_owned(),
                header: HEADER.to_owned(),
                options: vec![
                    Choice {
                        label: YES_LABEL.to_owned(),
                        description: YES_DESCRIPTION.to_owned(),
                    },
                    Choice {
                        label: NO_LABEL.to_owned(),
                        description: NO_DESCRIPTION.to_owned(),
                    },
                ],
                multiple: None,
            }]
        );
    }

    #[tokio::test]
    async fn a_no_answer_reads_as_the_dismissal_sentence_and_switches_nothing() {
        let asker = Scripted::new(Ok(vec![vec![NO_LABEL.to_owned()]]));
        let switcher = RecordingSwitcher::new();
        let error = PlanExitTool
            .run(
                serde_json::json!({}),
                &ctx(Some(asker), Some(switcher.clone())),
            )
            .await
            .expect_err("a declined switch fails the call");

        assert!(matches!(error, ToolError::Failed(_)));
        assert_eq!(error.to_string(), question::DISMISSED);
        assert_eq!(switcher.calls(), 0);
    }

    #[tokio::test]
    async fn a_dismissed_dialog_reads_the_same_as_a_no() {
        let asker = Scripted::new(Err(question::Unanswered::Dismissed));
        let switcher = RecordingSwitcher::new();
        let error = PlanExitTool
            .run(
                serde_json::json!({}),
                &ctx(Some(asker), Some(switcher.clone())),
            )
            .await
            .expect_err("a dismissed switch fails the call");

        assert!(matches!(error, ToolError::Failed(_)));
        assert_eq!(error.to_string(), question::DISMISSED);
        assert_eq!(switcher.calls(), 0);
    }

    #[tokio::test]
    async fn an_answer_that_is_not_no_still_switches() {
        let asker = Scripted::new(Ok(vec![vec!["Sure, let's go".to_owned()]]));
        let switcher = RecordingSwitcher::new();
        PlanExitTool
            .run(
                serde_json::json!({}),
                &ctx(Some(asker), Some(switcher.clone())),
            )
            .await
            .expect("only a literal No declines the switch");

        assert_eq!(switcher.calls(), 1);
    }

    #[tokio::test]
    async fn a_cancelled_question_is_a_cancelled_call_and_not_failure_text() {
        let asker = Scripted::new(Err(question::Unanswered::Cancelled));
        let switcher = RecordingSwitcher::new();
        let error = PlanExitTool
            .run(
                serde_json::json!({}),
                &ctx(Some(asker), Some(switcher.clone())),
            )
            .await
            .expect_err("a cancelled question fails the call");

        assert!(matches!(error, ToolError::Cancelled), "{error:?}");
        assert_eq!(switcher.calls(), 0);
    }

    #[tokio::test]
    async fn a_build_with_nobody_to_ask_says_so_in_the_words_a_dismissal_uses() {
        let switcher = RecordingSwitcher::new();
        let error = PlanExitTool
            .run(serde_json::json!({}), &ctx(None, Some(switcher.clone())))
            .await
            .expect_err("there is nobody to answer");

        assert_eq!(error.to_string(), question::DISMISSED);
        assert_eq!(switcher.calls(), 0);
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

    #[test]
    fn the_schema_asks_for_nothing_at_all() {
        let schema = serde_json::to_value(PlanExitTool.schema()).expect("a schema is JSON");
        let required = schema.get("required").and_then(serde_json::Value::as_array);
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object);

        assert!(required.is_none_or(|required| required.is_empty()));
        assert!(properties.is_none_or(|properties| properties.is_empty()));
    }
}
