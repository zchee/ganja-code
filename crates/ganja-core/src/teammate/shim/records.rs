//! The net under a lead that was `SIGKILL`'d: which shim children it owned,
//! and how a later lead proves they are orphans before it signals one
//! (**D508**).
//!
//! No upstream counterpart and no Claude Code counterpart: neither harness
//! runs somebody else's CLI as a teammate, so neither has a child that can
//! outlive the process that started it. [`crate::teammate::reaper`] is the
//! same idea for panes and is emphatically **not** the same mechanism — its
//! witness is an argv (`--agent-id` and `--parent-session-id` on the pane's
//! own process, D506), which a foreign CLI child carries neither of, and it
//! records no start time at all because tmux publishes none.
//!
//! # Three layers, and this is only the third
//!
//! Every shim child is spawned into its own process group and killed as a
//! group, so an ordinary shutdown or drop ends it from inside this process;
//! a resident child also holds a stdin pipe from the lead, so lead death
//! closes it and a well-behaved CLI exits on EOF. What is left over is the
//! `SIGKILL`-of-lead × stubborn-child case, and that is what this file is
//! for. It is also the only part of the design that can touch a process it
//! does not own, which is why the whole of it is one rule:
//!
//! > **No owner proof, no signal.** A signal is sent only where the sweep can
//! > prove the recording lead is gone *and* prove the recorded child is the
//! > one that was recorded. Anything it cannot prove is logged, never killed.
//!
//! # The file
//!
//! One file per lead session, at `<socket directory>/<stem>-<lead pid>.shims`,
//! mode `0600`, inside the `0700` directory D505's socket scheme already
//! describes. The extension is chosen so that
//! [`is_session_socket_name`](ganja_tool::socket::is_session_socket_name)
//! never classifies it — a session socket's name carries exactly `sock` — so
//! `ganja sessions --live` walks straight past it.
//!
//! The **lead's pid is in the name on purpose**. The stem alone collides:
//! two sessions minted inside one 65-second UUIDv7 bucket share their first
//! eight hex digits, which is the whole reason the binder walks candidates at
//! all. Two live leads sharing one record file would whole-file-rewrite over
//! each other, and while every individual failure of that is fail-safe, the
//! net would silently stop working for one of them — in exactly the
//! concurrency case this design exists to survive. A pid disambiguates
//! without a lock, and the pid is already the identity the owner line
//! carries.
//!
//! Three kinds of line, and the first two are positional:
//!
//! ```text
//! ganja-shims-1
//! 4711<TAB>Wed Aug 19 14:54:57 2026
//! codex<TAB>4823<TAB>4823<TAB>Wed Aug 19 14:55:02 2026
//! ```
//!
//! Line 0 is the format version, line 1 is the recording lead's own
//! `(pid, start-time)`, and every line after it is one live child as
//! `(cli, pid, pgid, start-time)`. Tab-separated because a rendered start
//! time carries spaces and no tabs, and the time is last on its line either
//! way.
//!
//! # One primitive, and that is the safety argument
//!
//! The owner check and the child check are the same call —
//! [`started_at`] — so any systematic failure of it (`ps` missing, refusing,
//! renaming its columns, rendering in some unforeseen way) fails **both**,
//! and every such failure lands on an arm that signals nothing. A broken
//! primitive produces a skipped file and a log line, never a kill.
//!
//! Which owner failures fail closed is worth stating exactly, because one of
//! them does not. A **missing** owner line, an **unparseable** one, and one
//! whose liveness could not be determined each skip the file. An owner pid
//! that exists with a **different start time** is read as gone — fail-open on
//! the owner — and its safety comes from the child lines instead: they are
//! compared with the same primitive on the same rendered bytes, so they
//! mismatch for whatever reason the owner's did and no signal is sent. That
//! arm earns the one hardening clause in the sweep's retention rule
//! ([`ShimFate`](crate::teammate::reaper::ShimFate)): such a file is swept but
//! never unlinked.
//!
//! # The identity token is a rendering, so the renderer is pinned
//!
//! `ps -o lstart=` prints through libc's zone and locale rules at *render*
//! time: the same live process reads `[Wed Aug 19 23:54:57 2026]` here and
//! `[Wed Aug 19 14:54:57 2026]` under `TZ=UTC`. So [`started_at`] sets
//! `TZ=UTC` and `LC_ALL=C` in the child's environment — the one thing it does
//! that [`crate::teammate::reaper`]'s `argv_of` does not — the owner line is
//! **re-derived on every rewrite and never cached**, and the comparison is
//! over the rendered bytes rather than a parsed timestamp. Recording an
//! absolute epoch instead is not available on this platform: macOS `ps` has
//! no `etimes` keyword, and its `etime` is an elapsed duration that changes
//! with the sampling moment and so is not an identity at all.

use std::{
    fmt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use ganja_team::ShimCli;
use ganja_tool::socket::{DirectoryRefusal, SOCKET_MODE};

/// The format token line 0 carries.
///
/// A version rather than a retention window in days, because it turns "how
/// old is too old" into a question about *who wrote the file*: a token this
/// build does not know means a newer lead owns it, and a newer lead's records
/// are not this build's to delete.
pub const VERSION: &str = "ganja-shims-1";

/// The extension a records file carries. Never [`ganja_tool::socket::EXTENSION`],
/// so the socket lister cannot see one.
pub const EXTENSION: &str = "shims";

/// The extension a rewrite stages under before its `rename(2)`.
///
/// Inside the sweep's own `*.shims` glob **on purpose**: a crash between the
/// write and the rename leaves this file behind, and the sweep's header-less
/// arm is what removes it. A distinct extension would need a second glob
/// where one name will do.
pub const TEMP_EXTENSION: &str = "tmp.shims";

/// What `ps` said about one pid.
///
/// Three answers rather than two, and the third is the whole of the
/// fail-closed rule: "gone" and "could not be established" must never be the
/// same branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Started {
    /// The process exists, and this is how its start time renders under the
    /// pinned `TZ`/`LC_ALL`.
    At(String),
    /// `ps` answered, and said there is no such process.
    Gone,
    /// `ps` could not be asked, or answered in a way this build cannot read.
    Unknown,
}

/// How one process was born, as a record identifies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The process id.
    pub pid: i32,
    /// [`started_at`]'s rendering of when it started.
    pub started: String,
}

impl Identity {
    /// Whether a process is *this* process: alive, and born when this says.
    ///
    /// Both halves matter and neither is enough. A pid alone is recycled; a
    /// start time alone identifies nothing. [`Started::Unknown`] is a `false`
    /// here as much as [`Started::Gone`] is, because the callers of this ask
    /// "may I signal" and the answer to "I could not tell" is no.
    #[must_use]
    pub fn matches(&self, found: &Started) -> bool {
        matches!(found, Started::At(rendered) if *rendered == self.started)
    }
}

/// One shim child, as its lead recorded it.
///
/// Named for the record rather than for the process, so `records::Recorded`
/// and the shim handle's own `Child` cannot be confused at a call site that
/// holds both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    /// Which CLI it is, so a log line can name what is being ended.
    pub cli: ShimCli,
    /// The child itself, and the identity a kill is gated on.
    pub process: Identity,
    /// The group to signal, which is the child's own: every shim child is
    /// spawned as its own group leader, so this is its pid — recorded beside
    /// it rather than derived, because the leader can die while the group
    /// lives and that case is decided on the two values separately.
    pub pgid: i32,
}

/// A records file, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Records {
    /// The lead that wrote it.
    pub owner: Identity,
    /// What it owned when it last wrote.
    pub children: Vec<Recorded>,
}

/// Why a records file could not be read as one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unreadable {
    /// There is no readable line 0 — a zero-byte file, a header-less one, or
    /// a temporary left by a crash between a write and its rename. No live
    /// lead can publish one, since a same-version writer publishes only whole
    /// files by `rename(2)`.
    Headerless,
    /// Line 0 names a version this build does not know: a newer lead owns
    /// this file.
    Version {
        /// What it called itself, capped so a corrupt file cannot fill a log
        /// line.
        token: String,
    },
    /// A known version, and content that is not this format's. Since a
    /// same-version writer is atomic, this can only be corruption.
    Malformed {
        /// What was wrong with it, for the log.
        reason: &'static str,
    },
}

impl fmt::Display for Unreadable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Headerless => formatter.write_str("it carries no version line"),
            Self::Version { token } => write!(
                formatter,
                "it is version {token}, which this build does not know"
            ),
            Self::Malformed { reason } => write!(formatter, "it is this version and {reason}"),
        }
    }
}

impl Unreadable {
    /// How much of an unknown version token reaches a log line.
    const TOKEN_CAP: usize = 32;

    /// Whether a file this unreadable may be unlinked.
    ///
    /// The whole of the sweep's middle two retention arms in one place: a
    /// header-less file and a corrupt one of **this** version have no future
    /// reader, while
    /// a file belonging to a newer build is somebody else's to remove.
    #[must_use]
    pub const fn removable(&self) -> bool {
        match self {
            Self::Headerless | Self::Malformed { .. } => true,
            Self::Version { .. } => false,
        }
    }
}

/// The name a lead's records file takes.
///
/// `<stem>-<pid>.shims`, and see the module doc for why the pid is in it.
#[must_use]
pub fn path_for(directory: &Path, stem: &str, lead_pid: i32) -> PathBuf {
    directory.join(format!("{stem}-{lead_pid}.{EXTENSION}"))
}

/// Where a rewrite of [`path_for`]'s file is staged.
#[must_use]
pub fn temp_path_for(directory: &Path, stem: &str, lead_pid: i32) -> PathBuf {
    directory.join(format!("{stem}-{lead_pid}.{TEMP_EXTENSION}"))
}

/// The file-name stem a session's records take.
///
/// The session's first eight hex digits where it has that many — dashes and
/// any non-hex ignored, case folded, the same eight the binder's shortest
/// candidate uses, so a person looking at the directory can pair a `.shims`
/// with a `.sock` at a glance — and a sanitized fallback for an id that
/// predates UUIDv7. The
/// fallback needs no ceremony because a `.shims` name is never measured against
/// [`is_session_stem`](ganja_tool::socket::is_session_stem): what makes two
/// leads' files distinct is the pid, not the stem.
#[must_use]
pub fn stem_of(session_id: &str) -> String {
    let hex: String = session_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|digit| digit.to_ascii_lowercase())
        .take(ganja_tool::socket::SHORTEST_NAME)
        .collect();
    if hex.len() == ganja_tool::socket::SHORTEST_NAME {
        return hex;
    }

    let sanitized: String = session_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .take(ganja_tool::socket::LONGEST_NAME)
        .collect();

    if sanitized.is_empty() {
        "session".to_owned()
    } else {
        sanitized
    }
}

/// How `pid`'s start time renders, under the pinned zone and locale.
///
/// The one primitive, called for owners and for children alike — see the
/// module doc for why that sameness is the safety argument rather than a
/// saving.
///
/// Blocking on purpose. Its two callers are a records rewrite, which happens
/// under a `std::sync::Mutex` and must not hold a guard across an `await`,
/// and the sweep, which runs as one blocking job. An async spelling would
/// force one of them to be wrong.
#[must_use]
pub fn started_at(pid: i32) -> Started {
    // A non-positive pid is never a live process the sweep tracks, and the
    // existence probe below reads 0 as "my own process group" and a negative
    // value as a broadcast target rather than an existence question — so it is
    // answered here instead of letting the probe signal the wrong thing.
    if pid <= 0 {
        return Started::Gone;
    }

    let Ok(output) = Command::new("ps")
        // `lstart` is the only start-time keyword this platform's `ps` has —
        // `etimes` does not exist here, and `etime` is an elapsed duration
        // rather than an identity.
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        // Renders through libc's zone and locale rules, so both are pinned:
        // the same process must read the same bytes in October as in August.
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    else {
        // `ps` could not be run at all.
        return Started::Unknown;
    };

    let rendered = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() && !rendered.is_empty() {
        return Started::At(rendered);
    }

    // `ps` rendered no start time, and its exit says nothing portable about
    // why. Measured on both: a *nonexistent* pid is a silent non-zero exit on
    // macOS and on procps alike, and each writes to stderr only for a pid it
    // refuses to parse — but they draw that line in different places. macOS
    // accepts 0 silently and complains above 99999; procps complains at <= 0
    // and is silent above pid_max. So empty stderr never meant "gone", it
    // meant "ps took the argument", and the old heuristic only looked right on
    // macOS because the two pids the tests used fell either side of its bound.
    // `kill(pid, 0)` is the portable existence probe: `ESRCH` is the only proof
    // of "gone", and anything else — a live pid (`0`), one that is not ours
    // (`EPERM`), or an unforeseen error — is not proof and stays `Unknown`.
    // SAFETY: signal 0 sends nothing; it only asks whether the pid exists and
    // whether this process could signal it. `pid` is a plain integer, already
    // guarded positive above, and the call cannot touch this process's memory.
    match unsafe { libc::kill(pid, 0) } {
        0 => Started::Unknown,
        _ if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) => Started::Gone,
        _ => Started::Unknown,
    }
}

/// The lead's own identity, re-derived now.
///
/// Never cached, and the reason is a killed bystander rather than tidiness: a
/// value held since construction would be re-emitted beside a
/// freshly-rendered child line after a zone transition, so a later sweep
/// would read the owner as gone and the child as matching — a live lead's
/// child ended through the one door the owner line exists to close.
#[must_use]
pub fn own_identity() -> Option<Identity> {
    let pid = own_pid();
    match started_at(pid) {
        Started::At(started) => Some(Identity { pid, started }),
        Started::Gone | Started::Unknown => None,
    }
}

/// This process's pid.
#[must_use]
pub fn own_pid() -> i32 {
    // SAFETY: `getpid` takes nothing, touches nothing, and cannot fail.
    unsafe { libc::getpid() }
}

/// This process's own process group, which no sweep may ever signal.
#[must_use]
pub fn own_pgid() -> i32 {
    // SAFETY: `getpgrp` takes nothing, touches nothing, and cannot fail.
    unsafe { libc::getpgrp() }
}

/// Renders a records file.
#[must_use]
pub fn render(records: &Records) -> String {
    let mut text = String::with_capacity(64 + records.children.len() * 48);
    text.push_str(VERSION);
    text.push('\n');
    text.push_str(&format!(
        "{}\t{}\n",
        records.owner.pid, records.owner.started
    ));
    for child in &records.children {
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            child.cli.backend_type(),
            child.process.pid,
            child.pgid,
            child.process.started
        ));
    }

    text
}

/// Reads back what [`render`] wrote.
///
/// # Errors
///
/// [`Unreadable`], whose variants are exactly the three retention arms a parse
/// failure maps to: a header-less file, a version this build does not own, and
/// corruption of a version it does. A read that failed never reaches here —
/// [`sweep_file`](crate::teammate::reaper) decides it at its own
/// `read_to_string` arm.
pub fn parse(text: &str) -> Result<Records, Unreadable> {
    let mut lines = text.lines();
    let Some(version) = lines.next() else {
        return Err(Unreadable::Headerless);
    };
    if version != VERSION {
        // A blank first line is header-less rather than a foreign version:
        // there is no token to attribute the file to anybody.
        if version.trim().is_empty() {
            return Err(Unreadable::Headerless);
        }

        return Err(Unreadable::Version {
            token: version.chars().take(Unreadable::TOKEN_CAP).collect(),
        });
    }

    let Some(owner) = lines.next() else {
        return Err(Unreadable::Malformed {
            reason: "it names no owner",
        });
    };
    let Some((pid, started)) = owner.split_once('\t') else {
        return Err(Unreadable::Malformed {
            reason: "its owner line has no start time",
        });
    };
    let Ok(pid) = pid.parse::<i32>() else {
        return Err(Unreadable::Malformed {
            reason: "its owner line names no pid",
        });
    };
    if started.trim().is_empty() {
        return Err(Unreadable::Malformed {
            reason: "its owner line has an empty start time",
        });
    }

    let mut children = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = line.splitn(4, '\t');
        let parsed = fields
            .next()
            .and_then(ShimCli::read)
            .zip(fields.next().and_then(|pid| pid.parse::<i32>().ok()))
            .zip(fields.next().and_then(|pgid| pgid.parse::<i32>().ok()))
            .zip(fields.next().filter(|started| !started.trim().is_empty()));
        let Some((((cli, pid), pgid), started)) = parsed else {
            return Err(Unreadable::Malformed {
                reason: "one of its child lines is not four fields",
            });
        };
        children.push(Recorded {
            cli,
            process: Identity {
                pid,
                started: started.to_owned(),
            },
            pgid,
        });
    }

    Ok(Records {
        owner: Identity {
            pid,
            started: started.to_owned(),
        },
        children,
    })
}

/// The single writer of one lead's records file.
///
/// Held by [`crate::teammate::TeammateRegistry`] behind a
/// `std::sync::Mutex`, which is the whole of the write concurrency answer: a
/// per-message shim registers once per *turn*, so several turn tasks would
/// otherwise read-modify-write one file. They do not — every mutation goes
/// through one value under one lock, the same single-writer discipline the
/// registry already keeps for its task list. Nothing here awaits, so no guard
/// is ever held across one.
#[derive(Debug)]
pub struct ShimRecords {
    directory: PathBuf,
    stem: String,
    lead_pid: i32,
    children: Vec<Recorded>,
    /// Whether the directory has been refused. Logged once and then inert:
    /// layer 3 is off for this session, which is an honest degradation rather
    /// than a failure to report on every spawn.
    refused: bool,
}

impl ShimRecords {
    /// The writer for `session_id`'s records under `directory`.
    #[must_use]
    pub fn new(directory: PathBuf, session_id: &str) -> Self {
        Self {
            directory,
            stem: stem_of(session_id),
            lead_pid: own_pid(),
            children: Vec::new(),
            refused: false,
        }
    }

    /// The directory the records live in — what the sweep enumerates, and
    /// what the first write creates.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Where this writer's file lives.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        path_for(&self.directory, &self.stem, self.lead_pid)
    }

    /// Records a child that has just started, and rewrites the file.
    ///
    /// Best-effort by construction: a records file that could not be written
    /// costs the *next* lead its net, and refusing a spawn over it would cost
    /// this one its teammate.
    pub fn add(&mut self, child: Recorded) {
        self.children
            .retain(|held| held.process.pid != child.process.pid);
        self.children.push(child);
        self.publish();
    }

    /// Forgets a child that has exited, and rewrites the file.
    pub fn remove(&mut self, pid: i32) {
        let before = self.children.len();
        self.children.retain(|held| held.process.pid != pid);
        if self.children.len() != before {
            self.publish();
        }
    }

    /// What this writer currently believes it owns.
    #[must_use]
    pub fn children(&self) -> &[Recorded] {
        &self.children
    }

    /// Writes the whole file, atomically.
    ///
    /// "Rewrite under the lock" is not atomic on its own: a lead `SIGKILL`'d
    /// mid-rewrite, or a sweep reading during one, would see a truncated file
    /// whose owner line is missing — which fails closed, so the orphans would
    /// never be reaped, in exactly the crash case this net exists for. So the
    /// staging file is written and then `rename(2)`d over the name, which is
    /// atomic on every filesystem this runs on and needs no dependency.
    fn publish(&mut self) {
        if self.refused {
            return;
        }
        // The directory is made here rather than at lead start: a session that
        // never spawns a shim teammate has no records to keep, and creating a
        // private directory in order to hold nothing is a directory made for
        // no reason.
        if let Err(refusal) = ganja_tool::socket::prepare_directory(&self.directory) {
            self.refuse(&refusal);

            return;
        }
        let Some(owner) = own_identity() else {
            // The one thing that cannot be worked around: without the owner
            // line a later sweep must fail closed, so publishing children
            // under no owner would be publishing a file that can never
            // authorize anything. Better to leave the old one, which at least
            // names an owner that will read as gone.
            tracing::warn!(
                "this lead's own start time could not be read, so its shim records were not \
                 rewritten"
            );

            return;
        };
        let records = Records {
            owner,
            children: self.children.clone(),
        };
        if let Err(error) = self.write(&render(&records)) {
            tracing::warn!(
                %error,
                "a shim child could not be recorded, so a crash would leave it to be found by \
                 hand"
            );
        }
    }

    /// Stages, chmods and renames.
    fn write(&self, text: &str) -> std::io::Result<()> {
        use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};

        let staging = temp_path_for(&self.directory, &self.stem, self.lead_pid);
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(SOCKET_MODE)
                .open(&staging)?;
            file.write_all(text.as_bytes())?;
            // The rename publishes; a sync before it is what makes the
            // published bytes the ones a reader after a crash sees.
            file.sync_all()?;
        }
        // `set_permissions` after the fact too, because `mode` only applies to
        // a file this call created and a staging file left by an earlier crash
        // is reused.
        std::fs::set_permissions(
            &staging,
            std::os::unix::fs::PermissionsExt::from_mode(SOCKET_MODE),
        )?;

        std::fs::rename(&staging, self.path())
    }

    /// Says once that the directory is not somewhere records can live.
    fn refuse(&mut self, refusal: &DirectoryRefusal) {
        self.refused = true;
        tracing::warn!(
            directory = %self.directory.display(),
            %refusal,
            "shim children cannot be recorded here, so a lead that is killed will leave them \
             to be found by hand"
        );
    }
}

#[cfg(test)]
#[path = "records_tests.rs"]
mod tests;
