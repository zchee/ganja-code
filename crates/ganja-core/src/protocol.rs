//! The wire protocol frontends speak, version 1.
//!
//! Every type here is serde-serializable so that the same values can later
//! cross a socket unchanged, and so that P4 can persist a session by writing
//! them out verbatim. The model follows upstream's `session/message-v2.ts`:
//! messages carry ordered parts, parts carry a type tag beside their id, and
//! ids sort in creation order.
//!
//! P2 ships text parts. P3 adds tool and step parts as new [`PartBody`]
//! variants, which changes nothing already on the wire.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// Prefix upstream gives message ids, kept so transcripts read the same.
const MESSAGE_PREFIX: &str = "msg";

/// Prefix upstream gives part ids.
const PART_PREFIX: &str = "prt";

/// Milliseconds since the Unix epoch, saturating rather than failing when the
/// clock is set before 1970.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Mints an identifier that sorts after every identifier minted before it.
///
/// The layout mirrors upstream's ascending ids: a millisecond timestamp
/// followed by a per-process counter, both fixed-width hex, so ids sort
/// lexicographically by creation time and cannot collide inside one process.
/// Ordering across processes is only as good as the clock, which is the same
/// guarantee upstream makes.
fn ascending(prefix: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let millis = now();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed) & 0xff_ffff;

    format!("{prefix}_{millis:011x}{sequence:06x}")
}

/// Identifies a [`Message`] within a session.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(String);

impl MessageId {
    /// Mints an id that sorts after every id minted before it.
    #[must_use]
    pub fn ascending() -> Self {
        Self(ascending(MESSAGE_PREFIX))
    }

    /// The id as it travels the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for MessageId {
    /// Adopts a stored id. The prefix is a convention rather than an
    /// invariant: a transcript read back from disk keeps whatever it was
    /// written with.
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Identifies a [`Part`] within a message.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PartId(String);

impl PartId {
    /// Mints an id that sorts after every id minted before it.
    #[must_use]
    pub fn ascending() -> Self {
        Self(ascending(PART_PREFIX))
    }

    /// The id as it travels the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for PartId {
    /// Adopts a stored id; see [`MessageId::from`].
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Who produced a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The person at the terminal.
    User,
    /// The model.
    Assistant,
}

/// What a turn spent, as the provider reported it.
///
/// Cost stays out until there is a model table to price tokens against; the
/// counts are what every provider reports and what P4 accumulates per session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Tokens the request cost.
    pub input_tokens: u64,
    /// Tokens the reply cost.
    pub output_tokens: u64,
    /// Output tokens the model spent thinking, where it reports them apart.
    pub reasoning_tokens: u64,
    /// Input tokens served from the provider's prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written into the provider's prompt cache.
    pub cache_write_tokens: u64,
}

/// The kinds of content a [`Part`] can carry.
///
/// The tag travels as a `type` field beside the part's id, which is the shape
/// upstream's parts have, so P3's tool and step parts and P4's stored
/// transcripts add variants without moving anything already on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PartBody {
    /// Plain text, streamed in fragments.
    Text {
        /// Everything accumulated so far.
        text: String,
    },
}

/// One piece of a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    /// Identifies the part so that [`Event::PartDelta`] can address it.
    pub id: PartId,
    /// What the part carries.
    #[serde(flatten)]
    pub body: PartBody,
}

impl Part {
    /// Builds a text part with a fresh id.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            id: PartId::ascending(),
            body: PartBody::Text { text: text.into() },
        }
    }

    /// The text this part carries, or [`None`] when it carries something else.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match &self.body {
            PartBody::Text { text } => Some(text),
        }
    }

    /// The text this part carries, for accumulating streamed fragments.
    pub fn as_text_mut(&mut self) -> Option<&mut String> {
        match &mut self.body {
            PartBody::Text { text } => Some(text),
        }
    }
}

/// When a message happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTime {
    /// Milliseconds since the Unix epoch.
    pub created: u64,
    /// Set once nothing more will be added to the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
}

/// One turn's worth of content from one side of the conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Identifies the message; ids sort in creation order.
    pub id: MessageId,
    /// Who produced it.
    pub role: Role,
    /// Its content, in the order it arrived.
    pub parts: Vec<Part>,
    /// When it started and, once known, finished.
    pub time: MessageTime,
    /// Model that produced it, absent on user messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What the turn spent, absent until the provider reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl Message {
    /// Builds the complete user message carrying `text`.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        let created = now();

        Self {
            id: MessageId::ascending(),
            role: Role::User,
            parts: vec![Part::text(text)],
            time: MessageTime {
                created,
                completed: Some(created),
            },
            model: None,
            usage: None,
        }
    }

    /// Opens an assistant message that `model` streams parts into.
    #[must_use]
    pub fn assistant(model: impl Into<String>) -> Self {
        Self {
            id: MessageId::ascending(),
            role: Role::Assistant,
            parts: Vec::new(),
            time: MessageTime {
                created: now(),
                completed: None,
            },
            model: Some(model.into()),
            usage: None,
        }
    }

    /// Marks the message complete, returning when that happened so the caller
    /// can report the same instant it recorded.
    pub fn complete(&mut self) -> u64 {
        let completed = now();
        self.time.completed = Some(completed);

        completed
    }

    /// Whether the message carries anything worth keeping. An assistant turn
    /// that failed before its first fragment does not.
    #[must_use]
    pub fn has_content(&self) -> bool {
        self.parts
            .iter()
            .any(|part| part.as_text().is_none_or(|text| !text.is_empty()))
    }
}

/// A request from a frontend to the engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Starts a turn that answers `text`.
    SendPrompt {
        /// What the user typed.
        text: String,
    },
    /// Stops the turn that is streaming; a no-op when the engine is idle.
    CancelTurn,
}

/// Something the engine observed, delivered to the subscribed frontend in
/// order and without loss.
///
/// The stream is the whole truth: a frontend that applies every event in order
/// holds the same transcript the engine does, which is what lets P4 rebuild a
/// session and P7 serve one over a socket. Names follow upstream's bus —
/// [`Event::PartDelta`] is `message.part.delta`, [`Event::PartStarted`] is
/// `message.part.updated`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A message entered the transcript. A user message arrives complete; an
    /// assistant message arrives empty and grows through the events below.
    MessageStarted {
        /// The message as it stands.
        message: Message,
    },
    /// A part was appended to a message that is still streaming. It arrives
    /// before any delta addresses it, so a frontend always knows a part's id
    /// and kind before its content.
    PartStarted {
        /// Message the part belongs to.
        message_id: MessageId,
        /// The part, empty of content.
        part: Part,
    },
    /// Content was appended to a part.
    PartDelta {
        /// Message the part belongs to.
        message_id: MessageId,
        /// Part to append to.
        part_id: PartId,
        /// What to append.
        delta: String,
    },
    /// The turn ended and the engine is idle again. Always the last event of a
    /// turn, whatever went wrong during it.
    MessageFinished {
        /// The assistant message that just closed.
        message_id: MessageId,
        /// Why it ended.
        reason: FinishReason,
        /// What the turn spent, when the provider reported it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// What went wrong, set exactly when `reason` is
        /// [`FinishReason::Failed`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Milliseconds since the Unix epoch, matching the message's
        /// [`MessageTime::completed`].
        completed: u64,
    },
}

/// Why a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The provider ran out of text.
    Completed,
    /// A [`Command::CancelTurn`] arrived before the provider finished.
    Cancelled,
    /// The provider could not answer.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::{
        Command, Event, FinishReason, Message, MessageId, MessageTime, Part, PartBody, PartId,
        Role, Usage,
    };

    /// Builds a message with pinned ids and times so a test can assert on the
    /// exact bytes that reach the wire.
    fn pinned_message() -> Message {
        Message {
            id: MessageId::from("msg_1".to_owned()),
            role: Role::User,
            parts: vec![Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Text {
                    text: "hi".to_owned(),
                },
            }],
            time: MessageTime {
                created: 7,
                completed: Some(7),
            },
            model: None,
            usage: None,
        }
    }

    #[test]
    fn ids_sort_in_creation_order_and_carry_their_prefix() {
        let ids: Vec<MessageId> = (0..64).map(|_| MessageId::ascending()).collect();

        assert!(
            ids.iter().all(|id| id.as_str().starts_with("msg_")),
            "ids should carry the upstream prefix: {ids:?}"
        );
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "ids should sort in creation order: {ids:?}"
        );

        let parts: Vec<PartId> = (0..64).map(|_| PartId::ascending()).collect();
        assert!(parts.iter().all(|id| id.as_str().starts_with("prt_")));
        assert!(parts.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_user_message_is_born_complete_and_a_reply_is_not() {
        let user = Message::user("hi");

        assert_eq!(user.role, Role::User);
        assert_eq!(user.time.completed, Some(user.time.created));
        assert_eq!(user.parts.first().and_then(Part::as_text), Some("hi"));
        assert!(user.model.is_none());

        let mut assistant = Message::assistant("canned");

        assert_eq!(assistant.role, Role::Assistant);
        assert!(assistant.parts.is_empty());
        assert!(assistant.time.completed.is_none());
        assert_eq!(assistant.model.as_deref(), Some("canned"));

        let completed = assistant.complete();
        assert_eq!(assistant.time.completed, Some(completed));
    }

    #[test]
    fn an_empty_part_is_not_content_but_a_filled_one_is() {
        let mut message = Message::assistant("canned");
        assert!(!message.has_content());

        message.parts.push(Part::text(""));
        assert!(!message.has_content());

        if let Some(text) = message.parts.last_mut().and_then(Part::as_text_mut) {
            text.push_str("hello");
        }
        assert!(message.has_content());
    }

    #[test]
    fn commands_round_trip_through_json() {
        let cases = [
            Command::SendPrompt {
                text: "hello".to_owned(),
            },
            Command::CancelTurn,
        ];

        for command in cases {
            let encoded = serde_json::to_string(&command).expect("a command serializes");
            let decoded: Command = serde_json::from_str(&encoded).expect("a command deserializes");
            assert_eq!(decoded, command, "round trip changed {encoded}");
        }
    }

    #[test]
    fn events_round_trip_through_json() {
        let message = pinned_message();
        let cases = [
            Event::MessageStarted {
                message: message.clone(),
            },
            Event::PartStarted {
                message_id: message.id.clone(),
                part: Part::text(""),
            },
            Event::PartDelta {
                message_id: message.id.clone(),
                part_id: PartId::from("prt_1".to_owned()),
                delta: "hi".to_owned(),
            },
            Event::MessageFinished {
                message_id: message.id,
                reason: FinishReason::Failed,
                usage: Some(Usage {
                    input_tokens: 3,
                    output_tokens: 4,
                    reasoning_tokens: 5,
                    cache_read_tokens: 6,
                    cache_write_tokens: 7,
                }),
                error: Some("no credentials".to_owned()),
                completed: 9,
            },
        ];

        for event in cases {
            let encoded = serde_json::to_string(&event).expect("an event serializes");
            let decoded: Event = serde_json::from_str(&encoded).expect("an event deserializes");
            assert_eq!(decoded, event, "round trip changed {encoded}");
        }
    }

    /// Pins the bytes of every variant. A change here is a protocol change: it
    /// invalidates stored sessions (P4) and anything speaking the protocol over
    /// a socket (P7), so it has to be a deliberate edit rather than a side
    /// effect of renaming a field.
    #[test]
    fn the_wire_format_is_stable() {
        let cases = [
            (
                serde_json::to_string(&Command::SendPrompt {
                    text: "hi".to_owned(),
                }),
                r#"{"type":"send_prompt","text":"hi"}"#,
            ),
            (
                serde_json::to_string(&Command::CancelTurn),
                r#"{"type":"cancel_turn"}"#,
            ),
            (
                serde_json::to_string(&Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::Text {
                        text: "hi".to_owned(),
                    },
                }),
                r#"{"id":"prt_1","type":"text","text":"hi"}"#,
            ),
            (
                serde_json::to_string(&Event::MessageStarted {
                    message: pinned_message(),
                }),
                r#"{"type":"message_started","message":{"id":"msg_1","role":"user","parts":[{"id":"prt_1","type":"text","text":"hi"}],"time":{"created":7,"completed":7}}}"#,
            ),
            (
                serde_json::to_string(&Event::PartStarted {
                    message_id: MessageId::from("msg_1".to_owned()),
                    part: Part {
                        id: PartId::from("prt_1".to_owned()),
                        body: PartBody::Text {
                            text: String::new(),
                        },
                    },
                }),
                r#"{"type":"part_started","message_id":"msg_1","part":{"id":"prt_1","type":"text","text":""}}"#,
            ),
            (
                serde_json::to_string(&Event::PartDelta {
                    message_id: MessageId::from("msg_1".to_owned()),
                    part_id: PartId::from("prt_1".to_owned()),
                    delta: "hi".to_owned(),
                }),
                r#"{"type":"part_delta","message_id":"msg_1","part_id":"prt_1","delta":"hi"}"#,
            ),
            (
                serde_json::to_string(&Event::MessageFinished {
                    message_id: MessageId::from("msg_1".to_owned()),
                    reason: FinishReason::Completed,
                    usage: Some(Usage {
                        input_tokens: 1,
                        output_tokens: 2,
                        ..Usage::default()
                    }),
                    error: None,
                    completed: 9,
                }),
                r#"{"type":"message_finished","message_id":"msg_1","reason":"completed","usage":{"input_tokens":1,"output_tokens":2,"reasoning_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0},"completed":9}"#,
            ),
            (
                serde_json::to_string(&Event::MessageFinished {
                    message_id: MessageId::from("msg_1".to_owned()),
                    reason: FinishReason::Cancelled,
                    usage: None,
                    error: None,
                    completed: 9,
                }),
                r#"{"type":"message_finished","message_id":"msg_1","reason":"cancelled","completed":9}"#,
            ),
            (
                serde_json::to_string(&Event::MessageFinished {
                    message_id: MessageId::from("msg_1".to_owned()),
                    reason: FinishReason::Failed,
                    usage: None,
                    error: Some("no credentials".to_owned()),
                    completed: 9,
                }),
                r#"{"type":"message_finished","message_id":"msg_1","reason":"failed","error":"no credentials","completed":9}"#,
            ),
        ];

        for (encoded, expected) in cases {
            assert_eq!(encoded.expect("the value serializes"), expected);
        }
    }

    /// A stored assistant message keeps its model and usage; a stored user
    /// message keeps neither, and reading one back does not invent them.
    #[test]
    fn an_assistant_message_round_trips_with_its_model_and_usage() {
        let mut message = Message::assistant("canned");
        message.parts.push(Part::text("hello"));
        message.usage = Some(Usage {
            input_tokens: 1,
            output_tokens: 2,
            ..Usage::default()
        });
        message.complete();

        let encoded = serde_json::to_string(&message).expect("a message serializes");
        let decoded: Message = serde_json::from_str(&encoded).expect("a message deserializes");

        assert_eq!(decoded, message, "round trip changed {encoded}");

        let user = Message::user("hi");
        let encoded = serde_json::to_string(&user).expect("a message serializes");
        assert!(
            !encoded.contains("model") && !encoded.contains("usage"),
            "a user message should carry neither: {encoded}"
        );
    }
}
