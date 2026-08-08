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
/// Updates this build does not model — the tool-call and thinking arms, and
/// whole server messages outside the update channel — are skipped, not
/// failed: the server adds arms between client versions, and a turn that
/// died on one would make every addition a breaking change. A skipped update
/// is logged at debug, which is where "why is the reply shorter than the
/// server's" is answered.
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

impl Mapping {
    /// Maps `frame`, appending whatever it means to `events`.
    pub(super) fn frame(&mut self, frame: &connect::Frame, events: &mut Vec<ProviderEvent>) {
        if frame.is_end_stream() {
            match connect::end_stream_error(&frame.payload) {
                Ok(Some((code, message))) => {
                    events.push(ProviderEvent::Failed(verdict(&code, &message)));
                }
                Ok(None) => events.push(ProviderEvent::Finish(FinishReason::Completed)),
                Err(error) => events.push(ProviderEvent::Failed(error)),
            }
            return;
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
                return;
            }
        };
        let Some(update) = message.interaction_update.as_option() else {
            tracing::debug!(
                provider = ID,
                "skipped a server message outside the update channel"
            );
            return;
        };

        if let Some(delta) = update.text_delta.as_option() {
            events.push(ProviderEvent::TextDelta(
                delta.text.clone().unwrap_or_default(),
            ));
        } else if update.turn_ended.is_set() {
            self.ended = true;
        } else if update.heartbeat.is_set() {
            // Liveness, carrying nothing.
        } else {
            tracing::debug!(provider = ID, "skipped an update this build does not model");
        }
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

#[cfg(test)]
mod tests {
    use buffa::Message as _;

    use super::{
        super::{connect, proto},
        FinishReason, Mapping, ProviderError, ProviderEvent, model_list, verdict,
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

    /// Runs `body` through the real splitter and one [`Mapping`], the way
    /// the live fold does; `eof` says whether the body then ended.
    fn mapped(body: &[u8], eof: bool) -> Vec<ProviderEvent> {
        let mut splitter = connect::Splitter::default();
        splitter.push(body);

        let mut mapping = Mapping::default();
        let mut events = Vec::new();
        while let Some(frame) = splitter.frame().expect("the fixture bodies parse") {
            mapping.frame(&frame, &mut events);
        }
        if eof {
            mapping.truncated(&mut events);
        }

        events
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
