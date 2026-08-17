//! The question tool: the model asks the person something and reads the answer.
//!
//! Spec: upstream `packages/opencode/src/tool/question.ts`, and the service it
//! calls, `packages/opencode/src/question/index.ts`. A call publishes one
//! request, blocks until somebody answers or dismisses it, and turns what came
//! back into the sentence the model reads next.
//!
//! # What is here and what is not
//!
//! Publishing an event and holding a turn open until a reply arrives is the
//! engine's vocabulary, not a tool's, so the waiting is reached through
//! [`Asker`] — questions in, [`Answer`]s or an [`Unanswered`] out, all of it
//! strings. What stays here is the tool's own half: the argument shape, the
//! description the model reads, and the bytes it reads back. The engine's
//! implementation lives in its `session` module beside the permission wait it
//! is built like.
//!
//! That seam is also where "nobody is watching" is stated: [`ToolCtx::ask`] is
//! [`None`] on a turn with no one to ask, and a call then fails with the same
//! sentence a dismissal produces. It is **not** the guard that keeps a headless
//! run safe — that is a standing permission rule refusing `question` at every
//! pattern, installed by whoever built the engine — but a build that registered
//! the tool and wired no asker must still answer the model in words rather than
//! panic.
//!
//! [`ToolCtx::ask`]: crate::ToolCtx::ask

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Tool, ToolCtx, ToolError, ToolOutput};

/// The tool id, which is also the permission key. Both are a permanent
/// commitment: a rule stored under this name has to keep meaning what it meant.
pub const ID: &str = "question";

/// What the model is told the tool is for, ported verbatim from upstream
/// `packages/opencode/src/tool/question.txt` (MIT; see `THIRD_PARTY_NOTICES.md`).
pub const DESCRIPTION: &str = include_str!("question.txt");

/// What the model reads when its question was dismissed rather than answered.
///
/// Upstream's `RejectedError` message (`question/index.ts`), which its tool
/// surfaces as the call's failure. It is a sentence rather than a code because
/// it is read by a model deciding what to do next.
pub const DISMISSED: &str = "The user dismissed this question";

/// One choice a question offers.
///
/// Spec: upstream `Question.Option`. The doc comments are the schema the model
/// is shown — `schemars` copies them into the generated JSON schema — so they
/// are upstream's annotation text rather than a description of the field.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct Choice {
    /// Display text (1-5 words, concise)
    pub label: String,
    /// Explanation of choice
    pub description: String,
}

/// One question, as the model asks it.
///
/// Spec: upstream `Question.Prompt` — the `Info` a frontend is shown **minus**
/// `custom`, which is the engine's field to fill and not the model's to claim.
///
/// **This shape is declared twice**, here and as `ganja-protocol`'s
/// `QuestionInfo`, because this crate may not depend on the protocol: a tool
/// answers to the rules and the filesystem, and a wire type is neither. The two
/// copies are held together by a round-trip pin in `ganja-core`, the one crate
/// that sees both — it destructures each exhaustively into the other and
/// compares serde representations, so a field added here fails to compile there
/// until the protocol moves too, and a rename or a default attribute that
/// drifts reddens rather than passing quietly.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct Prompt {
    /// Complete question
    pub question: String,
    /// Very short label (max 30 chars)
    pub header: String,
    /// Available choices
    pub options: Vec<Choice>,
    /// Allow selecting multiple choices
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
}

/// What the model passes to `question`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// Questions to ask
    questions: Vec<Prompt>,
}

/// One question's answer: the labels the person picked, or typed.
///
/// A list even for a single-choice question, which is upstream's shape:
/// `multiple` is about what the dialog permits, not about what an answer looks
/// like. An empty list is a question the person skipped, and the model is told
/// so by name.
pub type Answer = Vec<String>;

/// Why a question came back without answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unanswered {
    /// The person dismissed the question, or nothing was there to ask.
    ///
    /// Both become the same sentence on purpose: what the model can act on is
    /// "this question will not be answered", and a second wording for
    /// "there was no dialog" would be a fact about the deployment rather than
    /// about the conversation.
    Dismissed,
    /// The turn was cancelled while the question was open.
    Cancelled,
}

/// Asks the person one call's questions and waits for the answer.
///
/// Deliberately says nothing about *how*: an event stream, a pending slot and a
/// turn that blocks are the engine's vocabulary, and a tool that named them
/// would be a tool the engine cannot be assembled without. What crosses is a
/// request of strings and an answer of strings.
///
/// [`std::fmt::Debug`] is required because [`ToolCtx`] derives it, and an
/// implementation is expected to render where the call sits rather than the
/// machinery behind it.
#[async_trait]
pub trait Asker: Send + Sync + std::fmt::Debug {
    /// Publishes `questions` and blocks until they are answered, dismissed, or
    /// the turn is cancelled.
    ///
    /// The answers come back **positionally** — one per question, in the order
    /// they were asked — because that is how the model reads them back.
    async fn ask(&self, questions: Vec<Prompt>) -> Result<Vec<Answer>, Unanswered>;
}

/// The tool the model calls to ask the person something.
#[derive(Debug, Default)]
pub struct QuestionTool;

#[async_trait]
impl Tool for QuestionTool {
    fn id(&self) -> &str {
        ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    /// Upstream titles a *finished* call `Asked N questions`; a dialog and a
    /// transcript row both need something before the answer exists, and the
    /// first question's header is what the person is about to be shown.
    fn describe(&self, args: &serde_json::Value) -> String {
        let Ok(args) = serde_json::from_value::<Args>(args.clone()) else {
            return ID.to_owned();
        };

        match args.questions.split_first() {
            None => ID.to_owned(),
            Some((first, [])) => format!("ask {}", first.header),
            Some((first, rest)) => format!("ask {} (+{} more)", first.header, rest.len()),
        }
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        // Upstream's schema permits an empty list and its formatter then
        // produces `User has answered your questions: .`, a sentence about
        // nothing. Refused here instead, in the words the model retries on: a
        // call that asks nothing cannot be answered, so there is no dialog to
        // open and nothing to wait for (deviation: a-question-that-asks-nothing-is-refused).
        if args.questions.is_empty() {
            return Err(ToolError::Failed(
                "no questions were given; ask at least one".to_owned(),
            ));
        }

        let asked = args.questions.len();
        let answers = match ctx.ask.as_ref() {
            None => Err(Unanswered::Dismissed),
            Some(asker) => asker.ask(args.questions.clone()).await,
        };

        let answers = match answers {
            Ok(answers) => answers,
            // A dismissal is information the model reads and acts on, not a
            // reason to stop the turn — the loop's rule, and upstream's shape,
            // whose tool surfaces the rejection as the call's failure.
            Err(Unanswered::Dismissed) => return Err(ToolError::Failed(DISMISSED.to_owned())),
            Err(Unanswered::Cancelled) => return Err(ToolError::Cancelled),
        };

        Ok(ToolOutput {
            title: format!("Asked {asked} question{}", if asked > 1 { "s" } else { "" }),
            output: format!(
                "User has answered your questions: {}. You can now continue with the user's answers in mind.",
                formatted(&args.questions, &answers)
            ),
            metadata: serde_json::json!({ "answers": answers }),
        })
    }
}

/// Upstream's answer rendering: each question quoted beside what was picked,
/// comma-joined, with `Unanswered` where nothing was.
///
/// Indexing is positional and tolerant of a short list, exactly as upstream's
/// `answers[i]?.length` is: an answer that never arrived reads the same as one
/// that arrived empty, because to the model both mean the question went
/// unanswered.
fn formatted(questions: &[Prompt], answers: &[Answer]) -> String {
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let answer = match answers.get(index) {
                Some(answer) if !answer.is_empty() => answer.join(", "),
                _ => "Unanswered".to_owned(),
            };

            format!("\"{}\"=\"{answer}\"", question.question)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::{Answer, Asker, Choice, DISMISSED, Prompt, QuestionTool, Unanswered};
    use crate::{Credentials, FileTimes, Tool, ToolCtx, ToolError};

    /// An asker that answers whatever it was built with, and records what it
    /// was asked.
    #[derive(Debug)]
    struct Scripted {
        reply: Result<Vec<Answer>, Unanswered>,
        seen: Mutex<Vec<Prompt>>,
    }

    impl Scripted {
        fn new(reply: Result<Vec<Answer>, Unanswered>) -> Arc<Self> {
            Arc::new(Self {
                reply,
                seen: Mutex::new(Vec::new()),
            })
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
        ToolCtx {
            cwd: std::env::temp_dir(),
            cancel: tokio_util::sync::CancellationToken::new(),
            call_id: "call_1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: Credentials::Unguarded,
            spawn: None,
            postbox: None,
            ask: asker,
            switch: None,
            jobs: None,
        }
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
        assert_eq!(
            output.metadata,
            serde_json::json!({"answers": [["Postgres"]]})
        );
        assert_eq!(
            asker
                .seen
                .lock()
                .expect("the record is never poisoned")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_multiple_choice_answer_is_joined_the_way_upstream_joins_it() {
        let asker = Scripted::new(Ok(vec![vec!["Postgres".to_owned(), "SQLite".to_owned()]]));
        let output = QuestionTool
            .run(one(), &ctx(Some(asker)))
            .await
            .expect("an answered question completes");

        assert!(
            output
                .output
                .contains("\"Which database?\"=\"Postgres, SQLite\""),
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

        assert!(
            output.output.contains("\"Which database?\"=\"Unanswered\""),
            "{}",
            output.output
        );
    }

    /// A short answer list is upstream's `answers[i]?.length` case: the model
    /// is told the question went unanswered rather than the call failing.
    #[tokio::test]
    async fn a_question_the_reply_never_reached_reads_as_unanswered_too() {
        let asker = Scripted::new(Ok(Vec::new()));
        let output = QuestionTool
            .run(one(), &ctx(Some(asker)))
            .await
            .expect("a short reply still completes");

        assert!(
            output.output.contains("\"Which database?\"=\"Unanswered\""),
            "{}",
            output.output
        );
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
        let error = QuestionTool
            .run(one(), &ctx(None))
            .await
            .expect_err("there is nobody to answer");

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
        let required = prompt["required"]
            .as_array()
            .expect("the question shape names its required fields");

        assert!(required.contains(&serde_json::json!("question")));
        assert!(required.contains(&serde_json::json!("header")));
        assert!(required.contains(&serde_json::json!("options")));
        assert!(!required.contains(&serde_json::json!("multiple")));
    }

    /// `Choice` is a shape the model fills in, so both halves are required —
    /// a label with no explanation is not a choice anybody can weigh.
    #[test]
    fn a_choice_offers_both_a_label_and_the_reason_for_it() {
        let choice = Choice {
            label: "Postgres".to_owned(),
            description: "Relational".to_owned(),
        };

        assert_eq!(
            serde_json::to_value(&choice).expect("a choice is JSON"),
            serde_json::json!({"label": "Postgres", "description": "Relational"})
        );
    }
}
