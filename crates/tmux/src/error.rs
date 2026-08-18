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
//!
//! # `Error` is `Clone` (W2 divergence)
//!
//! Go's `error` interface needs no such bound; this port's [`Error`] does,
//! because W2's `Client` stores one abort cause and must hand an owned copy
//! of it to every subsequent caller that observes the client as closed
//! (`closed_error`, read many times, never consumed). [`Error::Io`] and
//! [`Error::Spawn`] therefore wrap their `std::io::Error` in `Arc` rather
//! than owning it directly — `Arc` is `Clone` regardless of whether its
//! payload is, which is what makes `#[derive(Clone)]` on the whole enum
//! possible without hand-rolling a clone that reconstructs an `io::Error`
//! from its `.kind()` and loses the original message.

use std::sync::Arc;

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
/// `#[non_exhaustive]` — see the module doc. `Clone` — see the module doc's
/// `Error is Clone` section.
#[derive(Clone, Debug, thiserror::Error)]
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
        /// The underlying I/O failure. `Arc`-wrapped — see the module doc.
        #[source]
        source: Arc<std::io::Error>,
    },

    /// `close` skipped the best-effort `detach-client` write because
    /// another in-flight call already holds the write lock.
    ///
    /// Ports Go's `errDetachSkippedWriteLocked` sentinel.
    #[error("tmux: detach-client skipped: write lock held")]
    DetachSkippedWriteLocked,

    /// An [`crate::options::Options`] combination failed validation.
    ///
    /// Ports the `fmt.Errorf` sites in Go's `Options.validate`; added in W2
    /// since that type does not exist until this wave.
    #[error("tmux: {message}")]
    InvalidOptions {
        /// The violated rule, in the same wording as the Go original.
        message: String,
    },

    /// Starting the tmux executable failed: it could not be found on
    /// `PATH`, or the subprocess would not spawn.
    ///
    /// Ports the two `fmt.Errorf("tmux: find executable: %w", err)` /
    /// `fmt.Errorf("tmux: start %q: %w", path, err)` sites in Go's `New`;
    /// added in W2 since spawning does not exist until this wave.
    #[error("tmux: start {path:?}: {source}")]
    Spawn {
        /// The resolved (or attempted) executable path.
        path: String,
        /// The underlying I/O failure. `Arc`-wrapped — see the module doc.
        #[source]
        source: Arc<std::io::Error>,
    },

    /// A command was sent while another command's response was still
    /// pending.
    ///
    /// Ports Go's `registerPending`'s `"tmux: another command is already
    /// pending"`; added in W2 since command serialization does not exist
    /// until this wave.
    #[error("tmux: another command is already pending")]
    AlreadyPending,

    /// Rendering a [`crate::CommandLine`] or validating a raw command line
    /// failed.
    ///
    /// Ports the `fmt.Errorf` sites `CommandLine.String` and
    /// `ExecRaw`/`validateRawLine` share in Go, folded here into one
    /// `#[from]` conversion of [`crate::commandline::RenderError`]; added in
    /// W2 since `Client::exec`/`exec_line`/`exec_raw` are this wave's.
    #[error(transparent)]
    InvalidCommand(#[from] crate::commandline::RenderError),

    /// [`crate::Client::new`]'s initial command did not complete
    /// successfully.
    ///
    /// Ports Go's `fmt.Errorf("tmux: initial command: %w", result.err)` in
    /// `New`; added in W2 since startup does not exist until this wave.
    #[error("tmux: initial command: {source}")]
    Startup {
        /// Why the initial command failed.
        #[source]
        source: Box<Error>,
    },

    /// One or more failures occurred while closing the client.
    ///
    /// Ports Go's `Close`'s `errors.Join(errs...)`; added in W2 since
    /// `Client::close` does not exist until this wave. Unlike
    /// `errors.Join`, a single failure still renders through this variant
    /// rather than surfacing bare, which keeps `close`'s return type a
    /// plain `Result<(), Error>` instead of `Result<(), Option<Error>>`.
    #[error("tmux: close: {}", join_close_errors(.errors))]
    Close {
        /// Every failure observed during shutdown, in the order Go's
        /// `Close` would have joined them.
        errors: Vec<Error>,
    },
}

fn join_close_errors(errors: &[Error]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
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
