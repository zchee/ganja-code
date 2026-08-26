use ganja_protocol::{
    Event, FinishReason, Message, MessageId, Part, PartBody, PartId, Role, SessionId, ToolState,
    Usage,
};
use serde_json::Value;

use super::{Format, Kind, Reporter, TYPES, resolve_input};

/// The session every fixture runs in, distinct from anything a part
/// carries so that a stamp read off the wrong place would show.
const SESSION: &str = "ses_the_runs_own";

/// The session the fixture events themselves carry — deliberately not
/// [`SESSION`], for the same reason that one is distinct from anything a
/// part carries: the stamp is the run's own local, and a stamp read off
/// the event instead would show.
fn event_session() -> SessionId {
    SessionId::from("ses_carried_on_events".to_owned())
}

/// Drives `events` through a reporter and hands back both channels.
fn report(format: Format, events: &[Event]) -> (String, String) {
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    {
        let mut reporter = Reporter::new(format, SESSION.to_owned(), &mut out, &mut err);
        for event in events {
            if reporter.apply(event, Some("build")) {
                break;
            }
        }
        reporter.finish().expect("a vector accepts every write");
    }

    (
        String::from_utf8(out).expect("the output is text"),
        String::from_utf8(err).expect("the diagnostics are text"),
    )
}

/// Every object of an nd-JSON stream, parsed.
fn objects(stream: &str) -> Vec<Value> {
    stream
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("every line is one JSON object"))
        .collect()
}

fn assistant() -> Message {
    Message {
        id: MessageId::from("msg_1".to_owned()),
        role: Role::Assistant,
        parts: Vec::new(),
        time: ganja_protocol::MessageTime {
            created: 1,
            completed: None,
        },
        model: Some("canned".to_owned()),
        usage: None,
    }
}

/// One turn's worth of stream: a step that says something and calls a
/// tool, then closes.
fn turn() -> Vec<Event> {
    let text = Part {
        id: PartId::from("prt_text".to_owned()),
        body: PartBody::Text {
            text: String::new(),
        },
    };
    let step = Part {
        id: PartId::from("prt_step".to_owned()),
        body: PartBody::StepStart,
    };
    let finish = Part {
        id: PartId::from("prt_finish".to_owned()),
        body: PartBody::StepFinish {
            usage: Usage::default(),
        },
    };
    let call = Part {
        id: PartId::from("prt_call".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "read".to_owned(),
            state: ToolState::Completed {
                input: serde_json::json!({"filePath": "src/main.rs"}),
                output: "fn main() {}".to_owned(),
                title: "Read src/main.rs".to_owned(),
                metadata: Value::Null,
                started: 1,
                completed: 2,
            },
        },
    };
    let message_id = MessageId::from("msg_1".to_owned());

    vec![
        Event::MessageStarted {
            session_id: event_session(),
            message: Message::user("what is in main"),
        },
        Event::MessageStarted {
            session_id: event_session(),
            message: assistant(),
        },
        Event::PartStarted {
            session_id: event_session(),
            message_id: message_id.clone(),
            part: step,
        },
        Event::PartStarted {
            session_id: event_session(),
            message_id: message_id.clone(),
            part: text,
        },
        Event::PartDelta {
            session_id: event_session(),
            message_id: message_id.clone(),
            part_id: PartId::from("prt_text".to_owned()),
            delta: "Reading it.".to_owned(),
        },
        Event::PartStarted {
            session_id: event_session(),
            message_id: message_id.clone(),
            part: finish,
        },
        Event::PartUpdated {
            session_id: event_session(),
            message_id: message_id.clone(),
            part: call,
        },
        Event::MessageFinished {
            session_id: event_session(),
            message_id,
            reason: FinishReason::Completed,
            usage: None,
            error: None,
            completed: 3,
        },
    ]
}

/// The set is the contract: a consumer switches on it, and a seventh name
/// would reach a default arm nobody wrote.
#[test]
fn the_wire_carries_exactly_upstreams_six_type_names() {
    assert_eq!(
        TYPES,
        [
            "tool_use",
            "step_start",
            "step_finish",
            "text",
            "reasoning",
            "error"
        ]
    );
    // Each kind names a distinct one of them, so no two objects can be
    // told apart by anything but their type. All six are listed: `as_str`
    // indexes `TYPES` by discriminant, so a kind left out of this array is
    // a kind whose index nothing checks — which is exactly how `reasoning`
    // sat here unverified while its slot was still a placeholder.
    let named = [
        Kind::ToolUse,
        Kind::StepStart,
        Kind::StepFinish,
        Kind::Text,
        Kind::Reasoning,
        Kind::Error,
    ]
    .map(Kind::as_str);
    assert!(
        named.iter().all(|name| TYPES.contains(name)),
        "a kind named something outside the set: {named:?}"
    );
    let mut sorted = named;
    sorted.sort_unstable();
    sorted.windows(2).for_each(|pair| {
        assert_ne!(pair[0], pair[1], "two kinds share a name");
    });
}

#[test]
fn every_emitted_object_carries_a_type_from_the_set() {
    let (out, _) = report(Format::Json, &turn());
    let emitted = objects(&out);

    assert!(!emitted.is_empty(), "a turn has to emit something");
    for object in &emitted {
        let kind = object["type"].as_str().expect("every object has a type");
        assert!(
            TYPES.contains(&kind),
            "an object carried a type outside the set: {kind}"
        );
    }
}

/// The rule the whole format hangs on: the id is the run's own, captured
/// once, and nothing about a part can move it.
#[test]
fn every_emitted_object_carries_the_runs_own_session_id() {
    let (out, _) = report(Format::Json, &turn());

    for object in objects(&out) {
        assert_eq!(
            object["sessionID"].as_str(),
            Some(SESSION),
            "an object carried a session that is not this run's: {object}"
        );
    }
}

/// A turn that said something and ran a tool emits both, in the order they
/// happened, and the text is closed by the step rather than left behind.
#[test]
fn a_turn_emits_its_step_its_text_and_its_call_in_order() {
    let (out, _) = report(Format::Json, &turn());
    let kinds: Vec<String> = objects(&out)
        .iter()
        .map(|object| object["type"].as_str().unwrap_or_default().to_owned())
        .collect();

    assert_eq!(kinds, ["step_start", "text", "step_finish", "tool_use"]);
}

#[test]
fn a_streamed_text_part_carries_every_fragment_that_was_appended() {
    let (out, _) = report(Format::Json, &turn());
    let text = objects(&out)
        .into_iter()
        .find(|object| object["type"] == "text")
        .expect("the turn said something");

    assert_eq!(text["part"]["text"].as_str(), Some("Reading it."));
}

/// Default format is for a person, so the model's words reach stdout whole
/// and the header names what answered.
#[test]
fn the_default_format_writes_the_header_and_the_reply_to_stdout() {
    let (out, err) = report(Format::Default, &turn());

    assert!(
        out.contains("> build \u{b7} canned"),
        "no header in {out:?}"
    );
    assert!(out.contains("Reading it."), "no reply in {out:?}");
    assert!(out.contains("Read src/main.rs"), "no tool line in {out:?}");
    assert!(
        err.is_empty(),
        "a turn that worked said nothing on stderr: {err:?}"
    );
}

/// A failed turn emits an `error` object *and* is what the run answers
/// with, which is what the caller turns into the exit code. The stderr
/// line is the caller's, so that the same sentence is not printed twice.
#[test]
fn a_failed_turn_emits_an_error_object_and_is_what_the_run_returns() {
    let mut events = turn();
    events.pop();
    events.push(Event::MessageFinished {
        session_id: event_session(),
        message_id: MessageId::from("msg_1".to_owned()),
        reason: FinishReason::Failed,
        usage: None,
        error: Some("the provider hung up".to_owned()),
        completed: 3,
    });

    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let returned = {
        let mut reporter = Reporter::new(Format::Json, SESSION.to_owned(), &mut out, &mut err);
        for event in &events {
            if reporter.apply(event, Some("build")) {
                break;
            }
        }
        reporter.finish().expect("a vector accepts every write")
    };
    let out = String::from_utf8(out).expect("the output is text");
    let err = String::from_utf8(err).expect("the diagnostics are text");

    let failure = objects(&out)
        .into_iter()
        .find(|object| object["type"] == "error")
        .expect("a failed turn emits an error object");
    assert_eq!(failure["error"].as_str(), Some("the provider hung up"));
    assert_eq!(failure["sessionID"].as_str(), Some(SESSION));
    assert_eq!(returned.as_deref(), Some("the provider hung up"));
    assert!(err.is_empty(), "the caller owns the stderr line: {err:?}");
}

/// A tool that failed is still a `tool_use` object — upstream emits the
/// part for both terminal states (`run.ts:719-720`) — and its reason is a
/// diagnostic rather than payload.
#[test]
fn a_failed_call_is_a_tool_use_object_and_its_reason_goes_to_stderr() {
    let call = Part {
        id: PartId::from("prt_call".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "edit".to_owned(),
            state: ToolState::Error {
                input: serde_json::json!({}),
                error: "the file was not read first".to_owned(),
                started: 1,
                completed: 2,
            },
        },
    };
    let events = [Event::PartUpdated {
        session_id: event_session(),
        message_id: MessageId::from("msg_1".to_owned()),
        part: call.clone(),
    }];

    let (out, err) = report(Format::Json, &events);
    assert_eq!(objects(&out).len(), 1);
    assert_eq!(objects(&out)[0]["type"], "tool_use");
    assert!(err.is_empty(), "json mode renders no lines: {err:?}");

    let (out, err) = report(Format::Default, &events);
    assert!(out.contains("edit failed"), "no failure line in {out:?}");
    assert!(
        err.contains("the file was not read first"),
        "the reason has to be a diagnostic: {err:?}"
    );
}

/// A title a tool wrote reaches a terminal that would execute an escape in
/// it, exactly as a stored session's title does in `sessions`.
#[test]
fn a_tool_title_cannot_move_the_terminals_cursor() {
    let call = Part {
        id: PartId::from("prt_call".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "read".to_owned(),
            state: ToolState::Completed {
                input: Value::Null,
                output: String::new(),
                title: "\u{1b}[2Jread \u{7}src/main.rs\r\nsecond row".to_owned(),
                metadata: Value::Null,
                started: 1,
                completed: 2,
            },
        },
    };

    let (out, _) = report(
        Format::Default,
        &[Event::PartUpdated {
            session_id: event_session(),
            message_id: MessageId::from("msg_1".to_owned()),
            part: call,
        }],
    );

    let leaked: Vec<char> = out
        .chars()
        .filter(|character| character.is_control() && *character != '\n')
        .collect();
    assert!(
        leaked.is_empty(),
        "control characters reached stdout: {leaked:?}"
    );
    assert!(
        out.contains("src/main.rs"),
        "the printable half survives: {out:?}"
    );
}

/// The warning is the whole difference between a run that refuses and one
/// that hangs, and it must not land in the middle of an nd-JSON stream.
#[test]
fn a_rejected_permission_is_warned_about_on_stderr_in_both_formats() {
    for format in [Format::Default, Format::Json] {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        {
            let mut reporter = Reporter::new(format, SESSION.to_owned(), &mut out, &mut err);
            reporter.rejecting("bash", "rm -rf /");
            reporter.finish().expect("a vector accepts every write");
        }

        let err = String::from_utf8(err).expect("the diagnostics are text");
        assert!(out.is_empty(), "a warning is never payload: {out:?}");
        assert!(err.contains("bash"), "the tool has to be named: {err:?}");
        assert!(
            err.contains("rm -rf /"),
            "what would run has to be named: {err:?}"
        );
        assert!(
            err.contains("auto-rejecting"),
            "the decision has to be said: {err:?}"
        );
    }
}

/// The `$` scan runs headless too: a run's prompt tokens reach the
/// provider expanded through the engine's own roots, exactly as a
/// screenful session's do. The nd-JSON stream carries only the
/// assistant's side, so the proof reads the fake provider's request log
/// — which *is* this run's wire.
#[tokio::test]
async fn a_runs_dollar_tokens_reach_the_provider_expanded() {
    let root = tempfile::tempdir().expect("a scratch directory");
    let dir = root.path().join("porting");
    std::fs::create_dir_all(&dir).expect("the fixture tree is creatable");
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: porting\n---\nRead upstream first.",
    )
    .expect("the fixture is writable");

    let provider = std::sync::Arc::new(ganja_core::provider::fake::FakeProvider::new(
        "done",
        std::time::Duration::ZERO,
    ));
    let engine = ganja_core::Engine::new(
        std::sync::Arc::clone(&provider) as _,
        "run-model",
        std::sync::Arc::new(ganja_core::tool::Registry::new(Vec::new())),
        ganja_core::permission::Permissions::default(),
    )
    .with_skill_roots(
        ganja_core::tool::skill::Roots::none().with_paths([root.path().to_path_buf()]),
    );

    let failure = super::drive(
        &engine,
        None,
        "use $porting and leave $PATH alone",
        None,
        false,
        Format::Default,
    )
    .await
    .expect("the canned turn drives to its end");
    assert_eq!(failure, None, "and ends clean");

    let requests = provider.recorded();
    let carried: Vec<&str> = requests
        .first()
        .expect("the run reached the provider")
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.parts.iter())
        .filter_map(ganja_protocol::Part::as_text)
        .collect();

    assert!(
        carried
            .iter()
            .any(|part| part.starts_with("<skill_content name=\"porting\">")
                && part.contains("Read upstream first.")),
        "the invoked skill rides the request whole: {carried:?}"
    );
    assert!(
        carried.iter().any(|part| part.contains("$PATH")),
        "and the un-invoked token is still just text: {carried:?}"
    );
    assert!(
        !carried.iter().any(|part| part.contains("name=\"PATH\"")),
        "nothing answered to $PATH, so nothing was loaded for it: {carried:?}"
    );
}

#[test]
fn a_typed_message_and_a_piped_one_join_with_the_pipe_last() {
    assert_eq!(
        resolve_input("explain this", "fn main() {}"),
        "explain this\nfn main() {}"
    );
    assert_eq!(resolve_input("explain this", ""), "explain this");
    assert_eq!(resolve_input("", "fn main() {}"), "fn main() {}");
    assert_eq!(resolve_input("", ""), "");
}
