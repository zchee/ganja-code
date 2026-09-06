use buffa::Message as _;

use super::super::{connect, proto};
use super::{
    Ask, ContextAsk, ExecArgs, ExecAsk, FinishReason, KvAsk, KvOp, Mapping, ProviderError,
    ProviderEvent, model_list, verdict,
};

/// An exec request carrying one args arm by number, the way a kind this
/// build does not model arrives: the arm is an unknown field and the
/// field number is the kind.
fn exec_of_kind(id: u32, kind: u32) -> proto::ExecRequest {
    let mut asked = proto::ExecRequest::default().with_id(id);
    asked.__buffa_unknown_fields.push(buffa::UnknownField {
        number: kind,
        data: buffa::UnknownFieldData::LengthDelimited(Vec::new()),
    });

    asked
}

/// A data frame holding one update, the way the server frames them.
fn framed(update: proto::Update) -> Vec<u8> {
    let message = proto::ServerMessage {
        interaction_update: buffa::MessageField::some(update),
        ..Default::default()
    };

    connect::envelope(&message.encode_to_vec())
}

fn text(delta: &str) -> proto::Update {
    proto::Update {
        text_delta: buffa::MessageField::some(proto::TextDelta::default().with_text(delta)),
        ..Default::default()
    }
}

fn turn_ended() -> proto::Update {
    proto::Update {
        turn_ended: buffa::MessageField::some(proto::TurnEnded::default()),
        ..Default::default()
    }
}

fn heartbeat() -> proto::Update {
    proto::Update {
        heartbeat: buffa::MessageField::some(proto::Heartbeat::default()),
        ..Default::default()
    }
}

fn thinking(delta: &str) -> proto::Update {
    proto::Update {
        thinking_delta: buffa::MessageField::some(proto::ThinkingDelta::default().with_text(delta)),
        ..Default::default()
    }
}

fn thinking_completed() -> proto::Update {
    proto::Update {
        thinking_completed: buffa::MessageField::some(proto::ThinkingCompleted::default()),
        ..Default::default()
    }
}

/// A data frame holding one kv request, the way the server frames one.
fn kv_framed(kv: proto::KvRequest) -> Vec<u8> {
    let message =
        proto::ServerMessage { kv_request: buffa::MessageField::some(kv), ..Default::default() };

    connect::envelope(&message.encode_to_vec())
}

/// An EndStream frame carrying `payload`.
fn end_stream(payload: &str) -> Vec<u8> {
    let mut frame = vec![0b0000_0010];
    frame.extend_from_slice(
        &u32::try_from(payload.len()).expect("a test payload fits").to_be_bytes(),
    );
    frame.extend_from_slice(payload.as_bytes());

    frame
}

/// A data frame holding one exec request, the way the server frames one.
fn exec_framed(exec: proto::ExecRequest) -> Vec<u8> {
    let message = proto::ServerMessage {
        exec_request: buffa::MessageField::some(exec),
        ..Default::default()
    };

    connect::envelope(&message.encode_to_vec())
}

/// Runs `body` through the real splitter and one [`Mapping`], the way
/// the live fold does; `eof` says whether the body then ended.
fn mapped(body: &[u8], eof: bool) -> Vec<ProviderEvent> {
    mapped_asks(body, eof).0
}

/// Like [`mapped`], also collecting what the mapping asked the caller
/// to answer.
fn mapped_asks(body: &[u8], eof: bool) -> (Vec<ProviderEvent>, Vec<Ask>) {
    let mut splitter = connect::Splitter::default();
    splitter.push(body);

    let mut mapping = Mapping::default();
    let mut events = Vec::new();
    let mut asks = Vec::new();
    while let Some(frame) = splitter.frame().expect("the fixture bodies parse") {
        asks.extend(mapping.frame(&frame, &mut events));
    }
    if eof {
        mapping.truncated(&mut events);
    }

    (events, asks)
}

#[test]
fn a_streamed_reply_becomes_its_deltas_and_a_finish() {
    let mut body = framed(heartbeat());
    body.extend(framed(text("Hello")));
    body.extend(framed(text(" world")));
    body.extend(framed(turn_ended()));
    body.extend(end_stream("{}"));

    assert_eq!(
        mapped(&body, false),
        vec![
            ProviderEvent::TextDelta("Hello".to_owned()),
            ProviderEvent::TextDelta(" world".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "the heartbeat carried nothing and carried it faithfully"
    );
}

#[test]
fn a_clean_end_stream_finishes_a_turn_the_server_never_marked_ended() {
    let mut body = framed(text("done"));
    body.extend(end_stream("{}"));

    assert_eq!(mapped(&body, false).last(), Some(&ProviderEvent::Finish(FinishReason::Completed)));
}

/// The exact exchange the live probe recorded: one heartbeat, then the
/// EndStream refusal. Under incremental delivery the turn has already
/// opened by the time the verdict arrives, so it fails **inside** the
/// stream — the terminal [`ProviderEvent::Failed`] every wire reports a
/// mid-stream death with — rather than at the opening the one-shot
/// decode failed it at.
#[test]
fn the_recorded_refusal_arrives_as_the_turns_failure() {
    let mut body = framed(heartbeat());
    body.extend(end_stream(
        "{\"error\":{\"code\":\"invalid_argument\",\"message\":\
             \"First message must be a run request or prewarm request\"}}",
    ));

    let events = mapped(&body, false);
    assert!(
        matches!(
            events.as_slice(),
            [ProviderEvent::Failed(ProviderError::Status { status: 400, message })]
                if message.contains("invalid_argument")
        ),
        "{events:?}"
    );
}

#[test]
fn an_error_after_visible_text_keeps_the_text() {
    let mut body = framed(text("partial"));
    body.extend(end_stream(r#"{"error":{"code":"internal","message":"boom"}}"#));

    let events = mapped(&body, false);
    assert_eq!(events[0], ProviderEvent::TextDelta("partial".to_owned()));
    assert!(
        matches!(&events[1], ProviderEvent::Failed(ProviderError::Status { status: 500, .. })),
        "{events:?}"
    );
}

#[test]
fn a_body_without_an_ending_is_a_truncation_not_a_short_answer() {
    let events = mapped(&framed(heartbeat()), true);
    assert!(
        matches!(events.as_slice(), [ProviderEvent::Failed(ProviderError::Transport(_))]),
        "{events:?}"
    );

    let events = mapped(&framed(text("half")), true);
    assert_eq!(events[0], ProviderEvent::TextDelta("half".to_owned()));
    assert!(matches!(&events[1], ProviderEvent::Failed(ProviderError::Transport(_))), "{events:?}");
}

/// The Anthropic reading of a body cut off after the stop reason: the
/// reply was complete, only the terminator was lost, and failing the
/// turn would throw away text the server finished saying.
#[test]
fn a_body_that_dies_after_turn_ended_lost_only_its_terminator() {
    let mut body = framed(text("whole"));
    body.extend(framed(turn_ended()));

    assert_eq!(
        mapped(&body, true),
        vec![
            ProviderEvent::TextDelta("whole".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]
    );
}

/// The verdict the one-shot decode used to drop: an EndStream error
/// arriving after `turn_ended`. The server's verdict outranks the
/// model's goodbye — the same late-error posture the SSE wires hold —
/// so the turn fails, keeping its text.
#[test]
fn an_end_stream_error_after_turn_ended_is_a_failure_not_a_dropped_frame() {
    let mut body = framed(text("said"));
    body.extend(framed(turn_ended()));
    body.extend(end_stream(r#"{"error":{"code":"resource_exhausted","message":"quota spent"}}"#));

    let events = mapped(&body, false);
    assert_eq!(events[0], ProviderEvent::TextDelta("said".to_owned()));
    assert!(
        matches!(&events[1], ProviderEvent::Failed(ProviderError::Status { status: 429, .. })),
        "{events:?}"
    );
}

#[test]
fn a_frame_that_is_not_a_server_message_fails_the_turn_readably() {
    // 0xff opens a field with wire type 7, which protobuf does not have.
    let events = mapped(&connect::envelope(&[0xff, 0xff, 0xff]), false);
    assert!(
        matches!(events.as_slice(), [ProviderEvent::Failed(ProviderError::Parse(_))]),
        "{events:?}"
    );
}

#[test]
fn an_unauthenticated_verdict_names_the_login() {
    let refused = verdict("unauthenticated", "token expired");
    let rendered = refused.to_string();
    assert!(matches!(refused, ProviderError::Auth(_)), "{rendered}");
    assert!(rendered.contains("ganja auth login cursor"), "{rendered}");
}

#[test]
fn an_update_from_a_newer_server_is_skipped_not_fatal() {
    // An update whose only content is an arm this build does not model
    // decodes to an empty Update whose unknown fields hold the arm.
    let mut body = framed(proto::Update::default());
    body.extend(framed(text("still here")));
    body.extend(framed(turn_ended()));
    body.extend(end_stream("{}"));

    let events = mapped(&body, false);
    assert_eq!(events[0], ProviderEvent::TextDelta("still here".to_owned()));
}

/// The exchange the 2026-08-10 live turn hung on: the server's context
/// ask is not an update to skip but a question to hand up, ids intact,
/// so the stream layer can answer it on the open request body.
#[test]
fn the_servers_context_ask_is_handed_up_with_its_ids() {
    let body = exec_framed(
        proto::ExecRequest {
            request_context_args: buffa::MessageField::some(proto::ContextArgs::default()),
            ..Default::default()
        }
        .with_id(7)
        .with_exec_id("exec-abc"),
    );

    let (events, asks) = mapped_asks(&body, false);
    assert!(events.is_empty(), "an ask is a question, not an event: {events:?}");
    assert_eq!(
        asks,
        vec![Ask::Context(ContextAsk { id: Some(7), exec_id: Some("exec-abc".to_owned()) })]
    );
}

/// The plugin forwards thinking as thinking (proxy.ts:1059-1061), and
/// so does this wire: a codex-family model reasons before it speaks,
/// and calling that reply text would put it in the transcript's mouth.
#[test]
fn a_thinking_delta_becomes_reasoning_not_reply_text() {
    let mut body = framed(thinking("Weighing a greeting."));
    body.extend(framed(text("Hello")));
    body.extend(framed(turn_ended()));
    body.extend(end_stream("{}"));

    assert_eq!(
        mapped(&body, false),
        vec![
            ProviderEvent::ReasoningDelta("Weighing a greeting.".to_owned()),
            ProviderEvent::TextDelta("Hello".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]
    );
}

/// The boundary the plugin announces between two thinking blocks becomes
/// a break, so two thoughts on one stream stay two thoughts (2026-08-25,
/// live-observed): without it they splice — the transcript's own account
/// read "…to see if those work.Since tool calls…".
#[test]
fn a_thinking_completed_breaks_the_thought_before_it() {
    let mut body = framed(thinking("Weighing a greeting."));
    body.extend(framed(thinking_completed()));
    body.extend(framed(thinking("Weighing the weather.")));
    body.extend(framed(turn_ended()));
    body.extend(end_stream("{}"));

    assert_eq!(
        mapped(&body, false),
        vec![
            ProviderEvent::ReasoningDelta("Weighing a greeting.".to_owned()),
            ProviderEvent::ReasoningBreak,
            ProviderEvent::ReasoningDelta("Weighing the weather.".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]
    );
}

/// The channel the 2026-08-10 live run left waiting: a kv set and a kv
/// get are questions to hand up with their ids, never events and never
/// skips, because the server holds the turn's ending until each is
/// answered.
#[test]
fn the_servers_kv_set_and_get_are_handed_up_with_their_ids() {
    let mut body = kv_framed(proto::KvRequest {
        id: Some(11),
        set_blob_args: buffa::MessageField::some(
            proto::SetBlobArgs::default()
                .with_blob_id(b"blob-a".to_vec())
                .with_blob_data(b"opaque-state".to_vec()),
        ),
        ..Default::default()
    });
    body.extend(kv_framed(proto::KvRequest {
        id: Some(12),
        get_blob_args: buffa::MessageField::some(
            proto::GetBlobArgs::default().with_blob_id(b"blob-a".to_vec()),
        ),
        ..Default::default()
    }));

    let (events, asks) = mapped_asks(&body, false);
    assert!(events.is_empty(), "a kv exchange is a question, not an event: {events:?}");
    assert_eq!(
        asks,
        vec![
            Ask::Kv(KvAsk {
                id: Some(11),
                op: KvOp::Set { blob_id: b"blob-a".to_vec(), data: b"opaque-state".to_vec() },
            }),
            Ask::Kv(KvAsk { id: Some(12), op: KvOp::Get { blob_id: b"blob-a".to_vec() } }),
        ]
    );
}

/// A kv kind beyond get and set gets the exec channel's discipline: the
/// server waits on it, so the turn fails naming the field — and the
/// span context riding beside the oneof (agent_pb.ts:7931) is never
/// mistaken for one.
#[test]
fn a_kv_kind_this_build_cannot_answer_fails_the_turn_by_name() {
    let mut asked = proto::KvRequest { id: Some(3), ..Default::default() };
    asked.__buffa_unknown_fields.push(buffa::UnknownField {
        number: 4,
        data: buffa::UnknownFieldData::LengthDelimited(Vec::new()),
    });
    asked.__buffa_unknown_fields.push(buffa::UnknownField {
        number: 9,
        data: buffa::UnknownFieldData::LengthDelimited(Vec::new()),
    });

    let (events, asks) = mapped_asks(&kv_framed(asked), false);
    assert!(asks.is_empty(), "nothing to answer: {asks:?}");
    assert!(
        matches!(
            events.as_slice(),
            [ProviderEvent::Failed(ProviderError::Parse(message))]
                if message.contains("kv request")
                    && message.contains("field 9")
                    && !message.contains('4')
        ),
        "{events:?}"
    );

    let (events, _) = mapped_asks(&kv_framed(proto::KvRequest::default()), false);
    assert!(
        matches!(
            events.as_slice(),
            [ProviderEvent::Failed(ProviderError::Parse(message))]
                if message.contains("no recognizable kind")
        ),
        "{events:?}"
    );
}

/// **AC-11.** A server message carrying only an arm this build does not
/// model — the checkpoint, field 3 — decodes to no event and no ask, and is
/// reported by field number and payload size through the seam the skip log
/// reads, never by content.
#[test]
fn a_server_message_outside_the_modelled_channels_is_reported_by_number_and_size() {
    let mut checkpoint = proto::ServerMessage::default();
    checkpoint.__buffa_unknown_fields.push(buffa::UnknownField {
        number: 3,
        data: buffa::UnknownFieldData::LengthDelimited(vec![0xAB; 40]),
    });
    let framed = connect::envelope(&checkpoint.encode_to_vec());

    let (events, asks) = mapped_asks(&framed, false);
    assert!(events.is_empty(), "nothing to hand out: {events:?}");
    assert!(asks.is_empty(), "nothing to answer: {asks:?}");

    let decoded = proto::ServerMessage::decode_from_slice(&framed[5..]).expect("the frame decodes");
    assert_eq!(super::unmodelled(&decoded), vec![(3, 40)]);

    // A scalar arm is sized by its encoded width, so the line still says
    // how much arrived rather than nothing.
    let mut scalar = proto::ServerMessage::default();
    scalar
        .__buffa_unknown_fields
        .push(buffa::UnknownField { number: 9, data: buffa::UnknownFieldData::Varint(300) });
    assert_eq!(super::unmodelled(&scalar), vec![(9, 2)], "300 is a two-byte varint");
    assert!(super::unmodelled(&proto::ServerMessage::default()).is_empty());
}

/// The arm names the skip log leans on: the plugin's own oneof spelling
/// for the numbers it declares, and the bare number for anything newer.
#[test]
fn a_skipped_arm_is_named_the_way_the_plugins_descriptor_names_it() {
    assert_eq!(super::update_arm(8), "token_delta (8)");
    assert_eq!(super::update_arm(16), "step_started (16)");
    assert_eq!(super::update_arm(42), "field 42");
}

/// The kind a live turn really died on: `shell_stream_args`, field 14 of
/// the args oneof — the server asking this client to run a shell for it.
/// It is a question to hand up with the kind named, never an event, and
/// no longer the failure that used to end the turn (**D486**). Since
/// **D550** the kind is recognised by the field it arrived on rather than
/// by an unknown number, and what it named comes back up with it.
#[test]
fn the_live_observed_shell_stream_exec_is_handed_up_with_its_arguments() {
    let asked = proto::ExecRequest {
        id: Some(5),
        exec_id: Some("exec-abc".to_owned()),
        shell_stream_args: buffa::MessageField::some(
            proto::ShellArgs::default().with_command("cargo test").with_working_directory("/repo"),
        ),
        ..Default::default()
    };
    let (events, asks) = mapped_asks(&exec_framed(asked), false);

    assert!(events.is_empty(), "an exec is a question to answer, not an event: {events:?}");
    assert_eq!(
        asks,
        vec![Ask::Exec(ExecAsk {
            id: Some(5),
            exec_id: Some("exec-abc".to_owned()),
            kind: "shell_stream_args".to_owned(),
            args: ExecArgs::ShellStream {
                command: "cargo test".to_owned(),
                working_directory: "/repo".to_owned(),
            },
        })]
    );
}

/// Every other named tool exec takes the same door, and the turn it
/// arrives on keeps going: the frames behind the ask are still mapped,
/// and the stream still reaches its finish.
#[test]
fn a_named_tool_exec_is_handed_up_and_the_turn_carries_on_past_it() {
    // shell_args is field 2 of the shipped oneof (index.js@6302201).
    let asked = proto::ExecRequest {
        id: Some(3),
        shell_args: buffa::MessageField::some(proto::ShellArgs::default().with_command("ls")),
        ..Default::default()
    };
    let mut body = exec_framed(asked);
    body.extend(framed(text("still generating")));
    body.extend(framed(turn_ended()));
    body.extend(end_stream("{}"));

    let (events, asks) = mapped_asks(&body, false);
    assert_eq!(
        asks,
        vec![Ask::Exec(ExecAsk {
            id: Some(3),
            exec_id: None,
            kind: "shell_args".to_owned(),
            args: ExecArgs::Shell {
                command: "ls".to_owned(),
                // Absent and empty are one answer: the arm has no way to
                // say the server did not send a working directory.
                working_directory: String::new(),
            },
        })]
    );
    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("still generating".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "an exec handed up is not a dead turn"
    );
}

/// The refusal channel is keyed on the exec id and names no kind, so a
/// kind no table has heard of is refusable too — named by number, which
/// is still enough to go derive. The span context riding beside the
/// oneof is never mistaken for a kind, and an exec carrying nothing
/// recognizable at all is refused rather than failed, because leaving
/// *it* unanswered would hang the turn just as surely.
#[test]
fn an_exec_kind_beyond_the_table_is_refused_by_its_field_number() {
    let mut asked = exec_of_kind(4, 39);
    // span_context = 19 rides beside the args oneof (index.js@6302201).
    asked.__buffa_unknown_fields.push(buffa::UnknownField {
        number: 19,
        data: buffa::UnknownFieldData::LengthDelimited(Vec::new()),
    });

    let (events, asks) = mapped_asks(&exec_framed(asked), false);
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(
        asks,
        vec![Ask::Exec(ExecAsk {
            id: Some(4),
            exec_id: None,
            kind: "field 39".to_owned(),
            args: ExecArgs::Unmodelled,
        })],
        "the span context is passed over rather than blamed"
    );

    let (events, asks) = mapped_asks(&exec_framed(proto::ExecRequest::default()), false);
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(
        asks,
        vec![Ask::Exec(ExecAsk {
            id: None,
            exec_id: None,
            kind: "no recognizable kind".to_owned(),
            args: ExecArgs::Unmodelled,
        })],
        "an id the server never sent is not invented"
    );
}

/// Every modelled kind, read off a frame with every member this build
/// reads set — so each argument is pinned to the field number `cursor.proto`
/// gives it, from the bytes and not from the generated struct alone. The two
/// numbers keep their absence: an `offset` of zero is a window at line zero
/// where an absent one is no window at all, and `case_insensitive` is read
/// both ways rather than only when set.
#[test]
fn each_modelled_exec_kind_decodes_into_its_arguments() {
    let shell =
        || proto::ShellArgs::default().with_command("ls -a").with_working_directory("/repo");
    let rows: Vec<(proto::ExecRequest, &str, ExecArgs)> = vec![
        (
            proto::ExecRequest {
                shell_args: buffa::MessageField::some(shell()),
                ..Default::default()
            },
            "shell_args",
            ExecArgs::Shell { command: "ls -a".to_owned(), working_directory: "/repo".to_owned() },
        ),
        (
            proto::ExecRequest {
                shell_stream_args: buffa::MessageField::some(shell()),
                ..Default::default()
            },
            "shell_stream_args",
            ExecArgs::ShellStream {
                command: "ls -a".to_owned(),
                working_directory: "/repo".to_owned(),
            },
        ),
        (
            proto::ExecRequest {
                write_args: buffa::MessageField::some(
                    proto::WriteArgs::default().with_path("/repo/new.txt").with_file_text("one\n"),
                ),
                ..Default::default()
            },
            "write_args",
            ExecArgs::Write { path: "/repo/new.txt".to_owned(), file_text: "one\n".to_owned() },
        ),
        (
            proto::ExecRequest {
                delete_args: buffa::MessageField::some(
                    proto::DeleteArgs::default().with_path("/repo/old.txt"),
                ),
                ..Default::default()
            },
            "delete_args",
            ExecArgs::Delete { path: "/repo/old.txt".to_owned() },
        ),
        (
            proto::ExecRequest {
                grep_args: buffa::MessageField::some(
                    proto::GrepArgs::default()
                        .with_pattern("todo")
                        .with_path("/repo")
                        .with_glob("*.rs")
                        .with_case_insensitive(true),
                ),
                ..Default::default()
            },
            "grep_args",
            ExecArgs::Grep {
                pattern: "todo".to_owned(),
                path: "/repo".to_owned(),
                glob: "*.rs".to_owned(),
                case_insensitive: true,
            },
        ),
        (
            proto::ExecRequest {
                grep_args: buffa::MessageField::some(
                    proto::GrepArgs::default().with_pattern("todo").with_case_insensitive(false),
                ),
                ..Default::default()
            },
            "grep_args",
            ExecArgs::Grep {
                pattern: "todo".to_owned(),
                path: String::new(),
                glob: String::new(),
                case_insensitive: false,
            },
        ),
        (
            proto::ExecRequest {
                read_args: buffa::MessageField::some(
                    proto::ReadArgs::default().with_path("/f").with_offset(0).with_limit(40),
                ),
                ..Default::default()
            },
            "read_args",
            ExecArgs::Read {
                redacted: false,
                path: "/f".to_owned(),
                offset: Some(0),
                limit: Some(40),
            },
        ),
        (
            proto::ExecRequest {
                read_args: buffa::MessageField::some(proto::ReadArgs::default().with_path("/f")),
                ..Default::default()
            },
            "read_args",
            ExecArgs::Read { redacted: false, path: "/f".to_owned(), offset: None, limit: None },
        ),
        (
            proto::ExecRequest {
                redacted_read_args: buffa::MessageField::some(
                    proto::ReadArgs::default().with_path("/.env").with_offset(3).with_limit(5),
                ),
                ..Default::default()
            },
            "redacted_read_args",
            ExecArgs::Read {
                redacted: true,
                path: "/.env".to_owned(),
                offset: Some(3),
                limit: Some(5),
            },
        ),
        (
            proto::ExecRequest {
                ls_args: buffa::MessageField::some(proto::LsArgs::default().with_path("/repo")),
                ..Default::default()
            },
            "ls_args",
            ExecArgs::Ls { path: "/repo".to_owned() },
        ),
        (
            proto::ExecRequest {
                fetch_args: buffa::MessageField::some(
                    proto::FetchArgs::default().with_url("https://example.com/"),
                ),
                ..Default::default()
            },
            "fetch_args",
            ExecArgs::Fetch { url: "https://example.com/".to_owned() },
        ),
        (
            proto::ExecRequest {
                mcp_args: buffa::MessageField::some(proto::McpArgs {
                    args: vec![proto::McpArgEntry {
                        key: Some("limit".to_owned()),
                        value: buffa::MessageField::some(crate::provider::cursor::value::encode(
                            &serde_json::json!(40),
                        )),
                        ..Default::default()
                    }],
                    ..proto::McpArgs::default()
                        .with_name("read")
                        .with_tool_name("read")
                        .with_tool_call_id("call-1")
                        .with_provider_identifier("ganja")
                        .with_smart_mode_approval_only(true)
                }),
                ..Default::default()
            },
            "mcp_args",
            ExecArgs::Mcp(super::McpCall {
                name: "read".to_owned(),
                tool_name: "read".to_owned(),
                tool_call_id: "call-1".to_owned(),
                provider_identifier: "ganja".to_owned(),
                approval_only: true,
                arguments: Some(serde_json::json!({ "limit": 40 })),
            }),
        ),
    ];

    for (exec, kind, args) in rows {
        let (events, asks) =
            mapped_asks(&exec_framed(exec.with_id(9).with_exec_id("exec-9")), false);
        assert!(events.is_empty(), "{kind}: an exec is a question, not an event: {events:?}");
        assert_eq!(
            asks,
            vec![Ask::Exec(ExecAsk {
                id: Some(9),
                exec_id: Some("exec-9".to_owned()),
                kind: kind.to_owned(),
                args,
            })],
            "{kind}"
        );
    }
}

/// The name table behind the throw covers the kinds D550 models no arm
/// for, and only those: a number that now decodes into a field of its own
/// can never reach it, and one the table has never heard of is still
/// reported as itself, which is enough to go derive.
#[test]
fn a_kind_with_no_modelled_arm_is_named_from_the_throws_own_table() {
    for (number, named) in [(9u32, "diagnostics_args"), (28, "subagent_args"), (56, "adopt_args")] {
        let (_, asks) = mapped_asks(&exec_framed(exec_of_kind(1, number)), false);
        assert_eq!(
            asks,
            vec![Ask::Exec(ExecAsk {
                id: Some(1),
                exec_id: None,
                kind: named.to_owned(),
                args: ExecArgs::Unmodelled,
            })],
        );
    }

    let (_, asks) = mapped_asks(&exec_framed(exec_of_kind(1, 99)), false);
    assert!(
        matches!(asks.as_slice(), [Ask::Exec(refusal)] if refusal.kind == "field 99"),
        "a kind newer than this file is refusable by number: {asks:?}"
    );
}

#[test]
fn the_model_listing_decodes_and_a_wrong_body_is_a_parse_error() {
    let listing = proto::GetUsableModelsResponse {
        models: vec![
            proto::ModelEntry::default().with_model_id("default").with_display_model_id("auto"),
            proto::ModelEntry::default().with_model_id("gpt-5.3-codex"),
        ],
        ..Default::default()
    }
    .encode_to_vec();

    let models = model_list(&listing).expect("the listing decodes");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].model_id.as_deref(), Some("default"));

    assert!(matches!(model_list(&[0xff, 0xff, 0xff]), Err(ProviderError::Parse(_))));
}

/// The `default` entry's first bytes, encoded by this build, are the
/// bytes the live probe recorded off the wire — the field numbers and
/// types in `cursor.proto` really are the server's.
#[test]
fn the_encoding_matches_the_bytes_recorded_off_the_live_wire() {
    let entry = proto::ModelEntry::default().with_model_id("default").with_display_model_id("auto");

    assert_eq!(
        &entry.encode_to_vec()[..15],
        // spike-wire-facts.md S4: `0a 07 default 1a 04 auto`, inside the
        // response's first entry.
        b"\x0a\x07default\x1a\x04auto",
    );
}
