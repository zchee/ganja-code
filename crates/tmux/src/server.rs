//! A tmux server, addressed one plain client invocation at a time: this
//! crate's second transport.
//!
//! Synthesized, not ported — the Go specification under
//! [`crate::control_mode`] speaks only the persistent protocol, so by the
//! convention that module's doc states, this module carries no `Spec:` line.
//! (The crate root's other two modules do: [`crate::ids`] and
//! [`crate::error`] were hoisted out of the port because both transports
//! need them, and they name the Go file they came from.)
//!
//! [`crate::control_mode::Client`] is one `tmux -C` process kept alive and
//! spoken to; a [`Server`] is the other half of how tmux is used — the plain
//! `tmux <command>` invocation, run to completion, that every shell and
//! script already speaks. It owns no subprocess between calls, keeps no
//! state, and has nothing to close.
//!
//! # `$TMUX` is read the way the tmux client reads it
//!
//! tmux exports `socket,pid,session-index` into every process it starts.
//! Only the first field addresses a server, and the split is made over raw
//! bytes on unix rather than over text: a socket is a path, and a path is not
//! obliged to be UTF-8.
//!
//! # What an absent `$TMUX` means here, and what it does not
//!
//! [`Error::NotInTmux`], and nothing beyond it. Whether that fact is a
//! sentence a person should read, a reason to start a private server, or a
//! reason to do something that is not tmux at all, is the consumer's
//! judgment — this crate ships the fact and keeps the policy out.
//!
//! # argv is execve, not protocol text
//!
//! A control-mode command travels as one *line* down a pipe, so
//! [`crate::control_mode`] must decide where each word ends: that is what its
//! bare/single/double-quote ladder is for. An invocation has no line. Its
//! words are handed to the kernel as separate arguments, and quoting one of
//! them would put the quotes *inside* the argument tmux reads. This module
//! therefore takes [`OsString`] words and passes them through byte for byte,
//! and imports nothing from [`crate::control_mode`] — least of all its
//! renderer. The two transports share vocabulary at the crate root
//! ([`crate::ids`], [`crate::error`]) and nothing else.
//!
//! # The socket is pinned, and stdin is closed
//!
//! Every call carries `-S <socket>`, so a client cannot wander off to the
//! default socket if the environment changes under a long-lived [`Server`]
//! value. Stdin is `/dev/null` because no call here is interactive, and the
//! child is `kill_on_drop`, so a dropped future leaves no client behind.
//!
//! Two things are **inherited, not scrubbed**: the child gets this process's
//! environment and working directory. The non-obvious half is what tmux then
//! does with the first — a server started by one of these clients copies the
//! client's environment into the server's *global* environment, so every
//! pane anybody creates on that server afterwards sees it, for the server's
//! whole life (measured; the Phase-4 security review's finding 2). Which
//! variables may travel is policy, and policy stays the consumer's: a caller
//! that must not leak a credential enumerates an environment itself before
//! calling, the way this workspace's D502 list does.
//!
//! ```no_run
//! # async fn run() -> Result<(), tmux::Error> {
//! use tmux::Server;
//!
//! let server = Server::current()?;
//! let captured = server
//!     .run(["display-message", "-p", "#{session_name}"])
//!     .await?;
//! println!("{}", captured.text_lossy().trim());
//! # Ok(())
//! # }
//! ```

use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use crate::error::Error;

/// The variable tmux exports into every process it runs: the server's
/// socket, its pid and the session index, comma-separated.
pub const TMUX: &str = "TMUX";

/// The variable tmux exports naming the pane a process runs in.
pub const TMUX_PANE: &str = "TMUX_PANE";

/// The client binary, resolved on `PATH`.
///
/// Not configurable, deliberately: a call goes to the tmux this machine
/// runs, and a build that let a caller name a different one would be
/// answering a question nobody has asked yet.
pub const BINARY: &str = "tmux";

/// A tmux server, and the pane a call against it was asked from.
///
/// One value rather than a global, so a test — or a consumer driving a
/// private server it started itself — can address a socket without touching
/// the process environment ([`Server::at`]), while a process running inside
/// tmux reads its own ([`Server::current`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Server {
    socket: PathBuf,
    pane: Option<String>,
}

impl Server {
    /// The server this process is running under, off [`TMUX`] and
    /// [`TMUX_PANE`].
    ///
    /// The socket is the value up to the first comma. The pane is
    /// [`TMUX_PANE`] when tmux set it, and its absence is not a failure: a
    /// call that names no pane target goes to the server's current one,
    /// which is all a caller ever wanted from a session with one window.
    ///
    /// The environment is read at every call rather than once, because a
    /// cached answer would be a wrong answer for a process whose tmux
    /// changed under it — and the read is two `getenv`s.
    ///
    /// # Errors
    ///
    /// [`Error::NotInTmux`] when [`TMUX`] is unset, empty, or carries an
    /// empty socket field — three ways of saying the same thing, since none
    /// of them addresses a server.
    pub fn current() -> Result<Self, Error> {
        Self::from_parts(std::env::var_os(TMUX), std::env::var_os(TMUX_PANE))
    }

    /// A server named by its socket, and optionally the pane calls should
    /// treat as the one they came from.
    #[must_use]
    pub fn at(socket: impl Into<PathBuf>, pane: Option<String>) -> Self {
        Self {
            socket: socket.into(),
            pane,
        }
    }

    /// The socket every call against this server is pinned to.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The pane this server value was read from, when there was one.
    #[must_use]
    pub fn pane(&self) -> Option<&str> {
        self.pane.as_deref()
    }

    /// Runs one tmux client invocation against this server, to completion,
    /// and hands back what it printed.
    ///
    /// `args` are the client's own words after the pinned socket — a
    /// subcommand and its flags for a plain call, optionally preceded by
    /// client flags such as `-f` for one pinning its own config. They are
    /// passed through unaltered: no shell, no quoting, no interpretation.
    ///
    /// # Errors
    ///
    /// [`Error::ClientStart`] when the client could not be run at all, and
    /// [`Error::ClientRefused`] when it ran and exited non-zero, carrying
    /// tmux's own stderr. A call that succeeds having printed nothing is an
    /// empty [`Captured`] and not an error: plenty of tmux commands answer
    /// with silence.
    ///
    /// # There is no timeout
    ///
    /// A command that blocks blocks this future, for as long as it blocks —
    /// `wait-for` is the command written to do exactly that, and it is the
    /// canonical case. Dropping the future kills the client, because the
    /// child is spawned `kill_on_drop`, so a caller who wants a deadline
    /// wraps this call in one of their own and drops it. No deadline is
    /// imposed here for the reason no refusal sentence is written here: how
    /// long a caller is willing to wait is the caller's judgment.
    pub async fn run<I, S>(&self, args: I) -> Result<Captured, Error>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        // Message text, not a parse: the leading word is the subcommand for a
        // plain call and a client flag for one that pins its own config, and
        // an error naming either is more use than one naming neither.
        let command = args.first().map(|word| word.to_string_lossy().into_owned());
        let argv = self.argv(args);
        let (program, arguments) = argv
            .split_first()
            .expect("an argv built here always begins with the client binary");

        let mut client = tokio::process::Command::new(program);
        client
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = client.output().await.map_err(|source| Error::ClientStart {
            command: command.clone(),
            source: Arc::new(source),
        })?;
        if !output.status.success() {
            return Err(Error::ClientRefused {
                command,
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }

        Ok(Captured {
            bytes: output.stdout,
        })
    }

    /// The exact argv [`Server::run`] executes: the client, the `-S` pin,
    /// then the caller's own words in the order they arrived.
    ///
    /// `pub(crate)` because it is a seam rather than a surface — it lets a
    /// process-free test assert what a call *would* execute, including that
    /// a word outside UTF-8 survives the trip, without executing one.
    pub(crate) fn argv<I, S>(&self, args: I) -> Vec<OsString>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut argv = vec![
            OsString::from(BINARY),
            OsString::from("-S"),
            self.socket.as_os_str().to_owned(),
        ];
        argv.extend(args.into_iter().map(Into::into));

        argv
    }

    /// The environment-reading half of [`Server::current`], with the two
    /// reads passed in so every branch below is testable without a
    /// process-wide mutation.
    ///
    /// A [`TMUX_PANE`] that is not text is treated as absent rather than
    /// repaired: tmux spells a pane `%N`, so bytes outside UTF-8 are not a
    /// damaged pane target, they are not a pane target at all.
    fn from_parts(tmux: Option<OsString>, pane: Option<OsString>) -> Result<Self, Error> {
        let raw = tmux
            .filter(|value| !value.is_empty())
            .ok_or(Error::NotInTmux)?;
        let socket = socket_of(&raw);
        if socket.as_os_str().is_empty() {
            return Err(Error::NotInTmux);
        }

        Ok(Self {
            socket,
            pane: pane
                .and_then(|value| value.into_string().ok())
                .filter(|value| !value.is_empty()),
        })
    }
}

/// The socket field of a `$TMUX` value: everything before the first comma,
/// which is how the tmux client itself reads it.
fn socket_of(raw: &OsStr) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let bytes = raw.as_bytes();
        let end = bytes
            .iter()
            .position(|byte| *byte == b',')
            .unwrap_or(bytes.len());

        PathBuf::from(OsString::from_vec(bytes[..end].to_vec()))
    }
    #[cfg(not(unix))]
    {
        // There is no `$TMUX` to read on a platform tmux does not run on;
        // the lossy conversion keeps the module compiling rather than
        // promising anything about such a value.
        let text = raw.to_string_lossy();
        let end = text.find(',').unwrap_or(text.len());

        PathBuf::from(&text[..end])
    }
}

/// What one client invocation printed on standard output.
///
/// Bytes, because tmux prints whatever a format string asked it for and a
/// pane title is not obliged to be UTF-8. Named for the capture rather than
/// called an `Output`, because in this crate that word already means a
/// pane's `%output` ([`crate::control_mode::decode_output_value`]) — a
/// different thing entirely. The strict/lossy pair mirrors the one that
/// module uses: [`Captured::text`] refuses what is not text,
/// [`Captured::text_lossy`] repairs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captured {
    bytes: Vec<u8>,
}

impl Captured {
    /// What tmux printed, exactly as it arrived.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// What tmux printed, taken by value.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// The capture as text.
    ///
    /// # Errors
    ///
    /// [`std::str::Utf8Error`] when it is not text, naming where it stopped
    /// being text — which is the caller's cue to read [`Captured::bytes`]
    /// instead.
    pub fn text(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.bytes)
    }

    /// The capture as text, with every invalid sequence repaired to U+FFFD.
    ///
    /// Borrowed when the capture already was text, which is the usual case.
    #[must_use]
    pub fn text_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
