use futures::StreamExt as _;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{
    ANTHROPIC_CAP, Aliases, AnthropicProvider, Body, DEFAULT_MAX_TOKENS, Frame, ID, Mapper as _,
    Mapping, NO_RESULT, alias,
};
use crate::catalog;
use crate::protocol::{FinishReason, Message, Part, PartBody, PartId, ToolState, Usage};
use crate::provider::{
    ChatRequest, PROVIDERS, Provider as _, ProviderError, ProviderEvent, replay, splice_effort,
};
use crate::tool::ToolDefinition;

/// The obligation `catalog`'s own table test states per *tier* and each
/// wire states for itself: a provider a session can select has to be one
/// the catalog can size and price, or the first turn has no model to ask
/// for and no cost to report.
///
/// Named per provider rather than derived, because the tier predicate
/// deliberately excuses a provider with no rows — a wire that lost its
/// rows would pass there and has to fail here.
#[test]
fn an_anthropic_session_that_names_no_model_gets_one_the_catalog_can_price() {
    assert!(PROVIDERS.contains(&ID));

    let id = catalog::default_model(ID).expect("anthropic has a pinned default");
    let info = catalog::model(id).expect("the default is in the table");

    assert_eq!(info.provider_id, ID);
    assert!(info.context_window > 0 && info.max_output > 0);
    assert!(
        info.pricing.input > 0.0 && info.pricing.output > 0.0,
        "a priced provider with a free row is a row nobody filled in"
    );
}

/// Runs a recorded transcript through the real splitter and mapper.
async fn events(transcript: &'static str) -> Vec<ProviderEvent> {
    replay(transcript, CancellationToken::new(), Mapping::default()).collect().await
}

/// The reply text a transcript streams.
fn text(events: &[ProviderEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_happy_path_transcript_maps_to_text_reasoning_and_a_bill() {
    let seen = events(include_str!("../../tests/fixtures/anthropic_happy_path.sse")).await;

    assert_eq!(text(&seen), "Hello, world!");
    assert_eq!(
        seen.iter()
            .filter_map(|event| match event {
                ProviderEvent::ReasoningDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect::<String>(),
        "The user wants a greeting.",
        "a thinking block should become reasoning, not reply text"
    );
    assert_eq!(
        &seen[seen.len() - 2..],
        &[
            ProviderEvent::Usage(Usage {
                input_tokens: 1_024,
                output_tokens: 12,
                reasoning_tokens: 0,
                cache_read_tokens: 768,
                cache_write_tokens: 256,
            }),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "the bill is reported before the finish, got {seen:?}"
    );
    assert!(
        !seen.iter().any(|event| matches!(event, ProviderEvent::Failed(_))),
        "ping, comments, signature deltas and an unknown event type are all skipped, \
             not fatal: {seen:?}"
    );
}

#[tokio::test]
async fn an_error_frame_ends_the_turn_as_a_failure() {
    let seen = events(include_str!("../../tests/fixtures/anthropic_mid_stream_error.sse")).await;

    assert_eq!(text(&seen), "Let me start by", "partial text is kept");
    assert_eq!(
        seen.last(),
        Some(&ProviderEvent::Failed(ProviderError::Status {
            status: 529,
            message: "Overloaded".to_owned(),
        })),
        "an overload should map to the status that makes it retryable, got {seen:?}"
    );
}

#[tokio::test]
async fn tool_blocks_interleave_with_text_without_losing_either() {
    let seen =
        events(include_str!("../../tests/fixtures/anthropic_tool_use_interleaved.sse")).await;

    assert_eq!(text(&seen), "Reading the file first. And listing the directory.");
    assert_eq!(
        seen.iter()
            .filter(|event| !matches!(event, ProviderEvent::TextDelta(_)))
            .collect::<Vec<_>>(),
        vec![
            &ProviderEvent::ToolCallStart {
                id: "toolu_01Read".to_owned(),
                name: "read".to_owned()
            },
            &ProviderEvent::ToolCallDelta {
                id: "toolu_01Read".to_owned(),
                json: "{\"file".to_owned()
            },
            &ProviderEvent::ToolCallDelta {
                id: "toolu_01Read".to_owned(),
                json: "Path\":\"src/main.rs\"}".to_owned()
            },
            &ProviderEvent::ToolCallEnd { id: "toolu_01Read".to_owned() },
            &ProviderEvent::ToolCallStart {
                id: "toolu_01Glob".to_owned(),
                name: "glob".to_owned()
            },
            &ProviderEvent::ToolCallDelta {
                id: "toolu_01Glob".to_owned(),
                json: "{\"pattern\":\"**/*.rs\"}".to_owned()
            },
            &ProviderEvent::ToolCallEnd { id: "toolu_01Glob".to_owned() },
            &ProviderEvent::Usage(Usage {
                input_tokens: 211,
                output_tokens: 94,
                cache_read_tokens: 128,
                ..Usage::default()
            }),
            &ProviderEvent::Finish(FinishReason::Completed),
        ],
        "every call should be opened, filled and closed exactly once"
    );
}

/// A call is executed when its arguments end, so closing one whose
/// arguments never arrived would run a tool on half a request. A stream
/// that died mid-call has to end as a failure with the call still open.
#[tokio::test]
async fn a_stream_that_dies_mid_call_never_closes_it() {
    let seen = events(include_str!("../../tests/fixtures/anthropic_tool_call_cut_short.sse")).await;

    assert_eq!(text(&seen), "Let me read that file.");
    assert_eq!(
        seen.iter()
            .filter(|event| !matches!(event, ProviderEvent::TextDelta(_)))
            .collect::<Vec<_>>(),
        vec![
            &ProviderEvent::ToolCallStart { id: "toolu_01Cut".to_owned(), name: "read".to_owned() },
            // The fragment the body was cut in half of never arrives: an
            // incomplete frame is not a frame.
            &ProviderEvent::ToolCallDelta {
                id: "toolu_01Cut".to_owned(),
                json: "{\"file".to_owned(),
            },
            &ProviderEvent::Failed(ProviderError::Transport(
                "the response body ended before the model finished".to_owned()
            )),
        ],
        "got {seen:?}"
    );
}

#[tokio::test]
async fn a_body_that_stops_mid_reply_fails_rather_than_completing() {
    let seen = events(include_str!("../../tests/fixtures/anthropic_truncated.sse")).await;

    assert_eq!(text(&seen), "The connection drops right");
    assert!(
        matches!(seen.last(), Some(ProviderEvent::Failed(ProviderError::Transport(_)))),
        "a dropped connection must never read as a finished turn, got {seen:?}"
    );
}

#[tokio::test]
async fn a_malformed_frame_ends_the_turn_rather_than_silently_skipping_it() {
    let seen = replay(
        "event: message_start\ndata: {\"type\":\"message_start\"\n\n",
        CancellationToken::new(),
        Mapping::default(),
    )
    .collect::<Vec<_>>()
    .await;

    assert!(
        matches!(seen.as_slice(), [ProviderEvent::Failed(ProviderError::Parse(_))]),
        "got {seen:?}"
    );
}

#[tokio::test]
async fn a_cancel_mid_transcript_ends_the_stream_without_a_verdict() {
    let cancel = CancellationToken::new();
    let mut stream = Box::pin(replay(
        include_str!("../../tests/fixtures/anthropic_happy_path.sse"),
        cancel.clone(),
        Mapping::default(),
    ));

    assert_eq!(
        stream.next().await,
        Some(ProviderEvent::ReasoningDelta("The user wants a greeting.".to_owned()))
    );
    cancel.cancel();

    let rest: Vec<ProviderEvent> = stream.collect().await;
    assert!(
        rest.is_empty(),
        "a cancelled stream ends; the engine is what calls that Cancelled, and it \
             cannot if a Finish or a Failed arrives: {rest:?}"
    );
}

#[test]
fn a_request_carries_the_transcript_and_the_system_prompt() {
    let mut empty = Message::assistant("claude");
    empty.parts.push(Part::text(""));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: Some("be brief".to_owned()),
        tools: Vec::new(),
        messages: vec![
            Message::user("hello"),
            Message::assistant("claude"),
            empty,
            Message::user("again"),
        ],
    };

    let body =
        serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS)).expect("the body serializes");

    assert_eq!(
        body,
        serde_json::json!({
            "model": "claude-test",
            "max_tokens": DEFAULT_MAX_TOKENS,
            "stream": true,
            "system": "be brief",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "user", "content": "again"},
            ],
        }),
        "a message with nothing in it is not sent: the API rejects it"
    );
}

/// Readable thinking is display-only, and this is the wire where that
/// stops being prose and starts being a test (bead `pwe`).
///
/// The invariant was held by one arm of one match — move
/// `PartBody::ReasoningText` up into the text arm and every later request
/// silently starts carrying the model's scratch paper as if the model had
/// said it. The transcript here is what a real reasoning turn leaves
/// behind: a thought, a reply, and the sealed state this API has no item
/// for either.
#[test]
fn a_transcript_held_thought_is_absent_from_the_body_this_wire_sends() {
    const THOUGHT: &str = "the-user-is-probably-testing-me";

    let mut turn = Message::assistant("claude");
    turn.parts.push(Part::reasoning_text(THOUGHT));
    turn.parts.push(Part::text("Hello!"));
    turn.parts.push(Part::reasoning("anthropic", "rs_1", Some("sealed-blob-0001".to_owned())));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: None,
        tools: Vec::new(),
        messages: vec![Message::user("hi"), turn, Message::user("again")],
    };
    let body = serde_json::to_string(&Body::new(&request, DEFAULT_MAX_TOKENS))
        .expect("the body serializes");

    assert!(
        !body.contains(THOUGHT),
        "the thought reached the wire; nothing sends readable reasoning: {body}"
    );
    assert!(
        !body.contains("sealed-blob-0001"),
        "this API's own thinking block is not ported, so a foreign wire's \
             sealed state must not travel either: {body}"
    );
    assert!(
        body.contains("Hello!"),
        "the reply still has to be sent — an assertion that passed by \
             encoding nothing would prove nothing: {body}"
    );
}

#[test]
fn a_request_without_a_system_prompt_omits_the_field() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
    };
    let body = serde_json::to_string(&Body::new(&request, 16)).expect("the body serializes");

    assert!(!body.contains("system"), "got {body}");
    assert!(body.contains(r#""max_tokens":16"#), "got {body}");
}

/// The splice order at this wire's send site: an effort adds what the body
/// does not carry — `thinking` is the catalog's use of it — and loses every
/// key the wire itself writes, because the wire's fields are what make the
/// request one the Messages API accepts.
#[test]
fn an_effort_adds_thinking_but_cannot_claim_max_tokens() {
    let request = ChatRequest {
        effort_options: serde_json::json!({
            "thinking": {"type": "enabled", "budget_tokens": 16000},
            "max_tokens": 1,
        })
        .as_object()
        .cloned()
        .expect("the fixture options are an object"),
        model: "claude-test".to_owned(),
        system: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
    };

    let own = Body::new(&request, DEFAULT_MAX_TOKENS);
    let body = serde_json::to_value(splice_effort(&request.effort_options, &own))
        .expect("a spliced body serializes");

    assert_eq!(
        body["thinking"],
        serde_json::json!({"type": "enabled", "budget_tokens": 16000}),
        "a key the wire does not write arrives verbatim"
    );
    assert_eq!(
        body["max_tokens"],
        serde_json::json!(DEFAULT_MAX_TOKENS),
        "a key the wire writes resolves to the wire"
    );
    assert_eq!(body["model"], serde_json::json!("claude-test"));
}

/// The exact source-block shapes the Messages API documents, pinned so a
/// drift in the encoder is a red test rather than a 400 from the vendor.
/// An image rides an `image` block and a PDF a `document` block, both with
/// the base64 the engine encoded at send time; a file part that carries no
/// content is a reference the engine already resolved, and encodes nothing.
#[test]
fn an_attachment_becomes_the_source_block_its_mime_names() {
    let mut user = Message::user("what are these");
    user.parts.push(Part {
        id: PartId::ascending(),
        body: PartBody::File {
            path: "shot.png".to_owned(),
            mime: "image/png".to_owned(),
            start: None,
            end: None,
            content: Some("aW1n".to_owned()),
        },
    });
    user.parts.push(Part {
        id: PartId::ascending(),
        body: PartBody::File {
            path: "paper.pdf".to_owned(),
            mime: "application/pdf".to_owned(),
            start: None,
            end: None,
            content: Some("cGRm".to_owned()),
        },
    });
    user.parts.push(Part::file("notes.md", "text/plain"));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: None,
        messages: vec![user],
        tools: Vec::new(),
    };
    let body =
        serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS)).expect("the body serializes");

    assert_eq!(
        body["messages"],
        serde_json::json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "what are these"},
                {
                    "type": "image",
                    "source": {"type": "base64", "media_type": "image/png", "data": "aW1n"},
                },
                {
                    "type": "document",
                    "source": {
                        "type": "base64",
                        "media_type": "application/pdf",
                        "data": "cGRm",
                    },
                },
            ],
        }]),
        "got {body}"
    );
}

/// What the engine consults before it fills a file part in: the mimes the
/// Messages API documents, and nothing else — `image/avif` is on the
/// attachment allowlist and still degrades, because sending a block the
/// vendor does not document is guessing.
#[test]
fn the_wire_accepts_the_mimes_the_api_documents_and_no_others() {
    let provider = AnthropicProvider::new("sk-test-canary-XYZ").expect("a client builds");

    for mime in ["image/jpeg", "image/png", "image/gif", "image/webp", "application/pdf"] {
        assert!(provider.accepts_attachment(mime), "{mime} is documented");
    }
    for mime in ["image/avif", "image/svg+xml", "text/plain", "video/mp4"] {
        assert!(!provider.accepts_attachment(mime), "{mime} degrades");
    }
}

/// A tool part carrying `state`, as an assistant message holds one.
fn tool_part(call_id: &str, tool: &str, state: ToolState) -> Part {
    Part {
        id: PartId::ascending(),
        body: PartBody::Tool { call_id: call_id.to_owned(), tool: tool.to_owned(), state },
    }
}

/// The transcript of a turn that read a file and failed to glob: a step
/// marker, some text, one call that worked, one that did not, and the step
/// marker that closed the request.
fn a_turn_with_two_calls() -> Message {
    let mut assistant = Message::assistant("claude-test");

    assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
    assistant.parts.push(Part::text("Reading the file first."));
    assistant.parts.push(tool_part(
        "toolu_01Read",
        "read",
        ToolState::Completed {
            input: json!({"filePath": "src/main.rs"}),
            output: "fn main() {}".to_owned(),
            title: "src/main.rs".to_owned(),
            metadata: json!({}),
            started: 1,
            completed: 2,
        },
    ));
    assistant.parts.push(tool_part(
        "toolu_01Glob",
        "glob",
        ToolState::Error {
            input: json!({"pattern": "**/*.rs"}),
            error: "no such directory".to_owned(),
            started: 3,
            completed: 4,
        },
    ));
    assistant.parts.push(Part {
        id: PartId::ascending(),
        body: PartBody::StepFinish { usage: Usage::default() },
    });

    assistant
}

/// A request offering `read`, which is what a session with a registry
/// sends on every turn.
fn a_tool() -> ToolDefinition {
    ToolDefinition {
        name: "read".to_owned(),
        description: "Reads a file from disk.".to_owned(),
        schema: json!({
            "type": "object",
            "properties": {"filePath": {"type": "string"}},
            "required": ["filePath"],
        }),
    }
}

/// The live field failure the alias exists for: a plugin-contributed MCP
/// server arrives namespaced `plugin:<name>:<server>` (**D473**), so its
/// tools are named like this — 69 characters, with colons besides. This
/// API's own cap is 128, so what it refuses here is the alphabet rather
/// than the length, and the alias must not truncate what fits.
const REFUSED_NAME: &str = "mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research_result";

/// What [`REFUSED_NAME`] is advertised as under this API's wider cap: the same
/// name with its colons scrubbed, and nothing else.
const ADVERTISED: &str = "mcp__plugin_mcp-gemini-search_mcp-gemini-search__deep_research_result";

/// [`a_tool`] under the name that got a live turn killed.
fn a_refused_tool() -> ToolDefinition {
    ToolDefinition { name: REFUSED_NAME.to_owned(), ..a_tool() }
}

#[test]
fn a_tool_name_this_api_refuses_is_advertised_under_a_conforming_alias() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: None,
        messages: vec![Message::user("research it")],
        tools: vec![a_refused_tool()],
    };

    let body =
        serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS)).expect("the body serializes");

    assert_eq!(body["tools"][0]["name"], ADVERTISED, "got {body}");
    assert_eq!(
        alias(REFUSED_NAME, ANTHROPIC_CAP),
        ADVERTISED,
        "128 characters is room enough that nothing is cut"
    );
}

/// The other half of the same seam. What the engine executes, what the
/// permission rules match and what the transcript records is the registry
/// name, so an alias the model calls back has to be undone before the
/// event leaves the wire.
#[test]
fn a_call_answering_with_the_alias_comes_back_out_under_the_registry_name() {
    let tools = vec![a_refused_tool()];
    let mut mapping = Mapping { aliases: Aliases::of(&tools, ANTHROPIC_CAP), ..Mapping::default() };
    let mut seen = Vec::new();

    mapping.frame(
        &Frame {
            event: Some("content_block_start".to_owned()),
            data: json!({
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_01Research",
                    "name": ADVERTISED,
                    "input": {},
                },
            })
            .to_string(),
        },
        &mut seen,
    );

    assert_eq!(
        seen,
        vec![ProviderEvent::ToolCallStart {
            id: "toolu_01Research".to_owned(),
            name: REFUSED_NAME.to_owned(),
        }],
        "got {seen:?}"
    );
}

/// A call replayed on a later request has to name what that request's own
/// roster named, or the model is handed a trace citing a tool it was never
/// offered. Aliasing is deterministic, so both sides recompute it rather
/// than remembering anything across turns.
#[test]
fn a_completed_call_replays_under_the_same_alias_the_roster_advertises() {
    let mut assistant = Message::assistant("claude-test");
    assistant.parts.push(tool_part(
        "toolu_01Research",
        REFUSED_NAME,
        ToolState::Completed {
            input: json!({"filePath": "src/main.rs"}),
            output: "a report".to_owned(),
            title: "deep research".to_owned(),
            metadata: json!({}),
            started: 1,
            completed: 2,
        },
    ));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: None,
        messages: vec![Message::user("research it"), assistant],
        tools: vec![a_refused_tool()],
    };

    let body =
        serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS)).expect("the body serializes");

    assert_eq!(
        body["messages"][1]["content"][0]["name"], ADVERTISED,
        "the replayed call has to name exactly what the roster named: {body}"
    );
    assert_eq!(body["tools"][0]["name"], ADVERTISED, "got {body}");
}

#[test]
fn a_request_advertises_the_tools_it_was_given() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: None,
        messages: vec![Message::user("read src/main.rs")],
        tools: vec![a_tool()],
    };

    let body =
        serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS)).expect("the body serializes");

    assert_eq!(
        body["tools"],
        json!([{
            "name": "read",
            "description": "Reads a file from disk.",
            "input_schema": {
                "type": "object",
                "properties": {"filePath": {"type": "string"}},
                "required": ["filePath"],
            },
        }]),
        "got {body}"
    );
}

/// A turn that called tools has to read back to the model the way it
/// happened: the calls in the assistant message that made them, and their
/// results in the user message that follows, which is the only place the
/// API accepts them.
#[test]
fn a_finished_call_is_sent_back_as_a_use_and_a_result() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: Some("be brief".to_owned()),
        messages: vec![
            Message::user("read src/main.rs"),
            a_turn_with_two_calls(),
            Message::user("thanks"),
        ],
        tools: vec![a_tool()],
    };

    let body =
        serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS)).expect("the body serializes");

    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "read src/main.rs"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Reading the file first."},
                {
                    "type": "tool_use",
                    "id": "toolu_01Read",
                    "name": "read",
                    "input": {"filePath": "src/main.rs"},
                },
                {
                    "type": "tool_use",
                    "id": "toolu_01Glob",
                    "name": "glob",
                    "input": {"pattern": "**/*.rs"},
                },
            ]},
            // One message for both results, because the API answers a
            // message's calls in the message that follows it.
            {"role": "user", "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_01Read",
                    "content": "fn main() {}",
                },
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_01Glob",
                    "content": "no such directory",
                    "is_error": true,
                },
            ]},
            {"role": "user", "content": "thanks"},
        ]),
        "got {body}"
    );
}

/// The transcript of a turn that took two model requests: it read a file,
/// read what came back, and only then said what it was going to do about
/// it. Both steps are parts of one assistant message, which is what makes
/// the boundary between them worth respecting.
fn a_turn_of_two_steps() -> Message {
    let mut assistant = Message::assistant("claude-test");

    for (text, call_id, tool, input, output) in [
        (
            "Reading.",
            "toolu_01Read",
            "read",
            json!({"filePath": "src/main.rs"}),
            "fn main() { let x = 1; }",
        ),
        (
            "Now editing.",
            "toolu_01Edit",
            "edit",
            json!({"filePath": "src/main.rs", "oldString": "1", "newString": "2"}),
            "1 replacement",
        ),
    ] {
        assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
        assistant.parts.push(Part::text(text));
        assistant.parts.push(tool_part(
            call_id,
            tool,
            ToolState::Completed {
                input,
                output: output.to_owned(),
                title: "src/main.rs".to_owned(),
                metadata: json!({}),
                started: 1,
                completed: 2,
            },
        ));
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepFinish { usage: Usage::default() },
        });
    }

    assistant
}

/// A turn that took two model requests reads back as two of them. The API
/// would accept one flattened message — both calls, then both results, is
/// indistinguishable from parallel tool use — but it would put "Now
/// editing." *before* the read it was said in response to, and a model
/// re-reading its own trace would find its reasoning shuffled out from
/// under the evidence it reasoned from.
#[test]
fn a_two_step_turn_is_sent_back_one_message_pair_per_step() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: None,
        messages: vec![Message::user("fix the bug"), a_turn_of_two_steps()],
        tools: vec![a_tool()],
    };

    let body =
        serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS)).expect("the body serializes");

    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "fix the bug"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Reading."},
                {
                    "type": "tool_use",
                    "id": "toolu_01Read",
                    "name": "read",
                    "input": {"filePath": "src/main.rs"},
                },
            ]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01Read",
                "content": "fn main() { let x = 1; }",
            }]},
            // The second step opens here, after its evidence rather than
            // before it.
            {"role": "assistant", "content": [
                {"type": "text", "text": "Now editing."},
                {
                    "type": "tool_use",
                    "id": "toolu_01Edit",
                    "name": "edit",
                    "input": {
                        "filePath": "src/main.rs",
                        "oldString": "1",
                        "newString": "2",
                    },
                },
            ]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01Edit",
                "content": "1 replacement",
            }]},
        ]),
        "got {body}"
    );

    // The property the shape above exists for, stated on its own so that a
    // future rearrangement of the blocks cannot quietly lose it.
    let wire = body["messages"].to_string();
    let position = |needle: &str| wire.find(needle).expect("the wire holds {needle}");
    assert!(
        position("Now editing.") > position("fn main() { let x = 1; }"),
        "what the model said in the second step must read as having been \
             said after the first step's result came back: {wire}"
    );
}

/// Older stored transcripts and hand-built messages carry no step markers
/// at all. There is one step in that case, not none: everything the message
/// holds, encoded exactly as it was before turns were ever split.
#[test]
fn a_turn_without_step_markers_is_one_step() {
    let mut assistant = Message::assistant("claude-test");
    assistant.parts.push(Part::text("Reading."));
    assistant.parts.push(tool_part(
        "toolu_01Read",
        "read",
        ToolState::Completed {
            input: json!({"filePath": "src/main.rs"}),
            output: "fn main() {}".to_owned(),
            title: "src/main.rs".to_owned(),
            metadata: json!({}),
            started: 1,
            completed: 2,
        },
    ));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: None,
        messages: vec![Message::user("read it"), assistant],
        tools: Vec::new(),
    };

    let body =
        serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS)).expect("the body serializes");

    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "read it"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Reading."},
                {
                    "type": "tool_use",
                    "id": "toolu_01Read",
                    "name": "read",
                    "input": {"filePath": "src/main.rs"},
                },
            ]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01Read",
                "content": "fn main() {}",
            }]},
        ]),
        "got {body}"
    );
}

/// Splitting a turn must never produce two messages in a row with the same
/// role: this API refuses a transcript whose roles do not alternate. Steps
/// alternate on their own whenever each ends in calls, so what is left is
/// the interrupted shape — a step that said something and called nothing,
/// with another step behind it — which was one message before the split and
/// stays one after it.
#[test]
fn two_steps_that_called_nothing_stay_one_message() {
    let mut assistant = Message::assistant("claude-test");
    for text in ["Thinking about it.", "Here is the answer."] {
        assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
        assistant.parts.push(Part::text(text));
    }

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: None,
        messages: vec![Message::user("hi"), assistant, Message::user("thanks")],
        tools: Vec::new(),
    };

    let body =
        serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS)).expect("the body serializes");

    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Thinking about it."},
                {"type": "text", "text": "Here is the answer."},
            ]},
            // And a message of its own is still a message of its own:
            // merging stops at the edge of the one it started in.
            {"role": "user", "content": "thanks"},
        ]),
        "got {body}"
    );
}

/// A turn cancelled while a tool was running leaves a call nobody answered.
/// Sending it as it stands is a request the API refuses outright, and
/// dropping it leaves the reply talking about a call that is not there, so
/// the pair is completed with a placeholder.
#[test]
fn a_call_that_never_finished_is_answered_rather_than_left_dangling() {
    for state in [
        ToolState::Pending { input: None },
        ToolState::Running {
            input: json!({"filePath": "src/main.rs"}),
            metadata: serde_json::Value::Null,
            started: 1,
        },
    ] {
        let running = matches!(state, ToolState::Running { .. });
        let mut assistant = Message::assistant("claude-test");
        assistant.parts.push(tool_part("toolu_01Read", "read", state));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs"), assistant],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert_eq!(
            body["messages"][1],
            json!({"role": "assistant", "content": [{
                "type": "tool_use",
                "id": "toolu_01Read",
                "name": "read",
                // A call the model never finished streaming has no
                // arguments, and the field is required.
                "input": if running { json!({"filePath": "src/main.rs"}) } else { json!({}) },
            }]}),
            "got {body}"
        );
        assert_eq!(
            body["messages"][2],
            json!({"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01Read",
                "content": NO_RESULT,
                "is_error": true,
            }]}),
            "an unanswered call must not reach the API unanswered: {body}"
        );
    }
}

/// Step markers are this crate's bookkeeping — where one model request
/// ended and the next began — rather than anything the model said, so a
/// message of nothing else is not a message at all.
#[test]
fn step_markers_are_not_sent() {
    let mut assistant = Message::assistant("claude-test");
    assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
    assistant.parts.push(Part {
        id: PartId::ascending(),
        body: PartBody::StepFinish { usage: Usage::default() },
    });

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "claude-test".to_owned(),
        system: None,
        messages: vec![Message::user("hi"), assistant],
        tools: Vec::new(),
    };

    let body =
        serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS)).expect("the body serializes");

    assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]), "got {body}");
}

/// Asking a model for more than it will generate is a 400 rather than a
/// longer reply, and the catalog is what knows the difference. Whichever of
/// the two ceilings is lower wins, in both directions.
#[test]
fn the_reply_cap_is_the_lower_of_the_catalog_and_the_configuration() {
    let provider = AnthropicProvider::new("sk-test-canary-XYZ").expect("a client builds");

    assert_eq!(
        provider.max_tokens("claude-test"),
        DEFAULT_MAX_TOKENS,
        "a model the table does not know keeps the configured ceiling"
    );
    assert_eq!(
        provider.max_tokens("claude-sonnet-5"),
        DEFAULT_MAX_TOKENS,
        "a model that will generate more than the cap is still capped: \
             sonnet's own limit is 128k"
    );

    let mut modest = AnthropicProvider::new("sk-test-canary-XYZ").expect("a client builds");
    modest.max_tokens = 4_096;

    assert_eq!(
        modest.max_tokens("claude-sonnet-5"),
        4_096,
        "a caller asking for less than the cap gets what it asked for"
    );

    let mut generous = provider;
    generous.max_tokens = 200_000;

    assert_eq!(
        generous.max_tokens("claude-haiku-4-5"),
        64_000,
        "the model's own limit is the ceiling once it is the smaller one"
    );
    assert_eq!(
        generous.max_tokens("claude-test"),
        200_000,
        "and an unknown model is still asked for what the caller configured"
    );
}

/// Both credentials a provider holds: the key it was built with, and
/// whatever the base URL carries. The second is configuration rather than
/// something this build asked for, which makes it easy to forget and no
/// less of a secret. `Debug` is the whole surface — there is no `Display`
/// for a provider — and it is what every `tracing` field holding one
/// renders through.
#[test]
fn a_provider_never_renders_its_credential() {
    // Both shapes `check_base_url` blesses: a gateway reached over https,
    // and the loopback endpoint the integration suite itself points at —
    // which is where a userinfo-bearing base URL actually shows up today.
    let cases = [
        (
            "https://ganja:sk-url-canary-9999@gateway.invalid:8443/v1?token=sk-query-canary-7777",
            "gateway.invalid:8443",
        ),
        ("http://ganja:sk-url-canary-9999@127.0.0.1:8080", "127.0.0.1:8080"),
    ];

    for (base_url, endpoint) in cases {
        let provider = AnthropicProvider::new("sk-test-canary-XYZ")
            .expect("an HTTP client builds")
            .with_base_url(base_url);

        let rendered = format!("{provider:?}");

        for secret in ["sk-test-canary-XYZ", "sk-url-canary-9999", "sk-query-canary-7777", "ganja:"]
        {
            assert!(!rendered.contains(secret), "a provider leaked {secret}: {rendered}");
        }
        assert!(rendered.contains("[redacted]"), "got {rendered}");
        // Still worth reading: which endpoint this provider is pointed at
        // is the first thing anyone debugging one wants to know.
        assert!(
            rendered.contains(endpoint),
            "the endpoint should survive being made safe to print: {rendered}"
        );
    }
}

#[test]
fn a_blank_credential_is_refused() {
    assert!(AnthropicProvider::new("  ").is_err());
}

#[tokio::test]
async fn a_request_that_cannot_be_built_reports_why_without_the_endpoint() {
    // A newline cannot go in a header value, so the request fails to build
    // after `check_base_url` has passed. Nothing strips the base URL out of
    // the message: a builder error carries no URL, and a `reqwest::Error`
    // that does carry one renders it with its credentials already removed.
    // Both are the dependency's behaviour rather than this crate's, which
    // is why they are worth holding here.
    let provider = AnthropicProvider::new("sk-test-canary-XYZ\nnewline")
        .expect("an HTTP client builds")
        .with_base_url(
            "https://ganja:sk-url-canary-9999@gateway.invalid:8443/v1?token=sk-query-canary-7777",
        );

    let opened = provider
        .stream(
            ChatRequest {
                effort_options: Default::default(),
                model: "claude-sonnet-5".to_owned(),
                system: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await;

    // A stream is not `Debug`, so this cannot go through `expect_err`.
    let Err(error) = opened else {
        panic!("a header value with a newline cannot be built");
    };

    let rendered = format!("{error} / {error:?}");
    for secret in ["sk-test-canary-XYZ", "sk-url-canary-9999", "sk-query-canary-7777", "ganja:"] {
        assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
    }
    assert!(
        rendered.contains("malformed request"),
        "the failure should still say what happened: {rendered}"
    );
}
