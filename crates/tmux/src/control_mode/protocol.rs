//! Spec: pandaemonium `pkg/tmux/protocol.go`.
//!
//! [`Parser`] incrementally parses tmux control-mode lines into guarded
//! response blocks and asynchronous `%` notifications. It is sans-io and
//! synchronous by design (see the workspace's principle 3): every protocol
//! decision here is unit-testable without a subprocess, and [`Parser`] stays
//! usable standalone by a caller who manages its own transport (an external
//! PTY-backed `-CC` connection, for instance — see the crate doc).
//!
//! # `Event`'s empty state moves outward (divergence)
//!
//! Go's `Event` is a struct with two nullable pointer fields
//! (`*Response`, `*Notification`); a "zero `Event`" (both nil) means "the
//! parser accepted a line but has not completed anything yet." Rust instead
//! makes that outer state explicit in [`Parser::feed`]'s return type —
//! `Result<Option<Event>, ProtocolError>`, `Ok(None)` for "nothing yet" — so
//! [`Event`] itself is a plain two-variant enum with no empty state to
//! forget to check.
//!
//! # Marker identity and the adversarial-mimic caveat
//!
//! tmux assigns response command numbers monotonically per control client,
//! so a payload line cannot collide with the in-flight `%begin` under normal
//! operation. The only adversarial collision shape is a payload line that
//! exactly mimics `%end <begin-time> <begin-command> <flags>`; tmux itself
//! never produces such a line, so [`same_marker_identity`] intentionally
//! accepts the first matching terminator — ported verbatim from Go's
//! `sameMarkerIdentity` doc comment, including its caveat that a payload
//! mimicking a *different* command's end marker must not terminate the
//! active block (see `mismatched_end_marker_remains_output` in
//! `protocol_tests.rs`).

use crate::{
    control_mode::notification::{self, Notification},
    error::ProtocolError,
};

/// A tmux `%begin`, `%end`, or `%error` response marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockMarker {
    /// The marker timestamp, in whole seconds since the Unix epoch.
    pub time: i64,
    /// tmux's unique command number for the response block.
    pub command: i64,
    /// The marker flags reported by tmux.
    pub flags: i64,
}

/// One guarded tmux command response block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// The opening `%begin` marker.
    pub begin: BlockMarker,
    /// The closing `%end` or `%error` marker.
    pub end: BlockMarker,
    /// The command output lines between the guard markers.
    pub lines: Vec<String>,
    /// Whether the block ended with `%error` rather than `%end`.
    pub error: bool,
}

/// One complete control-mode response block or asynchronous notification.
///
/// See the module doc for how this differs from Go's `Event`.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A complete `%begin`/`%end` or `%begin`/`%error` command response
    /// block.
    Response(Response),
    /// A complete `%`-prefixed notification line.
    Notification(Notification),
}

/// One in-progress `%begin` ... `%end`/`%error` response block.
#[derive(Debug)]
struct ResponseBuilder {
    begin: BlockMarker,
    lines: Vec<String>,
}

/// Incrementally parses tmux control-mode lines.
///
/// Reusable by a caller that already manages its own tmux control-mode
/// transport and wants the same response/notification splitting
/// [`crate::control_mode::Client`] uses internally.
#[derive(Debug, Default)]
pub struct Parser {
    active: Option<ResponseBuilder>,
}

impl Parser {
    /// Parses one newline-free tmux control-mode line.
    ///
    /// Returns `Ok(None)` when the line was accepted but did not complete a
    /// response block or notification — see the module doc for why that is
    /// the outer `Option` rather than a variant of [`Event`].
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError`] when `line` is a malformed `%begin` marker,
    /// or is a non-blank line outside an active response block that is
    /// neither a `%begin` marker nor a `%`-prefixed notification.
    pub fn feed(&mut self, line: &str) -> Result<Option<Event>, ProtocolError> {
        let Some(line) = normalize_control_line(line) else {
            return Ok(None);
        };

        if let Some(builder) = self.active.as_mut() {
            if let Some((is_error, marker)) = parse_terminator(line)
                && same_marker_identity(builder.begin, marker)
            {
                let begin = builder.begin;
                let lines = std::mem::take(&mut builder.lines);
                self.active = None;
                return Ok(Some(Event::Response(Response {
                    begin,
                    end: marker,
                    lines,
                    error: is_error,
                })));
            }
            builder.lines.push(line.to_string());
            return Ok(None);
        }

        match parse_begin(line) {
            Ok(Some(marker)) => {
                self.active = Some(ResponseBuilder {
                    begin: marker,
                    lines: Vec::new(),
                });
                return Ok(None);
            }
            Ok(None) => {}
            Err(message) => {
                return Err(ProtocolError {
                    line: Some(line.to_string()),
                    message,
                });
            }
        }

        if line.starts_with('%') {
            let notification = notification::parse(line)?;
            return Ok(Some(Event::Notification(notification)));
        }

        Err(ProtocolError {
            line: Some(line.to_string()),
            message: "unexpected non-control line outside response block".to_string(),
        })
    }

    /// Reports whether the input ended in the middle of a response block.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError`] naming the still-open `%begin` command
    /// number when a response block was never closed.
    pub fn close(&mut self) -> Result<(), ProtocolError> {
        match &self.active {
            None => Ok(()),
            Some(builder) => Err(ProtocolError {
                line: None,
                message: format!(
                    "unexpected EOF after %begin for command {}",
                    builder.begin.command
                ),
            }),
        }
    }
}

/// The growth point Go's `normalizeControlLine` reserves: today it only
/// filters blank lines, but keeping it as its own function (rather than an
/// inline emptiness check) is what Go's original does too.
fn normalize_control_line(line: &str) -> Option<&str> {
    if line.is_empty() { None } else { Some(line) }
}

fn parse_begin(line: &str) -> Result<Option<BlockMarker>, String> {
    match line.split_whitespace().next() {
        Some("%begin") => parse_marker(line, "%begin").map(Some),
        _ => Ok(None),
    }
}

/// Recognizes a `%end`/`%error` terminator line, reporting whether it is an
/// error terminator and the marker it carries.
///
/// A line whose marker fails to parse is treated as *not* a terminator
/// (`None`) rather than propagating a `ProtocolError` — ported from Go's
/// `parseTerminator`, which swallows the marker-parse error on this path so
/// a malformed `%end`/`%error`-looking payload line becomes ordinary output
/// instead of aborting the parse (see `malformed_end_like_payload_does_not_terminate`
/// below).
fn parse_terminator(line: &str) -> Option<(bool, BlockMarker)> {
    let mut fields = line.split_whitespace();
    let first = fields.next()?;
    let is_error = match first {
        "%end" => false,
        "%error" => true,
        _ => return None,
    };
    let marker = parse_marker(line, first).ok()?;
    Some((is_error, marker))
}

fn parse_marker(line: &str, prefix: &str) -> Result<BlockMarker, String> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() != 4 || fields[0] != prefix {
        return Err(format!("malformed {prefix} marker"));
    }
    let time: i64 = fields[1]
        .parse()
        .map_err(|source| format!("invalid marker time {:?}: {source}", fields[1]))?;
    let command: i64 = fields[2]
        .parse()
        .map_err(|source| format!("invalid marker command {:?}: {source}", fields[2]))?;
    let flags: i64 = fields[3]
        .parse()
        .map_err(|source| format!("invalid marker flags {:?}: {source}", fields[3]))?;
    Ok(BlockMarker {
        time,
        command,
        flags,
    })
}

/// Reports whether two [`BlockMarker`]s share the timestamp and command
/// number tmux assigns to a single response. See the module doc.
fn same_marker_identity(a: BlockMarker, b: BlockMarker) -> bool {
    a.command == b.command && a.time == b.time
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
