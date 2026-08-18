//! Spec: pandaemonium `pkg/tmux/errors.go`.
//!
//! Go's five error shapes (`ErrClosed`, `errDetachSkippedWriteLocked`,
//! `CommandError`, `ProtocolError`, `ExitError`) become one `#[non_exhaustive]`
//! [`Error`] enum plus the two structured leaf types every non-trivial
//! variant wraps. `#[non_exhaustive]` because W2's client and W3's flow
//! helpers add variants this wave cannot anticipate — Go's `error` interface
//! had no such closed set to begin with.
//!
//! # Quoting divergence
//!
//! Go formats a command line or a raw notification line with `%q`
//! (Go-escaped double-quoting); this port uses `{:?}` (`Debug` quoting for
//! `&str`) throughout. The two escaping rules agree on all printable ASCII
//! and differ only on exotic non-printable/non-ASCII input — acceptable
//! since this text is a human-readable error message, never wire data. Not
//! repeated at each call site below.
//!
//! # `ProtocolError`'s flattened source (divergence)
//!
//! Go's `ProtocolError.Err` is a boxed `error`, `Unwrap`-able with
//! `errors.Is`/`errors.As`. The cross-lane contract for this type is
//! `{ line: Option<String>, message: String }` — a plain message, no source
//! chain. One concrete case this loses: `Parser::close` reports an
//! EOF-mid-block by folding Go's `io.ErrUnexpectedEOF` sentinel into the
//! message text (`"unexpected EOF after %begin for command N"`) rather than
//! a typed, `errors::Is`-comparable value; a caller distinguishes it, if it
//! must, by matching that text. `line: Option<String>` is itself a small
//! improvement on Go's shape: Go uses `Line == ""` as its own "no line"
//! sentinel (the only site that leaves it unset is exactly this EOF case),
//! which Rust makes an explicit `None` instead of an ambiguous empty string.

use crate::protocol::Response;

fn command_error_message(line: &str, response: &Response) -> String {
    let output = response.lines.join("\n");
    if output.is_empty() {
        format!("tmux: command {line:?} failed")
    } else {
        format!("tmux: command {line:?} failed: {output}")
    }
}

/// A tmux command that completed with a `%error` marker.
///
/// Ports Go's `CommandError`: the command line tmux was asked to run,
/// paired with the response block it answered with.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("{}", command_error_message(.line, .response))]
pub struct CommandError {
    /// The command line tmux was asked to run.
    pub line: String,
    /// The response block tmux answered the command with (`error: true`).
    pub response: Response,
}

fn protocol_error_message(line: &Option<String>, message: &str) -> String {
    match line {
        None => format!("tmux: protocol error: {message}"),
        Some(line) => format!("tmux: protocol error on {line:?}: {message}"),
    }
}

/// Malformed tmux control-mode data.
///
/// Ports Go's `ProtocolError`; see the module doc for how its `Err` field
/// became a flat `message: String`.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("{}", protocol_error_message(.line, .message))]
pub struct ProtocolError {
    /// The offending line, when one was involved.
    pub line: Option<String>,
    /// What was wrong with it.
    pub message: String,
}

fn exit_message(reason: &str) -> String {
    if reason.is_empty() {
        "tmux: control client exited".to_string()
    } else {
        format!("tmux: control client exited: {reason}")
    }
}

/// Why a call against the tmux control client did not produce an answer.
///
/// Ports Go's `errors.go` as a whole: the package-level sentinels
/// (`ErrClosed`, `errDetachSkippedWriteLocked`) become unit variants, and
/// `CommandError`/`ProtocolError`/`ExitError` become wrapping variants.
/// `#[non_exhaustive]` — see the module doc.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An operation was attempted after client shutdown began.
    ///
    /// Ports Go's package-level `ErrClosed` sentinel.
    #[error("tmux: client closed")]
    Closed,

    /// A tmux command completed with a `%error` marker.
    #[error(transparent)]
    Command(#[from] CommandError),

    /// Malformed tmux control-mode data was read from the transport.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    /// A `%exit` notification arrived from the tmux control client.
    ///
    /// Ports Go's `ExitError`.
    #[error("{}", exit_message(.reason))]
    Exit {
        /// The optional reason tmux gave for exiting; empty when tmux gave
        /// none, mirroring Go's `ExitError.Reason`.
        reason: String,
    },

    /// An I/O operation on the control-mode transport failed.
    #[error("tmux: {context}: {source}")]
    Io {
        /// What was being attempted when it failed.
        context: String,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// `close` skipped the best-effort `detach-client` write because
    /// another in-flight call already holds the write lock.
    ///
    /// Ports Go's `errDetachSkippedWriteLocked` sentinel.
    #[error("tmux: detach-client skipped: write lock held")]
    DetachSkippedWriteLocked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::BlockMarker;

    fn marker() -> BlockMarker {
        BlockMarker {
            time: 1,
            command: 2,
            flags: 0,
        }
    }

    #[test]
    fn a_command_error_with_no_output_lines_names_only_the_command() {
        let err = CommandError {
            line: "list-panes".to_string(),
            response: Response {
                begin: marker(),
                end: marker(),
                lines: Vec::new(),
                error: true,
            },
        };
        assert_eq!(err.to_string(), r#"tmux: command "list-panes" failed"#);
    }

    #[test]
    fn a_command_error_with_output_lines_joins_them_after_a_colon() {
        let err = CommandError {
            line: "bogus".to_string(),
            response: Response {
                begin: marker(),
                end: marker(),
                lines: vec!["unknown command".to_string(), "near bogus".to_string()],
                error: true,
            },
        };
        assert_eq!(
            err.to_string(),
            "tmux: command \"bogus\" failed: unknown command\nnear bogus"
        );
    }

    #[test]
    fn a_protocol_error_without_a_line_omits_the_on_clause() {
        let err = ProtocolError {
            line: None,
            message: "unexpected EOF after %begin for command 2".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "tmux: protocol error: unexpected EOF after %begin for command 2"
        );
    }

    #[test]
    fn a_protocol_error_with_a_line_names_it() {
        let err = ProtocolError {
            line: Some("garbage".to_string()),
            message: "unexpected non-control line outside response block".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "tmux: protocol error on \"garbage\": unexpected non-control line outside response block"
        );
    }

    #[test]
    fn an_exit_error_with_no_reason_stays_terse() {
        let err = Error::Exit {
            reason: String::new(),
        };
        assert_eq!(err.to_string(), "tmux: control client exited");
    }

    #[test]
    fn an_exit_error_with_a_reason_names_it() {
        let err = Error::Exit {
            reason: "detached".to_string(),
        };
        assert_eq!(err.to_string(), "tmux: control client exited: detached");
    }

    #[test]
    fn a_command_error_converts_into_the_top_level_error_by_from() {
        let source = CommandError {
            line: "x".to_string(),
            response: Response {
                begin: marker(),
                end: marker(),
                lines: Vec::new(),
                error: true,
            },
        };
        let err: Error = source.clone().into();
        assert_eq!(err.to_string(), source.to_string());
    }

    #[test]
    fn a_protocol_error_converts_into_the_top_level_error_by_from() {
        let source = ProtocolError {
            line: None,
            message: "bad".to_string(),
        };
        let err: Error = source.clone().into();
        assert_eq!(err.to_string(), source.to_string());
    }
}
