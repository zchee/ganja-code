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

use std::process::Stdio;

use ganja_team::Surface;

use crate::teammate::{
    TeammateRegistry,
    pane::{AGENT_ID, PARENT_SESSION_ID},
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
        .filter_map(|member| match member.surface() {
            Surface::Pane { id } => Some((member.name.clone(), member.agent_id.clone(), id)),
            Surface::Leader | Surface::InProcess => None,
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

#[cfg(test)]
mod tests {
    use ganja_team::{
        MemberName, MemberRecord, Spawn, Surface, TeamFile, TeamName, TeamsRoot, record,
    };

    use super::{AGENT_ID, Fate, PARENT_SESSION_ID, Pane, argv_of, flagged, forget, verdict};
    use crate::teammate::TeammateRegistry;

    fn pane(id: &str, birth: &str) -> Pane {
        Pane {
            id: id.to_owned(),
            birth: birth.to_owned(),
        }
    }

    #[test]
    fn a_recycled_pane_id_is_not_the_pane_that_was_recorded() {
        let recorded = pane("%142", "48213");

        assert!(recorded.is(&pane("%142", "48213")));
        // Same id, a different first process: tmux handed `%142` out again.
        // Whether that pid is larger or smaller is nothing — a pid is a name
        // the kernel reuses, not a clock.
        assert!(!recorded.is(&pane("%142", "52117")));
        assert!(!recorded.is(&pane("%142", "9")));
        // And a different id is not it either, whatever runs in it.
        assert!(!recorded.is(&pane("%143", "48213")));
    }

    /// The rule, as a table: only a pane that is both live and witnessed is
    /// killed, and only a pane that could be looked at is decided about.
    #[test]
    fn only_a_live_and_witnessed_pane_is_ended() {
        let live = pane("%17", "48213");

        assert_eq!(verdict(Some(&live), Some(true)), None, "kill this one");
        assert_eq!(verdict(Some(&live), Some(false)), Some(Fate::Recycled));
        assert_eq!(verdict(Some(&live), None), Some(Fate::Undecided));
        assert_eq!(verdict(None, None), Some(Fate::Vanished));
        // A witness for a pane that is not there decides nothing: the pane is
        // what is missing, and it is missing either way.
        assert_eq!(verdict(None, Some(true)), Some(Fate::Vanished));
    }

    /// A record leaves the file when its teammate is demonstrably not running,
    /// and stays when nothing could be established.
    #[test]
    fn only_a_settled_fate_drops_a_record() {
        assert!(Fate::Reaped.drops_the_record());
        assert!(Fate::Vanished.drops_the_record());
        assert!(Fate::Recycled.drops_the_record());
        assert!(!Fate::Undecided.drops_the_record());
    }

    /// The word rule, and the two collisions a substring test cannot see: a
    /// member name that is a suffix of a sibling's, and a session id that is a
    /// prefix of another's.
    #[test]
    fn a_flag_matches_the_word_after_it_and_never_a_substring_of_the_line() {
        let argv = "/x/ganja --agent-id rebuild@session-01998ad0 \
                    --parent-session-id 01998ad0-0000-7000-8000-000000000000";

        assert!(flagged(argv, AGENT_ID, "rebuild@session-01998ad0"));
        assert!(
            !flagged(argv, AGENT_ID, "build@session-01998ad0"),
            "`build` is a suffix of `rebuild` and is not the same teammate"
        );
        assert!(flagged(
            argv,
            PARENT_SESSION_ID,
            "01998ad0-0000-7000-8000-000000000000"
        ));
        assert!(
            !flagged(argv, PARENT_SESSION_ID, "01998ad0-0000-7000-8000-0"),
            "a prefix of a session id is a different lead"
        );
        // A flag that is there without its value, and a value that is there
        // without its flag, are both nothing.
        assert!(!flagged("/x/ganja --agent-id", AGENT_ID, "worker@t"));
        assert!(!flagged("/x/ganja worker@t", AGENT_ID, "worker@t"));
        // clap's other spelling, which a person may well type by hand.
        assert!(flagged(
            "/x/ganja --agent-id=worker@t",
            AGENT_ID,
            "worker@t"
        ));
        assert!(!flagged(
            "/x/ganja --agent-id-of=worker@t",
            AGENT_ID,
            "worker@t"
        ));
    }

    /// The witness's own mechanism, against the one process this test is sure
    /// about: itself. Pins the `ps` invocation, which is the half of D506 a
    /// machine can break without any test noticing.
    #[tokio::test]
    async fn a_processs_own_command_line_is_what_the_witness_reads() {
        let argv = argv_of(&std::process::id().to_string())
            .await
            .expect("a live process has a command line");

        assert!(!argv.is_empty(), "and it is not empty: {argv:?}");
        assert!(
            argv_of("0").await.is_none(),
            "a pid nothing answers for is unknown, never a 'no'"
        );
    }

    /// Dropping one member rewrites the document without it and leaves every
    /// other row — the lead's included — where it was.
    #[tokio::test]
    async fn dropping_a_record_leaves_the_rest_of_the_team_file_alone() {
        let home = tempfile::tempdir().expect("a temporary teams root");
        let root = TeamsRoot::new(home.path().join("teams"));
        let team = TeamName::parse("session-01998ad0").expect("a team name");
        let session = "01998ad0-0000-7000-8000-000000000000";
        let cwd = home.path().to_string_lossy().into_owned();

        let mut file = TeamFile::new(&team, session, cwd.clone(), record::now_millis());
        for (name, id) in [("worker", "%17"), ("scribe", "%18")] {
            file.members.push(MemberRecord::teammate(
                &MemberName::parse(name).expect("a member name"),
                &team,
                Spawn {
                    agent_type: "general".to_owned(),
                    model: "fake/fake".to_owned(),
                    color: "blue".to_owned(),
                    prompt: "watch the build".to_owned(),
                    plan_mode_required: false,
                    surface: Surface::Pane { id: id.to_owned() },
                    cwd: cwd.clone(),
                },
                record::now_millis(),
            ));
        }
        let path = root.config_path(&team);
        std::fs::create_dir_all(path.parent().expect("a team directory"))
            .expect("the team directory is made");
        std::fs::write(
            &path,
            record::document(&file).expect("the team file encodes"),
        )
        .expect("the team file is written");

        let registry = TeammateRegistry::new(root, team, session, home.path());
        forget(&registry, "worker").await;

        let written: TeamFile =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the team file is read"))
                .expect("the team file decodes");
        let names: Vec<&str> = written
            .members
            .iter()
            .map(|member| member.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["team-lead", "scribe"],
            "only the dropped member is gone: {written:?}"
        );
    }
}
