//! `ChatRequest` → the Run stream's opening message.
//!
//! Spec: `.omc/research/cursor/spike-wire-facts.md` — the server refuses a
//! stream whose first message is not a run request, so this module builds
//! exactly that one message. What it carries is the minimal set the message
//! definitions in `cursor.proto` model: the model asked for (named twice,
//! because the server still reads the deprecated description beside the
//! forward-looking one), an empty conversation state marked present, and
//! the newest user message inline.
//!
//! **The newest user message, deliberately.** Everything a conversation
//! already holds — earlier turns, tool calls and their results — travels on
//! cursor's wire as content-addressed state over the stream's kv half.
//! [`kv_answer`] speaks that channel's serving side — mid-turn the server
//! stores blobs with this client and reads its own back, and it will not
//! end the turn while one is unanswered — but composing *history* into
//! blobs the request could name is still ahead, so the request carries what
//! it can carry truthfully and the rest arrives with the state machinery.
//! The advertised tools are unsent for the same reason: cursor's native
//! tool protocol is a channel of its own.
//!
//! **The system prompt rides the answer, not the request.** The
//! descriptor's one inline member for it, `custom_system_prompt = 8`, is an
//! allowlist-gated override ("Allowlisted for specific teams only", the
//! reference plugin's `src/proto/agent_pb.ts:2782`) that the plugin never
//! sets — and sending it LIVE-FAILED an ordinary seat's turn with 400
//! invalid_argument: "unknown option '--system-prompt'". Where the plugin's
//! system text really travels is `RequestContext.cloudRule`, its answer to
//! the server's mid-stream `requestContextArgs` exec (`src/proxy.ts:1132`;
//! its comment records that plain system messages are ignored server-side).
//! [`context_answer`] is that reply, spoken on the same open request body
//! the run request went out on — so `ChatRequest.system` reaches the model
//! on the one channel the server honors, and never through the member it
//! demonstrably refuses.
//!
//! # Tool execs are refused, not run (**D486**, `cursor-exec-refusal`)
//!
//! Cursor's server does not only *ask for* context mid-turn; it asks the
//! client to **run tools** for it — a shell command, a file read, an MCP
//! call — as exec requests on the same channel, and it holds generation
//! until each one is answered. The live-observed instance is
//! `shell_stream_args` (the args oneof's field 14), which arrived on an
//! ordinary turn and, until [`refusal_answer`], ended it: every exec kind
//! but the context ask became a `ProviderEvent::Failed` naming the kind,
//! because leaving it unanswered would have hung the turn instead.
//!
//! **What diverges.** There is no upstream counterpart to weigh this
//! against — upstream opencode v1.18.22 has no cursor wire at all, so no
//! ported behavior is being contradicted. The divergence is from *cursor's
//! own shipped client*, which executes these asks: it registers handlers
//! for shell, read, write, grep, MCP and the rest, runs them against the
//! user's machine, and streams the results back. Ganja deliberately does
//! not. Its tools run for *its* session, under [`crate`]'s permission
//! engine, on the engine's agent loop — running a second, invisible tool
//! loop on the provider's say-so would put a shell command outside every
//! dialog, rule and transcript this build has, driven by a party the user
//! is talking to rather than one they are running.
//!
//! **Why a refusal rather than a failure.** The same client shows what to
//! send when it *won't* run an exec. Its dispatcher, on finding no handler
//! for a server exec, writes two control messages and nothing else: a
//! `throw` carrying the exec id and a reason string, then a `stream_close`
//! carrying the id (`index.js@4272747` in the bundled
//! `2026.07.23-e383d2b` agent — byte offsets, per `cursor.proto`'s
//! citation note). That channel is keyed on the numeric id alone, naming
//! neither kind nor `exec_id`, which is precisely what makes it a *general*
//! refusal: `shell_stream_args`, a kind no table here knows, and an exec
//! carrying nothing recognizable at all are all refusable through it, so no
//! exec kind is left to fail a turn. The reason string names ganja and the
//! kind, because it is read by the server's own agent loop — a refusal is
//! information that loop can act on, the way a denied tool call is
//! information ganja's own loop acts on, and the turn survives it.

use std::collections::HashMap;
use std::fmt::Write as _;

use buffa::Message as _;

use super::{ID, decode, proto};
use crate::auth::pkce;
use crate::protocol::{PartBody, Role};
use crate::provider::{ChatRequest, ProviderError};

/// A fresh RFC 9562 v4 id in the spelling `crypto.randomUUID()` mints, which
/// is the shape the recorded client stamps on messages and requests alike.
///
/// # Errors
///
/// Returns [`ProviderError::Transport`] when the platform's random source
/// fails: nothing was sent, and nothing was refused.
pub(super) fn fresh_id() -> Result<String, ProviderError> {
    let mut bytes =
        pkce::random_bytes::<16>().map_err(|error| ProviderError::Transport(error.to_string()))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let mut rendered = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            rendered.push('-');
        }
        write!(rendered, "{byte:02x}").expect("writing hex into a String cannot fail");
    }

    Ok(rendered)
}

/// The bytes of the stream's opening message, assembled from `request`.
///
/// # Errors
///
/// Returns [`ProviderError::Transport`] when no message id can be minted;
/// see [`fresh_id`].
pub(super) fn run_message(request: &ChatRequest) -> Result<Vec<u8>, ProviderError> {
    let model = proto::ModelEntry::default()
        .with_model_id(&request.model)
        .with_display_model_id(&request.model)
        .with_display_name(&request.model)
        .with_display_name_short(&request.model);

    let action = proto::ConversationAction {
        user_message_action: buffa::MessageField::some(proto::UserMessageAction {
            user_message: buffa::MessageField::some(
                proto::UserMessage::default()
                    .with_text(newest_user_text(request))
                    .with_message_id(fresh_id()?),
            ),
            ..Default::default()
        }),
        ..Default::default()
    };

    let run = proto::RunRequest {
        conversation_state: buffa::MessageField::some(proto::ConversationState::default()),
        action: buffa::MessageField::some(action),
        model_details: buffa::MessageField::some(model),
        requested_model: buffa::MessageField::some(
            proto::RequestedModel::default().with_model_id(&request.model),
        ),
        ..Default::default()
    };

    Ok(proto::ClientMessage { run_request: buffa::MessageField::some(run), ..Default::default() }
        .encode_to_vec())
}

/// The bytes answering the server's context ask: the ids echoed the way the
/// plugin echoes them (`src/proxy.ts:1307-1310`), and the system prompt on
/// `RequestContext.cloud_rule`, the channel cursor's agent honors
/// (`src/proxy.ts:1133`).
///
/// An absent or empty prompt mirrors the plugin's no-prompt answer — its
/// `cloudRule` is `undefined` then, so the member is absent while the
/// context message itself is still present and still a success.
pub(super) fn context_answer(ask: decode::ContextAsk, system: Option<&str>) -> Vec<u8> {
    let context = proto::RequestContext {
        cloud_rule: system.map(str::to_owned).filter(|text| !text.is_empty()),
        ..Default::default()
    };
    let answer = proto::ExecResponse {
        id: ask.id,
        request_context_result: buffa::MessageField::some(proto::ContextResult {
            success: buffa::MessageField::some(proto::ContextSuccess {
                request_context: buffa::MessageField::some(context),
                ..Default::default()
            }),
            ..Default::default()
        }),
        exec_id: ask.exec_id,
        ..Default::default()
    };

    proto::ClientMessage { exec_response: buffa::MessageField::some(answer), ..Default::default() }
        .encode_to_vec()
}

/// The two messages refusing one tool exec (**D486**): the throw carrying
/// the reason, then the stream close that ends the exchange — the pair the
/// shipped client writes when no handler of its own claims a server exec
/// (`index.js@4272747`), in that order, because the close is what tells the
/// server the exec is over rather than still running.
///
/// Both echo the id the server minted and neither names the kind: the
/// channel has no member for one, so the kind travels inside the reason
/// string, which is where the server's agent loop reads it.
pub(super) fn refusal_answer(ask: &decode::ExecRefusal) -> Vec<Vec<u8>> {
    tracing::debug!(
        provider = ID,
        exec = ask.id,
        kind = ask.kind,
        "refusing an exec cursor asked this client to run"
    );

    let thrown = proto::ExecControl {
        throw: buffa::MessageField::some(proto::ExecThrow {
            id: ask.id,
            error: Some(refusal_reason(&ask.kind)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let closed = proto::ExecControl {
        stream_close: buffa::MessageField::some(proto::ExecStreamClose {
            id: ask.id,
            ..Default::default()
        }),
        ..Default::default()
    };

    [thrown, closed]
        .into_iter()
        .map(|control| {
            proto::ClientMessage {
                exec_control: buffa::MessageField::some(control),
                ..Default::default()
            }
            .encode_to_vec()
        })
        .collect()
}

/// What the server's agent loop is told about a refused exec.
///
/// It names ganja, so the sentence reads as a client's policy rather than a
/// malfunction, and it names the kind, so the loop can tell a refused shell
/// from a refused file read and choose differently. The shipped client's own
/// no-handler reason (`No handler found for server message of type <kind>`,
/// `index.js@4272747`) is the shape being matched — a plain sentence, the
/// kind in it, nothing machine-readable, because the channel offers no
/// structured field for either.
fn refusal_reason(kind: &str) -> String {
    format!(
        "ganja runs its tools itself and does not execute them for the server \
         (no handler for {kind})"
    )
}

/// The bytes answering one kv exchange, serviced against `blobs` — the
/// turn's in-memory blob store — the way the plugin's `handleKvMessage`
/// services its own (proxy.ts:1087-1120): a set stores the bytes and acks
/// with the empty result (proxy.ts:1113-1117), a get returns what was
/// stored or the not-found shape — a present result holding no data
/// (proxy.ts:1101-1105) — and every answer echoes the id the server minted
/// (proxy.ts:1075-1077).
///
/// The blob bytes are conversation state and never reach a log line: what
/// is logged is the id's leading hex and the sizes, the plugin's own debug
/// discipline.
pub(super) fn kv_answer(ask: decode::KvAsk, blobs: &mut HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let answer = match ask.op {
        decode::KvOp::Get { blob_id } => {
            let found = blobs.get(&blob_id).cloned();
            tracing::debug!(
                provider = ID,
                blob = blob_key(&blob_id),
                found = found.as_deref().map(<[u8]>::len),
                "answering the server's kv get"
            );
            let result = match found {
                Some(data) => proto::GetBlobResult::default().with_blob_data(data),
                None => proto::GetBlobResult::default(),
            };

            proto::KvResponse {
                id: ask.id,
                get_blob_result: buffa::MessageField::some(result),
                ..Default::default()
            }
        }
        decode::KvOp::Set { blob_id, data } => {
            tracing::debug!(
                provider = ID,
                blob = blob_key(&blob_id),
                size = data.len(),
                "answering the server's kv set"
            );
            blobs.insert(blob_id, data);

            proto::KvResponse {
                id: ask.id,
                set_blob_result: buffa::MessageField::some(proto::SetBlobResult::default()),
                ..Default::default()
            }
        }
    };

    proto::ClientMessage { kv_response: buffa::MessageField::some(answer), ..Default::default() }
        .encode_to_vec()
}

/// A blob id's leading eight bytes as hex — sixteen characters, the width
/// the plugin's own kv debug lines truncate to. Enough to correlate a get
/// with the set that stored it, and never the data.
fn blob_key(id: &[u8]) -> String {
    id.iter().take(8).fold(String::with_capacity(16), |mut rendered, byte| {
        let _ = write!(rendered, "{byte:02x}");
        rendered
    })
}

/// The text of the conversation's newest user **turn**: every user message
/// from the last one back to the reply before it — but never back past
/// [`ChatRequest::turn_start`] — their text parts in order, joined the way
/// distinct parts read as distinct paragraphs.
///
/// A run rather than one message, because the engine adds to a turn by
/// appending user messages rather than by editing the last one — a steer
/// drained at a step boundary, and the team guards' request-only block after
/// a reply (D547) — and a wire that sent only the newest of them would answer
/// a guard block while dropping the steer beside it, which is what this did
/// until 2026-09-02. What came before the run is history this wire does not
/// carry yet.
///
/// **The run's lower bound is two facts, not one, and the second cannot be
/// read off `messages`.** The reply is the near bound; the turn's own opening
/// is the far one. A finished turn that took a steer leaves the steer in
/// history *after* its reply, so the next turn's request reads `[prompt,
/// reply, steer, prompt2]` — the same four roles, in the same order, as the
/// within-turn `[prompt, reply, steer, block]`, every one of them a
/// `Message::user` whose id and timestamp ascend across the boundary exactly
/// as they do within it. Nothing here distinguishes them, which is why the
/// engine states where this turn began and this walk is clamped to it rather
/// than guessing.
///
/// **One shape the clamp does not close**, named rather than left to be
/// discovered: a continuation block emitted on the arm where nothing was
/// steered makes the request `[prompt, reply, block]`, and the block still
/// reaches this wire without the prompt it is about. The clamp raises the
/// run's lower bound and never lowers it — lowering it here would mean
/// reaching back past the assistant's reply, whose text this wire does not
/// send — so closing that one means carrying more than the newest user turn,
/// which is the history-over-blobs work this module's own header defers.
///
/// Empty when the conversation holds no user message at all, which is not a
/// request the engine builds — sending the empty message is more honest than
/// refusing a request this module was still asked to encode.
fn newest_user_text(request: &ChatRequest) -> String {
    let messages = &request.messages;
    let Some(newest) = messages.iter().rposition(|message| matches!(message.role, Role::User))
    else {
        return String::new();
    };
    let first = messages[..newest]
        .iter()
        .rposition(|message| !matches!(message.role, Role::User))
        .map_or(0, |reply| reply + 1)
        // Never past this turn's own opening: a steer the *previous* turn
        // consumed sits after that turn's reply, so the walk above would
        // reach back through it and re-send it as part of this prompt.
        .max(request.turn_start)
        // And never past the newest user message itself. `turn_start` is a
        // `pub` field on a `pub` struct, so its value is a caller's and not
        // this module's: a request whose last message is an assistant's, with
        // a marker pointing past the user message before it, would otherwise
        // slice `first > newest` and panic the wire. A run of one is the
        // honest answer to that — the newest user turn is still the newest
        // user message — where a panic is no answer at all.
        .min(newest);

    messages[first..=newest]
        .iter()
        .flat_map(|message| message.parts.iter())
        // Every variant is named, and the wildcard that used to stand here is
        // gone on purpose: this was the one place in the workspace where a
        // new `PartBody` would compile silently into "not text", and a part
        // this wire ought to send is not something to discover from a user's
        // bug report.
        .filter_map(|part| match &part.body {
            PartBody::Text { text } => Some(text.as_str()),
            // A peer's words are rendered into the user turn at request
            // assembly (D495); a wire never encodes a peer part as a message
            // of its own.
            PartBody::Peer { .. }
            | PartBody::File { .. }
            | PartBody::Tool { .. }
            | PartBody::ServerTool { .. }
            | PartBody::Reasoning { .. }
            | PartBody::ReasoningText { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
