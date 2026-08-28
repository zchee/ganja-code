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
use crate::protocol::FinishReason;
use crate::provider::{ProviderError, ProviderEvent};

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
/// the open request body. Every *other* exec kind — the tools cursor's
/// server asks its own client to run — is handed up as an [`ExecRefusal`]
/// instead, answered on the same body with a structured refusal rather than
/// killing the turn (**D486**, declared in [`super::request`]): the server's
/// agent loop reads a refusal the way it reads any other tool outcome and
/// keeps generating, which is one more turn surviving than the failure this
/// replaced.
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
/// on, so the layer that owns the request body sends the reply. Three kinds,
/// because the server waits on two channels and one of them carries two
/// different answers — the exec channel's context ask and its tool execs,
/// and the kv channel's blob exchanges.
#[derive(Debug, PartialEq)]
pub(super) enum Ask {
    Context(ContextAsk),
    Kv(KvAsk),
    /// A tool exec this build refuses to run for the server (**D486**).
    Refuse(ExecRefusal),
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

/// One tool exec the server asked this client to run, and this client will
/// not (**D486**): the id the refusal must echo, and the kind's name, which
/// travels into the refusal so the server's agent loop reads *what* was
/// refused rather than that something was.
///
/// The exec id is deliberately absent. The shipped client's refusal channel
/// carries the numeric id alone — no `exec_id`, no kind (`ExecClientThrow`,
/// `index.js@6032526`) — so echoing one here would be inventing a field the
/// answer has nowhere to put.
#[derive(Debug, PartialEq)]
pub(super) struct ExecRefusal {
    pub(super) id: Option<u32>,
    pub(super) kind: String,
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

            // Every other kind is a tool the server is asking this client
            // to run for it, and this client runs its tools for its own
            // session instead (**D486**). The server stops generating until
            // an exec is answered either way, so the kind's name goes back
            // as a refusal rather than as a turn-killing error: refused is
            // an outcome the server's agent loop can act on, and unanswered
            // is the silent hang this arm's modelling exists to end.
            return Some(Ask::Refuse(ExecRefusal { id: exec.id, kind: exec_kind(exec) }));
        }

        if let Some(kv) = message.kv_request.as_option() {
            if let Some(get) = kv.get_blob_args.as_option() {
                return Some(Ask::Kv(KvAsk {
                    id: kv.id,
                    op: KvOp::Get { blob_id: get.blob_id.clone().unwrap_or_default() },
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
            events.push(ProviderEvent::TextDelta(delta.text.clone().unwrap_or_default()));
        } else if let Some(delta) = update.thinking_delta.as_option() {
            // The plugin forwards thinking to its clients beside reply text,
            // marked as thinking (proxy.ts:1059-1061); here that mark is the
            // event the Anthropic wire's thinking blocks already arrive as.
            let text = delta.text.clone().unwrap_or_default();
            // Logged at the skip log's own level because the next boundary
            // question should read a log rather than patch one in: whether a
            // thinking message is a token, a sentence or a whole block is
            // what decides if a message edge could ever mean anything.
            tracing::debug!(provider = ID, bytes = text.len(), "thinking delta");
            events.push(ProviderEvent::ReasoningDelta(text));
        } else if update.thinking_completed.is_set() {
            // The plugin's own boundary between two thinking blocks,
            // live-observed (2026-08-25) between the thought groups of one
            // claude-fable-5-thinking turn: without it consecutive thoughts
            // splice into one block — the transcript's account of that
            // stream read "…to see if those work.Since tool calls…".
            // Announced unconditionally: the frame also closes a stream's
            // last block, where the loop finds nothing open and says
            // nothing.
            events.push(ProviderEvent::ReasoningBreak);
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

/// Names the kind a refused exec request carried.
///
/// The kinds this build does not model arrive as unknown fields, and the
/// field number *is* the kind: the table is the plugin's args oneof
/// (agent_pb.ts:6885-:6997, re-confirmed field for field against the
/// shipped client's own descriptor at `index.js@6039005`), so the refusal
/// names what the descriptor names. A number the table does not know is
/// reported as itself — still enough to go derive, and still refusable,
/// because the refusal channel is keyed on the exec id rather than on the
/// kind — and span_context (= 19, agent_pb.ts:6875) rides beside the oneof
/// without being a kind, so it is passed over rather than blamed.
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

    match fields.iter().map(|field| field.number).find(|number| *number != 19) {
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
    match kv.__buffa_unknown_fields.iter().map(|field| field.number).find(|number| *number != 4) {
        Some(number) => format!("field {number}"),
        None => "no recognizable kind".to_owned(),
    }
}

#[cfg(test)]
#[path = "decode_tests.rs"]
mod tests;
