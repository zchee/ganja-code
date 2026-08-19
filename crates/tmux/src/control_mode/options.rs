//! Spec: pandaemonium `pkg/tmux/options.go`.
//!
//! [`Options`] configures a [`crate::control_mode::Client`] before it starts:
//! the tmux executable and socket selection, the subprocess environment,
//! which session the client attaches to or creates, and the three tunables
//! ([`Options::event_buffer`], [`Options::stderr_line_limit`],
//! [`Options::shutdown_timeout`]) that bound a running client's resource
//! use. The zero value is not startable by itself, for the same reason as
//! Go's: starting tmux with neither an explicit session nor an explicit
//! command could silently attach to the caller's default server.
//!
//! # Functional options become a consuming builder (divergence)
//!
//! Go's `Option func(*Options)` closures compose through `applyOptions`,
//! validated once at the end. This port instead exposes [`Options::new`]
//! plus chained `with_*` methods that take and return `Self` by value —
//! this workspace's own builder idiom (see `ganja-core::engine::Engine`'s
//! `with_*` methods) — with validation happening once, explicitly, in
//! [`crate::control_mode::Client::new`] rather than folded into construction.
//! `Options` itself stays a plain public struct rather than growing a
//! private constructor, so a caller can also build one with a struct literal
//! plus `..Options::new()` when a chain reads worse than a literal.
//!
//! # `Env` becomes typed pairs (divergence)
//!
//! Go's `Env []string` holds raw `KEY=VALUE` entries, validated by
//! `strings.Contains(entry, "=")` in [`Options::validate`]'s Go original.
//! This port's [`Options::env`] is `Vec<(OsString, OsString)>`: the shape
//! itself rules out a missing `=`, so that whole validation rule has no
//! Rust counterpart — the type system, not a runtime check, is what
//! guarantees every entry is a well-formed key/value pair (AC-4 waiver).
//! `OsString` (not `String`) because this is exactly what
//! [`tokio::process::Command::envs`] accepts, and an environment value is
//! not guaranteed to be valid UTF-8 on every platform tmux runs on.
//!
//! # `stderr_line_limit` drops a runtime check too (divergence)
//!
//! The same shape of divergence recurs for [`Options::stderr_line_limit`]:
//! Go's `StderrLineLimit int` carries a runtime `>= 0` check in
//! `Options.validate`, with its own `TestOptionsValidation` subtest. This
//! port's field is `usize`, so a negative value is unrepresentable and that
//! whole check has no Rust counterpart (AC-4 waiver).

use std::{ffi::OsString, path::PathBuf, time::Duration};

use crate::error::Error;

const DEFAULT_EVENT_BUFFER: usize = 128;
const DEFAULT_STDERR_LINE_LIMIT: usize = 100;
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Configures a [`crate::control_mode::Client`] before it starts.
///
/// See the module doc for how this differs from Go's functional-options
/// `Option`/`applyOptions` pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    path: Option<PathBuf>,
    socket_name: Option<String>,
    socket_path: Option<PathBuf>,
    config_file: Option<PathBuf>,
    dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
    session_name: Option<String>,
    create_session: bool,
    initial_command: Vec<String>,
    event_buffer: usize,
    stderr_line_limit: usize,
    shutdown_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self::new()
    }
}

impl Options {
    /// Builds an [`Options`] with the package defaults: a 128-entry event
    /// buffer, a 100-line stderr tail, and a five-second shutdown timeout.
    ///
    /// The result is not yet startable — [`crate::control_mode::Client::new`]
    /// refuses it until [`Options::with_initial_command`] or
    /// [`Options::with_session_name`] has been set. See the struct doc.
    #[must_use]
    pub fn new() -> Self {
        Self {
            path: None,
            socket_name: None,
            socket_path: None,
            config_file: None,
            dir: None,
            env: Vec::new(),
            session_name: None,
            create_session: false,
            initial_command: Vec::new(),
            event_buffer: DEFAULT_EVENT_BUFFER,
            stderr_line_limit: DEFAULT_STDERR_LINE_LIMIT,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }

    /// Sets the tmux executable path. Unset resolves `tmux` on `PATH` at
    /// spawn time.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Sets the tmux `-L` socket name.
    #[must_use]
    pub fn with_socket_name(mut self, name: impl Into<String>) -> Self {
        self.socket_name = Some(name.into());
        self
    }

    /// Sets the tmux `-S` socket path.
    #[must_use]
    pub fn with_socket_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket_path = Some(path.into());
        self
    }

    /// Sets the tmux `-f` config file path.
    #[must_use]
    pub fn with_config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = Some(path.into());
        self
    }

    /// Sets the subprocess working directory. Unset inherits the current
    /// process directory.
    #[must_use]
    pub fn with_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    /// Appends `(KEY, VALUE)` environment entries to the subprocess
    /// environment. See the module doc for why this is typed pairs rather
    /// than Go's raw `KEY=VALUE` strings.
    #[must_use]
    pub fn with_env(
        mut self,
        entries: impl IntoIterator<Item = (impl Into<OsString>, impl Into<OsString>)>,
    ) -> Self {
        self.env
            .extend(entries.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Sets the session targeted by the default attach/create initial
    /// command.
    #[must_use]
    pub fn with_session_name(mut self, name: impl Into<String>) -> Self {
        self.session_name = Some(name.into());
        self
    }

    /// Makes the default initial command create or attach the configured
    /// session with `new-session -A -s`, rather than the default
    /// `attach-session -t`.
    #[must_use]
    pub fn with_create_session(mut self, create: bool) -> Self {
        self.create_session = create;
        self
    }

    /// Sets the argv elements placed after `tmux -C`, overriding
    /// [`Options::with_session_name`]/[`Options::with_create_session`].
    #[must_use]
    pub fn with_initial_command(
        mut self,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.initial_command = args.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the asynchronous notification buffer size.
    #[must_use]
    pub fn with_event_buffer(mut self, n: usize) -> Self {
        self.event_buffer = n;
        self
    }

    /// Sets the retained stderr line count.
    #[must_use]
    pub fn with_stderr_line_limit(mut self, n: usize) -> Self {
        self.stderr_line_limit = n;
        self
    }

    /// Sets the graceful close timeout.
    #[must_use]
    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// The tmux executable path, when explicitly set.
    #[must_use]
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// The subprocess working directory, when explicitly set.
    #[must_use]
    pub fn dir(&self) -> Option<&std::path::Path> {
        self.dir.as_deref()
    }

    /// The environment entries appended to the subprocess environment.
    #[must_use]
    pub fn env(&self) -> &[(OsString, OsString)] {
        &self.env
    }

    /// The asynchronous notification buffer size.
    #[must_use]
    pub fn event_buffer(&self) -> usize {
        self.event_buffer
    }

    /// The maximum number of stderr lines retained for diagnostics.
    #[must_use]
    pub fn stderr_line_limit(&self) -> usize {
        self.stderr_line_limit
    }

    /// The graceful close timeout.
    #[must_use]
    pub fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
    }

    /// Validates the configured combination.
    ///
    /// Ports Go's `Options.validate`, minus the `Env` `KEY=VALUE` check the
    /// module doc explains is unrepresentable here.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOptions`] naming the first violated rule:
    /// `socket_name`/`socket_path` are mutually exclusive; `event_buffer`
    /// must be positive; `shutdown_timeout` must be positive;
    /// `initial_command`/`session_name` are mutually exclusive and exactly
    /// one is required; and neither may contain a carriage return or
    /// newline.
    pub fn validate(&self) -> Result<(), Error> {
        if self.socket_name.is_some() && self.socket_path.is_some() {
            return Err(invalid(
                "socket_name and socket_path are mutually exclusive",
            ));
        }
        if self.event_buffer == 0 {
            return Err(invalid("event_buffer must be > 0"));
        }
        if self.shutdown_timeout.is_zero() {
            return Err(invalid("shutdown_timeout must be > 0"));
        }
        if self.initial_command.is_empty() && self.session_name.is_none() {
            return Err(invalid(
                "initial_command or session_name is required to avoid implicit default-server attach",
            ));
        }
        if !self.initial_command.is_empty() && self.session_name.is_some() {
            return Err(invalid(
                "initial_command and session_name are mutually exclusive",
            ));
        }
        for arg in &self.initial_command {
            if arg.contains(['\r', '\n']) {
                return Err(invalid(format!(
                    "initial_command argument {arg:?} contains a newline"
                )));
            }
        }
        if let Some(name) = &self.session_name
            && name.contains(['\r', '\n'])
        {
            return Err(invalid("session_name contains a newline"));
        }
        Ok(())
    }

    /// Renders the argv elements passed to the tmux executable, following
    /// `-L`/`-S`/`-f` with `-C` and either the explicit initial command or
    /// the default attach/create command.
    ///
    /// Ports Go's `Options.launchArgs`.
    #[must_use]
    pub fn launch_args(&self) -> Vec<String> {
        let mut args = Vec::with_capacity(10 + self.initial_command.len());
        if let Some(name) = &self.socket_name {
            args.push("-L".to_string());
            args.push(name.clone());
        }
        if let Some(path) = &self.socket_path {
            args.push("-S".to_string());
            args.push(path.display().to_string());
        }
        if let Some(path) = &self.config_file {
            args.push("-f".to_string());
            args.push(path.display().to_string());
        }
        args.push("-C".to_string());
        if !self.initial_command.is_empty() {
            args.extend(self.initial_command.iter().cloned());
            return args;
        }
        let session = self.session_name.clone().unwrap_or_default();
        if self.create_session {
            args.extend(["new-session", "-A", "-s"].into_iter().map(str::to_string));
        } else {
            args.extend(["attach-session", "-t"].into_iter().map(str::to_string));
        }
        args.push(session);
        args
    }

    /// Renders the command line the client registers as its own startup
    /// response — pending command #0, answered by the handshake.
    ///
    /// Ports Go's `Options.initialCommandLine`.
    #[must_use]
    pub fn initial_command_line(&self) -> String {
        if !self.initial_command.is_empty() {
            return self.initial_command.join(" ");
        }
        let session = self.session_name.as_deref().unwrap_or_default();
        if self.create_session {
            format!("new-session -A -s {session}")
        } else {
            format!("attach-session -t {session}")
        }
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidOptions {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC-4 note: Go's "environment entries must be KEY=VALUE" case
    // (`TestOptionsValidation`) has no counterpart — see the module doc's
    // `Env` divergence. Go's `TestOptionsCloneEnv` (defensive-copy proof for
    // a `[]string` field) also has no counterpart: `Options` here has no
    // `clone_env` accessor to defend — `Options: Clone` already makes a
    // caller's copy independent, and `env()` returns a borrowed slice a
    // caller cannot mutate through at all. Go's "stderr limit must be
    // non-negative" case (also `TestOptionsValidation`) has no counterpart
    // either — see the module doc's `stderr_line_limit` divergence: `usize`
    // makes a negative value unrepresentable.

    fn valid() -> Options {
        Options::new().with_session_name("safe")
    }

    #[test]
    fn an_explicit_initial_command_is_valid() {
        Options::new()
            .with_initial_command(["new-session", "-A", "-s", "safe"])
            .validate()
            .unwrap();
    }

    #[test]
    fn an_explicit_session_target_is_valid() {
        valid().validate().unwrap();
    }

    #[test]
    fn an_implicit_default_attach_is_rejected() {
        let err = Options::new().validate().unwrap_err();
        assert!(err.to_string().contains("initial_command or session_name"));
    }

    #[test]
    fn socket_name_and_path_conflict_is_rejected() {
        let err = Options::new()
            .with_socket_name("a")
            .with_socket_path("/tmp/a")
            .with_session_name("safe")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn a_zero_event_buffer_is_rejected() {
        let err = valid().with_event_buffer(0).validate().unwrap_err();
        assert!(err.to_string().contains("event_buffer must be > 0"));
    }

    #[test]
    fn a_zero_shutdown_timeout_is_rejected() {
        let err = valid()
            .with_shutdown_timeout(Duration::ZERO)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("shutdown_timeout must be > 0"));
    }

    #[test]
    fn an_initial_command_argument_with_a_newline_is_rejected() {
        let err = Options::new()
            .with_initial_command(["new-session\n"])
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("contains a newline"));
    }

    #[test]
    fn a_session_name_with_a_newline_is_rejected() {
        let err = Options::new()
            .with_session_name("bad\n")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("session_name contains a newline"));
    }

    #[test]
    fn initial_command_and_session_name_conflict_is_rejected() {
        let err = Options::new()
            .with_initial_command(["new-session", "-A", "-s", "safe"])
            .with_session_name("safe")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn launch_args_attach_explicit_session() {
        let opts = Options::new().with_session_name("safe");
        assert_eq!(
            opts.launch_args(),
            vec!["-C", "attach-session", "-t", "safe"]
        );
    }

    #[test]
    fn launch_args_create_explicit_session() {
        let opts = Options::new()
            .with_session_name("safe")
            .with_create_session(true);
        assert_eq!(
            opts.launch_args(),
            vec!["-C", "new-session", "-A", "-s", "safe"]
        );
    }

    #[test]
    fn launch_args_socket_name_and_config() {
        let opts = Options::new()
            .with_socket_name("sock")
            .with_config_file("/dev/null")
            .with_session_name("safe");
        assert_eq!(
            opts.launch_args(),
            vec![
                "-L",
                "sock",
                "-f",
                "/dev/null",
                "-C",
                "attach-session",
                "-t",
                "safe"
            ]
        );
    }

    #[test]
    fn launch_args_socket_path_and_initial_command() {
        let opts = Options::new()
            .with_socket_path("/tmp/tmux.sock")
            .with_initial_command(["new-session", "-A", "-s", "safe"]);
        assert_eq!(
            opts.launch_args(),
            vec![
                "-S",
                "/tmp/tmux.sock",
                "-C",
                "new-session",
                "-A",
                "-s",
                "safe"
            ]
        );
    }

    #[test]
    fn initial_command_line_renders_the_explicit_command() {
        let opts = Options::new().with_initial_command(["new-session", "-A", "-s", "test"]);
        assert_eq!(opts.initial_command_line(), "new-session -A -s test");
    }

    #[test]
    fn initial_command_line_renders_the_default_attach() {
        let opts = Options::new().with_session_name("test");
        assert_eq!(opts.initial_command_line(), "attach-session -t test");
    }

    #[test]
    fn initial_command_line_renders_the_default_create() {
        let opts = Options::new()
            .with_session_name("test")
            .with_create_session(true);
        assert_eq!(opts.initial_command_line(), "new-session -A -s test");
    }
}
