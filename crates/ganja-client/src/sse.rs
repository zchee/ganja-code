//! The frame vocabulary `ganja serve` speaks on `GET /event`, declared here.
//!
//! Spec: `crates/ganja-serve/src/sse.rs` — its `pump` has exactly four
//! frame-emitting sites (`sse.rs:80,99,110,116`), and this module names all
//! four plus the one JSON shape a control frame carries.
//!
//! **Declared here rather than published from `ganja-serve`** on purpose: a
//! `ganja-client → ganja-serve` dependency would drag `axum` and the whole
//! HTTP server into every consumer of this crate, which is the one thing its
//! dependency list exists to prevent. The duplicate is paid for with a pin —
//! `crates/ganja-cli/tests/frames.rs` drives a real server and holds this
//! declaration against the bytes that server actually writes, exhaustively in
//! both directions — so a drift on either side reddens a named test rather
//! than being discovered by a client that silently ignores a frame.
//!
//! The framing itself is the subset of server-sent events serve writes: one
//! `event:` line, one or more `data:` lines, a blank line. Anything else on
//! this stream is a server this build does not understand, which is
//! [`ClientError::Skew`] and not a shrug.

use ganja_protocol::Event;
use serde::{Deserialize, Serialize};

use crate::ClientError;

/// The frame that opens every stream, before anything the engine emits.
///
/// Reading it is the registration guarantee: serve claims its subscription
/// before the response exists, so a client holding this frame knows every
/// later engine event is either in its stream or after its registration.
pub const CONNECTED: &str = "connected";

/// The frame carrying one [`Event`], serialized whole.
pub const MESSAGE: &str = "message";

/// The frame a silent stream sends to prove it is still alive.
pub const HEARTBEAT: &str = "heartbeat";

/// The terminal frame of a subscriber the engine dropped: everything before
/// it is real and in order, and everything after it was never queued.
pub const EVICTED: &str = "evicted";

/// Every frame name this client understands, which is every frame name serve
/// writes. Exhaustive in both directions — that is what the pin asserts.
pub const FRAMES: [&str; 4] = [CONNECTED, MESSAGE, HEARTBEAT, EVICTED];

/// What an [`EVICTED`] frame carries (`ganja-serve/src/sse.rs:106-109`).
///
/// `deny_unknown_fields` is the skew posture applied to a payload rather than
/// a name: a server that grew a field here is a server this build does not
/// understand, and saying so beats quietly dropping whatever it added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvictedNotice {
    /// Always [`EVICTED`]; serve names the frame twice, once as the SSE event
    /// and once inside the payload.
    #[serde(rename = "type")]
    pub kind: String,
    /// Why the subscription ended, in the engine's own words.
    pub message: String,
}

/// One frame off the stream, parsed.
///
/// [`Frame::Message`] is boxed so the enum stays the size of a pointer plus a
/// discriminant rather than the size of the largest event.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    /// [`CONNECTED`].
    Connected,
    /// [`MESSAGE`], carrying the event it framed.
    Message(Box<Event>),
    /// [`HEARTBEAT`].
    Heartbeat,
    /// [`EVICTED`], carrying the notice that says why.
    Evicted(EvictedNotice),
}

impl Frame {
    /// The frame `name` carrying `data`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Skew`] for a name outside [`FRAMES`], and for a payload
    /// that does not parse — including an [`Event`] whose `type` this build
    /// has no variant for, which is exactly what a newer server sends.
    pub fn parse(name: &str, data: &str) -> Result<Self, ClientError> {
        match name {
            CONNECTED => Ok(Self::Connected),
            HEARTBEAT => Ok(Self::Heartbeat),
            MESSAGE => serde_json::from_str::<Event>(data)
                .map(|event| Self::Message(Box::new(event)))
                .map_err(|error| ClientError::Skew {
                    detail: format!("an event frame does not parse: {error}"),
                }),
            EVICTED => serde_json::from_str::<EvictedNotice>(data)
                .map(Self::Evicted)
                .map_err(|error| ClientError::Skew {
                    detail: format!("an evicted frame does not parse: {error}"),
                }),
            other => Err(ClientError::Skew {
                detail: format!(
                    "a frame named {other:?} is none of the {} this build knows",
                    FRAMES.join(", ")
                ),
            }),
        }
    }
}

/// Splits a byte stream into [`Frame`]s.
///
/// Public because it is the seam a test drives directly: fed the bytes a real
/// server wrote, it must yield exactly the frames this crate declares, which
/// is the representation half of the pin.
#[derive(Debug, Default)]
pub struct Frames {
    buffer: Vec<u8>,
}

impl Frames {
    /// An empty splitter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds bytes as they arrive; a frame split across two reads is finished
    /// by the second.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete frame, if the bytes so far hold one.
    ///
    /// # Errors
    ///
    /// [`ClientError::Skew`] for a frame this build cannot name or parse.
    pub fn pop(&mut self) -> Option<Result<Frame, ClientError>> {
        let end = find_blank_line(&self.buffer)?;
        let raw: Vec<u8> = self.buffer.drain(..end + 2).collect();
        let text = String::from_utf8_lossy(&raw);

        // The SSE default event type is `message`, which serve never relies on
        // — it names every frame — but honouring it costs nothing and keeps
        // this a reader of the format rather than of one writer's habits.
        let mut name = MESSAGE.to_owned();
        let mut data = String::new();
        for line in text.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(rest) = line.strip_prefix("event: ") {
                name = rest.to_owned();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest);
            }
        }

        Some(Frame::parse(&name, &data))
    }
}

/// Where the newline pair separating one frame from the next begins, so
/// `+ 2` is one past the end of the frame.
fn find_blank_line(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|window| window == b"\n\n")
}

#[cfg(test)]
mod tests {
    use super::{EvictedNotice, FRAMES, Frame, Frames};

    /// Serve writes `event: <name>\ndata: <json>\n\n`; a splitter that only
    /// worked when a whole frame arrived in one read would work in a test and
    /// fail against a real socket.
    #[test]
    fn a_frame_split_across_reads_is_still_one_frame() {
        let mut frames = Frames::new();
        frames.push(b"event: connect");
        assert!(frames.pop().is_none());
        frames.push(b"ed\ndata: {}\n\nevent: heartbeat\ndata: {}\n\n");

        assert_eq!(
            frames.pop().transpose().expect("a declared frame"),
            Some(Frame::Connected)
        );
        assert_eq!(
            frames.pop().transpose().expect("a declared frame"),
            Some(Frame::Heartbeat)
        );
        assert!(frames.pop().is_none());
    }

    #[test]
    fn a_frame_named_outside_the_vocabulary_is_a_version_mismatch() {
        let mut frames = Frames::new();
        frames.push(b"event: server.hello\ndata: {}\n\n");

        let error = frames
            .pop()
            .expect("a complete frame")
            .expect_err("a name outside the set is refused");
        let said = error.to_string();
        assert!(said.contains("server.hello"), "{said}");
        assert!(
            said.contains("different versions of ganja"),
            "the refusal names the mismatch: {said}"
        );
    }

    #[test]
    fn an_evicted_notice_round_trips_through_the_declared_shape() {
        let notice = EvictedNotice {
            kind: super::EVICTED.to_owned(),
            message: "this subscriber fell behind".to_owned(),
        };
        let written = serde_json::to_string(&notice).expect("the notice serializes");

        assert_eq!(
            serde_json::from_str::<EvictedNotice>(&written).expect("and reads back"),
            notice
        );
        // A field nobody declared is a server this build does not understand.
        assert!(
            serde_json::from_str::<EvictedNotice>(r#"{"type":"evicted","message":"x","why":1}"#)
                .is_err()
        );
    }

    #[test]
    fn the_vocabulary_is_four_distinct_names() {
        let mut sorted = FRAMES;
        sorted.sort_unstable();
        sorted
            .windows(2)
            .for_each(|pair| assert_ne!(pair[0], pair[1], "two frames share a name"));
        assert_eq!(FRAMES.len(), 4);
    }
}
