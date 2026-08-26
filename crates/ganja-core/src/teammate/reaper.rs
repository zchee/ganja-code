//! Panes a lead left behind when it died (**D506**).
//!
//! Upstream opencode has **no counterpart**, and neither does Claude Code:
//! §10.4's own closing line is "add what Claude lacks: an orphan reaper at
//! lead startup, because a dead lead leaves live panes either way". A lead
//! that is killed rather than shut down sends no `shutdown_request`, reads no
//! `shutdown_approved` and calls no `kill-pane` (§6.2), so its teammates' panes
//! keep running with nobody left to read what they write — and the team file
//! it wrote is the only record that they exist at all. Sweeping that file when
//! a lead next opens the team is what keeps a crashed session from costing
//! somebody a screen full of orphans.
//!
//! # D506 — a recorded pane is ended on what it is *running*, not on what was recorded
//!
//! **`%N` recycles** (§10.10). tmux hands a dead pane's id to the next pane it
//! makes, so a sweep that killed by id alone would eventually kill somebody's
//! editor, and "verify identity before `kill-pane`" is the hazard's own
//! mitigation. The identity a *live* lead verifies is the pair its spawn
//! recorded — `(pane_id, pane_pid)`, held in memory by
//! [`crate::teammate::TeammateRegistry`] and re-checked inside
//! [`crate::teammate::tmux::Server::kill`]. This module cannot have that pair:
//! it runs cold, and the lead that held it is the process that died.
//!
//! Nor is the pair on disk, deliberately. `ganja-team`'s record keeps birth out
//! of Claude's document on purpose — a key invented for it would be an
//! unstated amendment to a format somebody else owns — so what a team file
//! carries about a pane is its `tmuxPaneId` and nothing more. Persisting the
//! pid anyway would also buy less than it looks: pane ids restart at `%0` and
//! pids are reissued from a small space, so a pair recorded before a reboot can
//! *match* a stranger's pane after one. A pid is an identity for as long as the
//! machine keeps running, and the case this module exists for is exactly the
//! one where something stopped.
//!
//! So the second half of the identity is re-derived, from the pane itself: the
//! command line of the pane's first process must carry **both**
//! [`crate::teammate::pane::AGENT_ID`] with the record's own `agentId`
//! (`<name>@<team>`) **and** [`crate::teammate::pane::PARENT_SESSION_ID`] with
//! this lead's session — as flag-and-value pairs, not as substrings anywhere in
//! the line. That process *is* the teammate:
//! [`crate::teammate::pane`] types `exec` into the shell tmux forked, so
//! `#{pane_pid}` keeps naming the binary that replaced it, and both pane
//! backends put both flags on that command line (§4.1). A pane that cannot show
//! the pair is not this teammate's, whatever id it wears, and is left alone.
//!
//! Each half of the witness answers a way the other one alone is wrong, and
//! both were live defects before they were paired:
//!
//! - **the pair, rather than a substring.** `argv.contains(agent_id)` reads
//!   `build@session-01998ad0` inside `rebuild@session-01998ad0`, so a member
//!   whose name is a *suffix* of a sibling's would have the sibling killed the
//!   moment tmux reissued the dead pane's `%N` to it. Matching a flag against
//!   the word that follows it cannot do that: two agent ids are either equal or
//!   they are different words.
//! - **the lead's own session.** §2.1's team name is `session-<first 8 hex of
//!   the lead's session id>`, and a UUIDv7's first eight hex digits are its
//!   millisecond timestamp shifted down by sixteen bits — a **65.536-second
//!   bucket**. Two leads that start inside one bucket therefore share a team
//!   *file*, and [`crate::teammate::TeammateRegistry`]'s record write never
//!   restamps `leadSessionId`, so the document keeps naming whichever of them
//!   created it. The guard below then passes for that lead when it resumes, and
//!   without this half of the witness its sweep would end a **live** co-tenant
//!   lead's teammates. `--parent-session-id` is the pane's own answer to which
//!   lead launched it, and it is on every launch line already.
//!
//! Two further properties fall out and are worth naming: the check survives a
//! reboot, since it asks what is running rather than what once was; and a
//! record naming the **lead's own pane** cannot kill it, because the lead's
//! process carries neither flag.
//!
//! # Two decisions, not one
//!
//! Killing a pane and dropping a record are separate questions, and this module
//! answers them separately (the four [`Fate`](crate::teammate::reaper::Fate)s):
//!
//! - the pane is this teammate's → it is an orphan of a dead lead: **killed**,
//!   and its record goes with it ([`Fate::Reaped`](crate::teammate::reaper::Fate::Reaped));
//! - no pane wears that id → nothing to kill, and the record names a teammate
//!   that is gone: **dropped** ([`Fate::Vanished`](crate::teammate::reaper::Fate::Vanished));
//! - a pane wears the id but is not this teammate's → **never killed**; the
//!   record is still dropped, because the teammate it named is not running
//!   anywhere ([`Fate::Recycled`](crate::teammate::reaper::Fate::Recycled));
//! - the question could not be answered — tmux refused, or the pane's process
//!   could not be looked at → **nothing happens at all**
//!   ([`Fate::Undecided`](crate::teammate::reaper::Fate::Undecided)). A record
//!   whose pane cannot be examined is not a record known to be stale.
//!
//! A sweep that finds nothing writes nothing and says nothing.
//!
//! # Whose panes this may sweep
//!
//! Only this team's, and only when the team is this lead's. The registry knows
//! one team — §2.1's `session-<first 8 hex of the lead's session id>` — so the
//! sweep walks that file and no other, and it refuses even that one when the
//! document's `leadSessionId` is not this lead's session.
//!
//! **That guard is necessary and it is not sufficient.** It stops the
//! obvious case — the shared fallback team (`default`, which every session with
//! a pre-UUIDv7 id joins), whose document names one lead and is read by many.
//! What it cannot stop is two leads that share a *derived* team name: the name
//! buckets 65.536 seconds of session ids together (above), the record write
//! never restamps `leadSessionId`, and so a document created by lead A goes on
//! naming A while holding B's members too. A's own sweep passes this guard on
//! its own team. The witness is what keeps it from killing B's live panes —
//! `--parent-session-id` on the pane says B launched it — and the guard's
//! remaining job is the case where the document is not even nominally this
//! lead's.
//!
//! Within a team that *is* this lead's, a pane member found before this lead has
//! spawned anything cannot be this lead's own, by construction: the sweep runs
//! at startup, ahead of every spawn. Which is also the scope of the whole
//! module, and worth saying plainly: a **fresh** session derives a team name
//! nothing has written yet, so it reads no file and sweeps nothing. In practice
//! this fires for a **resumed** session — `--continue`, or `--session <id>` —
//! which is the one that can meet a team file older than the process reading
//! it. A document with no `leadSessionId` at all is not a document this build
//! guesses about: the field is required, so serde refuses it, the read fails,
//! and the sweep answers an empty [`Swept`](crate::teammate::reaper::Swept) —
//! nothing killed, nothing dropped.
//!
//! One case is left, and it is stated rather than mitigated: two processes
//! resuming the *same* session id at once. Only one of them is that session,
//! and this build has no way to ask which — the same fused-session hazard
//! `--continue` on an already-open session carries everywhere else. Both would
//! also put the same `--parent-session-id` on their panes, so the witness does
//! not separate them either.
//!
//! Members with no pane at all — the lead's record, and in-process teammates —
//! are passed over. An in-process teammate's row is as stale as an orphaned
//! pane's after its lead dies, but nothing about it is a *pane*, and D506 is a
//! ruling about panes; retiring those rows is a separate decision nobody has
//! taken.
//!
//! # The windows this leaves open
//!
//! A pane that was split but whose launch line has not been typed yet is
//! holding [`crate::teammate::pane::SHELL`] and carries no agent id, so a sweep
//! meeting one in that window reads it as a stranger: the pane is left alive
//! and the record is dropped, and an idle `sh` pane stays on the screen until a
//! person closes it. The window is the few milliseconds between the split and
//! the record's arrival on disk, it takes a lead dying inside them to be
//! reached, and the alternative — killing on a weaker witness — is the one
//! failure this module exists to prevent.
//!
//! The co-tenant case above leaves a smaller one of the same shape, and it is a
//! deliberate trade rather than an oversight. A live co-tenant lead's pane is
//! read as [`Fate::Recycled`](crate::teammate::reaper::Fate::Recycled) — *not
//! this teammate*, correctly, so it is never
//! killed — and a `Recycled` verdict still drops the record, because the
//! ordinary reason for one is a stale row over somebody else's window. So A's
//! sweep takes B's member rows out of the team file they share. What that costs:
//! B's own roster is in memory and unaffected, and B's retire of that member
//! finds nothing to remove and says so; but if B then dies, its panes are
//! orphans no team file names, which is the "stranded until a person closes it"
//! outcome the paragraph above already describes. Distinguishing "somebody
//! else's window" from "another lead's teammate" would need a fifth fate and a
//! rule about whose record a lead may drop; the kill is the harm that had to
//! stop, and it has.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use ganja_team::{ShimCli, Surface};

use crate::teammate::{
    TeammateRegistry,
    pane::{AGENT_ID, PARENT_SESSION_ID},
    shim::{
        records::{self, Started, Unreadable},
        signal_group,
    },
    tmux::{Killed, Server},
};

/// What answers, for a live pane's first process, what it was started as.
///
/// `ps(1)` rather than a crate: the one fact wanted is the command line of one
/// pid, the shape of the answer is POSIX, and this module already lives beside
/// one that speaks to the world by running a client
/// ([`crate::teammate::tmux`]).
const PS: &str = "ps";

/// A pane as something recorded it: the id, and the birth that disambiguates a
/// recycled one.
///
/// The pair every identity-checked kill in this build compares — the recorded
/// one against the live one, in [`crate::teammate::tmux::Server::kill`]. Read
/// [`crate::teammate::tmux`]'s own module doc for why the second half is a pid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    /// The `%N` tmux gave it.
    pub id: String,
    /// `#{pane_pid}`: the process tmux forked into the pane, fixed for the
    /// pane's life. tmux reports no creation *time* — there is no
    /// `pane_start_time` format — and this is what it reports instead.
    pub birth: String,
}

impl Pane {
    /// Whether `live` is really the pane this one recorded.
    ///
    /// Both halves must agree, and they are compared for **equality only**: a
    /// pid is a name the kernel reuses, not a clock, so there is no "later
    /// birth" to reason from — a differing pid says the two panes are
    /// different and says nothing about which came first. A live pane whose id
    /// matches and whose pid does not is a **recycled id**, a different pane
    /// wearing the dead one's name, and answering [`true`] here would be how a
    /// kill lands on a stranger's window.
    #[must_use]
    pub fn is(&self, live: &Self) -> bool {
        self.id == live.id && self.birth == live.birth
    }
}

/// What became of one recorded pane, and of the record that named it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// The pane was still this teammate's: an orphan of a dead lead. Killed,
    /// and its record dropped.
    Reaped,
    /// No pane wears that id any more. Nothing to kill; the record dropped.
    Vanished,
    /// A pane wears the id and is somebody else's. **Left alive**; the record
    /// dropped, because the teammate it named is running nowhere.
    Recycled,
    /// Nothing could be established. The pane and the record are both left
    /// exactly as they were found.
    Undecided,
}

impl Fate {
    /// Whether the member's record comes out of the team file.
    ///
    /// Three of the four, and the odd one out is the one where nothing is
    /// known: a record is dropped when its teammate is demonstrably not
    /// running, never merely because looking failed.
    #[must_use]
    pub fn drops_the_record(self) -> bool {
        matches!(self, Self::Reaped | Self::Vanished | Self::Recycled)
    }
}

/// What one sweep did, member by member, in the team file's own order.
///
/// Empty when there was nothing to sweep — which is the ordinary case, and the
/// reason a sweep is silent unless it acted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// Each pane-backed member the sweep looked at, and what became of it.
    pub fates: Vec<(String, Fate)>,
}

impl Swept {
    /// What became of `name`, if the sweep looked at it at all.
    #[must_use]
    pub fn fate_of(&self, name: &str) -> Option<Fate> {
        self.fates
            .iter()
            .find(|(member, _)| member == name)
            .map(|(_, fate)| *fate)
    }

    /// Whether the sweep looked at nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fates.is_empty()
    }
}

/// Sweeps this lead's team of the panes a previous lead left running.
///
/// What a frontend calls once at startup, after building its
/// [`TeammateRegistry`] and **before** spawning anything into it. Best-effort
/// by construction: every failure below is logged and answered with a fate,
/// never returned, because a lead that could not sweep is still a lead.
///
/// A session outside tmux sweeps nothing — `$TMUX` is what names the server a
/// pane would be on, and a lead without it cannot look at any pane, let alone
/// decide one is stale.
pub async fn sweep(registry: &TeammateRegistry) -> Swept {
    match Server::current() {
        Ok(server) => sweep_on(registry, &server).await,
        Err(error) => {
            tracing::debug!(%error, "no tmux session here, so this lead reaps nothing");

            Swept::default()
        }
    }
}

/// [`sweep`], against a named server rather than this session's own.
///
/// The seam a test drives: a private `tmux -S <socket>` server, so a sweep can
/// be watched killing a real pane without touching the person's own.
pub async fn sweep_on(registry: &TeammateRegistry, server: &Server) -> Swept {
    let file = match registry.read_team().await {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(%error, "the team file could not be read, so nothing was swept");

            return Swept::default();
        }
    };
    let recorded: Vec<(String, String, String)> = file
        .members
        .iter()
        .filter(|member| !member.is_lead())
        .filter_map(|member| {
            // A shim teammate in a pane (P28, D512) writes the **real** `%N`,
            // so its record reads back as `Surface::Pane` and would fall into
            // the arm below — but this sweep must not touch it. Its witness is
            // `--agent-id`/`--parent-session-id` on the pane's command line,
            // and a foreign CLI's argv carries neither, so `witnessed` never
            // matches and the pane is read as `Recycled`: **left alive**
            // (right) but its record **dropped** (wrong — that is this
            // teammate's own codex, not a stranger's window). Recognized here
            // by `backendType`, the one field still saying shim once
            // `tmuxPaneId` holds a real id (`retire_shim_records`'s own test),
            // and left entirely alone: the live pane keeps its record, which
            // `retire_shim_records` marks inactive instead. Teaching the sweep
            // to fully witness a shim pane is bead `ganja-code-3nz`; not
            // dropping its record is the safe half of that, done here.
            if member
                .backend_type
                .as_deref()
                .and_then(ShimCli::read)
                .is_some()
            {
                return None;
            }
            match member.surface() {
                Surface::Pane { id } => Some((member.name.clone(), member.agent_id.clone(), id)),
                // A headless shim child and an in-process teammate own no pane,
                // so this sweep has nothing to say about either; `Surface::Shim`
                // is unreachable through `surface()` besides (the shim guard
                // above catches every shim first, and `Surface::read` answers
                // in-process for a headless one). Written out rather than folded
                // into a wildcard so that a fifth surface still has to decide
                // what a pane sweep does with it.
                Surface::Leader | Surface::InProcess | Surface::Shim { .. } => None,
            }
        })
        .collect();
    if recorded.is_empty() {
        return Swept::default();
    }
    if file.lead_session_id != registry.lead_session_id {
        // Another lead's team, reached through a name this session shares with
        // it — `default`, which every pre-UUIDv7 session joins. Its panes are
        // its own lead's business, alive or dead.
        tracing::info!(
            team = registry.team.as_str(),
            panes = recorded.len(),
            "the team file names another lead's session, so its panes were left alone"
        );

        return Swept::default();
    }

    let live = match server.panes().await {
        Ok(live) => live,
        Err(error) => {
            tracing::warn!(%error, "tmux would not list its panes, so nothing was swept");

            return Swept {
                fates: recorded
                    .into_iter()
                    .map(|(name, _, _)| (name, Fate::Undecided))
                    .collect(),
            };
        }
    };

    let mut fates = Vec::with_capacity(recorded.len());
    for (name, agent_id, pane_id) in recorded {
        let pane = live.iter().find(|pane| pane.id == pane_id);
        let witness = match pane {
            Some(pane) => witnessed(pane, &agent_id, &registry.lead_session_id).await,
            None => None,
        };
        let fate = match (verdict(pane, witness), pane) {
            (Some(fate), _) => fate,
            // The pane is this teammate's, and its lead is not here to have
            // ended it. `kill` looks the pair up once more of its own accord,
            // so a pane that was reissued between the listing and now is
            // still not killed.
            (None, Some(pane)) => match server.kill(pane).await {
                Ok(Killed::Yes) => Fate::Reaped,
                Ok(Killed::AlreadyGone) => Fate::Vanished,
                Ok(Killed::Recycled) => Fate::Recycled,
                Err(error) => {
                    tracing::warn!(
                        teammate = name,
                        pane = pane_id,
                        %error,
                        "an orphaned pane could not be ended, so its record was kept"
                    );
                    Fate::Undecided
                }
            },
            // [`verdict`] answers [`None`] only for a live pane, so this arm
            // is the type system's rather than a case.
            (None, None) => Fate::Undecided,
        };
        report(&name, &pane_id, fate);
        if fate.drops_the_record() {
            forget(registry, &name).await;
        }
        fates.push((name, fate));
    }

    Swept { fates }
}

/// The fate that is known before anything is killed, or [`None`] when the pane
/// is this teammate's and has to be ended.
///
/// Pure, so the rule can be read and tested without a tmux server: `witness` is
/// [`None`] for a question that could not be answered, and the two halves are
/// exactly what the module doc's four fates are decided from.
fn verdict(live: Option<&Pane>, witness: Option<bool>) -> Option<Fate> {
    match (live, witness) {
        (None, _) => Some(Fate::Vanished),
        (Some(_), None) => Some(Fate::Undecided),
        (Some(_), Some(false)) => Some(Fate::Recycled),
        (Some(_), Some(true)) => None,
    }
}

/// Whether `pane`'s first process is the teammate `agent_id` names, launched by
/// the lead `lead_session` names — or [`None`] when its command line could not
/// be read.
///
/// The cold-start half of D506's identity, and **both** halves have to hold:
/// the module doc says which defect each one closes. `ps` failing — the process
/// gone between the listing and the question, a machine that answers nothing —
/// is deliberately not "no": a pane nothing is known about is left alone.
async fn witnessed(pane: &Pane, agent_id: &str, lead_session: &str) -> Option<bool> {
    let argv = argv_of(&pane.birth).await?;

    Some(flagged(&argv, AGENT_ID, agent_id) && flagged(&argv, PARENT_SESSION_ID, lead_session))
}

/// Whether `argv` carries `flag` with exactly `value`.
///
/// A **word** comparison, never a substring of the line: `--agent-id build` and
/// `--agent-id rebuild` differ here and do not differ to `str::contains`.
/// `ps` prints an argv space-separated and
/// unquoted, and neither an agent id nor a session id can hold a space — both
/// are grammar-checked ([`ganja_team::MemberName`], a bare UUID) — so splitting
/// on whitespace recovers the words the process was started with.
///
/// `--flag=value` is accepted beside `--flag value` because clap accepts it:
/// this build's own launch lines only ever emit the two-word form, but a pane a
/// person started by hand is still that teammate's pane and refusing to see it
/// would leave an orphan running.
fn flagged(argv: &str, flag: &str, value: &str) -> bool {
    let mut words = argv.split_ascii_whitespace();
    while let Some(word) = words.next() {
        // Only advances the iterator when the word really is the flag, so the
        // value it consumes is never a flag somebody else's check needed.
        if word == flag && words.next() == Some(value) {
            return true;
        }
        if word
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
            == Some(value)
        {
            return true;
        }
    }

    false
}

/// The command line of the process `pid` names, as `ps` prints it.
///
/// `-ww` because the answer is compared against an agent id that sits behind a
/// flag: a command line truncated to the terminal's width would read as a pane
/// that is not the teammate's, and this module's whole job is not to be wrong
/// about that.
async fn argv_of(pid: &str) -> Option<String> {
    let output = tokio::process::Command::new(PS)
        .args(["-ww", "-o", "args=", "-p", pid])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let argv = String::from_utf8_lossy(&output.stdout).trim().to_owned();

    (!argv.is_empty()).then_some(argv)
}

/// Says what happened to one member, at the level the event deserves.
///
/// A kill and a near-miss are both worth seeing — one ended somebody's window,
/// the other decided not to — while a record dropped over a pane that is simply
/// gone is housekeeping.
fn report(name: &str, pane_id: &str, fate: Fate) {
    match fate {
        Fate::Reaped => tracing::info!(
            teammate = name,
            pane = pane_id,
            "an orphaned teammate's pane outlived its lead and was ended"
        ),
        Fate::Vanished => tracing::debug!(
            teammate = name,
            pane = pane_id,
            "a teammate's pane is gone, so its record was dropped"
        ),
        Fate::Recycled => tracing::info!(
            teammate = name,
            pane = pane_id,
            "that pane id belongs to somebody else now and was left alone; \
             the stale record was dropped"
        ),
        Fate::Undecided => tracing::warn!(
            teammate = name,
            pane = pane_id,
            "nothing could be established about this pane, so it and its \
             record were left as they are"
        ),
    }
}

/// Takes one member out of the team file, through the same locked
/// read-modify-write a retire uses.
///
/// Failing to rewrite the document is not failing the sweep: the pane is
/// already dealt with, and a stale row costs the next lead one more look.
async fn forget(registry: &TeammateRegistry, name: &str) {
    match registry.unrecord(name).await {
        Ok(true) => {}
        Ok(false) => tracing::debug!(
            teammate = name,
            "the team file had already stopped naming this member"
        ),
        Err(error) => tracing::warn!(
            teammate = name,
            %error,
            "a swept member's record could not be taken out of the team file"
        ),
    }
}

/// How long a signalled group is given to go before it is killed.
///
/// Short on purpose, and much shorter than `SETTLE`: this runs at lead start,
/// in a blocking job the frontend is waiting on, and an orphan that ignores
/// `SIGTERM` is exactly the process the second signal exists for. Polled
/// rather than slept through, so the common case — a child that goes at once —
/// costs one check.
const GRACE: Duration = Duration::from_millis(500);

/// How often that grace is checked.
const GRACE_STEP: Duration = Duration::from_millis(25);

/// What one sweep decided about one `.shims` file.
///
/// Six arms, and five of them are about *not* signalling: "no owner proof, no
/// signal" is the whole rule, so the interesting part of this type is how many
/// ways it says no.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShimFate {
    /// The owner pid is live and its start time matches: the file is left
    /// **entirely** alone — not parsed further, not logged as a problem,
    /// nothing.
    OwnerLive,
    /// The owner's liveness could not be established at all, so it is not
    /// proven gone. Skipped, and left byte-identical for the next sweep.
    Undeterminable,
    /// No readable version line — a zero-byte file, a header-less one, or the
    /// staging file a crash left between a write and its rename. No live lead
    /// can publish one.
    Headerless,
    /// A version token this build does not know: a newer lead owns the file.
    Foreign,
    /// A known version and content that is not this format's. Since a
    /// same-version writer publishes only whole files, this can only be
    /// corruption.
    Corrupt,
    /// The owner is provably gone, and every child line was decided one way or
    /// another.
    Swept {
        /// How many children matched their recorded identity exactly and were
        /// ended.
        signalled: usize,
        /// How many were proven not to be the recorded child — a recycled pid,
        /// this lead's own group, or a pid that is simply gone — and were
        /// therefore left alone.
        spared: usize,
        /// How many could not be decided at all, which is what keeps the file
        /// from being unlinked.
        undecided: usize,
    },
}

/// One `.shims` file, and what became of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShimFile {
    /// Where it was.
    pub path: PathBuf,
    /// What the sweep decided.
    pub fate: ShimFate,
    /// Whether it was unlinked — never true where the owner-line pid still
    /// exists under any reading, because an unlink is the one action here that
    /// cannot be retried.
    pub removed: bool,
}

/// What a whole sweep of the records directory did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShimsSwept {
    /// One entry per file looked at, in the order they were read.
    pub files: Vec<ShimFile>,
    /// The shim members whose records this sweep retired — the other half of
    /// the same startup pass, over the team file rather than over `/tmp`.
    pub retired: Vec<String>,
}

impl ShimsSwept {
    /// What the sweep decided about one file, by name.
    #[must_use]
    pub fn fate_of(&self, path: &Path) -> Option<&ShimFate> {
        self.files
            .iter()
            .find(|file| file.path == path)
            .map(|file| &file.fate)
    }

    /// Whether the sweep did nothing at all.
    ///
    /// **Both halves**, and the second is not decoration: a startup that swept
    /// no `/tmp` records but retired three members did something worth a log
    /// line, and a predicate reading only `files` would have the one caller —
    /// the lead's startup, which logs `if !swept.is_empty()` — say nothing
    /// about it. The common shape of a resumed session is exactly that: the
    /// previous lead's records were already unlinked by an earlier sweep, and
    /// its member rows were not.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.retired.is_empty()
    }
}

/// Ends the shim children a previous lead left running (**D508**).
///
/// Called at lead startup **beside** [`sweep`] rather than inside it, and
/// **unconditionally**: `sweep` is gated on there being a tmux server to look
/// at, which is right for panes and fatal for shims — a shim child is headless
/// and its common case has no tmux at all. Hoisting that gate would change the
/// pane arm's own contract, so the shim arm gets its own caller.
///
/// Best-effort by construction, like its pane sibling: every failure below is
/// logged and answered with a fate, never returned. A lead that could not sweep
/// is still a lead.
///
/// **The directory is never created here.** This runs possibly before any shim
/// has ever spawned, and making a private directory in order to enumerate
/// nothing is a directory made for no reason; the first *record write* is what
/// creates it.
pub async fn sweep_shims(registry: &TeammateRegistry) -> ShimsSwept {
    let directory = registry
        .shims()
        .lock()
        .expect("the shim records are never poisoned")
        .directory()
        .to_path_buf();

    let mut swept = sweep_shims_in(directory).await;
    // The other half of the same startup pass, and a different document: the
    // `/tmp` records say which *processes* a dead lead owned, while the team
    // file says which *members* it recorded. A process ended above leaves a row
    // behind that would otherwise be listed as a running teammate forever.
    swept.retired = retire_shim_records(registry).await;

    swept
}

/// [`sweep_shims`], against a named directory rather than this session's own.
///
/// The seam a test drives, and the pane sweep's `sweep_on` is the precedent:
/// a private directory, so a sweep can be watched deciding about real
/// processes without going near `/tmp/ganja-<uid>`.
pub async fn sweep_shims_in(directory: PathBuf) -> ShimsSwept {
    sweep_shims_in_with(directory, records::started_at).await
}

/// [`sweep_shims_in`] with the start-time primitive handed in.
///
/// The seam exists for one fate a real pid cannot portably be made to take:
/// `Unknown`, the "could not be established" answer an owner or child reaches
/// only when the primitive itself cannot decide. A nonexistent pid is `Gone`
/// on every platform (the `kill(pid, 0)`/`ESRCH` probe in
/// [`records::started_at`]), so "undecided" is untestable through a real pid;
/// a test drives it by passing a primitive that declines one. Production
/// always passes [`records::started_at`].
#[doc(hidden)] // a test-only injection seam, not a supported entry point
pub async fn sweep_shims_in_with(directory: PathBuf, started: fn(i32) -> Started) -> ShimsSwept {
    // One blocking job for the whole sweep, because the identity primitive is
    // a `ps` fork and the decisions are pure file reads: an async spelling
    // would fork from a runtime worker for every line of every file.
    tokio::task::spawn_blocking(move || sweep_records(&directory, started))
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "the shim sweep was lost, so nothing was swept");

            ShimsSwept::default()
        })
}

/// Marks this team's shim members inactive, because their processes died with
/// the lead that recorded them.
///
/// **The one reader in this build that needs shim-ness off a record**, and the
/// field it reads is `backendType` and nothing else: `Surface::read` is
/// deliberately lossy — a shim member writes the in-process sentinel into
/// `tmuxPaneId`, so `MemberRecord::surface()` answers `InProcess` for one and
/// will never hand this an `InProcess`-versus-`Shim` distinction. Comparing
/// against the three `backendType` strings is the whole test, and
/// [`ShimCli::read`] is the one table that knows them.
///
/// # Marked, not dropped
///
/// A pane member's stale record is *dropped*, because the pane sweep can prove
/// what happened to the pane. This cannot: a shim child leaves no surface a
/// later process can interrogate, and the `/tmp` records are a best-effort net
/// that may be absent entirely. So the row stays and says it is not running,
/// which is what `isActive` is for.
///
/// **What that costs, stated (Dv-3):** the row is still a row, and
/// `TeammateRegistry::taken` counts every one of them without consulting
/// `isActive` — so `resolve_unique` gives the next `w1` the name `w1-2`. The
/// retired name is not freed, and freeing it is the worse answer rather than
/// the missing one: dropping the row would hand a dead teammate's identity to
/// the next live one, in a document a real `claude` may be reading at the same
/// time. "Respawnable" therefore means the work can be restaffed, not that the
/// string comes back.
///
/// # The co-tenant guard is the pane sweep's, transposed
///
/// Two leads that start inside one 65-second UUIDv7 bucket share a team *name*
/// and therefore a team *file*, and the record write never restamps
/// `leadSessionId`. Retiring rows in a document that names another lead's
/// session would mark a **live** co-tenant's members dead, so this bails on
/// exactly the check `sweep_on` makes (reaper.rs's own `lead_session_id`
/// clause) before it touches anything.
///
/// **The residual that guard does not cover, named rather than papered over
/// (D2).** It answers the case where the *document* is another lead's. The
/// mirror image survives: where **this** lead created the file and a co-tenant
/// later spawned into it, the document names this session, the guard passes,
/// and the co-tenant's live shim members are marked inactive. Nothing closes
/// it, by construction — `MemberRecord` carries no per-member lead witness,
/// and the pane arm's own witness (`--parent-session-id` on the pane's command
/// line) has no shim analogue, since a headless child's argv is the vendor's.
/// The blast radius is small and worth stating: **nothing in ganja reads
/// `isActive`**, so the wrong value costs this build nothing at all; its one
/// consumer is Claude's own document, where a live teammate would read as
/// stopped until its own lead's next write.
///
/// # Before the first spawn, and only then
///
/// Called at startup, ahead of everything this lead starts — which is what
/// makes "every shim row in this file belongs to a previous lead" true. Called
/// *after* a spawn it would mark this lead's own live members inactive, so the
/// precondition is asserted rather than trusted: a registry already holding
/// members retires nothing and says so by answering empty.
///
/// Best-effort throughout: a lead that could not retire a row is still a lead,
/// and the next startup meets the same row again.
pub async fn retire_shim_records(registry: &TeammateRegistry) -> Vec<String> {
    // Bound to a `let` rather than tested inline, so the member map's guard is
    // released at the end of this statement and cannot be held across the
    // awaits below.
    let already_spawning = !registry.members().is_empty();
    if already_spawning {
        tracing::debug!(
            "this lead has already spawned into its team, so no shim record was retired"
        );

        return Vec::new();
    }

    let mut file = match registry.read_team().await {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(%error, "the team file could not be read, so no shim record was retired");

            return Vec::new();
        }
    };
    if file.lead_session_id != registry.lead_session_id {
        tracing::info!(
            team = registry.team.as_str(),
            "the team file names another lead's session, so its shim members were left alone"
        );

        return Vec::new();
    }

    let mut retired = Vec::new();
    for member in &mut file.members {
        if member.is_lead() {
            continue;
        }
        // `backendType`, never `surface()` — see this function's own doc for
        // why the read that looks more natural cannot answer this.
        let shim = member
            .backend_type
            .as_deref()
            .and_then(ShimCli::read)
            .is_some();
        if !shim || member.is_active == Some(false) {
            continue;
        }
        member.is_active = Some(false);
        retired.push(member.name.clone());
    }
    if retired.is_empty() {
        return retired;
    }

    let writing = registry.team_file.lock().await;
    if let Err(error) = registry.write_team(file, &writing).await {
        tracing::warn!(
            %error,
            "a previous lead's shim members could not be marked inactive, so they will be \
             listed as running until the next startup"
        );

        return Vec::new();
    }
    tracing::info!(
        retired = retired.len(),
        "a previous lead's foreign-CLI members were marked inactive"
    );

    retired
}

/// The whole sweep, synchronously.
fn sweep_records(directory: &Path, started: fn(i32) -> Started) -> ShimsSwept {
    let mut swept = ShimsSwept::default();
    if !directory.exists() {
        // Nothing has ever recorded a shim child here, which is the ordinary
        // answer for every session that never spawned one.
        return swept;
    }
    if let Err(refusal) = ganja_tool::socket::vet_directory(directory) {
        tracing::warn!(
            directory = %directory.display(),
            %refusal,
            "the shim records directory is not a private one of ours, so nothing was swept"
        );

        return swept;
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                directory = %directory.display(),
                %error,
                "the shim records could not be listed, so nothing was swept"
            );

            return swept;
        }
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(records::EXTENSION))
        })
        .collect();
    // Read in a stable order, so a test asserting two files' fates does not
    // depend on what the filesystem felt like today.
    paths.sort();

    for path in paths {
        swept.files.push(sweep_file(&path, started));
    }

    swept
}

/// One `.shims` file, from its version line down.
fn sweep_file(path: &Path, started: fn(i32) -> Started) -> ShimFile {
    let decided = |fate: ShimFate, removed: bool| ShimFile {
        path: path.to_path_buf(),
        fate,
        removed,
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(
                file = %path.display(),
                %error,
                "a shim records file could not be read, so it was left for the next sweep"
            );

            return decided(ShimFate::Undeterminable, false);
        }
    };
    let records = match records::parse(&text) {
        Ok(records) => records,
        Err(unreadable) => {
            let fate = match &unreadable {
                Unreadable::Headerless => ShimFate::Headerless,
                Unreadable::Version { .. } => ShimFate::Foreign,
                Unreadable::Malformed { .. } => ShimFate::Corrupt,
            };
            // Logged **before** anything is unlinked, so the account of what
            // happened survives the file it is about.
            tracing::info!(
                file = %path.display(),
                "a shim records file was not read: {unreadable}"
            );

            return decided(fate, unreadable.removable() && remove(path));
        }
    };

    let owner = started(records.owner.pid);
    match &owner {
        // Fail-closed: "cannot prove gone" and "gone" must never be the same
        // branch.
        Started::Unknown => {
            tracing::info!(
                file = %path.display(),
                owner = records.owner.pid,
                "a shim records file's owner could not be established, so it was left alone"
            );

            decided(ShimFate::Undeterminable, false)
        }
        // The owner is live. The file is left **entirely** alone — a
        // concurrently running lead's children are its own business, alive or
        // dead, and this is the clause that says so.
        Started::At(rendered) if *rendered == records.owner.started => {
            decided(ShimFate::OwnerLive, false)
        }
        // Fail-**open** on the owner, and named as such: a pid that exists
        // with a different start time is read as gone. What makes that safe is
        // not this arm but the child lines, which are compared with the same
        // primitive on the same rendered bytes and so mismatch for whatever
        // reason the owner's did. The hardening clause is the `removed` half:
        // an owner pid that still exists means the file is never unlinked,
        // because an unlink is the one action here that cannot be retried.
        Started::At(_) | Started::Gone => {
            let (signalled, spared, undecided) = sweep_children(path, &records, started);
            let owner_present = matches!(owner, Started::At(_));
            let removable = undecided == 0 && !owner_present;

            decided(
                ShimFate::Swept {
                    signalled,
                    spared,
                    undecided,
                },
                removable && remove(path),
            )
        }
    }
}

/// Every child line of one file whose owner is provably gone.
fn sweep_children(
    path: &Path,
    records: &records::Records,
    started: fn(i32) -> Started,
) -> (usize, usize, usize) {
    let own = records::own_pgid();
    let (mut signalled, mut spared, mut undecided) = (0, 0, 0);
    for child in &records.children {
        let cli = child.cli.backend_type();
        // Belt beside braces. The owner rule should already have made this
        // unreachable — this lead's own group belongs to this lead, which is
        // alive by definition — and a guard that is unreachable in theory is
        // exactly the one worth keeping.
        if child.pgid == own {
            tracing::warn!(
                file = %path.display(),
                cli,
                pgid = child.pgid,
                "a shim record names this lead's own process group, so nothing was signalled"
            );
            spared += 1;
            continue;
        }
        match started(child.process.pid) {
            Started::At(rendered) if rendered == child.process.started => {
                end_group(child.pgid);
                tracing::info!(
                    file = %path.display(),
                    cli,
                    pid = child.process.pid,
                    "an orphaned shim child outlived its lead and was ended"
                );
                signalled += 1;
            }
            Started::At(_) => {
                tracing::info!(
                    file = %path.display(),
                    cli,
                    pid = child.process.pid,
                    "that pid belongs to somebody else now and was left alone"
                );
                spared += 1;
            }
            Started::Gone => {
                // The recorded child is gone, so no identity match is possible
                // — while its own subprocesses may still be alive. v1 leaves
                // them, naming what a person can run: the survivors are the
                // CLI's own tools, which the CLI would normally have reaped,
                // and signalling a group whose leader cannot be identified is
                // exactly what "no owner proof, no signal" forbids.
                if group_alive(child.pgid) {
                    tracing::warn!(
                        file = %path.display(),
                        cli,
                        pgid = child.pgid,
                        "a shim child is gone but its process group is not; \
                         `kill -- -{pgid}` ends what is left",
                        pgid = child.pgid
                    );
                }
                spared += 1;
            }
            Started::Unknown => {
                tracing::info!(
                    file = %path.display(),
                    cli,
                    pid = child.process.pid,
                    "nothing could be established about a recorded shim child, so it was left \
                     as it is"
                );
                undecided += 1;
            }
        }
    }

    (signalled, spared, undecided)
}

/// Whether anything is left in `pgid`.
fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 sends nothing; it only asks whether the group exists
    // and whether this process could signal it.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// TERM the group, and KILL whatever is still there after [`GRACE`].
fn end_group(pgid: i32) {
    signal_group(pgid, libc::SIGTERM);
    let deadline = std::time::Instant::now() + GRACE;
    while std::time::Instant::now() < deadline {
        if !group_alive(pgid) {
            return;
        }
        std::thread::sleep(GRACE_STEP);
    }
    signal_group(pgid, libc::SIGKILL);
}

/// Unlinks a records file that has no future reader, saying so when it could
/// not.
fn remove(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => {
            tracing::info!(file = %path.display(), "a shim records file with no future reader was removed");

            true
        }
        Err(error) => {
            tracing::warn!(
                file = %path.display(),
                %error,
                "a shim records file could not be removed, so the next sweep will meet it again"
            );

            false
        }
    }
}

#[cfg(test)]
#[path = "reaper_tests.rs"]
mod tests;
