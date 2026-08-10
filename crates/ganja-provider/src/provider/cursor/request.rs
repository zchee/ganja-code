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
//! cursor's wire as content-addressed state served over the stream's
//! blob-store half, which this build does not speak yet. Encoding history
//! into this message set would invent a channel the server does not read,
//! so the request carries what it can carry truthfully and the rest arrives
//! with the state machinery. The advertised tools are unsent for the same
//! reason: cursor's native tool protocol is a channel of its own.
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

use std::fmt::Write as _;

use buffa::Message as _;

use super::{decode, proto};
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
    use buffa::Message as _;

    use super::{
        super::proto, ChatRequest, Message, context_answer, decode, fresh_id, newest_user_text,
        run_message,
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
