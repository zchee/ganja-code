//! Spec: pandaemonium pkg/tmux/commandline.go.
//!
//! A [`Command`] plus its [`Arg`]s becomes one newline-free tmux command
//! line by [`CommandLine::render`]. The quoting ladder — bare charset as-is,
//! an empty argument as `''`, no embedded single quote as `'...'`,
//! otherwise a double-quoted string escaping `\`, `"`, and `$` — is
//! byte-for-byte the Go original's `quoteArg`.
//!
//! **Divergence**: Go validates every command token and argument as valid
//! UTF-8 (`utf8.ValidString`) before rendering, and `TestCommandLineInvalidUTF8`
//! exercises both rejections. A Rust `&str`/`String` is already a valid
//! UTF-8 sequence by construction, so that input — and the check that would
//! reject it — has no Rust counterpart; the type system makes it
//! unrepresentable rather than rejecting it at runtime (AC-4 waiver).

use std::borrow::Cow;

/// A canonical tmux command token, such as `display-message`.
///
/// Spec: pandaemonium pkg/tmux/commandline.go (`Command`). Command
/// constants for common control-client operations (`detach-client`,
/// `display-message`, …) are declared in the `flow` module, mirroring Go's
/// `flow.go`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command(Cow<'static, str>);

impl Command {
    /// Builds a [`Command`] from a `&'static str` known at compile time,
    /// avoiding an allocation — how a command constant is declared.
    pub const fn from_static(value: &'static str) -> Command {
        Command(Cow::Borrowed(value))
    }

    /// Returns the command token as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Command {
    fn from(value: String) -> Command {
        Command(Cow::Owned(value))
    }
}

impl From<&str> for Command {
    fn from(value: &str) -> Command {
        Command(Cow::Owned(value.to_owned()))
    }
}

/// One tmux command argument: either a value quoted for safety, or a
/// trusted raw syntax fragment.
///
/// Spec: pandaemonium pkg/tmux/commandline.go (`Arg`, `StringArg`,
/// `RawArg`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    value: String,
    raw: bool,
}

impl Arg {
    /// Builds a normal argument, rendered with tmux-safe quoting.
    ///
    /// Spec: pandaemonium pkg/tmux/commandline.go (`StringArg`).
    pub fn string(value: impl Into<String>) -> Arg {
        Arg { value: value.into(), raw: false }
    }

    /// Builds an explicit raw tmux syntax fragment, passed through
    /// unquoted (e.g. a flag like `-p`, or a syntax word like `Enter`).
    ///
    /// Spec: pandaemonium pkg/tmux/commandline.go (`RawArg`).
    pub fn raw(value: impl Into<String>) -> Arg {
        Arg { value: value.into(), raw: true }
    }
}

/// One rendered tmux command plus its arguments.
///
/// Spec: pandaemonium pkg/tmux/commandline.go (`CommandLine`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    command: Command,
    args: Vec<Arg>,
}

impl CommandLine {
    /// Builds a command line from a command and its arguments.
    ///
    /// Spec: pandaemonium pkg/tmux/commandline.go (`NewCommandLine`).
    pub fn new(command: Command, args: impl IntoIterator<Item = Arg>) -> CommandLine {
        CommandLine { command, args: args.into_iter().collect() }
    }

    /// Renders `self` as one newline-free tmux command line.
    ///
    /// Spec: pandaemonium pkg/tmux/commandline.go (`CommandLine.String`).
    pub fn render(&self) -> Result<String, RenderError> {
        validate_command_token(self.command.as_str())?;
        let mut rendered = String::from(self.command.as_str());
        for (index, arg) in self.args.iter().enumerate() {
            let arg = render_arg(arg)
                .map_err(|source| RenderError::new(format!("argument {index}: {source}")))?;
            rendered.push(' ');
            rendered.push_str(&arg);
        }
        Ok(rendered)
    }
}

/// An error rendering a [`CommandLine`] or validating a raw command line.
///
/// Spec: pandaemonium pkg/tmux/commandline.go (the `fmt.Errorf` sites in
/// `validateCommandToken`, `renderArg`, `validateArgument`, and
/// `validateRawLine`). Like the Go original's plain `error`, this carries a
/// message only — callers distinguish cases by matching the text, not a
/// variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RenderError(String);

impl RenderError {
    // Refresh-client validation in flow.rs uses this to surface Error::InvalidCommand.
    pub(crate) fn new(message: impl Into<String>) -> RenderError {
        RenderError(message.into())
    }
}

fn validate_command_token(command: &str) -> Result<(), RenderError> {
    if command.is_empty() {
        return Err(RenderError::new("tmux: command must not be empty"));
    }
    if command.trim() != command || command.contains([' ', '\t', '\r', '\n']) {
        return Err(RenderError::new(format!("tmux: command {command:?} must be a plain token")));
    }
    Ok(())
}

fn render_arg(arg: &Arg) -> Result<String, RenderError> {
    validate_argument(&arg.value)?;
    if arg.raw {
        if arg.value.trim().is_empty() {
            return Err(RenderError::new("tmux: raw argument must not be empty"));
        }
        return Ok(arg.value.clone());
    }
    Ok(quote_arg(&arg.value))
}

fn validate_argument(value: &str) -> Result<(), RenderError> {
    if value.contains(['\r', '\n']) {
        return Err(RenderError::new(format!("tmux: argument {value:?} contains a newline")));
    }
    Ok(())
}

fn quote_arg(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    if is_bare_arg(value) {
        return value.to_owned();
    }
    if !value.contains('\'') {
        return format!("'{value}'");
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        if matches!(ch, '\\' | '"' | '$') {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

fn is_bare_arg(value: &str) -> bool {
    value.chars().all(|ch| {
        ch.is_ascii_alphanumeric()
            || matches!(ch, '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-')
    })
}

/// Validates a raw tmux command line: non-blank and free of embedded
/// newlines.
///
/// Spec: pandaemonium pkg/tmux/commandline.go (`validateRawLine`); used by
/// [`crate::control_mode::Client::exec_raw`].
pub(crate) fn validate_raw_line(line: &str) -> Result<(), RenderError> {
    if line.trim().is_empty() {
        return Err(RenderError::new("tmux: command line must not be empty"));
    }
    if line.contains(['\r', '\n']) {
        return Err(RenderError::new("tmux: command line contains a newline"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "commandline_tests.rs"]
mod tests;
