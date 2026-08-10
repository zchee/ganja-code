//! The cursor wire's responses, decoded as they arrive.
//!
//! Spec: `.omc/research/cursor/spike-wire-facts.md`. Two shapes: the unary
//! model listing is bare protobuf with no framing at all (LIVE-OBSERVED —
//! the reference client's tolerance for a framed unary response is dead code
//! on the real server), decoded whole because a unary body is one message;
//! and the Run stream is Connect frames whose verdict rides the in-body
//! EndStream frame, mapped one frame at a time by [`Mapping`] so the reply
//! reaches the session while the server is still talking.

use buffa::Message as _;

use super::{ID, connect, proto};
use crate::{
    protocol::FinishReason,
    provider::{ProviderError, ProviderEvent},
};

/// The models the listing served, in the server's order.
///
/// # Errors
///
/// Returns [`ProviderError::Parse`] when the body is not the listing's
/// protobuf.
pub(super) fn model_list(body: &[u8]) -> Result<Vec<proto::ModelEntry>, ProviderError> {
    let decoded = proto::GetUsableModelsResponse::decode_from_slice(body).map_err(|error| {
        ProviderError::Parse(format!("the model listing did not decode: {error}"))
    })?;

    Ok(decoded.models)
}

/// Turns Run frames into events, one frame at a time.
///
/// The shape is the SSE wires' `Mapper`, spelled for Connect frames:
/// [`frame`](Self::frame) appends what one frame means, and
/// [`truncated`](Self::truncated) judges a body that ended without its
/// EndStream frame. [`ProviderEvent::Finish`] and [`ProviderEvent::Failed`]
/// are terminal — the stream layer hands out nothing after either.
///
/// **The exec arm is never skipped.** The server sends its context ask as an
/// exec request and waits on the answer before generating — a build that
/// skipped it hung a real turn in silence (LIVE-OBSERVED 2026-08-10, one
/// debug line then nothing until the process was killed). So [`frame`](Self::frame)
/// hands the ask up as a [`ContextAsk`] for the stream layer to answer on
/// the open request body, and an exec kind this build cannot answer fails
/// the turn with the kind's name on it rather than reproduce the silence.
///
/// **The kv arm is never skipped either.** The server stores and reads
/// conversation state mid-turn over the kv channel and waits on every
/// exchange before it will end the turn — the 2026-08-10 live run left four
/// of them unanswered and then sat silent until timeout. So a kv get or set
/// is handed up as an [`Ask`] beside the context ask, answered by the
/// stream layer against the turn's blob store, and a kv kind beyond get and
/// set fails the turn with its field number on it — the exec channel's
/// no-hang discipline, applied to the second channel the server waits on.
///
/// Updates this build does not model — the tool-call, summary, token and
/// step arms, and whole server messages outside the update, exec and kv
/// channels — are skipped, not failed: the server adds arms between client
/// versions, and a turn that died on one would make every addition a
/// breaking change. A skipped update is logged at debug with its set field
/// numbers named where the plugin's descriptor knows them, which is where
/// "why is the reply shorter than the server's" is answered — by arm, not
/// by guesswork.
///
/// **`turn_ended` is noted; the verdict waits for the EndStream frame.**
/// The two are the application and the protocol saying different things —
/// "the turn is over" and "here is how the stream ended" — exactly the
/// Anthropic wire's `stop_reason`/`message_stop` split, and they are handled
/// its way: a clean EndStream finishes the turn, a body that dies after
/// `turn_ended` lost only its terminator and finishes too, and an EndStream
/// **error** after `turn_ended` fails the turn — the server's verdict
/// outranks the model's goodbye. The one-shot decode this replaces broke at
/// `turn_ended` and never read that verdict at all.
#[derive(Debug, Default)]
pub(super) struct Mapping {
    /// The server marked the turn ended, so the reply is complete with or
    /// without the terminator.
    ended: bool,
}

/// A mid-stream question the server waits on, carried up to the stream
/// layer: the decode layer reads frames and holds no channel to answer one
/// on, so the layer that owns the request body sends the reply. Two kinds,
/// because the server waits on two channels — the exec channel's context
/// ask and the kv channel's blob exchanges.
#[derive(Debug, PartialEq)]
pub(super) enum Ask {
    Context(ContextAsk),
    Kv(KvAsk),
}

/// The server's context ask, ids and nothing else — presence is the whole
/// question.
#[derive(Debug, PartialEq)]
pub(super) struct ContextAsk {
    /// The exchange ids the answer must echo, verbatim — the plugin's own
    /// answers are built that way (proxy.ts:1307-1310).
    pub(super) id: Option<u32>,
    pub(super) exec_id: Option<String>,
}

/// One kv exchange the server opened: the id the answer must echo
/// (proxy.ts:1075-1077), and which of the two operations the oneof carried.
#[derive(Debug, PartialEq)]
pub(super) struct KvAsk {
    pub(super) id: Option<u32>,
    pub(super) op: KvOp,
}

/// The kv channel's whole vocabulary — get and set are the oneof's only
/// arms (agent_pb.ts:7941, :7948).
#[derive(Debug, PartialEq)]
pub(super) enum KvOp {
    /// The server reading back what it stored; answered from the blob store,
    /// found or not.
    Get { blob_id: Vec<u8> },
    /// The server storing state for the turn; answered with the empty ack.
    Set { blob_id: Vec<u8>, data: Vec<u8> },
}

impl Mapping {
    /// Maps `frame`, appending whatever it means to `events`; a returned
    /// [`Ask`] is the server waiting, and the caller must answer it.
    pub(super) fn frame(
        &mut self,
        frame: &connect::Frame,
        events: &mut Vec<ProviderEvent>,
    ) -> Option<Ask> {
        if frame.is_end_stream() {
            match connect::end_stream_error(&frame.payload) {
                Ok(Some((code, message))) => {
                    events.push(ProviderEvent::Failed(verdict(&code, &message)));
                }
                Ok(None) => events.push(ProviderEvent::Finish(FinishReason::Completed)),
                Err(error) => events.push(ProviderEvent::Failed(error)),
            }
            return None;
        }

        let message = match proto::ServerMessage::decode_from_slice(&frame.payload) {
            Ok(message) => message,
            Err(error) => {
                // Every data frame is a server message and every one of them
                // means something, so stepping past a broken one would
                // silently drop part of the reply.
                events.push(ProviderEvent::Failed(ProviderError::Parse(format!(
                    "a server message did not decode: {error}"
                ))));
                return None;
            }
        };

        if let Some(exec) = message.exec_request.as_option() {
            if exec.request_context_args.is_set() {
                return Some(Ask::Context(ContextAsk {
                    id: exec.id,
                    exec_id: exec.exec_id.clone(),
                }));
            }

            // The server stops generating until an exec is answered, so a
            // kind this build cannot answer ends the turn with its name on
            // it — the alternative was the silent hang this arm's modelling
            // exists to end.
            events.push(ProviderEvent::Failed(ProviderError::Parse(format!(
                "cursor made an exec request this build cannot answer ({}); \
                 leaving it unanswered would hang the turn",
                exec_kind(exec)
            ))));
            return None;
        }

        if let Some(kv) = message.kv_request.as_option() {
            if let Some(get) = kv.get_blob_args.as_option() {
                return Some(Ask::Kv(KvAsk {
                    id: kv.id,
                    op: KvOp::Get {
                        blob_id: get.blob_id.clone().unwrap_or_default(),
                    },
                }));
            }
            if let Some(set) = kv.set_blob_args.as_option() {
                return Some(Ask::Kv(KvAsk {
                    id: kv.id,
                    op: KvOp::Set {
                        blob_id: set.blob_id.clone().unwrap_or_default(),
                        data: set.blob_data.clone().unwrap_or_default(),
                    },
                }));
            }

            // The server waits on every kv exchange the way it waits on
            // every exec, so a kind this build cannot answer ends the turn
            // with its number on it rather than reproduce the silence the
            // unanswered channel produced live.
            events.push(ProviderEvent::Failed(ProviderError::Parse(format!(
                "cursor made a kv request this build cannot answer ({}); \
                 leaving it unanswered would hang the turn",
                kv_kind(kv)
            ))));
            return None;
        }

        let Some(update) = message.interaction_update.as_option() else {
            tracing::debug!(
                provider = ID,
                fields = ?message
                    .__buffa_unknown_fields
                    .iter()
                    .map(|field| field.number)
                    .collect::<Vec<_>>(),
                "skipped a server message outside the update channel"
            );
            return None;
        };

        if let Some(delta) = update.text_delta.as_option() {
            events.push(ProviderEvent::TextDelta(
                delta.text.clone().unwrap_or_default(),
            ));
        } else if let Some(delta) = update.thinking_delta.as_option() {
            // The plugin forwards thinking to its clients beside reply text,
            // marked as thinking (proxy.ts:1059-1061); here that mark is the
            // event the Anthropic wire's thinking blocks already arrive as.
            events.push(ProviderEvent::ReasoningDelta(
                delta.text.clone().unwrap_or_default(),
            ));
        } else if update.turn_ended.is_set() {
            self.ended = true;
        } else if update.heartbeat.is_set() {
            // Liveness, carrying nothing.
        } else {
            // The arms are named so the next live run reads as a list of
            // decisions rather than a count of mysteries: every number here
            // is one the plugin's descriptor declares and this build chose
            // not to model.
            tracing::debug!(
                provider = ID,
                arms = ?update
                    .__buffa_unknown_fields
                    .iter()
                    .map(|field| update_arm(field.number))
                    .collect::<Vec<_>>(),
                "skipped an update this build does not model"
            );
        }

        None
    }

    /// Reports a body that ended without its EndStream frame.
    ///
    /// After `turn_ended` the reply was complete and only the terminator was
    /// lost — the Anthropic wire's reading of a body cut off after the stop
    /// reason. Before it, reply text nobody can recover is gone, and calling
    /// that a short answer would be the lie this variant exists to prevent.
    pub(super) fn truncated(&mut self, events: &mut Vec<ProviderEvent>) {
        if self.ended {
            events.push(ProviderEvent::Finish(FinishReason::Completed));
            return;
        }

        events.push(ProviderEvent::Failed(ProviderError::Transport(
            "the response body ended before the exchange finished".to_owned(),
        )));
    }
}

/// What an EndStream error means to the session that asked.
///
/// `unauthenticated` is the one code whose repair is a command this build
/// ships, so it becomes [`ProviderError::Auth`] and names it. Everything
/// else is the provider answering unsuccessfully — [`ProviderError::Status`]
/// — under the HTTP status the Connect protocol itself assigns the code,
/// because the wire's own status was a 200 with the failure in the body and
/// "200: invalid_argument" is a sentence that reads as a defect.
fn verdict(code: &str, message: &str) -> ProviderError {
    if code == "unauthenticated" {
        return ProviderError::Auth(format!(
            "cursor rejected the credential: {message}; run `ganja auth login {ID}`"
        ));
    }

    ProviderError::Status {
        status: connect::http_status(code),
        message: format!("connect error {code}: {message}"),
    }
}

/// Names the kind an unanswerable exec request carried.
///
/// The kinds this build does not model arrive as unknown fields, and the
/// field number *is* the kind: the table is the plugin's args oneof
/// (agent_pb.ts:6885-:6997), so the refusal names what the descriptor
/// names. A number the table does not know is reported as itself — still
/// enough to go derive — and span_context (= 19, agent_pb.ts:6875) rides
/// beside the oneof without being a kind, so it is passed over rather than
/// blamed.
fn exec_kind(exec: &proto::ExecRequest) -> String {
    let named = |number: u32| {
        Some(match number {
            2 => "shell_args",
            3 => "write_args",
            4 => "delete_args",
            5 => "grep_args",
            7 => "read_args",
            8 => "ls_args",
            9 => "diagnostics_args",
            11 => "mcp_args",
            14 => "shell_stream_args",
            16 => "background_shell_spawn_args",
            17 => "list_mcp_resources_exec_args",
            18 => "read_mcp_resource_exec_args",
            20 => "fetch_args",
            21 => "record_screen_args",
            22 => "computer_use_args",
            23 => "write_shell_stdin_args",
            _ => return None,
        })
    };

    let fields = &exec.__buffa_unknown_fields;
    if let Some(kind) = fields.iter().find_map(|field| named(field.number)) {
        return kind.to_owned();
    }

    match fields
        .iter()
        .map(|field| field.number)
        .find(|number| *number != 19)
    {
        Some(number) => format!("field {number}"),
        None => "no recognizable kind".to_owned(),
    }
}

/// Names a skipped update's arm the way the plugin's descriptor does.
///
/// The table is the plugin's InteractionUpdate oneof (agent_pb.ts:3160-
/// :3272); the arms this build models — text_delta = 1, thinking_delta = 4,
/// heartbeat = 13, turn_ended = 14 — never reach it, because a modeled arm
/// decodes into its field rather than into the unknowns. A number outside
/// the table is a server newer than the descriptor, reported as itself —
/// still enough to go derive.
fn update_arm(number: u32) -> String {
    let named = match number {
        2 => "tool_call_started",
        3 => "tool_call_completed",
        5 => "thinking_completed",
        6 => "user_message_appended",
        7 => "partial_tool_call",
        8 => "token_delta",
        9 => "summary",
        10 => "summary_started",
        11 => "summary_completed",
        12 => "shell_output_delta",
        15 => "tool_call_delta",
        16 => "step_started",
        17 => "step_completed",
        _ => return format!("field {number}"),
    };

    format!("{named} ({number})")
}

/// Names the kind an unanswerable kv request carried.
///
/// Get and set are the plugin's whole oneof (agent_pb.ts:7941, :7948) and
/// both are modeled, so an unanswerable kind can only be an arm newer than
/// the descriptor, arriving as an unknown field whose number is the kind.
/// span_context (= 4, agent_pb.ts:7931) rides beside the oneof without
/// being a kind, so it is passed over rather than blamed.
fn kv_kind(kv: &proto::KvRequest) -> String {
    match kv
        .__buffa_unknown_fields
        .iter()
        .map(|field| field.number)
        .find(|number| *number != 4)
    {
        Some(number) => format!("field {number}"),
        None => "no recognizable kind".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use buffa::Message as _;

    use super::{
        super::{connect, proto},
        Ask, ContextAsk, FinishReason, KvAsk, KvOp, Mapping, ProviderError, ProviderEvent,
        model_list, verdict,
    };

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
            thinking_delta: buffa::MessageField::some(
                proto::ThinkingDelta::default().with_text(delta),
            ),
            ..Default::default()
        }
    }

    /// A data frame holding one kv request, the way the server frames one.
    fn kv_framed(kv: proto::KvRequest) -> Vec<u8> {
        let message = proto::ServerMessage {
            kv_request: buffa::MessageField::some(kv),
            ..Default::default()
        };

        connect::envelope(&message.encode_to_vec())
    }

    /// An EndStream frame carrying `payload`.
    fn end_stream(payload: &str) -> Vec<u8> {
        let mut frame = vec![0b0000_0010];
        frame.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("a test payload fits")
                .to_be_bytes(),
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

        assert_eq!(
            mapped(&body, false).last(),
            Some(&ProviderEvent::Finish(FinishReason::Completed))
        );
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
        body.extend(end_stream(
            r#"{"error":{"code":"internal","message":"boom"}}"#,
        ));

        let events = mapped(&body, false);
        assert_eq!(events[0], ProviderEvent::TextDelta("partial".to_owned()));
        assert!(
            matches!(
                &events[1],
                ProviderEvent::Failed(ProviderError::Status { status: 500, .. })
            ),
            "{events:?}"
        );
    }

    #[test]
    fn a_body_without_an_ending_is_a_truncation_not_a_short_answer() {
        let events = mapped(&framed(heartbeat()), true);
        assert!(
            matches!(
                events.as_slice(),
                [ProviderEvent::Failed(ProviderError::Transport(_))]
            ),
            "{events:?}"
        );

        let events = mapped(&framed(text("half")), true);
        assert_eq!(events[0], ProviderEvent::TextDelta("half".to_owned()));
        assert!(
            matches!(
                &events[1],
                ProviderEvent::Failed(ProviderError::Transport(_))
            ),
            "{events:?}"
        );
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
        body.extend(end_stream(
            r#"{"error":{"code":"resource_exhausted","message":"quota spent"}}"#,
        ));

        let events = mapped(&body, false);
        assert_eq!(events[0], ProviderEvent::TextDelta("said".to_owned()));
        assert!(
            matches!(
                &events[1],
                ProviderEvent::Failed(ProviderError::Status { status: 429, .. })
            ),
            "{events:?}"
        );
    }

    #[test]
    fn a_frame_that_is_not_a_server_message_fails_the_turn_readably() {
        // 0xff opens a field with wire type 7, which protobuf does not have.
        let events = mapped(&connect::envelope(&[0xff, 0xff, 0xff]), false);
        assert!(
            matches!(
                events.as_slice(),
                [ProviderEvent::Failed(ProviderError::Parse(_))]
            ),
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
        assert!(
            events.is_empty(),
            "an ask is a question, not an event: {events:?}"
        );
        assert_eq!(
            asks,
            vec![Ask::Context(ContextAsk {
                id: Some(7),
                exec_id: Some("exec-abc".to_owned()),
            })]
        );
    }

    /// An exec kind this build does not model must end the turn with the
    /// kind's name on it: the server waits on every exec, and the skipped
    /// alternative was a silent hang.
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
        assert!(
            events.is_empty(),
            "a kv exchange is a question, not an event: {events:?}"
        );
        assert_eq!(
            asks,
            vec![
                Ask::Kv(KvAsk {
                    id: Some(11),
                    op: KvOp::Set {
                        blob_id: b"blob-a".to_vec(),
                        data: b"opaque-state".to_vec(),
                    },
                }),
                Ask::Kv(KvAsk {
                    id: Some(12),
                    op: KvOp::Get {
                        blob_id: b"blob-a".to_vec(),
                    },
                }),
            ]
        );
    }

    /// A kv kind beyond get and set gets the exec channel's discipline: the
    /// server waits on it, so the turn fails naming the field — and the
    /// span context riding beside the oneof (agent_pb.ts:7931) is never
    /// mistaken for one.
    #[test]
    fn a_kv_kind_this_build_cannot_answer_fails_the_turn_by_name() {
        let mut asked = proto::KvRequest {
            id: Some(3),
            ..Default::default()
        };
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

    /// The arm names the skip log leans on: the plugin's own oneof spelling
    /// for the numbers it declares, and the bare number for anything newer.
    #[test]
    fn a_skipped_arm_is_named_the_way_the_plugins_descriptor_names_it() {
        assert_eq!(super::update_arm(8), "token_delta (8)");
        assert_eq!(super::update_arm(16), "step_started (16)");
        assert_eq!(super::update_arm(42), "field 42");
    }

    #[test]
    fn an_exec_kind_this_build_cannot_answer_fails_the_turn_by_name() {
        // The arm arrives as an unknown field; its number is the kind. The
        // shell_args arm is field 2 of the plugin's oneof (agent_pb.ts:6885).
        let mut asked = proto::ExecRequest::default().with_id(3);
        asked.__buffa_unknown_fields.push(buffa::UnknownField {
            number: 2,
            data: buffa::UnknownFieldData::LengthDelimited(Vec::new()),
        });

        let (events, asks) = mapped_asks(&exec_framed(asked), false);
        assert!(asks.is_empty(), "nothing to answer: {asks:?}");
        assert!(
            matches!(
                events.as_slice(),
                [ProviderEvent::Failed(ProviderError::Parse(message))]
                    if message.contains("shell_args")
            ),
            "{events:?}"
        );
    }

    /// A kind the table has never heard of is still named — by number —
    /// and the span context riding beside the oneof is never mistaken for
    /// one.
    #[test]
    fn an_unheard_of_exec_kind_is_named_by_its_field_number() {
        let mut asked = proto::ExecRequest::default();
        // span_context = 19 rides beside the args oneof (agent_pb.ts:6875).
        asked.__buffa_unknown_fields.push(buffa::UnknownField {
            number: 19,
            data: buffa::UnknownFieldData::LengthDelimited(Vec::new()),
        });
        asked.__buffa_unknown_fields.push(buffa::UnknownField {
            number: 42,
            data: buffa::UnknownFieldData::LengthDelimited(Vec::new()),
        });

        let (events, _) = mapped_asks(&exec_framed(asked), false);
        assert!(
            matches!(
                events.as_slice(),
                [ProviderEvent::Failed(ProviderError::Parse(message))]
                    if message.contains("field 42") && !message.contains("19")
            ),
            "{events:?}"
        );

        let (events, _) = mapped_asks(&exec_framed(proto::ExecRequest::default()), false);
        assert!(
            matches!(
                events.as_slice(),
                [ProviderEvent::Failed(ProviderError::Parse(message))]
                    if message.contains("no recognizable kind")
            ),
            "{events:?}"
        );
    }

    #[test]
    fn the_model_listing_decodes_and_a_wrong_body_is_a_parse_error() {
        let listing = proto::GetUsableModelsResponse {
            models: vec![
                proto::ModelEntry::default()
                    .with_model_id("default")
                    .with_display_model_id("auto"),
                proto::ModelEntry::default().with_model_id("gpt-5.3-codex"),
            ],
            ..Default::default()
        }
        .encode_to_vec();

        let models = model_list(&listing).expect("the listing decodes");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].model_id.as_deref(), Some("default"));

        assert!(matches!(
            model_list(&[0xff, 0xff, 0xff]),
            Err(ProviderError::Parse(_))
        ));
    }

    /// The `default` entry's first bytes, encoded by this build, are the
    /// bytes the live probe recorded off the wire — the field numbers and
    /// types in `cursor.proto` really are the server's.
    #[test]
    fn the_encoding_matches_the_bytes_recorded_off_the_live_wire() {
        let entry = proto::ModelEntry::default()
            .with_model_id("default")
            .with_display_model_id("auto");

        assert_eq!(
            &entry.encode_to_vec()[..15],
            // spike-wire-facts.md S4: `0a 07 default 1a 04 auto`, inside the
            // response's first entry.
            b"\x0a\x07default\x1a\x04auto",
        );
    }
}
