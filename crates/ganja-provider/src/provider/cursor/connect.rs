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
//! The body arrives in whatever pieces the transport produced — a frame may
//! span chunks and one chunk may hold several frames — so [`Splitter`]
//! buffers until a frame is whole and hands frames back as they complete,
//! which is what lets the wire surface events while the server is still
//! talking instead of after it has finished.
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

/// One frame, cut whole out of the arriving body.
///
/// Owned rather than borrowed: a frame may span transport chunks, so there
/// is no single buffer it could borrow from for as long as the mapping needs
/// it.
#[derive(Debug)]
pub(super) struct Frame {
    flags: u8,
    /// The payload: protobuf on a data frame, JSON on the EndStream frame.
    pub(super) payload: Vec<u8>,
}

impl Frame {
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
    framed.extend_from_slice(&u32::try_from(message.len()).unwrap_or(u32::MAX).to_be_bytes());
    framed.extend_from_slice(message);

    framed
}

/// Cuts Connect frames out of a streaming body as it arrives.
///
/// Fed chunks in transport order, it hands back each frame the moment its
/// last byte is in. The EndStream frame must be the body's last: bytes after
/// it are a body this build does not understand, refused rather than
/// half-read. What the splitter deliberately does **not** judge is the
/// ending — whether running out of body mid-frame is a truncation or only a
/// lost terminator depends on what the exchange had already said, which is
/// the mapping's state and therefore the mapping's call.
#[derive(Debug, Default)]
pub(super) struct Splitter {
    buffer: Vec<u8>,
    /// Bytes of `buffer` already cut into frames.
    read: usize,
    /// An EndStream frame has been produced, so nothing more may follow.
    ended: bool,
}

impl Splitter {
    /// Absorbs the next piece of the body; [`frame`](Self::frame) drains
    /// whatever it completed.
    pub(super) fn push(&mut self, chunk: &[u8]) {
        // Compacted before growing rather than on every cut, which keeps the
        // buffer bounded by one frame plus one chunk instead of the whole
        // body — the exact thing incremental delivery exists to avoid.
        if self.read > 0 {
            self.buffer.drain(..self.read);
            self.read = 0;
        }
        self.buffer.extend_from_slice(chunk);
    }

    /// The next whole frame, or [`None`] until more of the body arrives.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Parse`] when bytes follow the EndStream
    /// frame — the stream's status has been said, and a body that keeps
    /// talking past it is one this build does not understand.
    pub(super) fn frame(&mut self) -> Result<Option<Frame>, ProviderError> {
        let rest = &self.buffer[self.read..];
        if self.ended {
            if rest.is_empty() {
                return Ok(None);
            }
            return Err(ProviderError::Parse(
                "the response carried bytes after its EndStream frame".to_owned(),
            ));
        }

        let Some((prefix, tail)) = rest.split_at_checked(PREFIX) else {
            return Ok(None);
        };
        // The prefix split is exactly five bytes, so the length bytes are
        // there by construction.
        let declared = u32::from_be_bytes([prefix[1], prefix[2], prefix[3], prefix[4]]) as usize;
        let Some((payload, _)) = tail.split_at_checked(declared) else {
            return Ok(None);
        };

        let frame = Frame { flags: prefix[0], payload: payload.to_vec() };
        self.read += PREFIX + declared;
        self.ended = frame.is_end_stream();

        Ok(Some(frame))
    }
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
        return Err(ProviderError::Parse("the EndStream frame is not a JSON object".to_owned()));
    }

    Ok(parsed.get("error").map(|error| {
        let field =
            |name: &str| error.get(name).and_then(Value::as_str).unwrap_or_default().to_owned();

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
#[path = "connect_tests.rs"]
mod tests;
