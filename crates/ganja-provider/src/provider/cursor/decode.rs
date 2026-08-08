//! The cursor wire's responses, decoded whole.
//!
//! Spec: `.omc/research/cursor/spike-wire-facts.md`. Two shapes: the unary
//! model listing is bare protobuf with no framing at all (LIVE-OBSERVED —
//! the reference client's tolerance for a framed unary response is dead code
//! on the real server), and the Run stream is Connect frames whose verdict
//! rides the in-body EndStream frame. Both arrive here as a complete body:
//! the exchange is decoded once and handed back as the events it contained,
//! which is what lets a truncation or an in-body failure be judged with the
//! whole answer in hand.

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

/// One Run exchange's events, from its complete streaming body.
///
/// Updates this build does not model — the tool-call and thinking arms, and
/// whole server messages outside the update channel — are skipped, not
/// failed: the server adds arms between client versions, and a turn that
/// died on one would make every addition a breaking change. A skipped update
/// is logged at debug, which is where "why is the reply shorter than the
/// server's" is answered.
///
/// The EndStream verdict follows the [`Provider`](crate::provider::Provider)
/// contract's split: an error before anything streamed fails the turn's
/// opening, one after text became visible arrives as
/// [`ProviderEvent::Failed`] so the text is not thrown away. A body that
/// ends with no EndStream frame and no turn-ended update is a truncation,
/// judged the same way.
///
/// # Errors
///
/// Returns [`ProviderError::Parse`] when the framing or a frame's protobuf
/// cannot be read, and the EndStream error itself when nothing had streamed
/// yet.
pub(super) fn exchange(body: &[u8]) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut events: Vec<ProviderEvent> = Vec::new();
    let mut ended = false;

    for frame in connect::frames(body)? {
        if frame.is_end_stream() {
            match connect::end_stream_error(frame.payload)? {
                Some((code, message)) => {
                    let error = verdict(&code, &message);
                    if events.is_empty() {
                        return Err(error);
                    }
                    events.push(ProviderEvent::Failed(error));
                }
                None => events.push(ProviderEvent::Finish(FinishReason::Completed)),
            }
            ended = true;
            break;
        }

        let message = proto::ServerMessage::decode_from_slice(frame.payload).map_err(|error| {
            ProviderError::Parse(format!("a server message did not decode: {error}"))
        })?;
        let Some(update) = message.interaction_update.as_option() else {
            tracing::debug!(
                provider = ID,
                "skipped a server message outside the update channel"
            );
            continue;
        };

        if let Some(delta) = update.text_delta.as_option() {
            events.push(ProviderEvent::TextDelta(
                delta.text.clone().unwrap_or_default(),
            ));
        } else if update.turn_ended.is_set() {
            events.push(ProviderEvent::Finish(FinishReason::Completed));
            ended = true;
            // Anything after the turn's end would be dropped by the engine
            // anyway; stopping here keeps "what the exchange meant" one
            // place's decision.
            break;
        } else if update.heartbeat.is_set() {
            // Liveness, carrying nothing.
        } else {
            tracing::debug!(provider = ID, "skipped an update this build does not model");
        }
    }

    if !ended {
        let truncated = ProviderError::Transport(
            "the response body ended before the exchange finished".to_owned(),
        );
        if events.is_empty() {
            return Err(truncated);
        }
        events.push(ProviderEvent::Failed(truncated));
    }

    Ok(events)
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

#[cfg(test)]
mod tests {
    use buffa::Message as _;

    use super::{
        super::{connect, proto},
        FinishReason, ProviderError, ProviderEvent, exchange, model_list, verdict,
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

    #[test]
    fn a_streamed_reply_becomes_its_deltas_and_a_finish() {
        let mut body = framed(heartbeat());
        body.extend(framed(text("Hello")));
        body.extend(framed(text(" world")));
        body.extend(framed(turn_ended()));
        body.extend(end_stream("{}"));

        let events = exchange(&body).expect("a clean exchange decodes");
        assert_eq!(
            events,
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

        let events = exchange(&body).expect("decodes");
        assert_eq!(
            events.last(),
            Some(&ProviderEvent::Finish(FinishReason::Completed))
        );
    }

    /// The exact exchange the live probe recorded: one heartbeat, then the
    /// EndStream refusal. Nothing had streamed, so the turn fails at its
    /// opening rather than mid-reply.
    #[test]
    fn the_recorded_refusal_fails_the_turn_before_anything_streamed() {
        let mut body = framed(heartbeat());
        body.extend(end_stream(
            "{\"error\":{\"code\":\"invalid_argument\",\"message\":\
             \"First message must be a run request or prewarm request\"}}",
        ));

        let refused = exchange(&body).expect_err("the recorded stream refuses");
        assert!(
            matches!(&refused, ProviderError::Status { status: 400, message }
                if message.contains("invalid_argument")),
            "{refused:?}"
        );
    }

    #[test]
    fn an_error_after_visible_text_keeps_the_text() {
        let mut body = framed(text("partial"));
        body.extend(end_stream(
            r#"{"error":{"code":"internal","message":"boom"}}"#,
        ));

        let events = exchange(&body).expect("the text survives");
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
        let refused =
            exchange(&framed(heartbeat())).expect_err("nothing streamed and nothing ended");
        assert!(
            matches!(refused, ProviderError::Transport(_)),
            "{refused:?}"
        );

        let events = exchange(&framed(text("half"))).expect("the text survives");
        assert!(
            matches!(
                &events[1],
                ProviderEvent::Failed(ProviderError::Transport(_))
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

        let events = exchange(&body).expect("the unmodelled arm is stepped over");
        assert_eq!(events[0], ProviderEvent::TextDelta("still here".to_owned()));
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
