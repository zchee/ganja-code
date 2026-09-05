use std::collections::HashMap;

use buffa::Message as _;

use super::super::{connect, proto, serves_fetch};
use super::{
    ChatRequest, context_answer, decode, fresh_id, kv_answer, newest_user_text, refusal_answer,
    run_message,
};
use crate::protocol::{Message, Part};
use crate::tool::ToolDefinition;

/// The sentence every typed refusal but the MCP one carries, spelled out
/// once here rather than rebuilt from the constant under test: a test that
/// composes the string the same way the code does would pass through a
/// rewrite of it.
fn refusal(kind: &str) -> String {
    format!(
        "ganja does not run {kind} for a provider: its tools run for its own \
         session, under its own permission engine."
    )
}

/// The top-level field numbers a message went out with, in wire order, read
/// off the bytes rather than off the decoded struct.
///
/// A field *number* is what each row below pins, and only the bytes carry
/// one: a struct field can be read back correctly while the `.proto` has
/// renumbered underneath it, because the same generated decoder wrote and
/// read it. Every field these messages use is a varint or a
/// length-delimited, which is all the walk needs to step over.
fn field_numbers(bytes: &[u8]) -> Vec<u32> {
    let varint = |bytes: &[u8]| -> (u64, usize) {
        let mut value = 0u64;
        let mut shift = 0u32;
        for (index, byte) in bytes.iter().enumerate() {
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return (value, index + 1);
            }
            shift += 7;
        }
        panic!("a truncated varint");
    };

    let mut numbers = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let (tag, read) = varint(&bytes[cursor..]);
        cursor += read;
        numbers.push(u32::try_from(tag >> 3).expect("a field number fits"));
        match tag & 7 {
            0 => cursor += varint(&bytes[cursor..]).1,
            2 => {
                let (len, read) = varint(&bytes[cursor..]);
                cursor += read + usize::try_from(len).expect("a length fits");
            }
            other => panic!("a wire type these messages do not use: {other}"),
        }
    }

    numbers
}

/// One exec taken the whole way a live turn takes it: framed as the server
/// frames it, classified by the real [`decode::Mapping`], then answered.
///
/// Each row test below reads what comes out, so a row pins the
/// classification and the encoding together — a kind recognised but
/// answered at the wrong number, or answered right off a hand-built
/// refusal the decoder would never have produced, both fail here.
fn refused(exec: proto::ExecRequest) -> Vec<proto::ClientMessage> {
    let framed = connect::envelope(
        &proto::ServerMessage {
            exec_request: buffa::MessageField::some(exec),
            ..Default::default()
        }
        .encode_to_vec(),
    );

    let mut splitter = connect::Splitter::default();
    splitter.push(&framed);
    let frame = splitter.frame().expect("the fixture frames parse").expect("one whole frame");

    let mut events = Vec::new();
    let ask = decode::Mapping::default().frame(&frame, &mut events).expect("the server waits");
    assert!(events.is_empty(), "a refusal is an answer to send, not an event: {events:?}");
    let decode::Ask::Refuse(ask) = ask else { panic!("a tool exec is refused: {ask:?}") };

    refusal_answer(&ask)
        .iter()
        .map(|message| {
            proto::ClientMessage::decode_from_slice(message).expect("what was sent decodes")
        })
        .collect()
}

/// The typed half of a refusal: the `ExecResponse` its first message
/// carries, having asserted that its second is the stream close that ends
/// every exec and that there is nothing else.
fn typed(refused: &[proto::ClientMessage], id: u32) -> proto::ExecResponse {
    assert_eq!(refused.len(), 2, "the rejection, and the close that ends it");
    let closed = refused[1].exec_control.as_option().expect("the close rides the control channel");
    assert_eq!(closed.stream_close.as_option().and_then(|close| close.id), Some(id));
    assert!(closed.throw.as_option().is_none(), "a typed rejection is not also a throw");

    refused[0].exec_response.as_option().cloned().expect("the rejection rides the exec channel")
}

/// A three-message conversation — an older question and its reply, then a
/// newest user message with two text parts, the richest shape this assembly
/// reads. The reply between them is what makes the older question history
/// rather than part of the newest turn.
fn request() -> ChatRequest {
    let mut asked = Message::user("What does this crate do?");
    asked.parts.push(Part::text("Answer briefly."));

    ChatRequest {
        // The turn opened at the newest question: the pair before it is the
        // history this request carries and does not re-send.
        turn_start: 2,
        effort_options: Default::default(),
        model: "gpt-5.3-codex".to_owned(),
        system: Some("You are terse.".to_owned()),
        messages: vec![
            Message::user("An older question."),
            Message::assistant("gpt-5.3-codex"),
            asked,
        ],
        tools: Vec::new(),
    }
}

#[test]
fn the_assembled_bytes_decode_back_to_what_the_assembly_promised() {
    let bytes = run_message(&request()).expect("the assembly encodes");
    let decoded = proto::ClientMessage::decode_from_slice(&bytes).expect("what was sent decodes");

    let run = decoded.run_request.as_option().expect("a run request first");
    assert!(run.conversation_state.is_set(), "the state is present even when it holds nothing");

    let model = run.model_details.as_option().expect("the model description");
    assert_eq!(model.model_id.as_deref(), Some("gpt-5.3-codex"));
    assert_eq!(model.display_name.as_deref(), Some("gpt-5.3-codex"));
    assert_eq!(
        run.requested_model.as_option().and_then(|requested| requested.model_id.as_deref()),
        Some("gpt-5.3-codex"),
        "the model is named on both channels the server reads"
    );

    let user = run
        .action
        .as_option()
        .and_then(|action| action.user_message_action.as_option())
        .and_then(|action| action.user_message.as_option())
        .expect("the user message rides the action");
    assert_eq!(
        user.text.as_deref(),
        Some("What does this crate do?\n\nAnswer briefly."),
        "the newest user message travels whole, part by part"
    );
    assert_eq!(
        user.message_id.as_deref().map(str::len),
        Some(36),
        "the message is stamped in the shape the recorded client stamps"
    );
}

#[test]
fn the_system_prompt_never_rides_the_run_request() {
    let bytes = run_message(&request()).expect("the assembly encodes");

    let prompt = b"You are terse.";
    assert!(
        !bytes.windows(prompt.len()).any(|window| window == prompt),
        "the prompt must not ride the allowlist-gated member the server refuses; \
             its channel is the context answer"
    );
}

#[test]
fn the_context_answer_echoes_the_ids_and_carries_the_prompt_on_cloud_rule() {
    let bytes = context_answer(
        decode::ContextAsk { id: Some(7), exec_id: Some("exec-abc".to_owned()) },
        Some("You are terse."),
        true,
    );
    let decoded = proto::ClientMessage::decode_from_slice(&bytes).expect("what was sent decodes");

    assert!(decoded.run_request.as_option().is_none(), "an answer is not a second run request");
    let answer = decoded.exec_response.as_option().expect("the exec answer");
    assert_eq!(answer.id, Some(7), "the id the server minted comes back");
    assert_eq!(answer.exec_id.as_deref(), Some("exec-abc"));

    let context = answer
        .request_context_result
        .as_option()
        .and_then(|result| result.success.as_option())
        .and_then(|success| success.request_context.as_option())
        .expect("a success carrying the context");
    assert_eq!(context.cloud_rule.as_deref(), Some("You are terse."));
}

/// The plugin's no-prompt answer sends the context message with
/// `cloudRule` unset, and so does this one — present context, absent
/// member, never an empty string pretending to be a prompt.
#[test]
fn a_promptless_turn_answers_with_a_present_but_empty_context() {
    for system in [None, Some("")] {
        let bytes = context_answer(decode::ContextAsk { id: None, exec_id: None }, system, false);
        let decoded = proto::ClientMessage::decode_from_slice(&bytes).expect("decodes");
        let answer = decoded.exec_response.as_option().expect("the exec answer");
        assert_eq!(answer.id, None, "an id the server never sent is not invented");

        let context = answer
            .request_context_result
            .as_option()
            .and_then(|result| result.success.as_option())
            .and_then(|success| success.request_context.as_option())
            .expect("the context is present even without a prompt");
        assert_eq!(context.cloud_rule, None, "an absent prompt is absent, not empty");
    }
}

/// A kind with no modelled arm still takes D486's control channel, and it
/// still takes it the way the shipped client does: the throw carrying the
/// echoed id and a reason the server's agent loop can read, then the stream
/// close that ends the exchange. This is the catch-all D550 kept, which is
/// what keeps a kind newer than `cursor.proto` refusable at all.
#[test]
fn an_exec_with_no_modelled_arm_is_a_throw_naming_the_kind_and_then_a_stream_close() {
    let refused = refusal_answer(&decode::ExecRefusal {
        id: Some(5),
        exec_id: Some("exec-abc".to_owned()),
        kind: "subagent_args".to_owned(),
        arm: decode::RefusalArm::Throw,
    });
    assert_eq!(refused.len(), 2, "a throw, and the close that ends it");

    let decoded: Vec<proto::ClientMessage> = refused
        .iter()
        .map(|message| {
            proto::ClientMessage::decode_from_slice(message).expect("what was sent decodes")
        })
        .collect();

    for message in &decoded {
        assert!(
            message.run_request.as_option().is_none()
                && message.exec_response.as_option().is_none()
                && message.kv_response.as_option().is_none(),
            "a refusal travels the control channel and no other"
        );
    }

    let thrown = decoded[0]
        .exec_control
        .as_option()
        .and_then(|control| control.throw.as_option())
        .expect("the throw first");
    assert_eq!(thrown.id, Some(5), "the id the server minted comes back");
    assert_eq!(
        thrown.error.as_deref(),
        Some(refusal("subagent_args").as_str()),
        "the refusal names who refused, what, and why"
    );

    let closed = decoded[1].exec_control.as_option().expect("the control channel again");
    assert!(
        closed.throw.as_option().is_none(),
        "the close is the exchange ending, not a second failure"
    );
    assert_eq!(closed.stream_close.as_option().and_then(|close| close.id), Some(5));

    // The arm numbers themselves, on the wire: exec_control = 5 wraps
    // the ClientMessage (tag 0x2a), stream_close = 1 (0x0a) and
    // throw = 2 (0x12) are the arms inside it, and id = 1 (0x08) is the
    // echo. The close carries nothing else, so its six bytes are the
    // whole message.
    assert_eq!(refused[1], b"\x2a\x04\x0a\x02\x08\x05");
    // The throw's own length varies with the reason, so only its tags
    // are fixed: byte 1 is that length.
    assert_eq!(refused[0][0], 0x2a, "exec_control = 5, length-delimited");
    assert_eq!(refused[0][2], 0x12, "throw = 2, length-delimited");
}

/// An id the server never sent is not invented on the refusal channel
/// either — the same discipline the context and kv answers hold.
#[test]
fn a_refusal_for_an_exec_without_an_id_invents_none() {
    let refused = refusal_answer(&decode::ExecRefusal {
        id: None,
        exec_id: None,
        kind: "no recognizable kind".to_owned(),
        arm: decode::RefusalArm::Throw,
    });

    let decoded = proto::ClientMessage::decode_from_slice(&refused[0]).expect("decodes");
    let thrown = decoded
        .exec_control
        .as_option()
        .and_then(|control| control.throw.as_option())
        .expect("the throw");
    assert_eq!(thrown.id, None);
    assert!(
        thrown.error.as_deref().is_some_and(|reason| reason.contains("no recognizable kind")),
        "an unnameable kind is still named as far as it can be: {thrown:?}"
    );
}

/// The plugin's kv semantics, end to end: the set is stored and acked
/// with the empty result, and the get that follows is answered with the
/// bytes the server handed over — id echoed on each, because the id is
/// how the server matches answer to question.
#[test]
fn a_kv_set_is_stored_and_acked_and_the_get_that_follows_returns_it() {
    let mut blobs = HashMap::new();

    let stored = kv_answer(
        decode::KvAsk {
            id: Some(11),
            op: decode::KvOp::Set { blob_id: b"blob-a".to_vec(), data: b"opaque-state".to_vec() },
        },
        &mut blobs,
    );
    let decoded = proto::ClientMessage::decode_from_slice(&stored).expect("decodes");
    assert!(
        decoded.run_request.as_option().is_none() && decoded.exec_response.as_option().is_none(),
        "a kv answer is a kv answer and nothing else"
    );
    let answer = decoded.kv_response.as_option().expect("the kv answer");
    assert_eq!(answer.id, Some(11), "the id the server minted comes back");
    assert!(
        answer.set_blob_result.is_set(),
        "the ack is the present-but-empty result the plugin sends"
    );
    assert!(answer.get_blob_result.as_option().is_none());

    let read = kv_answer(
        decode::KvAsk { id: Some(12), op: decode::KvOp::Get { blob_id: b"blob-a".to_vec() } },
        &mut blobs,
    );
    let decoded = proto::ClientMessage::decode_from_slice(&read).expect("decodes");
    let answer = decoded.kv_response.as_option().expect("the kv answer");
    assert_eq!(answer.id, Some(12));
    assert_eq!(
        answer.get_blob_result.as_option().and_then(|result| result.blob_data.as_deref()),
        Some(b"opaque-state".as_slice()),
        "the get reads back exactly what the set stored"
    );
}

/// The plugin's miss shape (proxy.ts:1101-1105): the result message is
/// present, the data member is absent — never empty bytes pretending to
/// be a blob, and never a failure, because a fresh turn holding nothing
/// is a state the server itself created.
#[test]
fn a_kv_get_before_any_set_answers_not_found_without_inventing_bytes() {
    let mut blobs = HashMap::new();

    let read = kv_answer(
        decode::KvAsk { id: None, op: decode::KvOp::Get { blob_id: b"blob-b".to_vec() } },
        &mut blobs,
    );
    let decoded = proto::ClientMessage::decode_from_slice(&read).expect("decodes");
    let answer = decoded.kv_response.as_option().expect("the kv answer");
    assert_eq!(answer.id, None, "an id the server never sent is not invented");
    let result =
        answer.get_blob_result.as_option().expect("the result is present even without the blob");
    assert_eq!(result.blob_data, None, "not-found is absence, not empty bytes");
    assert!(blobs.is_empty(), "a get stores nothing");
}

/// A conversation whose current turn opens at `turn_start`, which is what the
/// engine hands this wire and what no walk over `messages` could work out for
/// itself.
fn turn(messages: Vec<Message>, turn_start: usize) -> ChatRequest {
    ChatRequest { messages, turn_start, ..request() }
}

#[test]
fn the_newest_user_turn_is_every_user_message_since_the_last_reply() {
    let conversation =
        vec![Message::user("first"), Message::assistant("gpt-5.3-codex"), Message::user("second")];
    assert_eq!(newest_user_text(&turn(conversation, 2)), "second");
    assert_eq!(newest_user_text(&turn(Vec::new(), 0)), "");

    // The engine appends to a turn — a steer, the team guards' request-only
    // block after a reply — and each of those is a message of its own, so
    // the newest turn is the whole run back to the reply and not its last
    // message alone (D547). The turn opened at "second", so the marker
    // bounds nothing here and the run is what it always was.
    let appended = vec![
        Message::user("first"),
        Message::assistant("gpt-5.3-codex"),
        Message::user("second"),
        Message::user("<team_still_working>keep going</team_still_working>"),
    ];
    assert_eq!(
        newest_user_text(&turn(appended, 2)),
        "second\n\n<team_still_working>keep going</team_still_working>"
    );
}

/// The turn marker's whole job: a steer a finished turn consumed belongs to
/// that turn and never rides into the next one's text.
///
/// The engine appends a consumed steer to history *after* the assistant it
/// interrupted, so a turn that took one ends as `[prompt, reply, steer]`; the
/// next turn pushes its own prompt and this wire sees `[prompt, reply, steer,
/// prompt2]` — byte-identical to the within-turn `[prompt, reply, steer,
/// block]` above, since every user message is a `Message::user` and ids and
/// timestamps ascend across a turn boundary exactly as they do within one. So
/// the run is bounded by [`ChatRequest::turn_start`] and not by anything this
/// module could read off `messages`: without it, this asserted the steer being
/// re-sent.
#[test]
fn a_finished_turns_steer_stays_in_that_turn() {
    let across_turns = vec![
        Message::user("write the config parser"),
        Message::assistant("gpt-5.3-codex"),
        Message::user("actually make it lenient about unknown keys"),
        Message::user("now add tests"),
    ];

    assert_eq!(
        newest_user_text(&turn(across_turns, 3)),
        "now add tests",
        "the previous turn consumed that steer; this turn is its prompt alone",
    );
}

/// And the shape the marker does **not** close, pinned as what it is rather
/// than left to be discovered: a continuation block emitted where nothing was
/// steered reaches this wire without the prompt it is about.
///
/// The request reads `[prompt, reply, block]` and the turn opened at the
/// prompt, so `turn_start` is `0` and the run is still the block alone — the
/// marker raises the run's lower bound and never lowers it, and lowering it
/// here would mean reaching back *past the assistant's reply*, whose text this
/// wire does not send. Closing it needs the wire to carry more than the newest
/// user turn, which is a different change than bounding that turn.
#[test]
fn a_continuation_block_still_arrives_without_the_prompt_it_is_about() {
    let continued = vec![
        Message::user("port the config loader"),
        Message::assistant("gpt-5.3-codex"),
        Message::user("<team_still_working>keep going</team_still_working>"),
    ];

    assert_eq!(
        newest_user_text(&turn(continued, 0)),
        "<team_still_working>keep going</team_still_working>",
    );
}

/// A marker pointing past the newest user message answers that message rather
/// than panicking the wire.
///
/// `turn_start` is a `pub` field, so its value is whatever a caller put there:
/// a request ending in an assistant message with the marker on the index after
/// the user message before it would slice `first > newest`, and a wire that
/// panics on a struct field's value is a wire that a caller's arithmetic can
/// crash. The run is clamped to the newest user message instead, which is the
/// most honest thing this walk can still say.
#[test]
fn a_turn_marker_past_the_newest_user_message_does_not_panic_the_walk() {
    let overshot = vec![
        Message::user("write the config parser"),
        Message::user("and make it lenient"),
        Message::assistant("gpt-5.3-codex"),
    ];

    assert_eq!(
        newest_user_text(&turn(overshot, 2)),
        "and make it lenient",
        "the newest user message alone, and no panic",
    );
}

#[test]
fn a_minted_id_is_a_v4_uuid_and_two_are_two() {
    let id = fresh_id().expect("entropy is available");
    assert_eq!(id.len(), 36);
    assert_eq!(id.as_bytes()[14], b'4', "the version nibble: {id}");
    assert!(matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'), "the variant bits: {id}");
    assert_ne!(id, fresh_id().expect("entropy is available"));
}

// One test per row of D550's table: a framed exec of that kind, classified
// and answered, with the answer's *field numbers* read off the wire. The
// numbers are the shipped client's own `ExecClientMessage`
// (`index.js@6305844`), where a result's number equals its args' number —
// so `[1, N, 15]` below reads as "the id, the arm at N, the exec id", and a
// renumbered `.proto` reddens the row it broke rather than the whole file.

#[test]
fn a_shell_exec_is_rejected_at_field_2_echoing_the_command_it_named() {
    let refused = refused(proto::ExecRequest {
        id: Some(1),
        exec_id: Some("exec-1".to_owned()),
        shell_args: buffa::MessageField::some(
            proto::ShellArgs::default().with_command("rm -rf /").with_working_directory("/tmp"),
        ),
        ..Default::default()
    });

    let answer = typed(&refused, 1);
    assert_eq!(field_numbers(&answer.encode_to_vec()), vec![1, 2, 15], "shell_result = 2");
    assert_eq!(answer.exec_id.as_deref(), Some("exec-1"), "both ids come back");

    let rejected = answer
        .shell_result
        .as_option()
        .and_then(|result| result.rejected.as_option())
        .expect("the rejected arm");
    assert_eq!(rejected.command.as_deref(), Some("rm -rf /"));
    assert_eq!(rejected.working_directory.as_deref(), Some("/tmp"));
    assert_eq!(rejected.reason.as_deref(), Some(refusal("shell_args").as_str()));
    assert_eq!(
        field_numbers(&rejected.encode_to_vec()),
        vec![1, 2, 3],
        "is_readonly = 4 states a permission verdict this refusal does not make"
    );
}

/// The one streamed kind, and the shape **AC-2** pins: exactly one
/// `ShellStream{rejected}` event and then the stream close, with nothing
/// between them. A served shell would have written stdout events first;
/// a refused one writes the rejection and stops.
#[test]
fn a_shell_stream_exec_is_one_rejected_event_at_field_14_and_then_the_close() {
    let refused = refused(proto::ExecRequest {
        id: Some(2),
        exec_id: Some("exec-2".to_owned()),
        shell_stream_args: buffa::MessageField::some(
            proto::ShellArgs::default().with_command("npm test").with_working_directory("/repo"),
        ),
        ..Default::default()
    });

    let answer = typed(&refused, 2);
    assert_eq!(field_numbers(&answer.encode_to_vec()), vec![1, 14, 15], "shell_stream = 14");

    let rejected = answer
        .shell_stream
        .as_option()
        .and_then(|stream| stream.rejected.as_option())
        .expect("the stream's rejected event");
    assert_eq!(rejected.command.as_deref(), Some("npm test"));
    assert_eq!(rejected.working_directory.as_deref(), Some("/repo"));
    assert_eq!(rejected.reason.as_deref(), Some(refusal("shell_stream_args").as_str()));
}

#[test]
fn a_write_exec_is_rejected_at_field_3_echoing_the_path_it_named() {
    let refused = refused(proto::ExecRequest {
        id: Some(3),
        exec_id: Some("exec-3".to_owned()),
        write_args: buffa::MessageField::some(proto::WriteArgs::default().with_path("src/main.rs")),
        ..Default::default()
    });

    let answer = typed(&refused, 3);
    assert_eq!(field_numbers(&answer.encode_to_vec()), vec![1, 3, 15], "write_result = 3");

    let rejected = answer
        .write_result
        .as_option()
        .and_then(|result| result.rejected.as_option())
        .expect("the rejected arm");
    assert_eq!(rejected.path.as_deref(), Some("src/main.rs"));
    assert_eq!(rejected.reason.as_deref(), Some(refusal("write_args").as_str()));
}

#[test]
fn a_delete_exec_is_rejected_at_field_4_echoing_the_path_it_named() {
    let refused = refused(proto::ExecRequest {
        id: Some(4),
        exec_id: Some("exec-4".to_owned()),
        delete_args: buffa::MessageField::some(
            proto::DeleteArgs::default().with_path("Cargo.lock"),
        ),
        ..Default::default()
    });

    let answer = typed(&refused, 4);
    assert_eq!(field_numbers(&answer.encode_to_vec()), vec![1, 4, 15], "delete_result = 4");

    let rejected = answer
        .delete_result
        .as_option()
        .and_then(|result| result.rejected.as_option())
        .expect("the rejected arm");
    assert_eq!(rejected.path.as_deref(), Some("Cargo.lock"));
    assert_eq!(rejected.reason.as_deref(), Some(refusal("delete_args").as_str()));
}

/// Grep has no rejected arm in the shipped descriptor, so its refusal
/// travels as the error — the only member of `GrepResult` that can say
/// anything at all — and echoes nothing, because `GrepError` has nowhere
/// to put the query.
#[test]
fn a_grep_exec_is_refused_on_its_error_arm_at_field_5() {
    let refused = refused(proto::ExecRequest {
        id: Some(5),
        exec_id: Some("exec-5".to_owned()),
        grep_args: buffa::MessageField::some(proto::GrepArgs::default()),
        ..Default::default()
    });

    let answer = typed(&refused, 5);
    assert_eq!(field_numbers(&answer.encode_to_vec()), vec![1, 5, 15], "grep_result = 5");

    let error = answer
        .grep_result
        .as_option()
        .and_then(|result| result.error.as_option())
        .expect("the error arm");
    assert_eq!(error.error.as_deref(), Some(refusal("grep_args").as_str()));
}

#[test]
fn a_read_exec_is_rejected_at_field_7_echoing_the_path_it_named() {
    let refused = refused(proto::ExecRequest {
        id: Some(7),
        exec_id: Some("exec-7".to_owned()),
        read_args: buffa::MessageField::some(proto::ReadArgs::default().with_path("README.md")),
        ..Default::default()
    });

    let answer = typed(&refused, 7);
    assert_eq!(field_numbers(&answer.encode_to_vec()), vec![1, 7, 15], "read_result = 7");

    let rejected = answer
        .read_result
        .as_option()
        .and_then(|result| result.rejected.as_option())
        .expect("the rejected arm");
    assert_eq!(rejected.path.as_deref(), Some("README.md"));
    assert_eq!(rejected.reason.as_deref(), Some(refusal("read_args").as_str()));
}

/// The redacted read is the same `ReadResult` under a second number, which
/// is what makes its arm free: the answer differs from the plain read's in
/// nothing but the field it goes out on.
#[test]
fn a_redacted_read_exec_is_rejected_at_field_29_with_the_plain_reads_own_arm() {
    let refused = refused(proto::ExecRequest {
        id: Some(29),
        exec_id: Some("exec-29".to_owned()),
        redacted_read_args: buffa::MessageField::some(proto::ReadArgs::default().with_path(".env")),
        ..Default::default()
    });

    let answer = typed(&refused, 29);
    assert_eq!(
        field_numbers(&answer.encode_to_vec()),
        vec![1, 15, 29],
        "redacted_read_result = 29, written after exec_id = 15 because that is field order"
    );
    assert!(answer.read_result.as_option().is_none(), "the plain read's field stays empty");

    let rejected = answer
        .redacted_read_result
        .as_option()
        .and_then(|result| result.rejected.as_option())
        .expect("the rejected arm");
    assert_eq!(rejected.path.as_deref(), Some(".env"));
    assert_eq!(rejected.reason.as_deref(), Some(refusal("redacted_read_args").as_str()));
}

#[test]
fn an_ls_exec_is_rejected_at_field_8_echoing_the_path_it_named() {
    let refused = refused(proto::ExecRequest {
        id: Some(8),
        exec_id: Some("exec-8".to_owned()),
        ls_args: buffa::MessageField::some(proto::LsArgs::default().with_path("crates")),
        ..Default::default()
    });

    let answer = typed(&refused, 8);
    assert_eq!(field_numbers(&answer.encode_to_vec()), vec![1, 8, 15], "ls_result = 8");

    let rejected = answer
        .ls_result
        .as_option()
        .and_then(|result| result.rejected.as_option())
        .expect("the rejected arm");
    assert_eq!(rejected.path.as_deref(), Some("crates"));
    assert_eq!(rejected.reason.as_deref(), Some(refusal("ls_args").as_str()));
}

/// The MCP call is refused about the *name* rather than about a policy:
/// this client publishes no roster, so the honest answer is that nothing
/// answers to what was called. `McpRejected` has no member for a roster,
/// which is why the sentence is the whole answer.
#[test]
fn an_mcp_exec_is_rejected_at_field_11_naming_the_tool_that_was_called() {
    let refused = refused(proto::ExecRequest {
        id: Some(11),
        exec_id: Some("exec-11".to_owned()),
        mcp_args: buffa::MessageField::some(
            proto::McpArgs::default().with_name("read_file").with_tool_call_id("call-9"),
        ),
        ..Default::default()
    });

    let answer = typed(&refused, 11);
    assert_eq!(field_numbers(&answer.encode_to_vec()), vec![1, 11, 15], "mcp_result = 11");

    let rejected = answer
        .mcp_result
        .as_option()
        .and_then(|result| result.rejected.as_option())
        .expect("the rejected arm");
    assert_eq!(
        rejected.reason.as_deref(),
        Some("no tool named read_file is served by this client"),
        "the name that was called is the subject, not the kind"
    );
    assert_eq!(
        field_numbers(&rejected.encode_to_vec()),
        vec![1],
        "is_readonly = 2 is absent for ShellRejected's reason"
    );
}

/// Fetch, like grep, has only an error arm — but that one does echo, so
/// the url the server named comes back beside the reason.
#[test]
fn a_fetch_exec_is_refused_on_its_error_arm_at_field_20_echoing_the_url() {
    let refused = refused(proto::ExecRequest {
        id: Some(20),
        exec_id: Some("exec-20".to_owned()),
        fetch_args: buffa::MessageField::some(
            proto::FetchArgs::default().with_url("https://example.invalid/"),
        ),
        ..Default::default()
    });

    let answer = typed(&refused, 20);
    assert_eq!(
        field_numbers(&answer.encode_to_vec()),
        vec![1, 15, 20],
        "fetch_result = 20, written after exec_id = 15 because that is field order"
    );

    let error = answer
        .fetch_result
        .as_option()
        .and_then(|result| result.error.as_option())
        .expect("the error arm");
    assert_eq!(error.url.as_deref(), Some("https://example.invalid/"));
    assert_eq!(error.error.as_deref(), Some(refusal("fetch_args").as_str()));
}

/// A kind outside D550's table takes the throw, all the way through the
/// real classifier — the row that proves the catch-all is still reachable
/// rather than shadowed by the ten arms in front of it.
#[test]
fn an_exec_kind_outside_the_table_still_reaches_the_throw() {
    let mut asked = proto::ExecRequest { id: Some(42), ..Default::default() };
    asked.__buffa_unknown_fields.push(buffa::UnknownField {
        number: 42,
        data: buffa::UnknownFieldData::LengthDelimited(Vec::new()),
    });

    let refused = refused(asked);
    assert_eq!(refused.len(), 2, "a throw, and the close that ends it");
    assert!(refused[0].exec_response.as_option().is_none(), "no result arm answers this kind");

    let thrown = refused[0]
        .exec_control
        .as_option()
        .and_then(|control| control.throw.as_option())
        .expect("the throw");
    assert_eq!(thrown.id, Some(42));
    assert_eq!(
        thrown.error.as_deref(),
        Some(refusal("mcp_allowlist_precheck_args").as_str()),
        "field 42 is a kind the table names but models no arm for"
    );
}

/// **AC-3**, the switchboard: the context answer carries exactly the three
/// members D550 chose and no others. `web_search_enabled = 17` is the one
/// this asserts *absent* — leaving it off costs the seat a capability for
/// nothing, and a later tidy-up that "completes" the set reddens here.
#[test]
fn the_context_answer_carries_exactly_the_three_switchboard_members() {
    let bytes = context_answer(decode::ContextAsk { id: None, exec_id: None }, None, true);
    let decoded = proto::ClientMessage::decode_from_slice(&bytes).expect("decodes");
    let context = decoded
        .exec_response
        .as_option()
        .and_then(|answer| answer.request_context_result.as_option())
        .and_then(|result| result.success.as_option())
        .and_then(|success| success.request_context.as_option())
        .expect("the context");

    assert_eq!(
        field_numbers(&context.encode_to_vec()),
        vec![23, 24, 35],
        "mcp_file_system_options, web_fetch_enabled, read_lints_enabled — and no 17"
    );
    assert_eq!(
        context.mcp_file_system_options.as_option().and_then(|options| options.enabled),
        Some(false),
        "the filesystem meta-tool is declined, not merely unmentioned"
    );
    assert_eq!(context.web_fetch_enabled, Some(true));
    assert_eq!(context.read_lints_enabled, Some(false));
}

/// The same three when a prompt rides along, so the switchboard is not
/// something the presence of a `cloud_rule` moves.
#[test]
fn a_prompt_joins_the_switchboard_rather_than_replacing_it() {
    let bytes =
        context_answer(decode::ContextAsk { id: None, exec_id: None }, Some("Be terse."), false);
    let decoded = proto::ClientMessage::decode_from_slice(&bytes).expect("decodes");
    let context = decoded
        .exec_response
        .as_option()
        .and_then(|answer| answer.request_context_result.as_option())
        .and_then(|result| result.success.as_option())
        .and_then(|success| success.request_context.as_option())
        .expect("the context");

    assert_eq!(field_numbers(&context.encode_to_vec()), vec![16, 23, 24, 35]);
    assert_eq!(context.web_fetch_enabled, Some(false), "a false boolean is sent, not omitted");
}

/// **AC-3**'s predicate: a request that declared tools can serve a fetch,
/// and one that declared none cannot. The second is the shape of a one-shot
/// title or summary turn, which correctly draws no fetch execs.
#[test]
fn the_fetch_predicate_follows_the_requests_own_tool_roster() {
    let mut with_tools = request();
    with_tools.tools = vec![ToolDefinition {
        name: "webfetch".to_owned(),
        description: "Fetch a url.".to_owned(),
        schema: serde_json::json!({"type": "object"}),
    }];
    assert!(serves_fetch(&with_tools), "a roster-bearing request can redirect a fetch");

    let one_shot = ChatRequest { turn_start: 0, tools: Vec::new(), ..request() };
    assert!(!serves_fetch(&one_shot), "a one-shot turn is offered no tools and asks for none");
}
