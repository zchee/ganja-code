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
//! `id:` line, one `event:` line, one or more `data:` lines, a blank line. Anything else on
//! this stream is a server this build does not understand, which is
//! [`ClientError::Skew`] and not a shrug.

use ganja_protocol::Event;
use serde::{Deserialize, Serialize};

use crate::ClientError;

/// A journal position, scoped to one server process incarnation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EventId {
    epoch: String,
    seq: u64,
}

impl EventId {
    /// The server incarnation component.
    #[must_use]
    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// The sequence within the incarnation.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}-{}", self.epoch, self.seq)
    }
}

impl std::str::FromStr for EventId {
    type Err = ClientError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let malformed = || ClientError::Skew { detail: "a malformed event id".to_owned() };
        let (epoch, seq) = value.rsplit_once('-').ok_or_else(malformed)?;
        if epoch.len() != 36
            || !epoch.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                }
            })
            || epoch.as_bytes().get(14) != Some(&b'7')
            || !epoch
                .as_bytes()
                .get(19)
                .is_some_and(|byte| matches!(byte, b'8' | b'9' | b'a' | b'b'))
            || seq.is_empty()
            || !seq.bytes().all(|byte| byte.is_ascii_digit())
            || (seq.len() > 1 && seq.starts_with('0'))
        {
            return Err(malformed());
        }
        Ok(Self { epoch: epoch.to_owned(), seq: seq.parse().map_err(|_| malformed())? })
    }
}

impl Serialize for EventId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EventId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }
}

/// How the server registered this subscription.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Replay {
    /// No cursor was supplied; messages begin after the head.
    Fresh,
    /// The supplied cursor is retained; later messages are replayed.
    From,
    /// The supplied cursor is unavailable; state must be read afresh.
    Gap,
}

/// The registration acknowledgement, including its journal head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connected {
    /// The journal head when the subscription registered.
    pub head: EventId,
    /// Whether this stream is fresh, replayed, or requires recovery.
    pub replay: Replay,
}

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
    Connected(EventId, Connected),
    /// [`MESSAGE`], carrying the event it framed.
    Message(EventId, Box<Event>),
    /// [`HEARTBEAT`].
    Heartbeat(EventId),
    /// [`EVICTED`], carrying the notice that says why.
    Evicted(EventId, EvictedNotice),
}

impl Frame {
    /// The frame `name` carrying `data`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Skew`] for a name outside [`FRAMES`], and for a payload
    /// that does not parse — including an [`Event`] whose `type` this build
    /// has no variant for, which is exactly what a newer server sends.
    /// The cursor carried by this frame; control frames never advance it.
    #[must_use]
    pub fn id(&self) -> &EventId {
        match self {
            Self::Connected(id, _)
            | Self::Message(id, _)
            | Self::Heartbeat(id)
            | Self::Evicted(id, _) => id,
        }
    }

    fn parse(name: &str, data: &str, id: EventId) -> Result<Self, ClientError> {
        match name {
            CONNECTED => serde_json::from_str::<Connected>(data)
                .map(|connected| Self::Connected(id, connected))
                .map_err(|error| ClientError::Skew {
                    detail: format!("a connected frame does not parse: {error}"),
                }),
            HEARTBEAT => Ok(Self::Heartbeat(id)),
            MESSAGE => serde_json::from_str::<Event>(data)
                .map(|event| Self::Message(id, Box::new(event)))
                .map_err(|error| ClientError::Skew {
                    detail: format!("an event frame does not parse: {error}"),
                }),
            EVICTED => serde_json::from_str::<EvictedNotice>(data)
                .map(|notice| Self::Evicted(id, notice))
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
        let mut id = None;
        for line in text.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(rest) = line.strip_prefix("id:") {
                if id.is_some() {
                    return Some(Err(ClientError::Skew {
                        detail: "an event frame repeats its id".to_owned(),
                    }));
                }
                id = Some(rest.strip_prefix(' ').unwrap_or(rest).parse::<EventId>());
            } else if let Some(rest) = line.strip_prefix("event: ") {
                name = rest.to_owned();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest);
            }
        }

        Some(
            id.unwrap_or_else(|| {
                Err(ClientError::Skew { detail: "an event frame has no id".to_owned() })
            })
            .and_then(|id| Frame::parse(&name, &data, id)),
        )
    }
}

/// Where the newline pair separating one frame from the next begins, so
/// `+ 2` is one past the end of the frame.
fn find_blank_line(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|window| window == b"\n\n")
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
