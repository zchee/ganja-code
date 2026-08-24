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
//!
//! # Three more calls, for a pane that holds somebody else's TUI (P28, **D512**)
//!
//! A shim teammate rendered in its CLI's own TUI is spoken to through the
//! pane rather than through a pipe, and three calls are what that takes.
//! [`Server::paste_submit`](crate::teammate::tmux::Server::paste_submit) is
//! the delivery wire: the text goes into a tmux buffer **through the client's
//! stdin** and is pasted bracketed — both inside **one client invocation**,
//! so the buffer's whole life is one process and a delivery dropped
//! mid-flight kills that process rather than stranding a cleartext prompt
//! between two — and Enter submits it — measured 2026-08-20 as the sequence
//! that lands a multi-line body in both codex's and agy's composer as one
//! message. Not `send-keys -l`, for two reasons: its text is
//! argv, which `ps` shows to every user on the machine (the rule the grok
//! prompt file already encodes), and it types the bytes bare, where a
//! composer reads a newline as a keystroke rather than as part of a message.
//! [`Server::capture`](crate::teammate::tmux::Server::capture) reads what the
//! pane shows — a readiness poll's source, and a dead pane's last words' —
//! and [`Server::remain_on_exit`](crate::teammate::tmux::Server::remain_on_exit)
//! is what keeps those words on screen once the process behind them has
//! ended.
//!
//! One fact about tmux the last of those surfaces: a pane kept on exit lists
//! with an **empty `#{pane_pid}`** — the process is gone, and the second half
//! of the pane's identity with it. [`Server::panes`](crate::teammate::tmux::Server::panes)
//! leaves such a pane out rather than refusing the whole listing, because a
//! pane with no process has no pair a recorded pane could match — but it
//! leaves it out on **tmux's own word**, `#{pane_dead}` asked in the same
//! listing ([`LIVENESS_FORMAT`](crate::teammate::tmux::LIVENESS_FORMAT)),
//! never inferred from the missing pid. The distinction is what keeps a
//! running orphan alive: a pane the listing cannot see is one
//! [`Server::kill`](crate::teammate::tmux::Server::kill) answers
//! [`Killed::AlreadyGone`](crate::teammate::tmux::Killed::AlreadyGone) for
//! and [`crate::teammate::reaper`] reads as vanished, deleting its record —
//! so a pane that lists pidless without tmux calling it dead stays loud
//! ([`TmuxError::Unreadable`](crate::teammate::tmux::TmuxError::Unreadable))
//! rather than quietly invisible. What a dead pane means for an
//! identity-checked kill is on `Killed::AlreadyGone`, and
//! [`Server::close_dead`](crate::teammate::tmux::Server::close_dead) is the
//! one door that ends a pane in that state — it asks `#{pane_dead}` inside
//! the server, in the same command as the kill, and leaves a live pane alone,
//! so it can never become a second, unchecked kill.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
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

/// What the liveness listing reads: tmux's own `0`/`1` for whether the pane's
/// process has ended, and then the [`PANE_FORMAT`] pair — **listing-only**,
/// because a split's `-P -F` answer is one pane that is by definition alive
/// and is read by [`PANE_FORMAT`]'s own parser.
///
/// The verdict comes **first** so that what follows it is a `PANE_FORMAT`
/// line verbatim for a live pane (`0 %2 48213`), and for a dead one the bare
/// id the empty pid leaves behind (`1 %2 `, measured on tmux next-3.8,
/// 2026-08-20). It is tmux's verdict and not the missing pid that
/// [`Server::panes`] skips a pane on: a pidless pane tmux does not call dead
/// is refused, loudly, rather than made invisible to a kill and to the
/// reaper. The tail is pinned to `PANE_FORMAT` by test rather than composed
/// from it, since a `const` cannot be.
pub const LIVENESS_FORMAT: &str = "#{pane_dead} #{pane_id} #{pane_pid}";

/// What a listing reads to place a pane: its id and its top-left corner.
const CORNER_FORMAT: &str = "#{pane_id} #{pane_left} #{pane_top}";

/// What a listing reads to tell a dead pane from a live one: its id and
/// tmux's own `0`/`1` for whether its process has ended.
const DEAD_FORMAT: &str = "#{pane_id} #{pane_dead}";

/// How the delivery invocation names itself in an error: the two commands it
/// carries, in the order tmux runs them. tmux's own stderr says which of the
/// two refused; this says that it was the one invocation they share.
const DELIVERY: &str = "load-buffer ; paste-buffer";

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
    /// Where on the screen the new pane goes.
    pub placement: Placement,
}

/// Where a teammate's pane goes: one column of teammates beside the lead,
/// filling downwards.
///
/// ```text
/// +--------+------------+
/// |        |     w1     |
/// |  lead  +------------+
/// |        |     w2     |
/// +--------+------------+
/// ```
///
/// Which of the two a spawn gets is read off tmux's own geometry rather than
/// remembered ([`Server::column_bottom`]), for the reason
/// [`crate::teammate::reaper`] gives about panes generally: what is on the
/// screen is the truth, and a pane this build opened may have been closed by
/// the person whose screen it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The first teammate: a column of its own, split off the lead and
    /// taking `share` percent of the width — 65 unless `teammates.pane_share`
    /// says otherwise ([`crate::teammate::pane::PaneShare`]). tmux's `-l`
    /// sizes the **new** pane, so the number is the teammates' and the lead
    /// keeps the rest.
    Beside {
        /// The teammates' column's share of the width, in percent.
        share: u8,
    },
    /// A later one, stacked under the pane named — the column's bottom.
    Under(String),
}

/// What [`Server::kill`] found when it went to end a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Killed {
    /// The pane was the recorded one, and it is gone now.
    Yes,
    /// No **running** pane by that id exists any more: nothing to do, and
    /// not an error — a teammate that already exited, or a kill asked for
    /// twice. A pane kept on screen after its process died
    /// ([`Server::remain_on_exit`]) answers this too and is left where it is:
    /// its process is gone, and the last words it shows are for a person to
    /// read — [`Server::close_dead`], the only door that ends a pane in that
    /// state, is how it is closed afterwards.
    AlreadyGone,
    /// A pane by that id exists **but it is not the recorded one**: tmux
    /// reissued the id to a stranger's pane, and it was left alone.
    Recycled,
}

/// What [`Server::close_dead`] found when it went to close a pane.
///
/// Every answer but [`Closed::Yes`] leaves the pane as it was, and the
/// listing read **after** the kill is what decides between them: only a pane
/// that listing shows **running** answers [`Closed::Alive`], so a caller that
/// keeps a member for as long as the answer is `Alive` is never keeping one
/// for a corpse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    /// The pane was dead, and it is gone now.
    Yes,
    /// The pane's process is running, so the pane was left alone: this door
    /// closes dead panes and nothing else. Running at the **second** listing
    /// — a pane found dead and then respawned under the kill is `Alive` too,
    /// and correctly, since it is.
    Alive,
    /// No pane by that id exists: nothing to do, and not an error.
    AlreadyGone,
    /// The pane was dead, the kill was sent, and the pane is **still there,
    /// still dead**: `if-shell` ran and said nothing — it exits 0 for a pane
    /// it killed and for one it left alone alike (measured, 2026-08-20) —
    /// and the second listing prints the id with tmux's own `1` beside it.
    /// Not an error, because tmux reported none; not `Alive`, because
    /// nothing is running there — a caller hears this and treats the pane as
    /// the corpse it is, rather than as a member.
    Refused,
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
    /// The client was handed text on its stdin and stopped reading before
    /// all of it was taken, yet exited as though it had — what
    /// [`Server::paste_submit`] reports when tmux's own status does not
    /// already say what went wrong.
    #[error("tmux {command} stopped taking the text it was handed: {source}")]
    Stdin {
        /// Which tmux command.
        command: &'static str,
        /// What the write into the pipe said.
        source: std::io::Error,
    },
    /// A word shlex refuses to quote — a NUL byte today, whatever it
    /// refuses tomorrow — caught when the line is composed, before any pane
    /// exists to unmake.
    #[error("a launch-line word cannot be shell-quoted ({source}): {word:?}")]
    Unquotable {
        /// The offending word, debug-rendered so the refused byte shows.
        word: OsString,
        /// shlex's own reason — the foreign type on purpose, for its
        /// `Display` and its place in the `Error` source chain; this crate
        /// is unpublished, so the coupling is a workspace fact rather than
        /// a semver one.
        source: shlex::QuoteError,
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
    /// `launch.placement` decides where it lands, and the direction flags are
    /// worth a sentence because tmux's read backwards: `-h` is the horizontal
    /// *arrangement*, `| lead | w1 |`, not a horizontal dividing line, and
    /// `-v` is the stacked one. Neither is the default, so leaving both off —
    /// as this did until 2026-08-20 — was choosing the stacked layout by
    /// omission rather than declining to choose.
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
        command.arg("split-window");
        match &launch.placement {
            Placement::Beside { share } => {
                command.arg("-h").arg("-l").arg(format!("{share}%"));
                if let Some(pane) = &self.pane {
                    command.arg("-t").arg(pane);
                }
            }
            // No size: halving the pane being divided is what tmux does
            // unasked, and evening the column out afterwards would take a
            // layout command that would move the lead as well.
            Placement::Under(pane) => {
                command.arg("-v").arg("-t").arg(pane);
            }
        }
        command
            .arg("-d")
            .arg("-P")
            .arg("-F")
            .arg(PANE_FORMAT)
            .arg("-c")
            .arg(launch.cwd);
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
    /// A pane whose process has ended but which is still on screen
    /// ([`Server::remain_on_exit`]) is **not** in it: a pane with no process
    /// has no pair a recorded pane could match — left out rather than
    /// refused, so one dead pane cannot make every other pane on the server
    /// unreadable. Dead is **tmux's verdict**, `#{pane_dead}` read off the
    /// same line ([`LIVENESS_FORMAT`]), not an inference from the empty pid
    /// such a pane also lists with: a pane this listing cannot see is one
    /// [`Server::kill`] answers [`Killed::AlreadyGone`] for and the reaper
    /// reads as vanished, so a pidless pane tmux does not call dead is
    /// refused out loud rather than silently dropped. The one other listing
    /// here, [`Server::column_bottom`], deliberately does **not** skip a dead
    /// pane — it has no identity, but it still occupies screen.
    ///
    /// # Errors
    ///
    /// The client failing to start, tmux refusing, or a line that is not
    /// [`LIVENESS_FORMAT`] — a live pane with no pid included.
    pub async fn panes(&self) -> Result<Vec<Pane>, TmuxError> {
        let mut command = self.command();
        command
            .arg("list-panes")
            .arg("-a")
            .arg("-F")
            .arg(LIVENESS_FORMAT);
        let output = run("list-panes", command).await?;

        output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter_map(|line| match parse_listing(line) {
                Some(Listed::Live(pane)) => Some(Ok(pane)),
                Some(Listed::Dead) => None,
                None => Some(Err(TmuxError::Unreadable {
                    command: "list-panes",
                    output: line.to_owned(),
                })),
            })
            .collect()
    }

    /// The bottom-most pane in the column beside this one, or [`None`] when
    /// there is no such column yet.
    ///
    /// "Beside" is decided on geometry — a pane whose left edge is right of
    /// this one's — rather than on which panes this build opened, and that is
    /// two decisions rather than one. A teammate's pane may hold a `claude`
    /// process rather than a `ganja` one, so no argv predicate covers both
    /// backends the way [`crate::teammate::reaper`]'s covers the one it reaps.
    /// And a column is a thing on a screen, which makes the screen the honest
    /// place to ask whether there is one.
    ///
    /// Scoped to the lead's own window by targeting its pane, so a person who
    /// switched windows since the last spawn still gets the column they have
    /// rather than the one in front of them.
    ///
    /// A pane kept on screen after its process died
    /// ([`Server::remain_on_exit`]) **is** in this listing, where
    /// [`Server::panes`] leaves it out, and the split is deliberate: that
    /// listing answers identity, which a dead pane has none of, while this one
    /// answers geometry, and a dead pane still occupies screen — a spawn
    /// placed under a column bottom that ignored it would split the wrong
    /// pane. The two listings ask tmux different questions and must keep
    /// getting different answers.
    ///
    /// # Errors
    ///
    /// The client failing to start, tmux refusing, or a listing line that is
    /// not the id-and-corner triple this asks for.
    pub async fn column_bottom(&self) -> Result<Option<String>, TmuxError> {
        let Some(lead) = &self.pane else {
            return Ok(None);
        };
        let mut command = self.command();
        command
            .arg("list-panes")
            .arg("-t")
            .arg(lead)
            .arg("-F")
            .arg(CORNER_FORMAT);
        let output = run("list-panes", command).await?;

        let corners: Vec<Corner> = output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                parse_corner(line.trim()).ok_or_else(|| TmuxError::Unreadable {
                    command: "list-panes",
                    output: line.to_owned(),
                })
            })
            .collect::<Result<_, TmuxError>>()?;

        // A window that does not list the pane this process is in is a
        // session that moved under us. Nothing here can place against it, so
        // the caller opens a column as though it were the first to.
        let Some(edge) = corners
            .iter()
            .find(|corner| &corner.id == lead)
            .map(|corner| corner.left)
        else {
            return Ok(None);
        };

        Ok(corners
            .into_iter()
            .filter(|corner| corner.left > edge)
            .max_by_key(|corner| corner.top)
            .map(|corner| corner.id))
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
    /// # A shutdown that also signals the pane's process group: TERM **first**
    ///
    /// A pane that holds somebody else's TUI ends by two means — its process
    /// group is signalled (`crate::teammate::shim::signal_group`, the
    /// TERM-then-KILL follow-through that ends the children the CLI itself
    /// started) and the pane is killed — and the order is load-bearing.
    /// Signal the group **while the pane still pins the pid**: TERM it,
    /// verify the process is gone, and only then `kill`. A `kill-pane` that
    /// goes first ends the pane's process and frees its pid, and a
    /// `signal_group` sent after that aims at a pid the kernel may have
    /// reissued by then — some other process group, this person's own —
    /// which is D506's recycled-identity hazard one level down, and exactly
    /// what the pair check here exists to refuse at the pane level. A pane
    /// that is live pins its `pane_pid`: tmux holds the process, the pid
    /// cannot be reissued, and a signal aimed at its group reaches the
    /// group it was aimed at. One practical note for a signaller:
    /// [`Pane::birth`](crate::teammate::reaper::Pane) is the pid as tmux
    /// printed it — a `String`, because it is matched and never arithmetic'd
    /// here — and `signal_group` wants an `i32`, so the signaller parses it,
    /// refusing rather than guessing at anything that is not a pid.
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
        self.press_enter(pane_id).await
    }

    /// What `pane_id` shows right now, as text: every visible row, with a
    /// row the pane wrapped at its width joined back into the one line it
    /// was (`-J`).
    ///
    /// Two readers want it and both want the join: a readiness poll looking
    /// for a composer's marker, and whoever reads a dead pane's last words —
    /// grok's refusal sentence is longer than a narrow pane is wide, and a
    /// comparison against the recorded sentence has to see it whole (measured
    /// 2026-08-20: plain `-p` hands a 145-column sentence back as three rows
    /// in a 56-column pane, `-J` as one). The **visible screen only** — no
    /// `-S`/`-E`, so nothing from the scrollback: what a person looking at
    /// the pane would see is the question both readers are asking. The
    /// consequence is worth knowing before reading the answer: text that has
    /// scrolled above the viewport is not in it, so a sentence that points
    /// upward — grok's refusal says "see the warning above" — names text this
    /// call may no longer be able to return.
    ///
    /// # Errors
    ///
    /// The client failing to start or tmux refusing — a pane that is gone,
    /// which under [`Server::remain_on_exit`] a dead one is not.
    pub async fn capture(&self, pane_id: &str) -> Result<String, TmuxError> {
        let mut command = self.command();
        command
            .arg("capture-pane")
            .arg("-p")
            .arg("-J")
            .arg("-t")
            .arg(pane_id);
        run("capture-pane", command).await
    }

    /// What `pane_id`'s foreground process is called — tmux's own
    /// `#{pane_current_command}`, the name of the process its tty has in the
    /// foreground, read fresh on every call.
    ///
    /// One reader, and the question it asks is not "which program" but
    /// "**still the shell?**": the shim readiness poll
    /// ([`crate::teammate::shim_tui`]) reads it once before the launch line
    /// and again on every pass, and only a name that *changed* says the
    /// shell handed the pane to the CLI. A name is all tmux offers — the pid
    /// it lists is the pane's first process, which `exec` keeps — and a name
    /// is enough, because the comparison is against what the same pane said
    /// a moment ago rather than against any spelling of any shell: a CLI
    /// that is a `#!/bin/sh` script under a `/bin/sh` pane reads as
    /// unchanged, which the poll takes for "not yet" and never for "ready".
    ///
    /// # Errors
    ///
    /// The client failing to start or tmux refusing — a pane that is gone.
    pub async fn current_command(&self, pane_id: &str) -> Result<String, TmuxError> {
        let mut command = self.command();
        command
            .arg("display-message")
            .arg("-p")
            .arg("-t")
            .arg(pane_id)
            .arg("#{pane_current_command}");
        run("display-message", command)
            .await
            .map(|name| name.trim().to_owned())
    }

    /// Whether `pane_id` is where typing goes right now: the active pane of
    /// the current window — `#{pane_active}` and `#{window_active}` both —
    /// which is what the pane's own focus would say if tmux told it.
    ///
    /// tmux does not: measured on next-3.8 (2026-08-25), a pane that enables
    /// focus reporting is sent nothing about the state it starts in, only the
    /// changes after, so a ganja that came up in a pane nobody was looking at
    /// — every teammate pane, split `-d` beside the lead — took itself for
    /// looked-at until the first change. The one reader asks this once, at
    /// startup, to seed what its notifications gate on. Not the client's own focus: whether the terminal
    /// window is looked at is the terminal's business, and the terminal
    /// reports that as a change like any other.
    ///
    /// # Errors
    ///
    /// The client failing to start or tmux refusing — a pane that is gone.
    pub async fn focused(&self, pane_id: &str) -> Result<bool, TmuxError> {
        let mut command = self.command();
        command
            .arg("display-message")
            .arg("-p")
            .arg("-t")
            .arg(pane_id)
            .arg("#{&&:#{pane_active},#{window_active}}");
        run("display-message", command)
            .await
            .map(|answer| answer.trim() == "1")
    }

    /// Lands `text` in `pane_id`'s composer as **one unsubmitted message** —
    /// into a tmux buffer through the client's stdin and pasted bracketed, one
    /// client invocation for both — and presses **no** Enter.
    ///
    /// This is the paste-only door; [`Server::paste_submit`] is this plus the
    /// Enter that submits what it left. The split is P28's readiness rule made
    /// real: a spawn whose composer marker never showed
    /// ([`crate::teammate::shim_tui`]) pastes *without* submitting, because an
    /// Enter into a pane that may be holding a trust or login dialog answers
    /// that dialog with its default on behalf of a person who never saw it — so
    /// the text is left in the composer, and the person, who is looking at the
    /// pane, submits it. A spawn that did see its marker uses `paste_submit`.
    ///
    /// The one client invocation is tmux's own command list, `load-buffer -b
    /// <name> - ';' paste-buffer -p -d -b <name> -t <pane>` handed to one
    /// client, each part measured on 2026-08-20: `load-buffer -` takes the
    /// text on **stdin**, so no byte of it is on an argv that `ps` shows to
    /// every user on the machine — the rule the grok prompt file already
    /// encodes, kept for the same text travelling another way — and
    /// `paste-buffer -p` wraps it in the bracketed-paste codes when the pane's
    /// program asked for them, which is how a TUI composer takes a multi-line
    /// body as one message with its newlines intact where the same bytes typed
    /// through the pty would reach it as keystrokes; `-d` frees the buffer the
    /// moment it is pasted, so the text does not sit in the server's buffer
    /// stack for a `list-buffers` to show. The buffer is **named**, once per
    /// call (`buffer_name`), because the unnamed stack is shared by everyone on
    /// the server and its top is whoever loaded last: two deliveries racing
    /// each other would otherwise paste each other's **text**. That is all the
    /// name buys — buffer contents kept apart, not composers. Two of these
    /// calls (or their `paste_submit` form) running at once against the
    /// **same pane** can still interleave their pastes, so neither is safe
    /// against concurrent calls to one pane, and the caller serializes
    /// deliveries per member (the shim runtime does, one at a time per
    /// teammate); what they are safe against is two panes at once.
    ///
    /// # The buffer's whole life is one process
    ///
    /// Load and paste share one invocation so that **cancellation cannot leak
    /// the buffer**: this future is awaited inside the shim runtime's
    /// `select!`, and a future dropped mid-await runs no cleanup of its own.
    /// Were the load one client and the paste another, a drop between the two
    /// would leave a teammate's prompt in cleartext on the server under a
    /// name nothing reclaims. As one invocation, a drop kills the one client
    /// (`feed` kills it **before** closing its stdin, for the reason given
    /// there), and a killed client's read is ended by the server with an
    /// error that sets no buffer — tmux loads a buffer only from a read it saw
    /// end (measured 2026-08-20: a client `SIGKILL`ed mid-stream leaves
    /// `list-buffers` empty and pastes nothing). The one way the buffer
    /// outlives the process is the load taking and the paste being refused —
    /// the pane gone — where tmux keeps what the load set (measured: the
    /// buffer survives a `can't find pane`); that case is reclaimed on a task
    /// of its own, spawned rather than awaited, so a caller letting go of this
    /// future on seeing the error cannot skip it either.
    ///
    /// An empty `text` is no message: nothing is loaded or pasted.
    ///
    /// # Errors
    ///
    /// The client failing to start, tmux refusing (the pane gone), or tmux
    /// ceasing to read the text before it was all handed over
    /// ([`TmuxError::Stdin`]). A refused invocation frees the named buffer,
    /// best-effort and off this future: a teammate's prompt must not sit on
    /// the server in cleartext for the server's life.
    pub async fn paste(&self, pane_id: &str, text: &str) -> Result<(), TmuxError> {
        if text.is_empty() {
            return Ok(());
        }
        let buffer = buffer_name();

        let mut delivery = self.command();
        delivery
            .arg("load-buffer")
            .arg("-b")
            .arg(&buffer)
            .arg("-")
            .arg(";")
            .arg("paste-buffer")
            .arg("-p")
            .arg("-d")
            .arg("-b")
            .arg(&buffer)
            .arg("-t")
            .arg(pane_id);
        if let Err(error) = feed(DELIVERY, delivery, text.as_bytes()).await {
            // The load may have taken and the paste been refused, and tmux
            // keeps what the load set. Spawned, not awaited: the spawn is
            // synchronous and the task outlives this future, so a caller
            // that drops it on the error cannot strand the buffer. The
            // failure reported is the invocation's own; a buffer that is
            // already gone, or a server that is, adds nothing to it.
            let mut delete = self.command();
            delete.arg("delete-buffer").arg("-b").arg(&buffer);
            tokio::spawn(async move {
                let _ = run("delete-buffer", delete).await;
            });
            return Err(error);
        }

        Ok(())
    }

    /// Delivers `text` to `pane_id` as **one submitted composer message**:
    /// [`Server::paste`] to land it, then Enter to submit it.
    ///
    /// Two client calls — the one-invocation load-and-paste, then the Enter —
    /// so the concurrency, stdin-not-argv and cancel-safety facts are all
    /// [`Server::paste`]'s and are not restated here. What the Enter adds is a
    /// second place the call can fail after the paste already landed: the paste
    /// having succeeded and the Enter having not leaves the text **pasted but
    /// unsubmitted** in the composer, so a failure here is **not** a delivery
    /// that did not happen — a caller that retried blindly would deliver it
    /// twice. A failed call is something to report, never to redo unseen.
    ///
    /// An empty `text` is no message: nothing is loaded, pasted or submitted,
    /// because the only thing an Enter could submit then is whatever a person
    /// had half-typed into that composer themselves.
    ///
    /// # Errors
    ///
    /// [`Server::paste`]'s errors, or the Enter's own `send-keys` refusing —
    /// after a paste that already succeeded, which is why the doc above calls
    /// a failure something to report rather than to redo.
    pub async fn paste_submit(&self, pane_id: &str, text: &str) -> Result<(), TmuxError> {
        if text.is_empty() {
            return Ok(());
        }
        self.paste(pane_id, text).await?;
        self.press_enter(pane_id).await
    }

    /// Keeps, or stops keeping, `pane_id` on screen after its process exits
    /// (`remain-on-exit`, as a pane option).
    ///
    /// Set **before** the launch line is typed: a CLI that refuses to start
    /// — grok's sandbox profile on this machine — says why and exits, and a
    /// pane that closed with it would take the sentence with it. Kept, the
    /// pane is dead but readable, [`Server::capture`] reads the words, and a
    /// person closes it by hand. A pane kept this way lists dead, and with
    /// no pid, which [`Server::panes`] and [`Killed::AlreadyGone`] account
    /// for.
    ///
    /// # Errors
    ///
    /// The client failing to start or tmux refusing (a pane already gone, or
    /// a server too old for pane options — tmux 3.0).
    pub async fn remain_on_exit(&self, pane_id: &str, on: bool) -> Result<(), TmuxError> {
        let mut command = self.command();
        command
            .arg("set-option")
            .arg("-p")
            .arg("-t")
            .arg(pane_id)
            .arg("remain-on-exit")
            .arg(if on { "on" } else { "off" });
        run("set-option", command).await?;

        Ok(())
    }

    /// Closes `pane_id` if, and only if, its process has ended — the pane a
    /// person has read under [`Server::remain_on_exit`] — and leaves a live
    /// pane exactly as it was.
    ///
    /// This exists because [`Server::kill`] answers [`Killed::AlreadyGone`]
    /// for a dead pane (no process, no pair), and it is kept from becoming a
    /// second, unchecked kill door by two things. The check and the kill are
    /// **one tmux command**: `if-shell -F -t <pane> '#{pane_dead}' 'kill-pane
    /// …'` is decided inside the server, so a pane respawned between a
    /// listing and a kill cannot lose its new process to a stale answer. And
    /// that command is reached **only for an id the listing just printed**,
    /// so the id tmux is handed in a command string is a `%N` tmux itself
    /// minted — never a string out of a document somebody else wrote, which
    /// is where a teammate's recorded pane id comes from and which could
    /// otherwise carry a second command in the same string.
    ///
    /// What it answers is read off a **second** listing, because `if-shell`
    /// says nothing about what it found — it exits 0 for a pane it killed,
    /// for a live pane it left alone and for an id nobody wears alike
    /// (measured, 2026-08-20) — and that listing is read by `after_kill`:
    /// gone is [`Closed::Yes`], running is [`Closed::Alive`], and still there
    /// and still dead is [`Closed::Refused`] — never `Alive`, which only a
    /// pane the listing shows running may answer.
    ///
    /// # Errors
    ///
    /// The client failing to start, tmux refusing, or a listing line that is
    /// not an id beside tmux's own `0`/`1`.
    pub async fn close_dead(&self, pane_id: &str) -> Result<Closed, TmuxError> {
        match self.dead(pane_id).await? {
            None => return Ok(Closed::AlreadyGone),
            Some(false) => return Ok(Closed::Alive),
            Some(true) => {}
        }

        let mut command = self.command();
        command
            .arg("if-shell")
            .arg("-F")
            .arg("-t")
            .arg(pane_id)
            .arg("#{pane_dead}")
            .arg(format!("kill-pane -t {pane_id}"));
        run("if-shell", command).await?;

        Ok(after_kill(self.dead(pane_id).await?))
    }

    /// Whether `pane_id` is on this server, and if it is, whether its process
    /// has ended — [`None`] for no such pane, [`Some`] of `#{pane_dead}`.
    async fn dead(&self, pane_id: &str) -> Result<Option<bool>, TmuxError> {
        let mut command = self.command();
        command
            .arg("list-panes")
            .arg("-a")
            .arg("-F")
            .arg(DEAD_FORMAT);
        let output = run("list-panes", command).await?;

        for line in output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let unreadable = || TmuxError::Unreadable {
                command: "list-panes",
                output: line.to_owned(),
            };
            let (id, dead) = line.split_once(' ').ok_or_else(unreadable)?;
            if id != pane_id {
                continue;
            }
            return match dead {
                "0" => Ok(Some(false)),
                "1" => Ok(Some(true)),
                _ => Err(unreadable()),
            };
        }

        Ok(None)
    }

    /// Enter, by key name, into `pane_id`: what submits a typed launch line
    /// and a pasted message alike.
    async fn press_enter(&self, pane_id: &str) -> Result<(), TmuxError> {
        let mut enter = self.command();
        enter.arg("send-keys").arg("-t").arg(pane_id).arg("Enter");
        run("send-keys", enter).await?;

        Ok(())
    }

    /// A client invocation against this server and nothing else: `-S` pins
    /// the socket, and stdin is closed because no call here is interactive —
    /// [`feed`] reopens it for the one call that hands tmux text.
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
/// every word quoted for the shell reading it.
///
/// `exec`, so the shell is replaced rather than parented — the pane's process
/// keeps the pid tmux forked, which is the `birth` half of its recorded
/// identity and what an identity-checked kill compares against. Quoted per
/// word ([`shell_quote`]) so a path with a space or a quote in it is one word
/// to the shell. Here beside the quoting rule because both pane backends
/// compose their line this way; only which `arguments` fills `argv` differs.
///
/// # Errors
///
/// [`TmuxError::Unquotable`] for a word no quoting can carry; both backends
/// take that refusal before a pane exists.
pub fn launch_line(binary: &Path, argv: &[OsString]) -> Result<OsString, TmuxError> {
    let mut line = OsString::from("exec ");
    line.push(shell_quote(binary.as_os_str())?);
    for argument in argv {
        line.push(" ");
        line.push(shell_quote(argument)?);
    }

    Ok(line)
}

/// `arg` as one shell word, through shlex's POSIX quoter: bare when every
/// byte is safe, single- or double-quoted chunks when one is not.
///
/// The shell reading the line is the person's own, and the classic `'\''`
/// idiom is wrong in one of them: fish reads a backslash inside single quotes
/// as an escape where sh reads it literally, and shlex keeps backslashes out
/// of single-quoted chunks for exactly that reason. Byte-level on unix, so a
/// path that is not UTF-8 quotes as itself rather than being replaced.
///
/// What quoting cannot do, stated because the delivery channel is an
/// interactive shell's pty ([`Server::type_line`]): shlex's own warning is
/// that control bytes keep their line-editing meaning there — a quoted `\r`
/// still executes the partial line — so the safety of a launch line rests on
/// its words, not this function: flag constants, an absolute path, and names
/// held to the member-name grammar.
///
/// # Errors
///
/// [`TmuxError::Unquotable`] — today only for the NUL byte no shell word can
/// represent.
pub fn shell_quote(arg: &OsStr) -> Result<OsString, TmuxError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        match shlex::bytes::try_quote(arg.as_bytes()) {
            Ok(quoted) => Ok(OsString::from_vec(quoted.into_owned())),
            Err(source) => Err(TmuxError::Unquotable {
                word: arg.to_owned(),
                source,
            }),
        }
    }
    #[cfg(not(unix))]
    {
        let text = arg.to_string_lossy();
        match shlex::try_quote(&text) {
            Ok(quoted) => Ok(OsString::from(quoted.into_owned())),
            Err(source) => Err(TmuxError::Unquotable {
                word: arg.to_owned(),
                source,
            }),
        }
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

/// A pane's id and where its top-left corner sits, as [`CORNER_FORMAT`] reads
/// it.
struct Corner {
    id: String,
    left: u16,
    top: u16,
}

/// One [`CORNER_FORMAT`] line, or [`None`] when it is not one.
fn parse_corner(line: &str) -> Option<Corner> {
    let mut words = line.split(' ');
    let id = words.next()?;
    let left = words.next()?.parse().ok()?;
    let top = words.next()?.parse().ok()?;
    if !id.starts_with('%') || words.next().is_some() {
        return None;
    }

    Some(Corner {
        id: id.to_owned(),
        left,
        top,
    })
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

/// What one line of the liveness listing ([`LIVENESS_FORMAT`]) says about a
/// pane.
#[derive(Debug, PartialEq, Eq)]
enum Listed {
    /// A running pane, as the pair a recorded pane is matched on.
    Live(Pane),
    /// A pane tmux itself calls dead: on screen under
    /// [`Server::remain_on_exit`], no process, and so no pair.
    Dead,
}

/// One trimmed [`LIVENESS_FORMAT`] line, or [`None`] when it is not one.
///
/// The verdict in front decides, and only that: `0` must be followed by a
/// whole [`PANE_FORMAT`] pair — a live pane with no pid is **not** read as
/// dead, it is unreadable, because a pane this parser drops is one a kill and
/// the reaper will never see again — and `1` by a pane id, whatever tmux
/// printed for the pid of a process it no longer has (nothing, measured).
fn parse_listing(line: &str) -> Option<Listed> {
    let (dead, pane) = line.split_once(' ')?;
    match dead {
        "0" => parse_pane(pane).map(Listed::Live),
        "1" if pane.starts_with('%') => Some(Listed::Dead),
        _ => None,
    }
}

/// What [`Server::close_dead`] answers for a pane it found dead and sent the
/// kill for, read off the listing taken **after** that kill — `listed` being
/// what [`Server::dead`] said: no such pane, or tmux's verdict on it.
///
/// A function of its own rather than a `match` at the call site so the one
/// rule it exists for is a thing a test can pin without a server: `Alive` is
/// for a pane the listing shows **running** and for nothing else. A pane that
/// is gone was closed; a pane that is running was respawned under the kill
/// and is left as the live pane it is; a pane that is still there and still
/// dead is one the kill did not take, and `if-shell`, which exits 0 either
/// way, will not be the one to say so.
fn after_kill(listed: Option<bool>) -> Closed {
    match listed {
        None => Closed::Yes,
        Some(false) => Closed::Alive,
        Some(true) => Closed::Refused,
    }
}

/// A buffer name no other delivery on this server is using: this process's
/// pid and a counter, because the buffer stack is one per tmux server and the
/// unnamed top of it is whoever loaded last. The name keeps two deliveries'
/// **text** apart and nothing more — what two of them do to one composer is
/// on [`Server::paste_submit`]. A name a crashed earlier process left behind
/// is simply overwritten by the load — and then pasted and freed.
fn buffer_name() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);

    format!("ganja-{}-{sequence}", std::process::id())
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

    settle(name, output)
}

/// [`run`], with `input` written to the client's stdin — the one door that
/// text which must not be on argv travels through.
///
/// The write and the wait run **together** rather than in turn, so neither
/// pipe can wedge the other: a client that stopped reading early does not
/// leave the write stuck against a full stdin, and a client with something
/// to say does not sit on a full stdout while the write is still going. The
/// write's end closes the pipe, because that EOF is what tells
/// `load-buffer -` the text has ended. tmux's own status is reported first
/// when both sides have something to say — it is the one that knows why.
///
/// Dropped mid-flight, this kills the client **before** it closes the
/// client's stdin, and the order is the point: a closed pipe is EOF, EOF is
/// "the text has ended", and a client that read the cut as the end would
/// hand the server the fraction that had arrived to be loaded and pasted as
/// if it were the whole — a partial prompt in a foreign composer. A client
/// killed first reads nothing more; the server ends its read with an error
/// and sets no buffer (measured 2026-08-20, see [`Server::paste_submit`]).
/// The order is the language's, not a comment's: the `Child` rides inside
/// the `join!` temporary, which a cancelled `await` drops before this
/// function's own `stdin` local — and `stdin` is a local, borrowed by the
/// writer and closed by it explicitly at the write's end, precisely so it is
/// not dropped as part of that temporary.
async fn feed(
    name: &'static str,
    mut command: tokio::process::Command,
    input: &[u8],
) -> Result<String, TmuxError> {
    use tokio::io::AsyncWriteExt as _;

    let mut child = command
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|source| TmuxError::Start {
            command: name,
            source,
        })?;
    let mut stdin = Some(
        child
            .stdin
            .take()
            .expect("stdin was asked for as a pipe a line above"),
    );
    let (output, fed) = tokio::join!(child.wait_with_output(), async {
        let Some(pipe) = stdin.as_mut() else {
            unreachable!("the pipe is taken once, below, after this write");
        };
        let fed = pipe.write_all(input).await;
        // Closing the pipe is the EOF that tells tmux the text has ended.
        drop(stdin.take());
        fed
    });
    let output = output.map_err(|source| TmuxError::Start {
        command: name,
        source,
    })?;
    let answer = settle(name, output)?;
    fed.map_err(|source| TmuxError::Stdin {
        command: name,
        source,
    })?;

    Ok(answer)
}

/// A finished client call's stdout, or its refusal in its own words.
fn settle(name: &'static str, output: std::process::Output) -> Result<String, TmuxError> {
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
    use std::{
        ffi::{OsStr, OsString},
        path::Path,
        time::Duration,
    };

    use ganja_testkit::tmux::PrivateServer;

    use super::{
        Closed, Killed, LIVENESS_FORMAT, Launch, Listed, PANE_FORMAT, Placement, REFUSED_NO_TMUX,
        Server, TmuxError, after_kill, buffer_name, environment, parse_listing, parse_pane,
        shell_quote, socket_of,
    };
    use crate::teammate::reaper::Pane;

    /// The recording of grok's own TUI probe. The refusal sentence a dead
    /// grok pane shows is read off it rather than restated here, so the
    /// capture test below exercises the very line a fixture comparison will
    /// look for.
    const GROK_TUI_PROBE: &str = include_str!("../../tests/fixtures/grok-tui-probe.txt");

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

    /// A word is quoted so the shell on the other end reads it back byte for
    /// byte — and only when it must be: a safe word rides bare, a backslash
    /// never rides inside single quotes (fish reads it as an escape there),
    /// and the NUL byte no shell word can carry is refused rather than sent.
    #[test]
    fn a_shell_word_survives_quoting() {
        let quote = |text: &str| {
            super::shell_quote(OsStr::new(text))
                .expect("no NUL rides these words")
                .into_string()
                .expect("ascii")
        };

        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("/with space/a;b"), "'/with space/a;b'");
        assert_eq!(quote("it's"), "\"it's\"");
        assert_eq!(quote("a\\b"), "\"a\\\\b\"");
        assert_eq!(quote(""), "''");

        #[cfg(unix)]
        {
            use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

            let quoted = super::shell_quote(OsStr::from_bytes(b"a\x80b"))
                .expect("bytes outside UTF-8 still quote");
            assert_eq!(quoted.into_vec(), b"'a\x80b'".to_vec());

            let refused = super::shell_quote(OsStr::from_bytes(b"a\0b"));
            assert!(matches!(refused, Err(super::TmuxError::Unquotable { .. })));
        }
    }

    /// The composed line quotes only the words that need it, and a word no
    /// quoting can carry refuses the whole line before tmux is handed
    /// anything.
    /// Exactly one pane of the window is where typing goes, and it is the one
    /// tmux itself calls active — a fresh split's answer is read off tmux
    /// rather than assumed, since whether a split takes the focus is tmux's
    /// choice.
    #[tokio::test]
    async fn focused_answers_for_the_active_pane_of_the_current_window_and_no_other() {
        let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
        let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
        let other = server.split(None, &[], &["sleep", "3600"]);

        let first = at.focused(server.first_pane()).await.expect("tmux answers");
        let second = at.focused(&other).await.expect("tmux answers");
        assert_ne!(first, second, "one pane of the window has the focus");
        let active = server.run(&["display-message", "-p", "-t", &other, "#{pane_active}"]);
        assert_eq!(second, active.trim() == "1");
    }

    #[test]
    fn a_launch_line_quotes_what_needs_it_and_refuses_a_nul() {
        let line = super::launch_line(
            std::path::Path::new("/opt/ganja builds/ganja"),
            &[OsString::from("--agent-name"), OsString::from("it's")],
        )
        .expect("no NUL rides these words")
        .into_string()
        .expect("ascii");
        assert_eq!(line, "exec '/opt/ganja builds/ganja' --agent-name \"it's\"");

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;

            let word = OsString::from_vec(b"a\0b".to_vec());
            let refused = super::launch_line(std::path::Path::new("/bin/ganja"), &[word]);
            assert!(matches!(refused, Err(super::TmuxError::Unquotable { .. })));
        }
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

    /// The liveness listing is the pair with tmux's verdict in front — pinned
    /// as that composition, so the split's format and the listing's cannot
    /// drift apart — and dead is that verdict's word alone: a `1` is skipped
    /// whatever follows the id (measured: nothing, the pid of a process the
    /// pane no longer has), while a `0` over a pane with no pid is
    /// **unreadable**, never dead, because a pane this parser drops is one a
    /// kill answers `AlreadyGone` for and the reaper deletes the record of.
    #[test]
    fn a_listing_line_is_live_dead_or_unreadable_on_tmuxs_own_verdict() {
        assert_eq!(
            LIVENESS_FORMAT.strip_prefix("#{pane_dead} "),
            Some(PANE_FORMAT),
            "the tail is the pair, spelled once"
        );

        assert_eq!(
            parse_listing("0 %2 48213"),
            Some(Listed::Live(Pane {
                id: "%2".to_owned(),
                birth: "48213".to_owned(),
            }))
        );
        assert_eq!(
            parse_listing("1 %2"),
            Some(Listed::Dead),
            "the measured shape"
        );
        assert_eq!(
            parse_listing("1 %2 48213"),
            Some(Listed::Dead),
            "tmux's word, not the tail, says dead"
        );

        assert!(
            parse_listing("0 %2").is_none(),
            "a live pane with no pid stays loud"
        );
        assert!(parse_listing("0 %2 forty").is_none(), "not a pid");
        assert!(parse_listing("%2 48213").is_none(), "no verdict");
        assert!(parse_listing("2 %2 48213").is_none(), "not a verdict");
        assert!(parse_listing("1 2").is_none(), "dead, but not a pane id");
        assert!(parse_listing("1").is_none(), "a verdict over nothing");
        assert!(parse_listing("").is_none());
    }

    /// After the kill, `Alive` is for a pane the listing shows running and
    /// for nothing else: gone is closed, running is left alone (it was
    /// respawned under the kill), and still-there-still-dead is the honest
    /// fourth answer — a kill that did not take is never reported as a live
    /// member. Pinned here without a server because a real tmux cannot be
    /// cheaply made to refuse a `kill-pane`; the real-server test below
    /// covers the three answers it can produce.
    #[test]
    fn after_the_kill_only_a_running_pane_answers_alive() {
        assert_eq!(after_kill(None), Closed::Yes);
        assert_eq!(after_kill(Some(false)), Closed::Alive);
        assert_eq!(
            after_kill(Some(true)),
            Closed::Refused,
            "a pane still listed dead after the kill is not alive"
        );
    }

    /// Every delivery loads its own buffer, named for this process and a
    /// counter, so two deliveries on one server cannot paste each other's
    /// text — which is the whole of what the name promises.
    #[test]
    fn every_delivery_gets_a_buffer_name_of_its_own() {
        let first = buffer_name();
        let second = buffer_name();
        assert_ne!(first, second);

        let prefix = format!("ganja-{}-", std::process::id());
        assert!(first.starts_with(&prefix), "{first}");
        assert!(second.starts_with(&prefix), "{second}");
    }

    // ---- Against a real tmux, on a private server of the test's own --------
    //
    // `PrivateServer` hard-fails without tmux and kills its server when it
    // drops, panics included, so no pane or process outlives a test. Nothing
    // below touches the process environment: the server is reached through
    // `Server::at`, never `$TMUX`.

    /// A pane on `at` running `argv` in `cwd`, beside the lead's.
    async fn split(at: &Server, cwd: &Path, argv: &[&str]) -> Pane {
        let argv: Vec<OsString> = argv.iter().map(OsString::from).collect();
        at.split(Launch {
            cwd,
            environment: &[],
            argv: &argv,
            placement: Placement::Beside {
                share: crate::teammate::pane::DEFAULT_SHARE,
            },
        })
        .await
        .expect("the private server splits a pane")
    }

    /// The column takes the share it is handed, and the lead keeps the rest:
    /// on a 200-column window a 65 splits off a pane about 130 wide (tmux
    /// rounds, and a border takes a column), and a 35 one about 70.
    #[tokio::test]
    async fn a_column_beside_the_lead_takes_the_share_it_is_given() {
        for (share, expected) in [(65u8, 130usize), (35, 70)] {
            let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
            let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
            let argv: Vec<OsString> = ["sleep", "3600"].iter().map(OsString::from).collect();
            let pane = at
                .split(Launch {
                    cwd: Path::new("/"),
                    environment: &[],
                    argv: &argv,
                    placement: Placement::Beside { share },
                })
                .await
                .expect("the private server splits a pane");
            let width: usize = server
                .run(&["display-message", "-p", "-t", &pane.id, "#{pane_width}"])
                .trim()
                .parse()
                .expect("a pane width");
            assert!(
                width.abs_diff(expected) <= 2,
                "a share of {share} on 200 columns gave the column {width} columns"
            );
        }
    }

    /// Polls `probe` until it is satisfied, or fails naming `what` and the
    /// last state it saw once the bound a real server is held to has passed.
    async fn eventually(what: &str, mut probe: impl AsyncFnMut() -> Result<(), String>) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let Err(state) = probe().await else {
                return;
            };
            assert!(
                tokio::time::Instant::now() < deadline,
                "gave up waiting for {what}; last seen: {state}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// What a pane shows comes back as text, and a line the pane wrapped at
    /// its width comes back as the one line it was — grok's recorded refusal
    /// sentence is longer than a narrow pane is wide, and whoever compares a
    /// dead pane's words against that recording has to see it whole.
    #[tokio::test]
    async fn a_capture_reads_what_the_pane_shows_with_wrapped_lines_rejoined() {
        // The sentence is the recording's, not a restatement of it: the very
        // bytes a fixture comparison will look for in a dead grok pane.
        let sentence = GROK_TUI_PROBE
            .lines()
            .find_map(|line| line.strip_prefix("error: "))
            .expect("the grok recording carries the vendor's refusal verbatim");
        let cwd = ganja_testkit::temp_dir();
        // 80 columns, of which the teammate's column takes 70%: narrower than
        // the sentence, so the pane has to wrap it.
        let server = PrivateServer::start_in(cwd.path(), (80, 24), &["sleep", "3600"], &[], &[]);
        let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
        let quoted = shell_quote(OsStr::new(sentence))
            .expect("no NUL in the sentence")
            .into_string()
            .expect("ascii");
        let script = format!("printf '%s\\n' {quoted}; exec sleep 3600");
        let pane = split(&at, cwd.path(), &["sh", "-c", &script]).await;

        let width: usize = server
            .run(&["display-message", "-p", "-t", &pane.id, "#{pane_width}"])
            .trim()
            .parse()
            .expect("a pane width");
        assert!(
            width < sentence.chars().count(),
            "the premise: the sentence wraps in a {width}-column pane"
        );

        eventually("the sentence to show in the pane, whole", async || {
            let shown = at
                .capture(&pane.id)
                .await
                .map_err(|error| error.to_string())?;
            if shown.lines().any(|line| line == sentence) {
                Ok(())
            } else {
                Err(format!("{shown:?}"))
            }
        })
        .await;
    }

    /// Multi-line text reaches the pane's program whole — its newlines, its
    /// quotes and its non-ASCII as given — followed by the one Enter that
    /// submits it; an empty text delivers nothing, not even that Enter; and
    /// the buffer the text travelled in is gone once it has been pasted.
    ///
    /// The stub is a `cat` writing the pane's input to a file, so what the
    /// pane's program received is read back as bytes rather than as a
    /// screen: the cooked pty turns the paste's `\r` separators back into
    /// the newlines that were loaded, which is what a TUI's bracketed paste
    /// does on its own side.
    #[tokio::test]
    async fn a_pasted_text_reaches_the_pane_whole_with_its_newlines_and_is_submitted() {
        const TEXT: &str = "line one\nline two; with 'quotes' and \"doubles\"\n\tindented ünïcödé";
        let dir = ganja_testkit::temp_dir();
        let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
        let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
        let received = dir.path().join("received.txt");
        let pane = split(
            &at,
            dir.path(),
            &[
                "sh",
                "-c",
                "exec cat > \"$0\"",
                received.to_str().expect("a utf-8 temp path"),
            ],
        )
        .await;
        // The redirection opens the file before `cat` runs: the stub is
        // listening once the file exists.
        eventually("the stub to open its file", async || {
            if received.exists() {
                Ok(())
            } else {
                Err("no file yet".to_owned())
            }
        })
        .await;

        at.paste_submit(&pane.id, "")
            .await
            .expect("an empty text is no message, and no error");
        at.paste_submit(&pane.id, TEXT)
            .await
            .expect("the text is delivered");

        // Exactly the text and the one Enter after it: had the empty paste
        // pressed Enter, the file would open with a newline of its own.
        let expected = format!("{TEXT}\n");
        eventually("the pasted text to reach the stub's file", async || {
            let got = std::fs::read_to_string(&received).map_err(|error| error.to_string())?;
            if got == expected {
                Ok(())
            } else {
                Err(format!("{got:?}"))
            }
        })
        .await;
        assert_eq!(
            server.run(&["list-buffers"]).trim(),
            "",
            "the buffer was freed by the paste"
        );
    }

    /// A delivery dropped mid-flight — the shim runtime's `select!` letting
    /// go of it — leaves no buffer on the server, pastes nothing and presses
    /// no Enter, and the pane takes the next delivery as if the dropped one
    /// had never been asked for.
    ///
    /// Mid-flight by construction rather than by timing: the text is well
    /// past what a pipe holds and the future is dropped after its **first**
    /// poll, which spawned the client and filled the pipe and then had to
    /// wait — so the writer itself had not handed the text over, and the
    /// client cannot have seen it end. What the drop then does is the thing
    /// under test: kill the client before closing its stdin, so
    /// the cut is never read as the text's end and tmux, which loads a
    /// buffer only from a read it saw end, sets nothing. `biased;` is what
    /// makes the first branch the one polled first; without it the select
    /// might never poll the delivery at all and prove nothing.
    #[tokio::test]
    async fn a_delivery_dropped_mid_flight_leaves_no_buffer_and_presses_no_enter() {
        let dir = ganja_testkit::temp_dir();
        let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
        let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
        let received = dir.path().join("received.txt");
        let pane = split(
            &at,
            dir.path(),
            &[
                "sh",
                "-c",
                "exec cat > \"$0\"",
                received.to_str().expect("a utf-8 temp path"),
            ],
        )
        .await;
        eventually("the stub to open its file", async || {
            if received.exists() {
                Ok(())
            } else {
                Err("no file yet".to_owned())
            }
        })
        .await;

        // ~4 MiB: two orders of magnitude past any pipe buffer (64 KiB at
        // most), so one poll of the writer ends in a full pipe, not in EOF.
        let dropped: String = (0..65536)
            .map(|line| format!("line {line:05}: the quick brown fox jumps over the lazy dog\n"))
            .collect();
        tokio::select! {
            biased;
            outcome = at.paste_submit(&pane.id, &dropped) => {
                panic!("a delivery this size cannot finish in one poll: {outcome:?}");
            }
            () = std::future::ready(()) => {}
        }
        // The select dropped the delivery, and with it the client. No await
        // has passed since: whatever the server holds for that client, it
        // is not a buffer — one is set only from a read that ended.
        assert_eq!(
            server.run(&["list-buffers"]).trim(),
            "",
            "a dropped delivery left no buffer on the server"
        );

        // The pane is untouched, and the next delivery is the first thing it
        // hears: had the dropped one pasted, the file would open with its
        // text; had it pressed Enter, with a newline.
        at.paste_submit(&pane.id, "after\n")
            .await
            .expect("the next delivery lands");
        eventually("the next delivery to reach the stub's file", async || {
            let got = std::fs::read_to_string(&received).map_err(|error| error.to_string())?;
            if got == "after\n\n" {
                Ok(())
            } else {
                Err(format!("{:?}", got.chars().take(120).collect::<String>()))
            }
        })
        .await;
        assert_eq!(
            server.run(&["list-buffers"]).trim(),
            "",
            "and the server's buffer stack is as the test found it"
        );
    }

    /// Text larger than a pipe holds is handed to tmux whole through stdin
    /// — the write and the wait run together, so neither side wedges the
    /// other — and none of it is on argv. The buffer is named the way every
    /// delivery's is and freed the way every delivery's is, so the test
    /// bypasses neither rule the module establishes.
    #[tokio::test]
    async fn a_large_text_is_handed_to_tmux_through_stdin_whole() {
        let dir = ganja_testkit::temp_dir();
        let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
        let at = Server::at(server.socket(), None);
        // ~230 KiB: past any pipe buffer this machine hands out (64 KiB at
        // most), so a write that waited for the read to finish would stall.
        let text: String = (0..4096)
            .map(|line| format!("line {line:04}: the quick brown fox jumps over the lazy dog\n"))
            .collect();
        let buffer = buffer_name();

        let mut load = at.command();
        load.arg("load-buffer").arg("-b").arg(&buffer).arg("-");
        super::feed("load-buffer", load, text.as_bytes())
            .await
            .expect("tmux takes the whole text");

        let saved = dir.path().join("buffer.txt");
        server.run(&[
            "save-buffer",
            "-b",
            &buffer,
            saved.to_str().expect("a utf-8 temp path"),
        ]);
        assert_eq!(
            std::fs::read_to_string(&saved).expect("the saved buffer reads"),
            text
        );

        server.run(&["delete-buffer", "-b", &buffer]);
        assert_eq!(
            server.run(&["list-buffers"]).trim(),
            "",
            "the test leaves the server's buffer stack as it found it"
        );
    }

    /// A pane kept on exit is still there after its process ended — dead,
    /// its last words readable through `capture` — while the liveness
    /// listing neither lists it (tmux's own word is that it is dead, and a
    /// pane with no process has no pair) nor refuses because of it, so an
    /// identity-checked kill of the recorded pair finds it already gone and
    /// leaves the dead pane on screen for a person to read. Against a real
    /// server, this is also where the listing format's dead shape is pinned:
    /// a line the parser could not read would fail the listing here.
    #[tokio::test]
    async fn a_pane_kept_on_exit_stays_readable_after_its_process_dies() {
        let dir = ganja_testkit::temp_dir();
        let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
        let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
        let pane = split(
            &at,
            dir.path(),
            &[
                "sh",
                "-c",
                "read line; printf 'last words: %s\\n' \"$line\"; exit 1",
            ],
        )
        .await;

        at.remain_on_exit(&pane.id, true)
            .await
            .expect("the pane option is set");
        assert_eq!(
            server
                .run(&["show-options", "-p", "-v", "-t", &pane.id, "remain-on-exit"])
                .trim(),
            "on"
        );

        at.type_line(&pane.id, OsStr::new("refused by the vendor"))
            .await
            .expect("the stub hears its line");
        eventually("the pane's process to die", async || {
            let dead = server.run(&["display-message", "-p", "-t", &pane.id, "#{pane_dead}"]);
            if dead.trim() == "1" {
                Ok(())
            } else {
                Err(format!("pane_dead={}", dead.trim()))
            }
        })
        .await;

        assert!(
            server.panes().contains(&pane.id),
            "the dead pane is still on the server"
        );
        let shown = at
            .capture(&pane.id)
            .await
            .expect("a dead pane still captures");
        assert!(
            shown
                .lines()
                .any(|line| line == "last words: refused by the vendor"),
            "its last words are readable: {shown:?}"
        );

        let live = at
            .panes()
            .await
            .expect("a dead pane does not make the listing unreadable");
        assert!(
            !live.iter().any(|listed| listed.id == pane.id),
            "and it is not in the liveness listing: {live:?}"
        );
        assert_eq!(
            at.kill(&pane).await.expect("the kill reads the listing"),
            Killed::AlreadyGone
        );
        assert!(
            server.panes().contains(&pane.id),
            "the kill left the dead pane where it was"
        );
    }

    /// Turned back off, the option is really off: the pane closes with its
    /// process, and there is nothing left to capture.
    #[tokio::test]
    async fn a_pane_no_longer_kept_on_exit_closes_with_its_process() {
        let dir = ganja_testkit::temp_dir();
        let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
        let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
        let pane = split(&at, dir.path(), &["sh", "-c", "read line; exit 1"]).await;

        at.remain_on_exit(&pane.id, true).await.expect("on");
        at.remain_on_exit(&pane.id, false)
            .await
            .expect("and off again");
        at.type_line(&pane.id, OsStr::new("bye"))
            .await
            .expect("the stub hears its line");

        eventually("the pane to close", async || {
            let listed = server.panes();
            if listed.contains(&pane.id) {
                Err(format!("still listed: {listed:?}"))
            } else {
                Ok(())
            }
        })
        .await;
        assert!(
            matches!(
                at.capture(&pane.id).await,
                Err(TmuxError::Failed {
                    command: "capture-pane",
                    ..
                })
            ),
            "a closed pane has nothing to capture"
        );
    }

    /// `close_dead` ends a pane only once its process has: asked of a live
    /// pane it answers `Alive` and touches nothing, asked of the same pane
    /// dead it closes it, asked again it finds nothing by that id — and an
    /// id the listing does not print never reaches the server's command
    /// string, which is what keeps a recorded id from carrying a second
    /// command in. The three answers a real server produces; the fourth,
    /// `Refused`, needs a `kill-pane` that does not take, which a real
    /// server cannot cheaply be made to do, and is pinned on `after_kill`
    /// without one.
    #[tokio::test]
    async fn close_dead_closes_a_dead_pane_and_leaves_a_live_one_alone() {
        let dir = ganja_testkit::temp_dir();
        let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
        let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
        let pane = split(&at, dir.path(), &["sh", "-c", "read line; exit 1"]).await;
        at.remain_on_exit(&pane.id, true)
            .await
            .expect("kept on exit");

        assert_eq!(
            at.close_dead(&pane.id)
                .await
                .expect("a live pane is classified, not an error"),
            Closed::Alive
        );
        assert!(
            server.panes().contains(&pane.id),
            "and was left where it was"
        );
        assert_eq!(
            server
                .run(&["display-message", "-p", "-t", &pane.id, "#{pane_dead}"])
                .trim(),
            "0",
            "with its process still running"
        );

        at.type_line(&pane.id, OsStr::new("bye"))
            .await
            .expect("the stub hears its line");
        eventually("the pane's process to die", async || {
            let dead = server.run(&["display-message", "-p", "-t", &pane.id, "#{pane_dead}"]);
            if dead.trim() == "1" {
                Ok(())
            } else {
                Err(format!("pane_dead={}", dead.trim()))
            }
        })
        .await;

        assert_eq!(
            at.close_dead(&pane.id).await.expect("a dead pane closes"),
            Closed::Yes
        );
        assert!(
            !server.panes().contains(&pane.id),
            "the dead pane is gone: {:?}",
            server.panes()
        );
        assert_eq!(
            at.close_dead(&pane.id)
                .await
                .expect("nothing by that id is not an error"),
            Closed::AlreadyGone
        );

        // An id that is not one tmux printed — here one that would read as a
        // second command inside `if-shell`'s string — is answered off the
        // listing alone, and the server is still standing afterwards.
        let forged = format!("{}; kill-server", server.first_pane());
        assert_eq!(
            at.close_dead(&forged)
                .await
                .expect("an unknown id is not an error"),
            Closed::AlreadyGone
        );
        assert!(
            server.panes().contains(&server.first_pane().to_owned()),
            "and nothing reached the server"
        );
    }
}
