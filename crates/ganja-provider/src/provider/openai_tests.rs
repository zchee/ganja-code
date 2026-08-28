use futures::StreamExt as _;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{
    Aliases, Body, Frame, Mapper as _, Mapping, NO_RESULT, OPENAI_CAP, OpenAiProvider, alias,
};
use crate::catalog;
use crate::protocol::{FinishReason, Message, Part, PartBody, PartId, ToolState, Usage};
use crate::provider::{ChatRequest, ProviderError, ProviderEvent, replay, splice_effort};
use crate::tool::ToolDefinition;

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
    let seen = events(include_str!("../../tests/fixtures/openai_happy_path.sse")).await;

    assert_eq!(text(&seen), "Hello, world!");
    assert!(
        seen.contains(&ProviderEvent::ReasoningDelta("A greeting is enough.".to_owned())),
        "reasoning_content should not be dropped, got {seen:?}"
    );
    assert_eq!(
        &seen[seen.len() - 2..],
        &[
            ProviderEvent::Usage(Usage {
                // The chunk says `prompt_tokens: 42` with 16 of them
                // cached, and `Usage` keeps its counters disjoint, so the
                // fresh half of that prompt is 26.
                input_tokens: 26,
                output_tokens: 9,
                reasoning_tokens: 4,
                cache_read_tokens: 16,
                cache_write_tokens: 0,
            }),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "the trailing usage chunk should be reported before the finish, got {seen:?}"
    );
}

/// This API reports the whole prompt as `prompt_tokens` and then says how
/// much of it the cache served; [`Usage`] keeps the two apart so each can be
/// billed at its own rate, a cache read costing a fraction of fresh input.
/// Handing both counts through unchanged bills the cached half twice.
#[tokio::test]
async fn a_cached_prompt_reports_only_its_fresh_half_as_input() {
    let cases = [
        (
            "a cached prompt bills only what the cache did not serve",
            concat!(
                r#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}],"#,
                r#""usage":{"prompt_tokens":1000,"completion_tokens":20,"#,
                r#""prompt_tokens_details":{"cached_tokens":800}}}"#,
                "\n\ndata: [DONE]\n\n",
            ),
            Usage {
                input_tokens: 200,
                output_tokens: 20,
                cache_read_tokens: 800,
                ..Usage::default()
            },
        ),
        (
            "an endpoint claiming more cached tokens than prompt tokens reads as \
                 nothing fresh rather than wrapping into a bill nobody owes",
            concat!(
                r#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}],"#,
                r#""usage":{"prompt_tokens":100,"completion_tokens":5,"#,
                r#""prompt_tokens_details":{"cached_tokens":900}}}"#,
                "\n\ndata: [DONE]\n\n",
            ),
            Usage { input_tokens: 0, output_tokens: 5, cache_read_tokens: 900, ..Usage::default() },
        ),
        (
            "a prompt nothing was cached for is fresh in full",
            concat!(
                r#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}],"#,
                r#""usage":{"prompt_tokens":1000,"completion_tokens":20}}"#,
                "\n\ndata: [DONE]\n\n",
            ),
            Usage { input_tokens: 1_000, output_tokens: 20, ..Usage::default() },
        ),
    ];

    for (name, transcript, expected) in cases {
        let seen = events(transcript).await;

        assert!(seen.contains(&ProviderEvent::Usage(expected)), "{name}: got {seen:?}");
    }
}

/// The bill the corrected counts actually produce. Priced apart, a heavily
/// cached prompt costs a fraction of what the same tokens would fresh —
/// which is exactly the difference double-counting used to erase.
#[test]
fn a_cached_prompt_is_billed_once_rather_than_twice() {
    let model = catalog::model("gpt-5.6").expect("the catalog knows the model");
    let corrected = Usage {
        input_tokens: 200_000,
        output_tokens: 0,
        cache_read_tokens: 800_000,
        ..Usage::default()
    };
    // What the same response cost before the cached tokens came back out of
    // `prompt_tokens`: the whole million counted as fresh input *and* the
    // cached 800k counted again beside it.
    let doubled = Usage { input_tokens: 1_000_000, ..corrected };

    let billed = catalog::cost(&corrected, &model).total_usd;
    let expected = model.pricing.input * 0.2 + model.pricing.cache_read * 0.8;

    assert!(
        (billed - expected).abs() < 1e-9,
        "200k fresh at ${}/Mtok plus 800k cached at ${}/Mtok is ${expected}, got ${billed}",
        model.pricing.input,
        model.pricing.cache_read,
    );
    assert!(
        catalog::cost(&doubled, &model).total_usd > billed * 3.0,
        "the old counts over-reported by more than a factor of three, which is \
             the size of the error this pins"
    );
}

#[tokio::test]
async fn tool_calls_are_opened_filled_and_closed() {
    let seen = events(include_str!("../../tests/fixtures/openai_tool_calls.sse")).await;

    assert_eq!(text(&seen), "Reading the file first.");
    assert_eq!(
        seen.iter()
            .filter(|event| !matches!(event, ProviderEvent::TextDelta(_) | ProviderEvent::Usage(_)))
            .collect::<Vec<_>>(),
        vec![
            &ProviderEvent::ToolCallStart { id: "call_read".to_owned(), name: "read".to_owned() },
            &ProviderEvent::ToolCallDelta {
                id: "call_read".to_owned(),
                json: "{\"file".to_owned()
            },
            &ProviderEvent::ToolCallDelta {
                id: "call_read".to_owned(),
                json: "Path\":\"src/main.rs\"}".to_owned()
            },
            &ProviderEvent::ToolCallStart { id: "call_glob".to_owned(), name: "glob".to_owned() },
            &ProviderEvent::ToolCallDelta {
                id: "call_glob".to_owned(),
                json: "{\"pattern\":\"**/*.rs\"}".to_owned()
            },
            // Chat completions has no per-call terminator, so both calls
            // close when the stream does, in index order.
            &ProviderEvent::ToolCallEnd { id: "call_read".to_owned() },
            &ProviderEvent::ToolCallEnd { id: "call_glob".to_owned() },
            &ProviderEvent::Finish(FinishReason::Completed),
        ]
    );
}

/// A model that talks while it calls, which chat completions carries as
/// content and `tool_calls` in the same chunk. Neither may swallow the
/// other, and a call's fragments have to find their way back to the call
/// they belong to across everything in between.
#[tokio::test]
async fn text_and_a_fragmented_call_interleave_without_losing_either() {
    let seen = events(include_str!("../../tests/fixtures/openai_tool_calls_interleaved.sse")).await;

    assert_eq!(text(&seen), "Reading the file first. Then the directory.");
    assert_eq!(
        seen.iter()
            .filter(|event| !matches!(event, ProviderEvent::TextDelta(_)))
            .collect::<Vec<_>>(),
        vec![
            &ProviderEvent::ToolCallStart { id: "call_read".to_owned(), name: "read".to_owned() },
            &ProviderEvent::ToolCallDelta {
                id: "call_read".to_owned(),
                json: "{\"file".to_owned(),
            },
            &ProviderEvent::ToolCallDelta {
                id: "call_read".to_owned(),
                json: "Path\":\"src/".to_owned(),
            },
            &ProviderEvent::ToolCallDelta {
                id: "call_read".to_owned(),
                json: "main.rs\"}".to_owned(),
            },
            &ProviderEvent::ToolCallStart { id: "call_glob".to_owned(), name: "glob".to_owned() },
            &ProviderEvent::ToolCallDelta {
                id: "call_glob".to_owned(),
                json: "{\"pattern\"".to_owned(),
            },
            &ProviderEvent::ToolCallDelta {
                id: "call_glob".to_owned(),
                json: ":\"**/*.rs\"}".to_owned(),
            },
            // Chat completions has no per-call terminator, so both calls
            // close when the stream does, in index order.
            &ProviderEvent::ToolCallEnd { id: "call_read".to_owned() },
            &ProviderEvent::ToolCallEnd { id: "call_glob".to_owned() },
            // 317 prompt tokens of which the cache served 256: 61 fresh.
            &ProviderEvent::Usage(Usage {
                input_tokens: 61,
                output_tokens: 58,
                cache_read_tokens: 256,
                ..Usage::default()
            }),
            &ProviderEvent::Finish(FinishReason::Completed),
        ],
        "got {seen:?}"
    );
}

/// A call is executed when its arguments end, so closing one whose
/// arguments never arrived would run a tool on half a request. A stream
/// that died mid-call has to end as a failure with the call still open —
/// which for this API means the `[DONE]` that closes calls never came.
#[tokio::test]
async fn a_stream_that_dies_mid_call_never_closes_it() {
    let seen = events(include_str!("../../tests/fixtures/openai_tool_call_cut_short.sse")).await;

    assert_eq!(text(&seen), "Let me read that file.");
    assert_eq!(
        seen.iter()
            .filter(|event| !matches!(event, ProviderEvent::TextDelta(_)))
            .collect::<Vec<_>>(),
        vec![
            &ProviderEvent::ToolCallStart { id: "call_cut".to_owned(), name: "read".to_owned() },
            // The chunk the body was cut in half of never arrives: an
            // incomplete frame is not a frame.
            &ProviderEvent::ToolCallDelta { id: "call_cut".to_owned(), json: "{\"file".to_owned() },
            &ProviderEvent::Failed(ProviderError::Transport(
                "the response body ended before the model finished".to_owned()
            )),
        ],
        "got {seen:?}"
    );
}

/// Pins the choice: a chunk that will not parse ends the turn. Skipping it
/// would drop reply text with nothing downstream able to tell that a gap
/// exists, and a transcript with a silent hole in it is worse than one that
/// says it broke.
#[tokio::test]
async fn a_malformed_chunk_ends_the_turn_rather_than_being_skipped() {
    let seen = events(include_str!("../../tests/fixtures/openai_malformed_frame.sse")).await;

    assert_eq!(text(&seen), "Hello", "text before the break is kept");
    assert!(
        matches!(seen.last(), Some(ProviderEvent::Failed(ProviderError::Parse(_)))),
        "got {seen:?}"
    );
    assert_eq!(seen.len(), 2, "nothing after the broken chunk is read, got {seen:?}");
}

#[tokio::test]
async fn a_body_that_stops_mid_reply_fails_rather_than_completing() {
    let seen = events(include_str!("../../tests/fixtures/openai_truncated.sse")).await;

    assert_eq!(text(&seen), "The connection drops right");
    assert!(
        matches!(seen.last(), Some(ProviderEvent::Failed(ProviderError::Transport(_)))),
        "a dropped connection must never read as a finished turn, got {seen:?}"
    );
}

#[tokio::test]
async fn a_missing_done_sentinel_after_a_finish_reason_still_completes() {
    // Plenty of compatible servers just close the socket. The model said it
    // stopped, so the reply is whole and only the sentinel went missing.
    let seen = events(concat!(
        r#"data: {"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "\n\n",
    ))
    .await;

    assert_eq!(text(&seen), "hi");
    assert_eq!(seen.last(), Some(&ProviderEvent::Finish(FinishReason::Completed)));
}

#[tokio::test]
async fn an_error_chunk_ends_the_turn_as_a_failure() {
    let seen = events(concat!(
        r#"data: {"choices":[{"index":0,"delta":{"content":"partial"}}]}"#,
        "\n\n",
        r#"data: {"error":{"message":"upstream capacity exceeded","type":"server_error"}}"#,
        "\n\n",
    ))
    .await;

    assert_eq!(text(&seen), "partial");
    assert_eq!(
        seen.last(),
        Some(&ProviderEvent::Failed(ProviderError::Status {
            status: 500,
            message: "upstream capacity exceeded".to_owned(),
        }))
    );
}

#[tokio::test]
async fn a_cancel_mid_transcript_ends_the_stream_without_a_verdict() {
    let cancel = CancellationToken::new();
    let mut stream = Box::pin(replay(
        include_str!("../../tests/fixtures/openai_happy_path.sse"),
        cancel.clone(),
        Mapping::default(),
    ));

    assert_eq!(
        stream.next().await,
        Some(ProviderEvent::ReasoningDelta("A greeting is enough.".to_owned()))
    );
    cancel.cancel();

    let rest: Vec<ProviderEvent> = stream.collect().await;
    assert!(rest.is_empty(), "a cancelled stream ends: {rest:?}");
}

#[test]
fn the_system_prompt_becomes_the_first_message() {
    let mut empty = Message::assistant("gpt");
    empty.parts.push(Part::text(""));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: Some("be brief".to_owned()),
        messages: vec![Message::user("hello"), empty, Message::user("again")],
        tools: Vec::new(),
    };

    let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

    assert_eq!(
        body,
        serde_json::json!({
            "model": "gpt-test",
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hello"},
                {"role": "user", "content": "again"},
            ],
        })
    );
}

/// The same lock the other two wires carry (bead `pwe`): thinking this
/// build renders never reaches a body this build sends.
///
/// Sharper here than anywhere else, because this wire has *no* reasoning
/// item at all — sealed or readable — so a `ReasoningText` that escaped
/// its arm could only arrive as content, indistinguishable from something
/// the model told the user.
#[test]
fn a_transcript_held_thought_is_absent_from_the_body_this_wire_sends() {
    const THOUGHT: &str = "the-user-is-probably-testing-me";

    let mut turn = Message::assistant("gpt-test");
    turn.parts.push(Part::reasoning_text(THOUGHT));
    turn.parts.push(Part::text("Hello!"));
    turn.parts.push(Part::reasoning("openai", "rs_1", Some("sealed-blob-0001".to_owned())));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        tools: Vec::new(),
        messages: vec![Message::user("hi"), turn, Message::user("again")],
    };
    let body = serde_json::to_string(&Body::new(&request)).expect("the body serializes");

    assert!(
        !body.contains(THOUGHT),
        "the thought reached the wire; nothing sends readable reasoning: {body}"
    );
    assert!(
        !body.contains("sealed-blob-0001"),
        "chat completions has no item for sealed reasoning, so it must \
             not be smuggled in as content either: {body}"
    );
    assert!(
        body.contains("Hello!"),
        "the reply still has to be sent — an assertion that passed by \
             encoding nothing would prove nothing: {body}"
    );
}

#[test]
fn a_request_without_a_system_prompt_starts_with_the_user() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
    };
    let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

    assert_eq!(body["messages"], serde_json::json!([{"role": "user", "content": "hi"}]));
}

/// The splice order at this wire's send site: the map passes through
/// verbatim — this wire maps none of its keys — and still loses every key
/// the wire itself writes.
#[test]
fn an_effort_passes_through_but_cannot_claim_the_model() {
    let request = ChatRequest {
        effort_options: serde_json::json!({
            "reasoning_effort": "high",
            "model": "someone-elses",
            "stream": false,
        })
        .as_object()
        .cloned()
        .expect("the fixture options are an object"),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
    };

    let own = Body::new(&request);
    let body = serde_json::to_value(splice_effort(&request.effort_options, &own))
        .expect("a spliced body serializes");

    assert_eq!(
        body["reasoning_effort"],
        serde_json::json!("high"),
        "a key the wire does not write arrives verbatim"
    );
    assert_eq!(
        body["model"],
        serde_json::json!("gpt-test"),
        "a key the wire writes resolves to the wire"
    );
    assert_eq!(body["stream"], serde_json::json!(true));
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
    let mut assistant = Message::assistant("gpt-test");

    assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
    assistant.parts.push(Part::text("Reading the file first."));
    assistant.parts.push(tool_part(
        "call_read",
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
        "call_glob",
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

#[test]
fn a_request_advertises_the_tools_it_was_given() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("read src/main.rs")],
        tools: vec![a_tool()],
    };

    let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

    assert_eq!(
        body["tools"],
        json!([{
            "type": "function",
            "function": {
                "name": "read",
                "description": "Reads a file from disk.",
                "parameters": {
                    "type": "object",
                    "properties": {"filePath": {"type": "string"}},
                    "required": ["filePath"],
                },
            },
        }]),
        "got {body}"
    );
}

/// The live field failure the alias exists for: a plugin-contributed MCP
/// server arrives namespaced `plugin:<name>:<server>` (**D473**), so its
/// tools are named like this — 69 characters, with colons besides, which
/// `meta/muse-spark-1.2` over openrouter refused as
/// ``\`name\` must be at most 64 characters, got 69``.
const REFUSED_NAME: &str = "mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research_result";

/// [`a_tool`] under the name that got a live turn killed.
fn a_refused_tool() -> ToolDefinition {
    ToolDefinition { name: REFUSED_NAME.to_owned(), ..a_tool() }
}

/// Whether `name` is one this API's `^[a-zA-Z0-9_-]{1,64}$` accepts.
fn conforms(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= OPENAI_CAP
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[test]
fn a_tool_name_this_api_refuses_is_advertised_under_a_conforming_alias() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("research it")],
        tools: vec![a_refused_tool()],
    };

    let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");
    let advertised = body["tools"][0]["function"]["name"].as_str().expect("the tool is advertised");

    assert_ne!(advertised, REFUSED_NAME, "the refused name must not go out again");
    assert!(conforms(advertised), "{advertised} is still refusable");
    assert_eq!(
        body["tools"][0]["function"]["description"], "Reads a file from disk.",
        "only the name is answered here: {body}"
    );
}

/// The other half of the same seam. What the engine executes, what the
/// permission rules match and what the transcript records is the registry
/// name, so an alias the model calls back has to be undone before the
/// event leaves the wire.
#[test]
fn a_call_answering_with_the_alias_comes_back_out_under_the_registry_name() {
    let tools = vec![a_refused_tool()];
    let advertised = alias(REFUSED_NAME, OPENAI_CAP).into_owned();
    let mut mapping = Mapping { aliases: Aliases::of(&tools, OPENAI_CAP), ..Mapping::default() };
    let mut seen = Vec::new();

    mapping.frame(
        &Frame {
            event: None,
            data: json!({
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": advertised, "arguments": ""},
                }]}}],
            })
            .to_string(),
        },
        &mut seen,
    );

    assert_eq!(
        seen,
        vec![ProviderEvent::ToolCallStart {
            id: "call_1".to_owned(),
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
    let mut assistant = Message::assistant("gpt-test");
    assistant.parts.push(tool_part(
        "call_1",
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
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("research it"), assistant],
        tools: vec![a_refused_tool()],
    };

    let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");
    let advertised = &body["tools"][0]["function"]["name"];

    assert!(conforms(advertised.as_str().expect("a name")), "got {advertised}");
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["function"]["name"], *advertised,
        "the replayed call has to name exactly what the roster named: {body}"
    );
}

/// A turn that called tools has to read back to the model the way it
/// happened: the calls on the assistant message that made them, and each
/// result as the `tool` message that answers it.
#[test]
fn a_finished_call_is_sent_back_as_a_call_and_a_tool_message() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![
            Message::user("read src/main.rs"),
            a_turn_with_two_calls(),
            Message::user("thanks"),
        ],
        tools: vec![a_tool()],
    };

    let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "read src/main.rs"},
            {"role": "assistant", "content": "Reading the file first.", "tool_calls": [
                {
                    "id": "call_read",
                    "type": "function",
                    // Arguments travel as a string, not as an object: the
                    // model streams them as text and this API carries them
                    // the way it received them.
                    "function": {"name": "read", "arguments": r#"{"filePath":"src/main.rs"}"#},
                },
                {
                    "id": "call_glob",
                    "type": "function",
                    "function": {"name": "glob", "arguments": r#"{"pattern":"**/*.rs"}"#},
                },
            ]},
            {"role": "tool", "content": "fn main() {}", "tool_call_id": "call_read"},
            // A failure has nowhere to be flagged here, so it travels as
            // the text the model reads.
            {"role": "tool", "content": "no such directory", "tool_call_id": "call_glob"},
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
    let mut assistant = Message::assistant("gpt-test");

    for (text, call_id, tool, input, output) in [
        (
            "Reading.",
            "call_read",
            "read",
            json!({"filePath": "src/main.rs"}),
            "fn main() { let x = 1; }",
        ),
        (
            "Now editing.",
            "call_edit",
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
/// would accept one flattened message — both calls in one `tool_calls`
/// array, then both `tool` messages — but it would join "Reading." and "Now
/// editing." into a single content string sitting ahead of every result, so
/// a model re-reading its own trace would find its reasoning shuffled out
/// from under the evidence it reasoned from.
#[test]
fn a_two_step_turn_is_sent_back_one_message_pair_per_step() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("fix the bug"), a_turn_of_two_steps()],
        tools: vec![a_tool()],
    };

    let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "fix the bug"},
            {"role": "assistant", "content": "Reading.", "tool_calls": [{
                "id": "call_read",
                "type": "function",
                "function": {"name": "read", "arguments": r#"{"filePath":"src/main.rs"}"#},
            }]},
            {
                "role": "tool",
                "content": "fn main() { let x = 1; }",
                "tool_call_id": "call_read",
            },
            // The second step opens here, after its evidence rather than
            // before it.
            {"role": "assistant", "content": "Now editing.", "tool_calls": [{
                "id": "call_edit",
                "type": "function",
                "function": {
                    "name": "edit",
                    "arguments":
                        r#"{"filePath":"src/main.rs","newString":"2","oldString":"1"}"#,
                },
            }]},
            {"role": "tool", "content": "1 replacement", "tool_call_id": "call_edit"},
        ]),
        "got {body}"
    );

    // The property the shape above exists for, stated on its own so that a
    // future rearrangement of the messages cannot quietly lose it.
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
    let mut assistant = Message::assistant("gpt-test");
    assistant.parts.push(Part::text("Reading."));
    assistant.parts.push(tool_part(
        "call_read",
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
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("read it"), assistant],
        tools: Vec::new(),
    };

    let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "read it"},
            {"role": "assistant", "content": "Reading.", "tool_calls": [{
                "id": "call_read",
                "type": "function",
                "function": {"name": "read", "arguments": r#"{"filePath":"src/main.rs"}"#},
            }]},
            {"role": "tool", "content": "fn main() {}", "tool_call_id": "call_read"},
        ]),
        "got {body}"
    );
}

/// A turn cancelled while a tool was running leaves a call nobody answered,
/// and an assistant turn following an unanswered call is a request this API
/// refuses. Dropping the call instead would leave the reply talking about
/// one that is not there, so the pair is completed with a placeholder.
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
        let mut assistant = Message::assistant("gpt-test");
        assistant.parts.push(tool_part("call_read", "read", state));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs"), assistant],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["messages"][1],
            json!({"role": "assistant", "tool_calls": [{
                "id": "call_read",
                "type": "function",
                "function": {
                    "name": "read",
                    // A call the model never finished streaming has no
                    // arguments, and the field is required.
                    "arguments": if running { r#"{"filePath":"src/main.rs"}"# } else { "{}" },
                },
            }]}),
            "a turn that only called a tool has no content to send: {body}"
        );
        assert_eq!(
            body["messages"][2],
            json!({
                "role": "tool",
                "content": NO_RESULT,
                "tool_call_id": "call_read",
            }),
            "an unanswered call must not reach the API unanswered: {body}"
        );
    }
}

/// Step markers never travel as content of their own — they are this
/// crate's bookkeeping — but they do decide where one message ends and the
/// next begins, so text either side of one is two messages rather than one
/// joined string. A message holding nothing but markers is not a message at
/// all.
#[test]
fn a_step_marker_starts_a_new_message_rather_than_being_dropped() {
    let mut assistant = Message::assistant("gpt-test");
    assistant.parts.push(Part::text("Reading the file."));
    assistant.parts.push(Part {
        id: PartId::ascending(),
        body: PartBody::StepFinish { usage: Usage::default() },
    });
    assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
    assistant.parts.push(Part::text("It holds a main function."));

    let mut markers_only = Message::assistant("gpt-test");
    markers_only.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("hi"), assistant, markers_only],
        tools: Vec::new(),
    };

    let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "Reading the file."},
            {"role": "assistant", "content": "It holds a main function."},
        ]),
        "got {body}"
    );
}

/// Fragments of one step's reply, on the other hand, are one message: this
/// API has a single content string per message, and joining them without a
/// separator would run the last word of one into the first of the next.
#[test]
fn text_fragments_within_one_step_are_joined_into_one_message() {
    let mut assistant = Message::assistant("gpt-test");
    assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
    assistant.parts.push(Part::text("Reading the file."));
    assistant.parts.push(Part::text("It holds a main function."));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("hi"), assistant],
        tools: Vec::new(),
    };

    let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

    assert_eq!(
        body["messages"][1],
        json!({
            "role": "assistant",
            "content": "Reading the file.\nIt holds a main function.",
        }),
        "got {body}"
    );
}

/// Both credentials a provider holds: the key it was built with, and
/// whatever the base URL carries — which for this provider is the common
/// case, since pointing it at a gateway is the whole reason the base URL is
/// configurable.
#[test]
fn a_provider_never_renders_its_credential() {
    // Both shapes `check_base_url` blesses. The loopback one is not
    // hypothetical: it is what a local inference server is reached on, and
    // what the integration suite points this provider at.
    let cases = [
        (
            "https://ganja:sk-url-canary-9999@gateway.invalid:8443/v1?token=sk-query-canary-7777",
            "gateway.invalid:8443",
        ),
        ("http://ganja:sk-url-canary-9999@127.0.0.1:8080", "127.0.0.1:8080"),
    ];

    for (base_url, endpoint) in cases {
        let provider = OpenAiProvider::new("sk-test-canary-XYZ")
            .expect("an HTTP client builds")
            .with_base_url(base_url);

        let rendered = format!("{provider:?}");

        for secret in ["sk-test-canary-XYZ", "sk-url-canary-9999", "sk-query-canary-7777", "ganja:"]
        {
            assert!(!rendered.contains(secret), "a provider leaked {secret}: {rendered}");
        }
        assert!(rendered.contains("[redacted]"), "got {rendered}");
        assert!(
            rendered.contains(endpoint),
            "the endpoint should survive being made safe to print: {rendered}"
        );
    }
}

#[test]
fn a_blank_credential_is_refused() {
    assert!(OpenAiProvider::new("\t").is_err());
}
