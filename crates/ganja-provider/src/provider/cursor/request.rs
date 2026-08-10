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

use std::{collections::HashMap, fmt::Write as _};

use buffa::Message as _;

use super::{ID, decode, proto};
use crate::{
    auth::pkce,
    protocol::{Message, PartBody, Role},
    provider::{ChatRequest, ProviderError},
};

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
                    .with_text(newest_user_text(&request.messages))
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

    Ok(proto::ClientMessage {
        run_request: buffa::MessageField::some(run),
        ..Default::default()
    }
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

    proto::ClientMessage {
        exec_response: buffa::MessageField::some(answer),
        ..Default::default()
    }
    .encode_to_vec()
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

    proto::ClientMessage {
        kv_response: buffa::MessageField::some(answer),
        ..Default::default()
    }
    .encode_to_vec()
}

/// A blob id's leading eight bytes as hex — sixteen characters, the width
/// the plugin's own kv debug lines truncate to. Enough to correlate a get
/// with the set that stored it, and never the data.
fn blob_key(id: &[u8]) -> String {
    id.iter()
        .take(8)
        .fold(String::with_capacity(16), |mut rendered, byte| {
            let _ = write!(rendered, "{byte:02x}");
            rendered
        })
}

/// The text of the conversation's newest user message: its text parts in
/// order, joined the way distinct parts read as distinct paragraphs.
///
/// Empty when the conversation holds no user message at all, which is not a
/// request the engine builds — sending the empty message is more honest than
/// refusing a request this module was still asked to encode.
fn newest_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| matches!(message.role, Role::User))
        .map(|message| {
            message
                .parts
                .iter()
                .filter_map(|part| match &part.body {
                    PartBody::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use buffa::Message as _;

    use super::{
        super::proto, ChatRequest, Message, context_answer, decode, fresh_id, kv_answer,
        newest_user_text, run_message,
    };
    use crate::protocol::Part;

    /// A two-message conversation whose newest user message has two text
    /// parts, the richest shape this assembly reads.
    fn request() -> ChatRequest {
        let mut asked = Message::user("What does this crate do?");
        asked.parts.push(Part::text("Answer briefly."));

        ChatRequest {
            model: "gpt-5.3-codex".to_owned(),
            system: Some("You are terse.".to_owned()),
            messages: vec![Message::user("An older question."), asked],
            tools: Vec::new(),
        }
    }

    #[test]
    fn the_assembled_bytes_decode_back_to_what_the_assembly_promised() {
        let bytes = run_message(&request()).expect("the assembly encodes");
        let decoded =
            proto::ClientMessage::decode_from_slice(&bytes).expect("what was sent decodes");

        let run = decoded
            .run_request
            .as_option()
            .expect("a run request first");
        assert!(
            run.conversation_state.is_set(),
            "the state is present even when it holds nothing"
        );

        let model = run
            .model_details
            .as_option()
            .expect("the model description");
        assert_eq!(model.model_id.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(model.display_name.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(
            run.requested_model
                .as_option()
                .and_then(|requested| requested.model_id.as_deref()),
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
            decode::ContextAsk {
                id: Some(7),
                exec_id: Some("exec-abc".to_owned()),
            },
            Some("You are terse."),
        );
        let decoded =
            proto::ClientMessage::decode_from_slice(&bytes).expect("what was sent decodes");

        assert!(
            decoded.run_request.as_option().is_none(),
            "an answer is not a second run request"
        );
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
            let bytes = context_answer(
                decode::ContextAsk {
                    id: None,
                    exec_id: None,
                },
                system,
            );
            let decoded = proto::ClientMessage::decode_from_slice(&bytes).expect("decodes");
            let answer = decoded.exec_response.as_option().expect("the exec answer");
            assert_eq!(
                answer.id, None,
                "an id the server never sent is not invented"
            );

            let context = answer
                .request_context_result
                .as_option()
                .and_then(|result| result.success.as_option())
                .and_then(|success| success.request_context.as_option())
                .expect("the context is present even without a prompt");
            assert_eq!(
                context.cloud_rule, None,
                "an absent prompt is absent, not empty"
            );
        }
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
                op: decode::KvOp::Set {
                    blob_id: b"blob-a".to_vec(),
                    data: b"opaque-state".to_vec(),
                },
            },
            &mut blobs,
        );
        let decoded = proto::ClientMessage::decode_from_slice(&stored).expect("decodes");
        assert!(
            decoded.run_request.as_option().is_none()
                && decoded.exec_response.as_option().is_none(),
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
            decode::KvAsk {
                id: Some(12),
                op: decode::KvOp::Get {
                    blob_id: b"blob-a".to_vec(),
                },
            },
            &mut blobs,
        );
        let decoded = proto::ClientMessage::decode_from_slice(&read).expect("decodes");
        let answer = decoded.kv_response.as_option().expect("the kv answer");
        assert_eq!(answer.id, Some(12));
        assert_eq!(
            answer
                .get_blob_result
                .as_option()
                .and_then(|result| result.blob_data.as_deref()),
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
            decode::KvAsk {
                id: None,
                op: decode::KvOp::Get {
                    blob_id: b"blob-b".to_vec(),
                },
            },
            &mut blobs,
        );
        let decoded = proto::ClientMessage::decode_from_slice(&read).expect("decodes");
        let answer = decoded.kv_response.as_option().expect("the kv answer");
        assert_eq!(
            answer.id, None,
            "an id the server never sent is not invented"
        );
        let result = answer
            .get_blob_result
            .as_option()
            .expect("the result is present even without the blob");
        assert_eq!(
            result.blob_data, None,
            "not-found is absence, not empty bytes"
        );
        assert!(blobs.is_empty(), "a get stores nothing");
    }

    #[test]
    fn the_newest_user_message_wins_and_other_roles_are_passed_over() {
        let conversation = [
            Message::user("first"),
            Message::assistant("gpt-5.3-codex"),
            Message::user("second"),
        ];
        assert_eq!(newest_user_text(&conversation), "second");
        assert_eq!(newest_user_text(&[]), "");
    }

    #[test]
    fn a_minted_id_is_a_v4_uuid_and_two_are_two() {
        let id = fresh_id().expect("entropy is available");
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4', "the version nibble: {id}");
        assert!(
            matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "the variant bits: {id}"
        );
        assert_ne!(id, fresh_id().expect("entropy is available"));
    }
}
