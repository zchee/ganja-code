//! `@`-mentioned files: a reference on the message, content in the request.
//!
//! The whole point is *when* the file is read. A mention is a reference and
//! nothing more — the content is read when a request is built — so a file the
//! user saves between attaching it and sending reaches the model as it is now.
//! It is also not a *read*: nothing here records the file in `FileTimes`, so
//! `edit` still refuses a file the model itself has never opened (R9).
//!
//! Mention paths are absolute here so the fixtures live in a temporary
//! directory rather than in whatever checkout the suite is running in; the
//! relative case is unit-tested where the join happens (`src/session.rs`).

use std::sync::Arc;

use ganja_core::{
    Engine,
    permission::Permissions,
    protocol::{Command, Event, Mention, PartBody, Role, ToolState},
    provider::{ChatRequest, Provider},
    tool::Registry,
};
use ganja_testkit::{ScriptedProvider, drain_allowing, says, tool_call};
use serde_json::json;

/// Everything the user side of `request` says, blocks and all.
fn user_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.parts.iter())
        .filter_map(ganja_core::protocol::Part::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn engine(provider: Arc<dyn Provider>, tools: Registry) -> Engine {
    Engine::new(
        provider,
        "recorder-model",
        Arc::new(tools),
        Permissions::default(),
    )
}

#[tokio::test]
async fn a_mention_becomes_a_file_part_on_the_message_and_content_in_the_request() {
    let workspace = ganja_testkit::temp_dir();
    let path = workspace.path().join("notes.md");
    std::fs::write(&path, "the objective is to ship").expect("the fixture writes");

    let (provider, requests) = ScriptedProvider::new(vec![says("noted")]);
    let engine = engine(provider, Registry::new(Vec::new()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "what does this say".to_owned(),
            mentions: vec![Mention {
                path: path.to_string_lossy().into_owned(),
            }],
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_allowing(&engine, &mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let sent = user_text(&requests[0]);
    assert!(
        sent.contains("what does this say"),
        "the prompt is still the prompt: {sent}"
    );
    assert!(
        sent.contains(&format!("<attached-file path=\"{}\">", path.display())),
        "and the attachment names where it came from: {sent}"
    );
    assert!(
        sent.contains("the objective is to ship"),
        "with the file's contents inside it: {sent}"
    );
}

#[tokio::test]
async fn the_users_message_carries_the_mention_as_a_reference() {
    let workspace = ganja_testkit::temp_dir();
    let path = workspace.path().join("notes.md");
    std::fs::write(&path, "contents").expect("the fixture writes");

    let (provider, _) = ScriptedProvider::new(vec![says("noted")]);
    let engine = engine(provider, Registry::new(Vec::new()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "look".to_owned(),
            mentions: vec![Mention {
                path: path.to_string_lossy().into_owned(),
            }],
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_allowing(&engine, &mut events).await;

    let Some(Event::MessageStarted { message: user }) = seen.first() else {
        panic!("a turn opens with the user's message, got {seen:?}");
    };
    assert_eq!(user.role, Role::User);
    assert_eq!(
        user.parts.len(),
        2,
        "the text, then the file: {:?}",
        user.parts
    );
    let PartBody::File { path: named, mime } = &user.parts[1].body else {
        panic!("the second part is the mention, got {:?}", user.parts[1]);
    };
    assert_eq!(named, &path.to_string_lossy());
    assert_eq!(mime, "text/plain");
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            Event::MessageStarted { message } if message.parts.iter().any(|part| part
                .as_text()
                .is_some_and(|text| text.contains("contents")))
        )),
        "the transcript keeps the reference, not the contents: {seen:?}"
    );
}

/// The non-vacuity proof for send-time resolution: the file changes *after* it
/// was attached, and the next request carries what it says now. Resolving at
/// attach time would send the stale text and fail here.
#[tokio::test]
async fn a_mentioned_file_is_read_when_the_request_is_built_not_when_it_was_attached() {
    let workspace = ganja_testkit::temp_dir();
    let path = workspace.path().join("notes.md");
    std::fs::write(&path, "the first draft").expect("the fixture writes");

    let (provider, requests) = ScriptedProvider::new(vec![says("noted"), says("noted again")]);
    let engine = engine(provider, Registry::new(Vec::new()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "read it".to_owned(),
            mentions: vec![Mention {
                path: path.to_string_lossy().into_owned(),
            }],
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_allowing(&engine, &mut events).await;

    std::fs::write(&path, "the second draft").expect("the fixture rewrites");

    engine
        .send(Command::SendPrompt {
            text: "read it again".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("a finished turn leaves the engine idle");
    drain_allowing(&engine, &mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    assert!(
        user_text(&requests[0]).contains("the first draft"),
        "the first request read the file as it was then"
    );
    let second = user_text(&requests[1]);
    assert!(
        second.contains("the second draft"),
        "and the second read it as it is now: {second}"
    );
    assert!(
        !second.contains("the first draft"),
        "a reference resolved once would have gone stale: {second}"
    );
}

/// A mention is not a read. The read-before-write rule is about what the
/// *model* has opened, and attaching a file is the user's act, not the model's.
#[tokio::test]
async fn a_mention_does_not_let_the_model_edit_a_file_it_never_read() {
    let workspace = ganja_testkit::temp_dir();
    let path = workspace.path().join("notes.md");
    std::fs::write(&path, "the original").expect("the fixture writes");

    let (provider, _) = ScriptedProvider::new(vec![
        tool_call(
            "edit",
            json!({
                "filePath": path.to_string_lossy(),
                "oldString": "the original",
                "newString": "something else",
            }),
        ),
        says("I could not"),
    ]);
    let engine = engine(provider, Registry::with_builtins());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "change it".to_owned(),
            mentions: vec![Mention {
                path: path.to_string_lossy().into_owned(),
            }],
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_allowing(&engine, &mut events).await;

    let refused = seen
        .iter()
        .rev()
        .find_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool {
                    state: ToolState::Error { error, .. },
                    ..
                } => Some(error.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the edit was refused");
    assert!(
        refused.contains("has not been read this session"),
        "attaching a file is not opening it: {refused}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("the file is still there"),
        "the original",
        "and the file is untouched"
    );
}

#[tokio::test]
async fn a_mention_naming_something_unreadable_says_so_rather_than_vanishing() {
    let workspace = ganja_testkit::temp_dir();
    let directory = workspace.path().join("src");
    std::fs::create_dir(&directory).expect("the fixture makes a directory");

    let (provider, requests) = ScriptedProvider::new(vec![says("noted")]);
    let engine = engine(provider, Registry::new(Vec::new()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "look".to_owned(),
            mentions: vec![
                Mention {
                    path: directory.to_string_lossy().into_owned(),
                },
                Mention {
                    path: workspace
                        .path()
                        .join("absent.md")
                        .to_string_lossy()
                        .into_owned(),
                },
            ],
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_allowing(&engine, &mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let sent = user_text(&requests[0]);
    assert!(
        sent.contains("(this is a directory"),
        "a directory says what it is: {sent}"
    );
    assert!(
        sent.contains("(could not be read"),
        "and a file that is not there says that: {sent}"
    );
}
