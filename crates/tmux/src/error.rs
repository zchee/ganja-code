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

use crate::control_mode::protocol::Response;

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

/// Why a call against tmux did not produce an answer, on either transport.
///
/// Ports Go's `errors.go` as a whole: the package-level sentinels
/// (`ErrClosed`, `errDetachSkippedWriteLocked`) become unit variants, and
/// `CommandError`/`ProtocolError`/`ExitError` become wrapping variants. The
/// last three variants are the one-shot surface's and have no Go
/// counterpart at all; one enum covers both transports so a caller holding
/// both matches once, as the crate doc says.
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

    /// An [`crate::control_mode::options::Options`] combination failed
    /// validation.
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

    /// Rendering a [`crate::control_mode::CommandLine`] or validating a raw
    /// command line failed.
    ///
    /// Ports the `fmt.Errorf` sites `CommandLine.String` and
    /// `ExecRaw`/`validateRawLine` share in Go, folded here into one
    /// `#[from]` conversion of
    /// [`crate::control_mode::commandline::RenderError`]; added in W2 since
    /// `Client::exec`/`exec_line`/`exec_raw` are this wave's.
    #[error(transparent)]
    InvalidCommand(#[from] crate::control_mode::commandline::RenderError),

    /// [`crate::control_mode::Client::new`]'s initial command did not
    /// complete successfully.
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

    /// A [`crate::Server`] was asked for from outside tmux: `$TMUX` is
    /// unset, empty, or carries an empty socket field, so there is no server
    /// to address.
    ///
    /// Synthesized, with no Go counterpart — the specification launches a
    /// server of its own and never reads that variable. Deliberately a bare
    /// fact and not a sentence to show somebody: what an absent tmux *means*
    /// belongs to the consumer, per [`crate::server`]'s doc.
    #[error("tmux: $TMUX is unset or names no socket")]
    NotInTmux,

    /// A client invocation against a [`crate::Server`] could not be run at
    /// all: no `tmux` on `PATH`, or the subprocess would not start.
    ///
    /// Synthesized, with no Go counterpart. Distinct from [`Error::Spawn`],
    /// which is the same misfortune befalling the persistent `tmux -C`
    /// client: that one names the executable path it resolved, this one
    /// names the word the call led with, because a plain invocation has one
    /// and a control-mode launch does not.
    #[error("tmux: {} could not be run: {source}", client_subject(.command))]
    ClientStart {
        /// The word the call led with, when it had one.
        command: Option<String>,
        /// The underlying I/O failure. `Arc`-wrapped — see the module doc.
        #[source]
        source: Arc<std::io::Error>,
    },

    /// A client invocation against a [`crate::Server`] ran and refused, in
    /// its own words.
    ///
    /// Synthesized, with no Go counterpart: the control-mode transport
    /// learns of a refusal as a `%error`-marked response block
    /// ([`CommandError`]), which is a protocol event and not an exit status.
    #[error("{}", client_refusal(.command, .status, .stderr))]
    ClientRefused {
        /// The word the call led with, when it had one.
        command: Option<String>,
        /// How the client ended.
        status: std::process::ExitStatus,
        /// The client's stderr, trimmed — tmux's own account of the refusal,
        /// carried verbatim rather than re-worded here.
        stderr: String,
    },
}

/// The word a client invocation led with — its subcommand, for a call that
/// pinned no client flags of its own — or a noun for one that led with no
/// word at all, which is legal if unusual.
fn client_subject(command: &Option<String>) -> &str {
    command.as_deref().unwrap_or("the client")
}

fn client_refusal(
    command: &Option<String>,
    status: &std::process::ExitStatus,
    stderr: &str,
) -> String {
    let subject = client_subject(command);
    if stderr.is_empty() {
        // tmux almost always says why; when it says nothing, how it ended is
        // the only fact left to report, and reporting none would be worse.
        format!("tmux: {subject} failed: {status}")
    } else {
        format!("tmux: {subject} failed: {stderr}")
    }
}

fn join_close_errors(errors: &[Error]) -> String {
    errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
