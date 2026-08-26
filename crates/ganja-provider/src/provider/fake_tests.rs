use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::{EXHAUSTED, FakeProvider, ID, MODEL, REPLY, SCRIPT_ENV, split_into_chunks};
use crate::{
    protocol::{FinishReason, Message, Usage},
    provider::{ChatRequest, Provider as _, ProviderError, ProviderEvent},
};

/// A script with a turn that calls one tool, then a turn that calls two —
/// the second of which names no arguments, so the default is exercised too.
const SCRIPT: &str = r#"{
        "cadence_ms": 0,
        "turns": [
            {
                "text": "Reading it.",
                "tool_calls": [{"name": "read", "args": {"filePath": "src/main.rs"}}]
            },
            {
                "text": "Two calls now.",
                "tool_calls": [
                    {"name": "glob", "args": {"pattern": "**/*.rs"}},
                    {"name": "todo"}
                ]
            }
        ]
    }"#;

fn request(prompt: &str) -> ChatRequest {
    ChatRequest {
        effort_options: Default::default(),
        model: MODEL.to_owned(),
        system: None,
        messages: vec![Message::user(prompt)],
        tools: Vec::new(),
    }
}

/// A turn's `thinking` streams first, as readable reasoning, one word at a
/// time like the text after it; a turn without the key streams none.
#[tokio::test]
async fn a_turns_thinking_streams_before_its_text_as_readable_reasoning() {
    let (_dir, path) = script_file(
        r#"{"cadence_ms": 0, "turns": [
                {"thinking": "short is enough", "text": "Hello there."},
                {"text": "No thought this time."}
            ]}"#,
    );
    let provider = FakeProvider::new(REPLY, Duration::from_secs(60)).with_script(&path);

    let first = turn(&provider).await;
    let thought: Vec<&str> = first
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ReasoningDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thought.concat(), "short is enough");
    let first_text = first
        .iter()
        .position(|event| matches!(event, ProviderEvent::TextDelta(_)))
        .expect("the reply follows");
    let last_thought = first
        .iter()
        .rposition(|event| matches!(event, ProviderEvent::ReasoningDelta(_)))
        .expect("the thought came");
    assert!(
        last_thought < first_text,
        "the thought streams before the text"
    );

    let second = turn(&provider).await;
    assert!(
        !second
            .iter()
            .any(|event| matches!(event, ProviderEvent::ReasoningDelta(_))),
        "a turn without the key thinks nothing aloud: {second:?}"
    );
}

/// Writes `script` to a file that goes away with the test.
///
/// The directory is returned because dropping it deletes the file, and a
/// script that vanished mid-test would be indistinguishable from a bug.
fn script_file(script: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("script.json");
    std::fs::write(&path, script).expect("the script is writable");

    (dir, path)
}

/// Everything one turn of `provider` streams.
async fn turn(provider: &FakeProvider) -> Vec<ProviderEvent> {
    provider
        .stream(request("read src/main.rs"), CancellationToken::new())
        .await
        .expect("the script plays")
        .collect()
        .await
}

/// The error a turn failed with.
async fn failure(provider: &FakeProvider) -> ProviderError {
    // A stream is not `Debug`, so this cannot go through `expect_err`.
    let Err(error) = provider
        .stream(request("read src/main.rs"), CancellationToken::new())
        .await
    else {
        panic!("a script that cannot be played is not a turn");
    };

    error
}

#[test]
fn chunks_concatenate_back_into_the_reply() {
    assert_eq!(split_into_chunks(REPLY).concat(), REPLY);
}

#[test]
fn the_reply_opens_with_a_word_a_pty_test_can_wait_for() {
    let chunks = split_into_chunks(REPLY);

    assert_eq!(chunks.first().map(String::as_str), Some("Acknowledged. "));
}

#[test]
fn blank_input_produces_no_chunks() {
    assert!(split_into_chunks("").is_empty());
}

#[tokio::test]
async fn the_stream_paces_itself_and_reports_what_it_spent() {
    let cadence = Duration::from_millis(2);
    let provider = FakeProvider::new("one two three", cadence);
    assert_eq!(provider.id(), ID);

    let started = Instant::now();
    let events: Vec<ProviderEvent> = provider
        .stream(request("count to three"), CancellationToken::new())
        .await
        .expect("the fake provider always answers")
        .collect()
        .await;

    let text: String = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(text, "one two three");
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finish(FinishReason::Completed)),
        "a turn has to end with a finish, got {events:?}"
    );
    assert!(
        events.contains(&ProviderEvent::Usage(Usage {
            input_tokens: 3,
            output_tokens: 3,
            ..Usage::default()
        })),
        "usage should count the prompt and the reply, got {events:?}"
    );
    assert!(
        started.elapsed() >= cadence * 3,
        "three fragments should take at least three cadences, took {:?}",
        started.elapsed()
    );
}

/// One turn per request, in order, with the call identifiers running on
/// across the script rather than restarting — the engine keys a tool part
/// on the identifier, and one assistant turn spans several requests once
/// tools are in play.
#[tokio::test]
async fn a_script_plays_one_turn_per_request_and_then_says_so() {
    let (_dir, path) = script_file(SCRIPT);
    let provider = FakeProvider::new(REPLY, Duration::from_secs(60)).with_script(&path);

    assert_eq!(
        turn(&provider).await,
        vec![
            ProviderEvent::TextDelta("Reading ".to_owned()),
            ProviderEvent::TextDelta("it.".to_owned()),
            ProviderEvent::ToolCallStart {
                id: "call_1".to_owned(),
                name: "read".to_owned(),
            },
            ProviderEvent::ToolCallDelta {
                id: "call_1".to_owned(),
                json: "{\"filePath\":\"".to_owned(),
            },
            ProviderEvent::ToolCallDelta {
                id: "call_1".to_owned(),
                json: "src/main.rs\"}".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "call_1".to_owned(),
            },
            ProviderEvent::Usage(Usage {
                input_tokens: 2,
                output_tokens: 2,
                ..Usage::default()
            }),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "the script's cadence should override the provider's, or this test \
             would take a minute"
    );

    let second: Vec<ProviderEvent> = turn(&provider)
        .await
        .into_iter()
        .filter(|event| !matches!(event, ProviderEvent::TextDelta(_)))
        .collect();

    assert_eq!(
        second,
        vec![
            ProviderEvent::ToolCallStart {
                id: "call_2".to_owned(),
                name: "glob".to_owned(),
            },
            ProviderEvent::ToolCallDelta {
                id: "call_2".to_owned(),
                json: "{\"pattern\"".to_owned(),
            },
            ProviderEvent::ToolCallDelta {
                id: "call_2".to_owned(),
                json: ":\"**/*.rs\"}".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "call_2".to_owned(),
            },
            ProviderEvent::ToolCallStart {
                id: "call_3".to_owned(),
                name: "todo".to_owned(),
            },
            // A call that names no arguments still sends the empty object
            // the schema requires, and still sends it in pieces.
            ProviderEvent::ToolCallDelta {
                id: "call_3".to_owned(),
                json: "{".to_owned(),
            },
            ProviderEvent::ToolCallDelta {
                id: "call_3".to_owned(),
                json: "}".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "call_3".to_owned(),
            },
            ProviderEvent::Usage(Usage {
                input_tokens: 2,
                output_tokens: 3,
                ..Usage::default()
            }),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "the second turn should follow the first, numbering on from it"
    );

    let third = turn(&provider).await;
    let text: String = third
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(
        text, EXHAUSTED,
        "a request past the end of the script should say so rather than hang"
    );
    assert_eq!(
        third.last(),
        Some(&ProviderEvent::Finish(FinishReason::Completed)),
        "and still end like a turn, got {third:?}"
    );
}

/// Arguments arrive in pieces, which is the point: anything that parses the
/// first fragment it is handed instead of buffering the call has to fail
/// here rather than against a real provider.
#[tokio::test]
async fn a_call_streams_its_arguments_in_more_than_one_fragment() {
    let (_dir, path) = script_file(SCRIPT);
    let provider = FakeProvider::new(REPLY, Duration::ZERO).with_script(&path);

    let fragments: Vec<String> = turn(&provider)
        .await
        .into_iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallDelta { json, .. } => Some(json),
            _ => None,
        })
        .collect();

    assert!(
        fragments.len() >= 2,
        "one call's arguments should not arrive whole, got {fragments:?}"
    );
    assert!(
        fragments.iter().all(|fragment| !fragment.is_empty()),
        "an empty fragment says nothing, got {fragments:?}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fragments.concat())
            .expect("the fragments join back into the arguments"),
        serde_json::json!({"filePath": "src/main.rs"}),
        "the pieces have to reassemble into what the script said"
    );
}

/// Two runs of one script are the same run. A scripted demo is only worth
/// recording if what it records does not move.
#[tokio::test]
async fn the_same_script_streams_the_same_events_every_run() {
    let (_dir, path) = script_file(SCRIPT);

    let mut runs = Vec::new();
    for _ in 0..2 {
        let provider = FakeProvider::new(REPLY, Duration::ZERO).with_script(&path);
        let mut events = Vec::new();
        for _ in 0..3 {
            events.push(turn(&provider).await);
        }
        runs.push(events);
    }

    assert_eq!(runs[0], runs[1]);
}

/// A script that cannot be played fails the turn and says why. Falling back
/// to the canned reply would leave a demo silently proving nothing, which
/// is the failure this exists to prevent.
#[tokio::test]
async fn a_script_that_cannot_be_played_fails_loudly() {
    let (dir, path) = script_file("{ this is not a script");
    let malformed = FakeProvider::new(REPLY, Duration::ZERO).with_script(&path);
    let error = failure(&malformed).await;
    let rendered = format!("{error}");

    assert!(
        matches!(error, ProviderError::Parse(_)),
        "a script that will not parse is not a turn, got {error:?}"
    );
    assert!(
        rendered.contains(SCRIPT_ENV) && rendered.contains("script.json"),
        "the failure has to say which file to go and fix: {rendered}"
    );

    // An unknown key is a typo in a hand-written file, and a mistyped
    // `tool_calls` that quietly played as a plain reply would look exactly
    // like a bug in whatever is being demonstrated.
    let (_typo_dir, mistyped) = script_file(r#"{"turns": [{"text": "hi", "tool_call": []}]}"#);
    let typo = FakeProvider::new(REPLY, Duration::ZERO).with_script(mistyped);

    assert!(
        matches!(failure(&typo).await, ProviderError::Parse(_)),
        "an unknown key should be refused rather than ignored"
    );

    let missing = FakeProvider::new(REPLY, Duration::ZERO).with_script(dir.path().join("gone"));
    let error = failure(&missing).await;
    let rendered = format!("{error}");

    assert!(
        matches!(error, ProviderError::Transport(_)),
        "a script that is not there is not a turn, got {error:?}"
    );
    assert!(
        rendered.contains(SCRIPT_ENV) && rendered.contains("gone"),
        "the failure has to name the file it looked for: {rendered}"
    );
}

#[tokio::test]
async fn a_cancelled_stream_stops_yielding() {
    let provider = FakeProvider::new("one two three", Duration::from_millis(5));
    let cancel = CancellationToken::new();
    let mut events = provider
        .stream(request("count to three"), cancel.clone())
        .await
        .expect("the fake provider always answers");

    assert_eq!(
        events.next().await,
        Some(ProviderEvent::TextDelta("one ".to_owned()))
    );
    cancel.cancel();

    assert_eq!(events.next().await, None, "a cancelled stream should end");
}
