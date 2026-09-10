use serde_json::json;

use super::{HEADER, MID_TURN_HEADER, MID_TURN_RESUME, carried, message_text, render, render_turn};
use crate::protocol::{Message, Part, PartBody, ToolState, Usage};

fn user(id: &str, text: &str) -> Message {
    let mut message = Message::user(text);
    message.id = crate::protocol::MessageId::from(id.to_owned());

    message
}

fn assistant(id: &str, text: &str) -> Message {
    let mut message = Message::assistant("claude-opus-5");
    message.id = crate::protocol::MessageId::from(id.to_owned());
    message.parts.push(Part::text(text));

    message
}

/// An assistant message carrying a finished call and its result — the trail
/// that makes an answered ask read as answered.
fn called(id: &str, call_id: &str, tool: &str, input: serde_json::Value, output: &str) -> Message {
    let mut message = Message::assistant("claude-opus-5");
    message.id = crate::protocol::MessageId::from(id.to_owned());
    message.parts.push(Part {
        id: crate::protocol::PartId::ascending(),
        body: PartBody::Tool {
            call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            state: ToolState::Completed {
                input,
                output: output.to_owned(),
                title: String::new(),
                metadata: json!({}),
                started: 0,
                completed: 0,
            },
        },
    });

    message
}

/// **The** pin of this landing. Runs 9a, 9b and 9c carried a rendering with
/// assistant text and were refused three of three; run 9d carried the same
/// conversation without it and was served. So: no rendered line may begin
/// `[Assistant]`, against a transcript that holds assistant text.
#[test]
fn no_rendered_line_begins_assistant_though_the_transcript_holds_two_replies() {
    let history = [
        user("m1", "call the tool"),
        assistant("m2", "I will call it now"),
        called("m3", "toolu_1", "read", json!({"path": "/f"}), "contents"),
        user("m4", "and again"),
        assistant("m5", "here is what I found"),
    ];

    let rendered = render(&history);

    assert!(
        !rendered.text.lines().any(|line| line.starts_with("[Assistant]")),
        "the safeguard refused exactly this shape:\n{}",
        rendered.text
    );
    assert!(!rendered.text.contains("I will call it now"));
    assert!(!rendered.text.contains("here is what I found"));
    assert_eq!(
        rendered.assistant_turns_dropped, 3,
        "the cost is counted, so the log line can say it"
    );
}

#[test]
fn the_three_line_kinds_are_rendered_under_the_header() {
    let history = [
        user("m1", "read the file"),
        called("m2", "toolu_1", "read", json!({"path": "/f"}), "contents"),
    ];

    let rendered = render(&history);

    assert!(rendered.text.starts_with(HEADER));
    assert!(rendered.text.contains("[User] read the file"));
    assert!(rendered.text.contains(r#"[Tool Call] read {"path":"/f"}"#));
    assert!(rendered.text.contains("[Tool Result]\ncontents"));
}

#[test]
fn a_failed_call_renders_its_own_marker_and_its_error() {
    let mut message = Message::assistant("claude-opus-5");
    message.id = crate::protocol::MessageId::from("m2".to_owned());
    message.parts.push(Part {
        id: crate::protocol::PartId::ascending(),
        body: PartBody::Tool {
            call_id: "toolu_1".to_owned(),
            tool: "read".to_owned(),
            state: ToolState::Error {
                input: json!({}),
                error: "no such file".to_owned(),
                started: 0,
                completed: 0,
            },
        },
    });

    let rendered = render(&[user("m1", "read it"), message]);

    assert!(rendered.text.contains("[Tool Result (error)]\nno such file"));
}

/// A call still pending or running has neither line: rendering an unanswered
/// call is exactly what makes a model redo work, which run 9d did.
#[test]
fn a_call_that_has_not_finished_renders_no_line_at_all() {
    let mut message = Message::assistant("claude-opus-5");
    message.id = crate::protocol::MessageId::from("m2".to_owned());
    message.parts.push(Part {
        id: crate::protocol::PartId::ascending(),
        body: PartBody::Tool {
            call_id: "toolu_1".to_owned(),
            tool: "read".to_owned(),
            state: ToolState::Running { input: json!({}), metadata: json!({}), started: 0 },
        },
    });

    let rendered = render(&[user("m1", "read it"), message]);

    assert!(!rendered.text.contains("[Tool Call]"));
    assert!(!rendered.text.contains("[Tool Result]"));
}

/// The rule the whole fresh-record arm rests on: the render is a **write**,
/// so it returns exactly the ids it rendered, in order.
#[test]
fn the_render_returns_exactly_the_user_ids_its_lines_carry_in_order() {
    let history =
        [user("m1", "first"), assistant("m2", "a reply"), user("m3", "second"), user("m4", "   ")];

    let rendered = render(&history);

    assert_eq!(
        rendered.user_ids,
        ["m1", "m3"],
        "an empty message is no line, so it is no write either"
    );
    assert_eq!(rendered.text.matches("[User]").count(), 2);
}

#[test]
fn an_empty_history_renders_nothing_at_all_and_owes_nothing() {
    let rendered = render(&[]);

    assert!(rendered.text.is_empty());
    assert!(rendered.user_ids.is_empty());
}

/// A compaction summary is `Message::assistant`, so it renders as nothing —
/// `/compact` on this wire opens a fresh record with the prompt alone, and
/// the conversation's memory goes with it.
#[test]
fn a_compaction_summary_renders_as_nothing_because_it_is_assistant_text() {
    let rendered = render(&[assistant("m0", "Summary of the conversation so far: …")]);

    assert!(rendered.text.is_empty(), "rendered: {}", rendered.text);
    assert_eq!(rendered.assistant_turns_dropped, 1);
}

/// A `Peer` part is another agent's words and is treated as the assistant's
/// for this rule.
#[test]
fn another_agents_words_are_rendered_no_more_than_the_assistants() {
    let mut message = Message::user("");
    message.id = crate::protocol::MessageId::from("m1".to_owned());
    message.parts = vec![Part {
        id: crate::protocol::PartId::ascending(),
        body: PartBody::Peer {
            from: "backend@session-1".to_owned(),
            summary: None,
            color: None,
            body: "I finished the migration".to_owned(),
        },
    }];

    let rendered = render(&[message]);

    assert!(rendered.text.is_empty());
    assert!(rendered.user_ids.is_empty(), "a message with no rendered line is not a write");
}

// ------------------------------------------------------- the recover arm

#[test]
fn a_recovered_turn_renders_its_prompt_and_its_tool_trail_and_closes_with_the_resume_line() {
    let turn = [
        user("m4", "read the file and tell me what it says"),
        called("m5", "toolu_1", "read", json!({"path": "/f"}), "contents"),
    ];

    let rendered = render_turn(&turn);

    assert!(rendered.text.contains("[User] read the file and tell me what it says"));
    assert!(rendered.text.contains("[Tool Result]\ncontents"));
    assert!(rendered.text.ends_with(MID_TURN_RESUME));
    assert_eq!(rendered.user_ids, ["m4"], "the turn's prompt is a write too");
}

#[test]
fn a_recovered_turn_never_renders_the_partial_reply_it_had_produced() {
    let turn = [
        user("m4", "read it"),
        assistant("m5", "Let me read that for you"),
        called("m6", "toolu_1", "read", json!({}), "contents"),
    ];

    let rendered = render_turn(&turn);

    assert!(!rendered.text.lines().any(|line| line.starts_with("[Assistant]")));
    assert!(!rendered.text.contains("Let me read that for you"));
}

#[test]
fn a_turn_with_nothing_renderable_still_says_the_calls_were_answered() {
    assert_eq!(render_turn(&[]).text, MID_TURN_RESUME);
}

// ------------------------------------------------- the deny path's carry

#[test]
fn a_carried_message_is_introduced_by_the_header_that_says_who_said_it() {
    let steer = user("m9", "actually, stop");

    assert_eq!(carried(&steer), "[User, while the tool ran] actually, stop");
}

/// Both constants are literals, pinned here, because a reword changes what
/// the model reads and nothing else would say so.
#[test]
fn the_two_constants_read_as_they_are_spelled() {
    assert_eq!(HEADER, "[Conversation so far]");
    assert_eq!(MID_TURN_HEADER, "[User, while the tool ran]");
    assert_eq!(MID_TURN_RESUME, "[the tool calls above have been answered; continue the turn]");
}

/// The preamble never carries a mid-turn block by construction: it renders
/// ganja's own messages, in which a steer is an ordinary user message.
#[test]
fn a_steer_inside_the_history_renders_as_an_ordinary_user_line() {
    let rendered = render(&[user("m1", "go"), user("m2", "actually, stop")]);

    assert!(!rendered.text.contains(MID_TURN_HEADER));
    assert_eq!(rendered.text.matches("[User]").count(), 2);
}

// -------------------------------------------------------- message_text

/// Every `PartBody` variant, named — **no wildcard**, so a variant added to
/// the protocol is a decision somebody has to make here rather than a silent
/// omission.
#[test]
fn every_part_kind_says_what_it_contributes_to_a_frames_text() {
    let mut message = Message::user("");
    message.parts = vec![
        Part::text("the words"),
        Part {
            id: crate::protocol::PartId::ascending(),
            body: PartBody::File {
                path: "/tmp/diagram.png".to_owned(),
                mime: "image/png".to_owned(),
                start: None,
                end: None,
                content: None,
            },
        },
        Part::tool("toolu_1", "read"),
        Part {
            id: crate::protocol::PartId::ascending(),
            body: PartBody::Peer {
                from: "backend".to_owned(),
                summary: None,
                color: None,
                body: "hello".to_owned(),
            },
        },
        Part { id: crate::protocol::PartId::ascending(), body: PartBody::StepStart },
        Part {
            id: crate::protocol::PartId::ascending(),
            body: PartBody::StepFinish { usage: Usage::default() },
        },
        Part {
            id: crate::protocol::PartId::ascending(),
            body: PartBody::Reasoning {
                provider: "anthropic".to_owned(),
                item: "sealed".to_owned(),
                encrypted: None,
            },
        },
        Part {
            id: crate::protocol::PartId::ascending(),
            body: PartBody::ReasoningText { text: "thinking out loud".to_owned() },
        },
        Part {
            id: crate::protocol::PartId::ascending(),
            body: PartBody::ServerTool {
                tool: "openrouter:web_search".to_owned(),
                input: json!({}),
                output: "results".to_owned(),
            },
        },
        Part {
            id: crate::protocol::PartId::ascending(),
            body: PartBody::Patch { hash: "deadbeef".to_owned(), files: Vec::new() },
        },
    ];

    let text = message_text(&message);

    // Two contribute: the words, and a file degraded to its name — this wire
    // carries no attachment, so naming it is better than a silence.
    assert_eq!(text, "the words\n\n[attached: /tmp/diagram.png]");
    for absent in ["read", "hello", "thinking out loud", "results", "deadbeef", "sealed"] {
        assert!(!text.contains(absent), "{absent} must contribute nothing: {text}");
    }
}

#[test]
fn a_messages_text_parts_are_joined_by_a_blank_line() {
    let mut message = Message::user("first");
    message.parts.push(Part::text("second"));

    assert_eq!(message_text(&message), "first\n\nsecond");
}

/// The bound one `[Tool Call]` input is cut at, shared with the renderer
/// D553 wrote rather than restated.
#[test]
fn a_huge_tool_input_is_cut_at_the_shared_bound() {
    let huge = "x".repeat(super::INPUT_LIMIT * 2);
    let history =
        [user("m1", "go"), called("m2", "toolu_1", "write", json!({"content": huge}), "done")];

    let rendered = render(&history);
    let call =
        rendered.text.lines().find(|line| line.starts_with("[Tool Call]")).expect("a call line");

    assert!(call.len() < super::INPUT_LIMIT + 64, "the input is clamped, not carried whole");
}
