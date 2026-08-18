//! The tmux calls the two pane backends are built on.
//!
//! Upstream opencode has **no counterpart**; what is ported is Claude Code's
//! §4.1 spawn sequence and §10.2's step-by-step reading of it against this
//! tree. Every call here is one `tmux` client invocation against the server
//! this session already lives in — `split-window` reporting the new pane's
//! identity in one go, the pane-border title, `kill-pane`, and the listing a
//! reaper compares against — and none of them knows what a teammate is. The
//! two backends that do ([`crate::teammate::pane`], [`crate::teammate::claude`])
//! compose a launch and hand it here.
//!
//! # `$TMUX` is a capability, never a selector (**D501**)
//!
//! The backend is an explicit argument on both doors. This variable decides
//! whether the two pane values can *run*, and a session without it refuses them
//! readably rather than quietly spawning an in-process teammate instead:
//! somebody who asked for a window and silently got none has been told
//! something untrue about their own session, and self-hosting a detached tmux
//! server to conjure one is a non-goal of this landing. [`Server::current`](crate::teammate::tmux::Server::current) is
//! that rule as a value: it reads the variable at the moment a spawn asks, and
//! its refusal is [`REFUSED_NO_TMUX`](crate::teammate::tmux::REFUSED_NO_TMUX), the sentence AC-16 asserts.
//!
//! # A pane is identified by a pair, and the second half is its first process
//!
//! **`%N` recycles.** tmux hands a dead pane's id to the next pane it makes, so
//! a lead that killed by id alone would eventually kill somebody's editor. The
//! plan recorded `#{pane_id} #{pane_start_time}` as the format; **tmux has no
//! `pane_start_time`** — checked against `man tmux` (next-3.8: the pane
//! formats are `pane_dead_time`, an *exit* time, `pane_pid`, `pane_start_command`,
//! `pane_start_path`, and no creation time) and live, where the format expands
//! to nothing. This is a deviation from the plan's recorded string, not from
//! its rule: what tmux does report beside the id, at the split and in every
//! listing, is `#{pane_pid}` — the pid of the process it forked into the pane,
//! fixed for the pane's life — so **birth is `pane_pid`**, and D506's reaper
//! contract "(pane_id, birth)" reads that pair. [`PANE_FORMAT`](crate::teammate::tmux::PANE_FORMAT)
//! spells it, [`Server::split`](crate::teammate::tmux::Server::split) reads both halves off one `-P -F` answer,
//! [`Server::panes`](crate::teammate::tmux::Server::panes) reads the same pair off every live pane, and
//! [`Server::kill`](crate::teammate::tmux::Server::kill) ends a pane only when *both* halves match what was
//! recorded. A recycled id wearing a different pid is a stranger's pane and is
//! left alone; for the pair to lie, tmux would have to reissue the id *and* the
//! kernel reissue the pid to the very process it forks into it, on the same
//! server.
//!
//! # What this module needs of tmux
//!
//! Two features, both years old: a `shell-command` given as several arguments
//! is executed **directly** rather than through `sh -c` (tmux 3.0), which is
//! what lets a launch carry a path with a space in it and never meet a shell;
//! and `split-window -e NAME=VALUE` (tmux 3.2), which is how the enumerated
//! environment travels — through tmux's own door rather than an `env` prefix
//! the pane's command line would then display. A server too old for either
//! fails the split, and the failure is the client's own words.
//!
//! The first of those is also a **security fact**, not only a quoting
//! convenience: a *one*-word command goes through the person's login shell
//! (`$SHELL -c`), which sources its startup files before exec'ing, and a
//! `.zshenv` that exports credentials puts them straight back into a pane this
//! module carefully did not give them to (measured, 2026-08-17). So every
//! `argv` handed to [`Server::split`](crate::teammate::tmux::Server::split)
//! must be at least two words — [`crate::teammate::pane::SHELL`] is two for
//! that reason alone.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
};

use crate::teammate::reaper::Pane;

/// What a pane spawn says when there is no tmux to put it in.
///
/// The sentence AC-16 asserts — the *sentence*, because the useful half of
/// this answer is that the session, not the build, is what is missing.
pub const REFUSED_NO_TMUX: &str = "there is no tmux session here ($TMUX is \
     unset), and ganja does not start one of its own; run ganja inside tmux, \
     or spawn this teammate in-process";

/// The variable tmux exports into every process it runs: the server's socket,
/// its pid and the session index, comma-separated.
pub const TMUX: &str = "TMUX";

/// The variable tmux exports naming the pane a process runs in, which is the
/// pane a split made from inside it should split.
pub const TMUX_PANE: &str = "TMUX_PANE";

/// The client binary, resolved on `PATH` — it is tmux's, not ours, so the rule
/// that keeps a *ganja* pane off `PATH` (§10.10) does not reach it.
pub const BINARY: &str = "tmux";

/// The two facts a pane is identified by, as one format string.
///
/// Read off `split-window -P -F` when the pane is made and off `list-panes -F`
/// whenever it is looked for again, so the recorded pair and the live pair are
/// spelled by the same code. Why the second half is a pid is in the module doc.
pub const PANE_FORMAT: &str = "#{pane_id} #{pane_pid}";

/// Whether this process is running inside a tmux pane.
///
/// Reads the environment on every call rather than once: a lead started outside
/// tmux and re-attached is not a case this build handles, but caching the
/// answer would make it a case it handles *wrongly* — and the read is one
/// `getenv`.
#[must_use]
pub fn hosted() -> bool {
    std::env::var_os(TMUX).is_some_and(|value| !value.is_empty())
}

/// The named variables of this process's environment, as `NAME=VALUE` pairs a
/// pane can be started with.
///
/// This is the mechanism half of **D502** (minted in [`crate::teammate::pane`]):
/// it takes a **closed list of names** and reads exactly those, so nothing a
/// caller did not spell out can travel — every `*_API_KEY` and
/// `GANJA_SERVER_PASSWORD` in the parent's environment is excluded by never
/// being asked for, not by a pattern that has to keep up with new secrets. A
/// name that is unset in this process is simply absent from the answer, which
/// lets the pane inherit whatever the server has for it.
#[must_use]
pub fn environment<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<OsString> {
    names
        .into_iter()
        .filter_map(|name| {
            std::env::var_os(name).map(|value| {
                let mut pair = OsString::from(name);
                pair.push("=");
                pair.push(value);
                pair
            })
        })
        .collect()
}

/// A pane to be made: where, with what environment, running what.
///
/// Borrowed rather than owned, because a backend composes it once and hands it
/// straight to [`Server::split`]; nothing keeps one.
#[derive(Debug)]
pub struct Launch<'a> {
    /// The pane's working directory (`-c`).
    pub cwd: &'a Path,
    /// `NAME=VALUE` pairs for the pane's environment (`-e`), as
    /// [`environment`] renders them.
    pub environment: &'a [OsString],
    /// The program and its arguments, executed directly — no shell.
    pub argv: &'a [OsString],
}

/// What [`Server::kill`] found when it went to end a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Killed {
    /// The pane was the recorded one, and it is gone now.
    Yes,
    /// No pane by that id exists any more: nothing to do, and not an error —
    /// a teammate that already exited, or a kill asked for twice.
    AlreadyGone,
    /// A pane by that id exists **but it is not the recorded one**: tmux
    /// reissued the id to a stranger's pane, and it was left alone.
    Recycled,
}

/// A tmux call that did not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    /// This process is not inside a tmux session, so there is no server to
    /// ask. The message is exactly [`REFUSED_NO_TMUX`].
    #[error("{REFUSED_NO_TMUX}")]
    NotHosted,
    /// The client could not be started at all.
    #[error("tmux {command} could not be run: {source}")]
    Start {
        /// Which tmux command.
        command: &'static str,
        /// What the OS said.
        source: std::io::Error,
    },
    /// The client ran and refused, in its own words.
    #[error("tmux {command} failed: {stderr}")]
    Failed {
        /// Which tmux command.
        command: &'static str,
        /// The client's stderr, trimmed.
        stderr: String,
    },
    /// The client answered something [`PANE_FORMAT`] does not describe.
    #[error("tmux {command} answered something this build cannot read: {output:?}")]
    Unreadable {
        /// Which tmux command.
        command: &'static str,
        /// What it printed.
        output: String,
    },
}

/// The tmux server this session runs in, and the pane it runs in.
///
/// One value per session rather than a global, so a test can point one at a
/// private server ([`Server::at`]) without touching the process environment,
/// while production reads it off `$TMUX` at every spawn ([`Server::current`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    /// The server's socket, passed to every call as `-S` so the client cannot
    /// wander to a default socket if the environment changes under it.
    socket: PathBuf,
    /// The pane this process runs in, when tmux said: what a split splits.
    pane: Option<String>,
}

impl Server {
    /// The server this process is running under, off `$TMUX`.
    ///
    /// The socket is the value up to the first comma, which is exactly how the
    /// tmux client itself reads the variable; the pane is `$TMUX_PANE` when it
    /// is set, and its absence is not an error — a split without a target
    /// pane goes to the server's current one, which is the only pane a session
    /// with one window has.
    ///
    /// # Errors
    ///
    /// [`TmuxError::NotHosted`] when the variable is unset or empty: the D501
    /// refusal, in the sentence AC-16 asserts.
    pub fn current() -> Result<Self, TmuxError> {
        let raw = std::env::var_os(TMUX).filter(|value| !value.is_empty());
        let Some(raw) = raw else {
            return Err(TmuxError::NotHosted);
        };
        let socket = socket_of(&raw);
        if socket.as_os_str().is_empty() {
            return Err(TmuxError::NotHosted);
        }
        let pane = std::env::var(TMUX_PANE)
            .ok()
            .filter(|value| !value.is_empty());

        Ok(Self { socket, pane })
    }

    /// A server named by its socket, and optionally the pane to split from.
    ///
    /// For a private server a test started with `tmux -S <socket>`; production
    /// goes through [`Server::current`].
    #[must_use]
    pub fn at(socket: impl Into<PathBuf>, pane: Option<String>) -> Self {
        Self {
            socket: socket.into(),
            pane,
        }
    }

    /// The socket every call goes to.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Splits a new pane off this one and returns what identifies it.
    ///
    /// One call carries the whole §4.1 step 1: the working directory, the
    /// enumerated environment, the program and its arguments, and — via
    /// `-P -F` — the pane's id **and** its first process's pid printed
    /// together, because a second call to fetch the pid would already be
    /// racing whatever recycled the id. `-d` keeps the person's focus in the
    /// pane they were in: a teammate's window opening is not a reason to move
    /// their cursor into it.
    ///
    /// # Errors
    ///
    /// The client failing to start, tmux refusing (the pane target gone, a
    /// server too old for `-e`, a program that does not exist — tmux
    /// reports the last as a pane that dies at once rather than an error
    /// here), or an answer that is not [`PANE_FORMAT`].
    pub async fn split(&self, launch: Launch<'_>) -> Result<Pane, TmuxError> {
        // The module doc's security fact, as something a build can trip over:
        // a one-word command is handed to the person's **login shell**, which
        // sources its startup files first, so a `.zshenv` that exports a
        // credential puts back exactly what the enumerated environment (D502)
        // withheld. A debug assertion rather than a refusal because every
        // caller in this tree composes its argv from a constant — this is here
        // to fail the suite the day one stops, not to answer at run time for a
        // mistake the type system cannot catch.
        debug_assert!(
            launch.argv.len() >= 2,
            "a one-word pane command is re-read by the person's login shell, which would \
             re-import the credentials D502 withheld: {:?}",
            launch.argv
        );
        let mut command = self.command();
        command
            .arg("split-window")
            .arg("-d")
            .arg("-P")
            .arg("-F")
            .arg(PANE_FORMAT)
            .arg("-c")
            .arg(launch.cwd);
        if let Some(pane) = &self.pane {
            command.arg("-t").arg(pane);
        }
        for pair in launch.environment {
            command.arg("-e").arg(pair);
        }
        command.arg("--").args(launch.argv);

        let output = run("split-window", command).await?;
        parse_pane(output.trim()).ok_or(TmuxError::Unreadable {
            command: "split-window",
            output,
        })
    }

    /// Titles a pane with its teammate's name and makes the border that shows
    /// titles visible (§4.1 step 3, `enablePaneBorderStatus`).
    ///
    /// Cosmetic, and treated as such by every caller: a failure here is a
    /// pane without a name on it, not a teammate that did not start. The
    /// border status is turned on only when the window has it **off** — read
    /// with what it inherits (`-A`), so a person who put theirs at the bottom
    /// globally keeps it there.
    ///
    /// # Errors
    ///
    /// The client failing to start or tmux refusing.
    pub async fn title(&self, pane_id: &str, title: &str) -> Result<(), TmuxError> {
        let mut select = self.command();
        select
            .arg("select-pane")
            .arg("-t")
            .arg(pane_id)
            .arg("-T")
            .arg(title);
        run("select-pane", select).await?;

        let mut show = self.command();
        show.arg("show-options")
            .arg("-wqvA")
            .arg("-t")
            .arg(pane_id)
            .arg("pane-border-status");
        let current = run("show-options", show).await?;
        if matches!(current.trim(), "" | "off") {
            let mut set = self.command();
            set.arg("set-option")
                .arg("-w")
                .arg("-t")
                .arg(pane_id)
                .arg("pane-border-status")
                .arg("top");
            run("set-option", set).await?;
        }

        Ok(())
    }

    /// Every pane on this server, as the pair a recorded pane is matched on.
    ///
    /// The liveness listing: what a reaper compares the team file's panes
    /// against. All sessions, not only this one — a lead's teammates live
    /// on this server, and which session's window they were split into is
    /// not what identifies them.
    ///
    /// # Errors
    ///
    /// The client failing to start, tmux refusing, or a line that is not
    /// [`PANE_FORMAT`].
    pub async fn panes(&self) -> Result<Vec<Pane>, TmuxError> {
        let mut command = self.command();
        command
            .arg("list-panes")
            .arg("-a")
            .arg("-F")
            .arg(PANE_FORMAT);
        let output = run("list-panes", command).await?;

        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                parse_pane(line.trim()).ok_or_else(|| TmuxError::Unreadable {
                    command: "list-panes",
                    output: line.to_owned(),
                })
            })
            .collect()
    }

    /// Ends `pane` if, and only if, it is still the pane that was recorded.
    ///
    /// The identity check before the kill is the whole reason this takes a
    /// [`Pane`] rather than an id (§10.10, "verify identity before
    /// `kill-pane`, as the job registry already does with process groups"):
    /// the listing is read first, and `kill-pane -t %N` is sent only when the
    /// live pane wearing that id has the recorded birth. Idempotent by
    /// construction — a pane that is already gone is [`Killed::AlreadyGone`],
    /// which is what a `kill` asked twice, or asked after the teammate exited
    /// on its own, should hear.
    ///
    /// # Errors
    ///
    /// The client failing to start or tmux refusing. A `kill-pane` that races
    /// the pane's own exit and finds nothing is reported as
    /// [`Killed::AlreadyGone`], not as an error, by looking again.
    pub async fn kill(&self, pane: &Pane) -> Result<Killed, TmuxError> {
        match self.panes().await?.iter().find(|live| live.id == pane.id) {
            None => return Ok(Killed::AlreadyGone),
            Some(live) if !pane.is(live) => return Ok(Killed::Recycled),
            Some(_) => {}
        }

        let mut command = self.command();
        command.arg("kill-pane").arg("-t").arg(&pane.id);
        match run("kill-pane", command).await {
            Ok(_) => Ok(Killed::Yes),
            Err(error) => {
                // Between the listing and the kill the pane may have gone on
                // its own; the listing is what decides, not tmux's wording.
                if self.panes().await?.iter().any(|live| live.id == pane.id) {
                    Err(error)
                } else {
                    Ok(Killed::AlreadyGone)
                }
            }
        }
    }

    /// Types `line` into `pane_id` and presses Enter — the launch line of
    /// §4.1 step 6, delivered to the shell the split left idle.
    ///
    /// Two `send-keys` calls: the line itself under `-l`, so every byte lands
    /// **literally** — no key-name lookup, so a `;` or a `Space` in a path is
    /// text and not a key — and then `Enter` by name. What the shell makes of
    /// the text is the caller's business, which is why [`shell_quote`] exists.
    ///
    /// # Errors
    ///
    /// The client failing to start or tmux refusing (a pane already gone).
    pub async fn type_line(&self, pane_id: &str, line: &OsStr) -> Result<(), TmuxError> {
        let mut text = self.command();
        text.arg("send-keys")
            .arg("-t")
            .arg(pane_id)
            .arg("-l")
            .arg("--")
            .arg(line);
        run("send-keys", text).await?;

        let mut enter = self.command();
        enter.arg("send-keys").arg("-t").arg(pane_id).arg("Enter");
        run("send-keys", enter).await?;

        Ok(())
    }

    /// A client invocation against this server and nothing else: `-S` pins
    /// the socket, and stdin is closed because no call here is interactive.
    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(BINARY);
        command
            .arg("-S")
            .arg(&self.socket)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        command
    }
}

/// The line typed into a pane's idle shell: `exec` the binary with `argv`,
/// every word quoted for `sh`.
///
/// `exec`, so the shell is replaced rather than parented — the pane's process
/// keeps the pid tmux forked, which is the `birth` half of its recorded
/// identity and what an identity-checked kill compares against. Quoted per
/// word ([`shell_quote`]) so a path with a space or a quote in it is one word
/// to the shell. Here beside the quoting rule because both pane backends
/// compose their line this way; only which `arguments` fills `argv` differs.
#[must_use]
pub fn launch_line(binary: &Path, argv: &[OsString]) -> OsString {
    let mut line = OsString::from("exec ");
    line.push(shell_quote(binary.as_os_str()));
    for argument in argv {
        line.push(" ");
        line.push(shell_quote(argument));
    }

    line
}

/// `arg` as one POSIX shell word: single-quoted, with every embedded single
/// quote closed, escaped and reopened (`'\''`) — the one quoting a `sh` line
/// needs and the only one that leaves every other byte alone.
///
/// Byte-level on unix, so a path that is not UTF-8 quotes as itself rather
/// than being replaced. `shell_quote("it's")` is `'it'\''s'`.
#[must_use]
pub fn shell_quote(arg: &OsStr) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let mut quoted = Vec::with_capacity(arg.len() + 2);
        quoted.push(b'\'');
        for byte in arg.as_bytes() {
            if *byte == b'\'' {
                quoted.extend_from_slice(b"'\\''");
            } else {
                quoted.push(*byte);
            }
        }
        quoted.push(b'\'');

        OsString::from_vec(quoted)
    }
    #[cfg(not(unix))]
    {
        let text = arg.to_string_lossy().replace('\'', "'\\''");

        OsString::from(format!("'{text}'"))
    }
}

/// The socket path in a `$TMUX` value: everything before the first comma,
/// which is how the tmux client reads it too.
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
        let text = raw.to_string_lossy();
        let end = text.find(',').unwrap_or(text.len());

        PathBuf::from(&text[..end])
    }
}

/// One line of [`PANE_FORMAT`] as a [`Pane`], or [`None`] for anything else.
fn parse_pane(line: &str) -> Option<Pane> {
    let (id, birth) = line.split_once(' ')?;
    if !id.starts_with('%') || birth.is_empty() || !birth.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }

    Some(Pane {
        id: id.to_owned(),
        birth: birth.to_owned(),
    })
}

/// Runs one client call to completion and hands back its stdout, or the
/// failure in tmux's own words.
async fn run(
    name: &'static str,
    mut command: tokio::process::Command,
) -> Result<String, TmuxError> {
    let output = command.output().await.map_err(|source| TmuxError::Start {
        command: name,
        source,
    })?;
    if !output.status.success() {
        return Err(TmuxError::Failed {
            command: name,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use super::{PANE_FORMAT, REFUSED_NO_TMUX, environment, parse_pane, socket_of};

    /// The refusal has to say which variable, because that is the whole of what
    /// somebody reading it can act on.
    #[test]
    fn the_refusal_names_the_variable_and_the_way_out() {
        assert!(REFUSED_NO_TMUX.contains("$TMUX"));
        assert!(REFUSED_NO_TMUX.contains("in-process"));
    }

    /// `$TMUX` is `socket,pid,index`, and only the socket is wanted — read the
    /// way the client reads it, up to the first comma.
    #[test]
    fn the_socket_is_the_value_up_to_the_first_comma() {
        assert_eq!(
            socket_of(OsStr::new("/private/tmp/tmux-501/default,4242,0")),
            std::path::Path::new("/private/tmp/tmux-501/default")
        );
        assert_eq!(
            socket_of(OsStr::new("/tmp/sock")),
            std::path::Path::new("/tmp/sock")
        );
        assert!(socket_of(OsStr::new(",1,2")).as_os_str().is_empty());
    }

    /// The pair is spelled by one format and read by one parser, and the
    /// parser refuses what is not a pane id beside a pid.
    #[test]
    fn a_format_line_reads_back_as_the_pair() {
        assert_eq!(PANE_FORMAT, "#{pane_id} #{pane_pid}");
        let pane = parse_pane("%17 48213").expect("a pane line parses");
        assert_eq!(pane.id, "%17");
        assert_eq!(pane.birth, "48213");

        assert!(parse_pane("%17").is_none(), "no second half");
        assert!(parse_pane("17 48213").is_none(), "not a pane id");
        assert!(parse_pane("%17 forty").is_none(), "not a pid");
        assert!(parse_pane("").is_none());
    }

    /// A word is quoted so a POSIX shell reads it back byte for byte: spaces
    /// and `;` inert, an embedded quote closed-escaped-reopened.
    #[test]
    fn a_shell_word_survives_quoting() {
        let quote = |text: &str| {
            super::shell_quote(OsStr::new(text))
                .into_string()
                .expect("ascii")
        };

        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("/with space/a;b"), "'/with space/a;b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote(""), "''");
    }

    /// The environment helper renders exactly the names it is given that are
    /// set, and never a name it was not given — the D502 mechanism.
    ///
    /// Reads two variables every process has and one no process has, rather
    /// than setting any: this module's tests share a process.
    #[test]
    fn only_the_named_variables_travel_and_only_when_set() {
        let path = std::env::var_os("PATH").expect("every process has a PATH");
        let mut expected = OsString::from("PATH=");
        expected.push(&path);

        let carried = environment(["PATH", "GANJA_TMUX_TEST_NOBODY_SETS_THIS"]);
        assert_eq!(carried, vec![expected]);

        assert!(environment(std::iter::empty()).is_empty());
    }
}
