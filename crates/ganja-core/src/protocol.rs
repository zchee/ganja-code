//! The wire protocol frontends speak, version 0.
//!
//! Every type here is serde-serializable so that the same values can later
//! cross a socket unchanged. P1 covers a single turn of plain text; P2 grows
//! these enums into the full message/part model.

use serde::{Deserialize, Serialize};

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A turn began; assistant text follows.
    TurnStarted,
    /// The next fragment of assistant text.
    TextDelta {
        /// Text to append to the reply being streamed.
        text: String,
    },
    /// The turn ended and the engine is idle again.
    TurnFinished {
        /// Why the turn ended.
        reason: FinishReason,
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
}

#[cfg(test)]
mod tests {
    use super::{Command, Event, FinishReason};

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
        let cases = [
            Event::TurnStarted,
            Event::TextDelta {
                text: "hi".to_owned(),
            },
            Event::TurnFinished {
                reason: FinishReason::Cancelled,
            },
        ];

        for event in cases {
            let encoded = serde_json::to_string(&event).expect("an event serializes");
            let decoded: Event = serde_json::from_str(&encoded).expect("an event deserializes");
            assert_eq!(decoded, event, "round trip changed {encoded}");
        }
    }

    #[test]
    fn the_tagged_layout_is_stable() {
        let encoded = serde_json::to_string(&Event::TextDelta {
            text: "hi".to_owned(),
        })
        .expect("an event serializes");

        assert_eq!(encoded, r#"{"type":"text_delta","text":"hi"}"#);
    }
}
