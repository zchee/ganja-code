//! Connect framing for the cursor wire's streaming RPC.
//!
//! Spec: `.omc/research/cursor/spike-wire-facts.md` (LIVE-OBSERVED against
//! `api2.cursor.sh`), corroborated by the public Connect protocol
//! (connectrpc.com/docs/protocol). Two shapes and nothing more:
//!
//! - a **data frame** is a 5-byte prefix — one flag byte, then a big-endian
//!   `u32` length — followed by exactly that many payload bytes;
//! - the **EndStream frame** is the same prefix with flag bit `0b0000_0010`
//!   set, carrying a JSON object in place of protobuf. An `"error"` member is
//!   the RPC's failure; its absence is a clean end. The status lives *here*,
//!   in the body — the live probe saw no HTTP/2 trailers on any stream.
//!
//! The unary RPC needs none of this: the live probe pinned bare protobuf in
//! both directions there, so this module is only ever handed a streaming
//! body.

use serde_json::Value;

use crate::provider::ProviderError;

/// The flag bit that marks the EndStream frame, as the live probe recorded
/// it.
const END_STREAM_FLAG: u8 = 0b0000_0010;

/// Bytes of prefix before every frame's payload: one flag byte and a
/// big-endian `u32` length.
const PREFIX: usize = 5;

/// One frame, borrowed out of a complete response body.
#[derive(Debug)]
pub(super) struct Frame<'a> {
    flags: u8,
    /// The payload: protobuf on a data frame, JSON on the EndStream frame.
    pub(super) payload: &'a [u8],
}

impl Frame<'_> {
    /// Whether this frame is the stream's in-body status.
    pub(super) fn is_end_stream(&self) -> bool {
        self.flags & END_STREAM_FLAG != 0
    }
}

/// Wraps one encoded message in the 5-byte envelope of an ordinary data
/// frame, which is the whole of what a request this build sends needs.
pub(super) fn envelope(message: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(PREFIX + message.len());
    framed.push(0);
    // The message is an in-memory encoding whose size buffa already bounded
    // below the protobuf 2 GiB ceiling, so the cast cannot truncate.
    framed.extend_from_slice(
        &u32::try_from(message.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    framed.extend_from_slice(message);

    framed
}

/// Splits a complete streaming body into its frames.
///
/// The whole body is required to be frames: a prefix that runs off the end,
/// a payload shorter than its declared length, or bytes after the EndStream
/// frame are each a body this build does not understand, reported rather
/// than half-read — a partial parse here would present a truncated answer as
/// a short one.
///
/// # Errors
///
/// Returns [`ProviderError::Parse`] as above.
pub(super) fn frames(body: &[u8]) -> Result<Vec<Frame<'_>>, ProviderError> {
    let mut split = Vec::new();
    let mut rest = body;

    while !rest.is_empty() {
        if split.last().is_some_and(Frame::is_end_stream) {
            return Err(ProviderError::Parse(
                "the response carried bytes after its EndStream frame".to_owned(),
            ));
        }

        let Some((prefix, tail)) = rest.split_at_checked(PREFIX) else {
            return Err(ProviderError::Parse(format!(
                "the response ended inside a frame prefix ({} of {PREFIX} bytes)",
                rest.len()
            )));
        };
        // The prefix split is exactly five bytes, so the length bytes are
        // there by construction.
        let declared = u32::from_be_bytes([prefix[1], prefix[2], prefix[3], prefix[4]]) as usize;
        let Some((payload, tail)) = tail.split_at_checked(declared) else {
            return Err(ProviderError::Parse(format!(
                "the response ended inside a frame payload ({} of {declared} bytes)",
                tail.len()
            )));
        };

        split.push(Frame {
            flags: prefix[0],
            payload,
        });
        rest = tail;
    }

    Ok(split)
}

/// Reads the EndStream frame's payload: the error it carried, or [`None`]
/// for a clean end.
///
/// The JSON's `error.code` and `error.message` are the Connect vocabulary
/// the live probe recorded
/// (`{"error":{"code":"invalid_argument","message":…}}`); a member this
/// build does not model is ignored rather than refused, because the frame's
/// job is the verdict and the verdict is in these two.
///
/// # Errors
///
/// Returns [`ProviderError::Parse`] when the payload is not a JSON object —
/// a stream whose ending cannot be read is a stream whose outcome is
/// unknown, and guessing "clean" is the one wrong answer.
pub(super) fn end_stream_error(payload: &[u8]) -> Result<Option<(String, String)>, ProviderError> {
    let parsed: Value = serde_json::from_slice(payload).map_err(|error| {
        ProviderError::Parse(format!("the EndStream frame is not JSON: {error}"))
    })?;
    if !parsed.is_object() {
        return Err(ProviderError::Parse(
            "the EndStream frame is not a JSON object".to_owned(),
        ));
    }

    Ok(parsed.get("error").map(|error| {
        let field = |name: &str| {
            error
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };

        (field("code"), field("message"))
    }))
}

/// The HTTP status the Connect protocol assigns `code`.
///
/// The failure arrives in the body of a 200, so there is no wire status to
/// report; what there is instead is the protocol's own published mapping
/// (connectrpc.com/docs/protocol, "Error Codes"), which is what the same
/// refusal would have carried on the unary form of the protocol. Using it
/// keeps [`ProviderError::Status`] honest — the provider *did* answer — and
/// classifies retryability the way the vendor means it: `unavailable` lands
/// on 503 and `resource_exhausted` on 429, both worth another try, while
/// `invalid_argument`'s 400 is not.
pub(super) fn http_status(code: &str) -> u16 {
    match code {
        "canceled" | "deadline_exceeded" => 408,
        "invalid_argument" | "out_of_range" => 400,
        "not_found" => 404,
        "already_exists" | "aborted" => 409,
        "permission_denied" => 403,
        "resource_exhausted" => 429,
        "failed_precondition" => 412,
        "unimplemented" => 501,
        "unavailable" => 503,
        "unauthenticated" => 401,
        // `unknown`, `internal`, `data_loss`, and anything the protocol adds
        // later: the server failed, whatever it calls that.
        _ => 500,
    }
}

#[cfg(test)]
mod tests {
    use super::{END_STREAM_FLAG, ProviderError, end_stream_error, envelope, frames, http_status};

    /// The exact EndStream JSON the live probe recorded, 104 bytes, which is
    /// the `len 0x68` its frame prefix declared.
    const RECORDED_END_STREAM: &str = "{\"error\":{\"code\":\"invalid_argument\",\
        \"message\":\"First message must be a run request or prewarm request\"}}";

    /// The two-frame body the live probe recorded on the Run RPC: a data
    /// frame holding a heartbeat update, then the EndStream frame above.
    fn recorded_body() -> Vec<u8> {
        assert_eq!(RECORDED_END_STREAM.len(), 0x68, "the recorded length");

        let mut body = vec![0x00, 0x00, 0x00, 0x00, 0x04, 0x0a, 0x02, 0x6a, 0x00];
        body.extend_from_slice(&[END_STREAM_FLAG, 0x00, 0x00, 0x00, 0x68]);
        body.extend_from_slice(RECORDED_END_STREAM.as_bytes());

        body
    }

    #[test]
    fn the_recorded_run_body_splits_into_its_two_frames() {
        let body = recorded_body();
        let split = frames(&body).expect("the live recording parses");

        assert_eq!(split.len(), 2);
        assert!(!split[0].is_end_stream());
        assert_eq!(split[0].payload, [0x0a, 0x02, 0x6a, 0x00]);
        assert!(split[1].is_end_stream());

        let (code, message) = end_stream_error(split[1].payload)
            .expect("the recorded JSON parses")
            .expect("the recorded frame carried an error");
        assert_eq!(code, "invalid_argument");
        assert!(message.contains("must be a run request"), "{message}");
    }

    #[test]
    fn the_envelope_this_build_sends_is_the_one_the_probe_sent() {
        // The probe's opening message: flag 0, length 0 — five zero bytes.
        assert_eq!(envelope(&[]), [0x00; 5]);

        let framed = envelope(&[0x0a, 0x01, 0x78]);
        assert_eq!(framed, [0x00, 0x00, 0x00, 0x00, 0x03, 0x0a, 0x01, 0x78]);
    }

    #[test]
    fn a_body_that_ends_mid_frame_is_refused_not_half_read() {
        let mut body = recorded_body();
        body.truncate(body.len() - 10);
        assert!(matches!(frames(&body), Err(ProviderError::Parse(_))));

        // Three bytes cannot even hold a prefix.
        assert!(matches!(
            frames(&[0x00, 0x00, 0x00]),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn bytes_after_the_end_stream_frame_are_refused() {
        let mut body = recorded_body();
        body.push(0x00);
        let refused = frames(&body).expect_err("nothing follows the stream's ending");

        assert!(refused.to_string().contains("EndStream"), "{refused}");
    }

    #[test]
    fn an_end_stream_without_an_error_member_is_a_clean_end() {
        assert_eq!(
            end_stream_error(b"{}").expect("an empty object parses"),
            None
        );
        // Members this build does not model are not a reason to fail a turn
        // that succeeded.
        assert_eq!(
            end_stream_error(br#"{"metadata":{"x":"y"}}"#).expect("parses"),
            None
        );
        assert!(matches!(
            end_stream_error(b"not json"),
            Err(ProviderError::Parse(_))
        ));
    }

    #[test]
    fn the_status_mapping_is_the_connect_protocols_own() {
        assert_eq!(http_status("invalid_argument"), 400);
        assert_eq!(http_status("unauthenticated"), 401);
        assert_eq!(http_status("resource_exhausted"), 429);
        assert_eq!(http_status("unavailable"), 503);
        assert_eq!(http_status("internal"), 500);
        assert_eq!(http_status("a-code-from-the-future"), 500);
    }
}
