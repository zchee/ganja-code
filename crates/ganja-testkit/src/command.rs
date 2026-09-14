//! The [`Command`] values suites spell the same way.

use ganja_protocol::Command;

/// A prompt saying `text`, with the four optional fields every frontend
/// leaves empty.
///
/// ```
/// let command = ganja_testkit::prompt("hello");
/// assert!(matches!(
///     command,
///     ganja_protocol::Command::SendPrompt { ref text, .. } if text == "hello"
/// ));
/// ```
#[must_use]
pub fn prompt(text: &str) -> Command {
    Command::SendPrompt {
        text: text.to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
        session_mentions: Vec::new(),
        peers: Vec::new(),
    }
}

/// A moment `seconds` from now, in the epoch milliseconds
/// [`Command::SetDeadline`] spells one in.
///
/// Ahead or behind by construction rather than by waiting: the engine's slot
/// holds a wall-clock instant and no runtime clock control moves one, so a
/// negative `seconds` *is* the overdue case — nothing sleeps.
///
/// ```
/// assert!(ganja_testkit::deadline_millis(-60) < ganja_testkit::deadline_millis(60));
/// ```
#[must_use]
pub fn deadline_millis(seconds: i64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("this machine's clock is after the epoch");

    u64::try_from(now.as_millis())
        .expect("the epoch fits a u64 of millis")
        .checked_add_signed(seconds * 1000)
        .expect("the moment is representable")
}
