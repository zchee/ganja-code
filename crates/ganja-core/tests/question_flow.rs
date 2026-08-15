//! The question quad through a real turn: asked, then answered, dismissed, or
//! refused by a cancel.
//!
//! Spec: upstream `packages/opencode/src/question/index.ts` and
//! `packages/schema/src/v1/question.ts`. The invariant every test here is about
//! is the one a frontend hangs its dialog on:
//!
//! > **Every `QuestionAsked` that reaches a subscriber is followed by exactly
//! > one terminal event** — `QuestionReplied` or `QuestionRejected` — so a
//! > dialog can be retired unconditionally.
//!
//! That is the permission wait's contract restated, but it is **not** the same
//! proof: a permission's refusal is a `PermissionReplied` carrying `Reject`,
//! while a dismissal here is an event of its own with its own payload. Where
//! the permission suite counts one terminal shape, this one has to count two
//! and prove they never both arrive.
//!
//! Nothing here stores anything: an in-memory engine over
//! `Permissions::default` has no store to write to, so there is no user state
//! to redirect away from. `question` is not in the ask-by-default set — the
//! asking *is* the interaction, and gating it would put a dialog in front of a
//! dialog — so asking raises no permission request of its own, which the first
//! test asserts directly. The delegating scripts at the end still meet one,
//! because `task` asks; that dialog is let through on the way past.

use std::sync::Arc;

use futures::StreamExt as _;
use ganja_core::{
    Config, Engine,
    permission::Permissions,
    protocol::{
        Command, Event, FinishReason, PartBody, PermissionId, PermissionReply, QuestionId,
        ToolState,
    },
    provider::Provider,
    tool::{Registry, question::QuestionTool},
};
use ganja_testkit::{ScriptedProvider, drain, says, tool_call};
use serde_json::json;

/// The question every script here asks.
const QUESTION: &str = "Which database?";

/// A step that asks one question with two choices.
fn asks_one() -> Vec<ganja_core::provider::ProviderEvent> {
    tool_call(
        "question",
        json!({
            "questions": [{
                "question": QUESTION,
                "header": "Database",
                "options": [
                    {"label": "Postgres", "description": "Relational"},
                    {"label": "SQLite", "description": "One file"},
                ],
            }],
        }),
    )
}

/// An engine offering the real `question` tool over `script`.
fn engine(script: Vec<Vec<ganja_core::provider::ProviderEvent>>) -> Engine {
    let (provider, _) = ScriptedProvider::new(script);

    Engine::new(
        provider as Arc<dyn Provider>,
        "recorder-model",
        Arc::new(Registry::new(vec![Arc::new(QuestionTool)])),
        Permissions::default(),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()))
}

/// Reads until the question is asked, handing back its id and everything seen
/// on the way.
///
/// Permission dialogs met on the way are let through, because some of these
/// scripts have to get past `task` — which asks by default — before anything
/// can ask a question at all. A stream that ends first is a broken fixture and
/// panics here rather than letting a "no terminal arrived" assertion pass
/// vacuously.
async fn until_question(
    engine: &Engine,
    events: &mut futures::stream::BoxStream<'static, Event>,
) -> (QuestionId, Vec<Event>) {
    let mut seen = Vec::new();
    while let Some(event) = events.next().await {
        let asked = match &event {
            Event::QuestionAsked { id, .. } => Some(id.clone()),
            Event::PermissionRequested { id, .. } => {
                engine
                    .send(Command::ReplyPermission {
                        id: id.clone(),
                        reply: PermissionReply::Once,
                    })
                    .await
                    .expect("a permission reply is never refused");
                None
            }
            _ => None,
        };
        seen.push(event);
        if let Some(id) = asked {
            return (id, seen);
        }
    }

    panic!("the stream ended before anything was asked, saw {seen:?}");
}

/// Every terminal event for `id`, in the order it arrived — which is what
/// "exactly one" is counted over.
fn terminals(seen: &[Event], id: &QuestionId) -> Vec<String> {
    seen.iter()
        .filter_map(|event| match event {
            Event::QuestionReplied {
                id: named, answers, ..
            } if named == id => Some(format!("replied:{}", answers.len())),
            Event::QuestionRejected { id: named, .. } if named == id => Some("rejected".to_owned()),
            _ => None,
        })
        .collect()
}

/// How the `question` call ended: its output, or the error text the model
/// reads instead.
fn call_outcome(seen: &[Event]) -> Option<Result<String, String>> {
    seen.iter().rev().find_map(|event| match event {
        Event::PartUpdated { part, .. } => match &part.body {
            PartBody::Tool { tool, state, .. } if tool == "question" => match state {
                ToolState::Completed { output, .. } => Some(Ok(output.clone())),
                ToolState::Error { error, .. } => Some(Err(error.clone())),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    })
}

/// Why the turn ended.
fn finish(seen: &[Event]) -> FinishReason {
    match seen.last() {
        Some(Event::MessageFinished { reason, .. }) => *reason,
        other => panic!("a turn always ends with a finish, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The three races. Each produces exactly one terminal.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_answered_question_produces_one_reply_and_the_model_reads_the_labels() {
    let engine = engine(vec![asks_one(), says("noted")]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "ask me".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, before) = until_question(&engine, &mut events).await;

    // The request names the session the turn is running in, and the call it
    // came from, so a dialog can be attributed and correlated.
    let Some(Event::QuestionAsked {
        session_id,
        questions,
        source,
        ..
    }) = before.last()
    else {
        panic!("the last thing seen is the question, got {before:?}");
    };
    assert_eq!(*session_id, engine.session_id());
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].question, QUESTION);
    assert_eq!(questions[0].options.len(), 2);
    // `custom` is the asking side's field and the model never sends one.
    assert_eq!(questions[0].custom, None);
    let source = source.as_ref().expect("a tool call asked this");
    assert!(!source.call_id.is_empty());

    engine
        .send(Command::ReplyQuestion {
            id: id.clone(),
            answers: vec![vec!["Postgres".to_owned()]],
        })
        .await
        .expect("a reply is never refused");
    let mut seen = before;
    seen.extend(drain(&mut events).await);

    assert_eq!(terminals(&seen, &id), ["replied:1"]);
    let output = call_outcome(&seen).expect("the call ended");
    assert_eq!(
        output,
        Ok(format!(
            "User has answered your questions: \"{QUESTION}\"=\"Postgres\". \
             You can now continue with the user's answers in mind."
        ))
    );
    assert_eq!(finish(&seen), FinishReason::Completed);
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "asking is the interaction; it must not also raise a dialog: {seen:?}"
    );
}

/// A dismissal is its own terminal and its own payload — upstream makes
/// `question.rejected` a separate event rather than a reply carrying a
/// refusing value, and the call fails rather than completing.
#[tokio::test]
async fn a_dismissed_question_produces_one_rejection_and_the_turn_carries_on() {
    let engine = engine(vec![asks_one(), says("never mind then")]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "ask me".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, before) = until_question(&engine, &mut events).await;

    engine
        .send(Command::RejectQuestion { id: id.clone() })
        .await
        .expect("a rejection is never refused");
    let mut seen = before;
    seen.extend(drain(&mut events).await);

    assert_eq!(terminals(&seen, &id), ["rejected"]);
    assert_eq!(
        call_outcome(&seen),
        Some(Err("The user dismissed this question".to_owned())),
        "the model reads upstream's own sentence"
    );
    // A refusal is information the model reads, never a turn abort — the
    // loop's rule — so the script reaches its end.
    assert_eq!(finish(&seen), FinishReason::Completed);
}

/// The cancel case, which is where the two kinds differ most: a cancelled
/// permission is answered with `Reject`, and a cancelled question is answered
/// with the event a dismissal uses. Either way the request that reached the
/// subscriber is answered exactly once, unconditionally.
#[tokio::test]
async fn a_cancel_refuses_the_open_question_with_exactly_one_rejection() {
    let engine = engine(vec![asks_one(), says("unreachable")]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "ask me".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, before) = until_question(&engine, &mut events).await;

    engine
        .send(Command::CancelTurn)
        .await
        .expect("a cancel is never refused");
    let mut seen = before;
    seen.extend(drain(&mut events).await);

    assert_eq!(terminals(&seen, &id), ["rejected"]);
    assert_eq!(finish(&seen), FinishReason::Cancelled);
}

/// And the answer that arrives after the cancel changes nothing: the request
/// was already answered, so there is no second terminal to confuse a dialog
/// that has already been retired.
#[tokio::test]
async fn an_answer_that_arrives_after_the_cancel_adds_no_second_terminal() {
    let engine = engine(vec![asks_one(), says("unreachable")]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "ask me".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, before) = until_question(&engine, &mut events).await;

    engine
        .send(Command::CancelTurn)
        .await
        .expect("a cancel is never refused");
    let mut seen = before;
    seen.extend(drain(&mut events).await);

    engine
        .send(Command::ReplyQuestion {
            id: id.clone(),
            answers: vec![vec!["Postgres".to_owned()]],
        })
        .await
        .expect("a late reply is ignored rather than refused");

    assert_eq!(terminals(&seen, &id), ["rejected"]);
    assert_eq!(finish(&seen), FinishReason::Cancelled);
}

// ---------------------------------------------------------------------------
// Routing: the cell holds one request, and it knows which kind it is.
// ---------------------------------------------------------------------------

/// A permission reply naming the open question's id is a miss, not a
/// mis-delivery. The two kinds share one slot — the turn is blocked inside
/// whichever is open — so the discriminant is what stops a `ReplyPermission`
/// from unblocking a question with a decision nobody asked for.
#[tokio::test]
async fn a_permission_reply_cannot_answer_an_open_question() {
    let engine = engine(vec![asks_one(), says("noted")]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "ask me".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, before) = until_question(&engine, &mut events).await;

    // Same text, wrong kind: the id a question was minted under, sent as a
    // permission decision.
    engine
        .send(Command::ReplyPermission {
            id: PermissionId::from(id.as_str().to_owned()),
            reply: PermissionReply::Always,
        })
        .await
        .expect("a stray reply is ignored rather than refused");

    // The question is still open, and a real answer still reaches it.
    engine
        .send(Command::ReplyQuestion {
            id: id.clone(),
            answers: vec![vec!["SQLite".to_owned()]],
        })
        .await
        .expect("a reply is never refused");
    let mut seen = before;
    seen.extend(drain(&mut events).await);

    assert_eq!(terminals(&seen, &id), ["replied:1"]);
    assert!(
        matches!(call_outcome(&seen), Some(Ok(output)) if output.contains("SQLite")),
        "the answer that decided is the one addressed to the question"
    );
    assert_eq!(finish(&seen), FinishReason::Completed);
}

/// An answer for a request nobody is waiting on is defined to be ignored — the
/// turn task owns answering each request exactly once, so there is nothing to
/// repair — and it must not disturb the request that *is* open.
#[tokio::test]
async fn an_answer_naming_an_unknown_question_is_ignored() {
    let engine = engine(vec![asks_one(), says("noted")]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "ask me".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, before) = until_question(&engine, &mut events).await;

    for stray in [
        Command::ReplyQuestion {
            id: QuestionId::from("que_nothing".to_owned()),
            answers: vec![vec!["Postgres".to_owned()]],
        },
        Command::RejectQuestion {
            id: QuestionId::from("que_nothing".to_owned()),
        },
    ] {
        engine
            .send(stray)
            .await
            .expect("a stray answer is ignored rather than refused");
    }

    engine
        .send(Command::ReplyQuestion {
            id: id.clone(),
            answers: vec![vec!["Postgres".to_owned()]],
        })
        .await
        .expect("a reply is never refused");
    let mut seen = before;
    seen.extend(drain(&mut events).await);

    assert_eq!(terminals(&seen, &id), ["replied:1"]);
    assert_eq!(finish(&seen), FinishReason::Completed);
}

// ---------------------------------------------------------------------------
// What the model reads back.
// ---------------------------------------------------------------------------

/// A question the person skipped is named to the model as unanswered rather
/// than silently dropped, which is upstream's rendering — and the reply event
/// carries the empty answer, so a frontend replaying the stream sees the same
/// thing.
#[tokio::test]
async fn a_skipped_question_is_named_to_the_model_as_unanswered() {
    let engine = engine(vec![asks_one(), says("noted")]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "ask me".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, before) = until_question(&engine, &mut events).await;

    engine
        .send(Command::ReplyQuestion {
            id: id.clone(),
            answers: vec![Vec::new()],
        })
        .await
        .expect("a reply is never refused");
    let mut seen = before;
    seen.extend(drain(&mut events).await);

    assert_eq!(terminals(&seen, &id), ["replied:1"]);
    assert!(
        matches!(call_outcome(&seen), Some(Ok(output)) if output.contains("\"Unanswered\"")),
        "got {:?}",
        call_outcome(&seen)
    );
}

// ---------------------------------------------------------------------------
// A subagent's question crosses, exactly as its permission dialogs do.
// ---------------------------------------------------------------------------

/// A child's question reaches the parent's stream **re-addressed**: it carries
/// the parent's session id, not the child's.
///
/// The child session is invisible to every frontend — never seeded, never
/// listed — so a request naming it would hand a session-filtering client a
/// dialog it could not attribute, about a conversation it cannot see. The
/// parent's is the conversation whose turn is actually waiting on the answer,
/// because the parent is blocked inside the `task` call the child is running.
///
/// Both terminals cross too, and must: a dialog opened on the child's
/// `QuestionAsked` would never be retired if the reply stayed behind.
#[tokio::test]
async fn a_crossing_question_carries_the_parents_session_id() {
    let engine = engine(vec![
        tool_call(
            "task",
            json!({
                "description": "ask the user",
                "prompt": "find out which database",
                "subagent_type": "general",
            }),
        ),
        asks_one(),
        says("the child is done"),
        says("so is the parent"),
    ]);
    let parent = engine.session_id();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, before) = until_question(&engine, &mut events).await;

    let crossing: Vec<&Event> = before
        .iter()
        .filter(|event| matches!(event, Event::QuestionAsked { .. }))
        .collect();
    assert_eq!(crossing.len(), 1, "{crossing:?}");
    assert_eq!(
        *crossing[0].session_id(),
        parent,
        "a child's question is re-addressed to the conversation that is waiting"
    );

    engine
        .send(Command::ReplyQuestion {
            id: id.clone(),
            answers: vec![vec!["Postgres".to_owned()]],
        })
        .await
        .expect("a reply routed to the parent reaches the child");
    let mut seen = before;
    seen.extend(drain(&mut events).await);

    assert_eq!(terminals(&seen, &id), ["replied:1"]);
    for event in seen
        .iter()
        .filter(|event| matches!(event, Event::QuestionReplied { .. }))
    {
        assert_eq!(*event.session_id(), parent, "{event:?}");
    }
    assert_eq!(finish(&seen), FinishReason::Completed);
}

/// And a cancel that lands while a *child* is asking answers the crossing
/// request once, on the parent's stream — the discipline is the seam's, not
/// the root turn's.
#[tokio::test]
async fn a_cancel_during_a_childs_question_still_produces_one_rejection() {
    let engine = engine(vec![
        tool_call(
            "task",
            json!({
                "description": "ask the user",
                "prompt": "find out which database",
                "subagent_type": "general",
            }),
        ),
        asks_one(),
        says("unreachable"),
        says("unreachable"),
    ]);
    let parent = engine.session_id();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, before) = until_question(&engine, &mut events).await;

    engine
        .send(Command::CancelTurn)
        .await
        .expect("a cancel is never refused");
    let mut seen = before;
    seen.extend(drain(&mut events).await);

    assert_eq!(terminals(&seen, &id), ["rejected"]);
    for event in seen
        .iter()
        .filter(|event| matches!(event, Event::QuestionRejected { .. }))
    {
        assert_eq!(*event.session_id(), parent, "{event:?}");
    }
    assert_eq!(finish(&seen), FinishReason::Cancelled);
}

/// Several questions are answered together, in order, and the model reads them
/// back paired with what it asked.
#[tokio::test]
async fn several_questions_are_answered_together_and_read_back_in_order() {
    let engine = engine(vec![
        tool_call(
            "question",
            json!({
                "questions": [
                    {"question": "Which database?", "header": "Database", "options": []},
                    {"question": "Which runtime?", "header": "Runtime", "options": [],
                     "multiple": true},
                ],
            }),
        ),
        says("noted"),
    ]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "ask me".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, before) = until_question(&engine, &mut events).await;

    let Some(Event::QuestionAsked { questions, .. }) = before.last() else {
        panic!("the last thing seen is the question, got {before:?}");
    };
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0].multiple, None);
    assert_eq!(questions[1].multiple, Some(true));

    engine
        .send(Command::ReplyQuestion {
            id: id.clone(),
            answers: vec![
                vec!["Postgres".to_owned()],
                vec!["tokio".to_owned(), "smol".to_owned()],
            ],
        })
        .await
        .expect("a reply is never refused");
    let mut seen = before;
    seen.extend(drain(&mut events).await);

    assert_eq!(terminals(&seen, &id), ["replied:2"]);
    let Some(Ok(output)) = call_outcome(&seen) else {
        panic!("the call completed, got {:?}", call_outcome(&seen));
    };
    assert!(
        output.contains("\"Which database?\"=\"Postgres\""),
        "{output}"
    );
    assert!(
        output.contains("\"Which runtime?\"=\"tokio, smol\""),
        "{output}"
    );
}
