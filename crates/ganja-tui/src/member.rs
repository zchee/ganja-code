//! Running the terminal UI **as** a pane teammate (§4.1, §6.1, §10.3).
//!
//! Upstream opencode has **no counterpart**: nothing there is a member of
//! anything. What is ported is the reference's pane teammate — a separate OS
//! process running the full CLI, with its own session and transcript, launched
//! by a lead with `--agent-id/--agent-name/--team-name/--agent-color/
//! --parent-session-id` and told what to do through its mailbox rather than
//! its command line (§4.1). §10.3 names the shape a `ganja` pane takes: the
//! ordinary TUI plus four integrations, every one of them landing on a seam
//! the frontend already has, and **the engine untouched** (§10.3-3).
//!
//! # The four integrations, and where each one lands
//!
//! 1. **Inbound** — [`Inbox::poll`] is the §6.1 pass over this member's own
//!    inbox, run from the app's tick exactly as the lead's `LeadInbox` pass
//!    is: `shutdown_request` first, then the frames, then the plain messages
//!    handed back for the lane that already exists — a peer batch that steers
//!    a running turn at its next step boundary or prompts an idle one (D-3).
//! 2. **The seeded task** — the preamble the lead wrote into this inbox, around the task, before
//!    launching the pane is simply the first message the first pass finds (D514), and
//!    it becomes the first turn through the same lane; no dedicated mechanism.
//! 3. **`idle_notification`** — [`Inbox::report_idle`] writes the frame to the
//!    lead when a turn ends, mapping the finish reason the engine already
//!    reports (completed / cancelled / failed) onto `available` /
//!    `interrupted` / `failed`.
//! 4. **Control frames** — a `shutdown_request` is answered with
//!    [`Inbox::approve_shutdown`] once the turn has ended, and the app then
//!    quits through the exit path it always had, so the MCP servers, the jobs
//!    and the terminal are torn down in the order they always were.
//!    `mode_set_request` maps onto `Command::SetPermissionMode` (D-15,
//!    AC-19); `plan_approval_response` is stale by definition here, since
//!    nothing in this build raises the request it would answer.
//!
//! # What is deliberately not here
//!
//! - **The reading and the ruling stay apart.** This module reads the file
//!   and decides what each entry is; the app decides what to send the engine.
//!   [`Pass`] is the whole of what crosses, so a test drives one pass and
//!   asserts the §6.1 ordering without a terminal.
//! - **The lead-only check is the type's.** `plan_approval_response` and
//!   `mode_set_request` are reachable only through
//!   [`ganja_protocol::team::LeadFrame`], which cannot be built from a peer's
//!   frame (§7-2). A `task_assignment` is held to the same bar: it becomes this
//!   member's next turn only when the lead wrote it.
//! - **A pane teammate does not lead a team of its own.** No registry is
//!   installed for it: a teammate is not a place to nest a second team, and
//!   the lead's registry would offer its model a `send_message` stamped with
//!   the lead's name. What it speaks through instead is `ganja-core`'s
//!   [`ganja_core::teammate::member::MemberPostbox`], installed by the entry
//!   with the name the launch line carried — the roster read off the team
//!   file per call, the lead always addressable — so its results reach the
//!   lead as messages, as its `idle_notification`, and as its own session's
//!   transcript.
//!
//! # Posture, and the record a pane reads (D-5, AC-8)
//!
//! A pane's default posture is [`Posture::ForwardToLead`], and it rides
//! `ganja-core`'s own [`Asks`] — the one dialect both ends of the inbox
//! speak: an ask the pane's rules raise draws no dialog here but is written
//! to the lead as §5's `permission_request` ([`Asks::forward`], driven off
//! [`Inbox::asks`] by the app), the
//! lead's own pass puts it in front of the same dialog its in-process
//! teammates use, and the answer comes back as a `permission_response`
//! through this inbox, resolved by the type that refuses a peer's
//! ([`Asks::resolve`] takes a [`LeadFrame`]) into the `ReplyPermission` the
//! app sends — once, always or reject, exactly as the person at the lead's
//! dialog answered *this member's own* open ask. Nothing on the launch line
//! says otherwise: until 2026-08-22 a lead composed the bypass trio (D479)
//! onto it for a spawn that had asked to skip dialogs, and **D513** retired
//! that axis, so a pane's `--auto` is now only ever a person's word about
//! their own session and never a posture the lead chose for it; nothing
//! selects [`Posture::HumanAttended`] yet, and the plan selects it by nothing
//! either.
//!
//! The member record — the model this teammate was spawned to run, and
//! whether it must start in plan mode — is read off the team file **after a
//! bounded wait** ([`Membership::await_record`]), and that wait is
//! **defensive rather than expected**. The lead orders the two the other way
//! round: the registry writes the record once the backend's `spawn` has answered
//! with the pane it made, and only *then* is the launch line typed — through
//! `GanjaPane`'s own watch for that record, or through the `launch` hook the
//! registry calls after its record write. So by the time this process exists at
//! all, its row is on disk. What the wait covers is a lead that died between the
//! two, a rename this process's filesystem view has not caught up with, and the
//! case where somebody started a `ganja` with these flags by hand — none of them
//! ordinary, all of them better answered by a sentence naming the file than by a
//! panic. `planModeRequired` becomes the `plan` agent, which is what
//! plan mode is in this build — and **no more than that**: the
//! `plan_approval_request` round trip is not wired, so a member's `plan_exit`
//! door behaves as it does in any session and asks nobody at the lead. The
//! record's flag today means "starts as the plan agent"; the lead-side
//! approval flow is future scope, and a reader who took the flag for the
//! whole of §5's plan handshake would be reading more than is there.
//!
//! # One write, not two
//!
//! The runner's rule, for its reason: everything a pass finished leaves the
//! inbox in one [`mailbox::prune_delivered`], because each write is a full
//! read-modify-write under `ganja-team`'s lock. What the app **delivers** is
//! pruned by the app once the engine provably took it ([`Inbox::delivered`]),
//! which is what keeps the mailbox the durable queue: a batch the engine
//! refused, or a pane that died between the read and the turn, leaves the
//! message where the next pass finds it again.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use ganja_core::{
    team::{
        MailboxMessage, MemberName, MemberRecord, Surface, TeamFile, TeamName, TeamsRoot, mailbox,
        record,
    },
    teammate::{
        Delivery, TeammateRegistry,
        lead_inbox::Delivered,
        member::{Asks, Resolved},
        posture::Posture,
        runner::{self, DROPPED_FRAME, IGNORED_STALE},
    },
};
use ganja_protocol::{
    FinishReason, PermissionId, PermissionMode, PermissionReply,
    team::{
        Frame, IdleNotification, IdleReason, LeadFrame, ModeSetRequest, ShutdownApproved,
        TaskAssignment, cap_for_display,
    },
};

/// The teammate's own cadence (§6), and the same constant the in-process
/// runner keeps: the member is the side that has to notice a shutdown
/// promptly.
pub const POLL: Duration = runner::POLL;

/// The environment variable tmux sets in every pane it runs, naming the pane
/// — core's own spelling of it, re-exported.
///
/// Read rather than passed: §4.1's launch line carries no pane id, and the
/// pane is the one process that can ask its own environment which `%N` it is.
pub use ganja_core::teammate::tmux::TMUX_PANE;

/// How long a pane waits for its own member record before refusing.
///
/// Defensive rather than the ordinary path: the lead writes the record before
/// it types this launch line, so the first look should already answer. The
/// bound turns the cases where it does not — a lead that died between the
/// record and the launch, a `ganja` started with these flags by hand — into a
/// sentence naming the file, instead of a pane sitting in a window waiting
/// for a record nobody will write.
pub const RECORD_WAIT: Duration = Duration::from_secs(5);

/// Between looks at the team file while waiting for the record.
const RECORD_POLL: Duration = Duration::from_millis(50);

/// What §4.1's launch line said, as the command line hands it in.
///
/// Strings rather than the team crate's own names, for [`crate::Resume`]'s
/// reason: the CLI carries the words in, and every refusal — a name that
/// does not parse, an id that names a different member — happens once, in
/// [`Membership::resolve`], before the terminal is taken over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flags {
    /// `--agent-id`: §2.2's `<name>@<team>`, as the lead recorded it.
    pub agent_id: String,
    /// `--agent-name`: the member name this process answers to.
    pub name: String,
    /// `--team-name`: the team whose documents this process reads.
    pub team: String,
    /// `--agent-color`: §4.3's assigned colour, where the lead gave one.
    pub color: Option<String>,
    /// `--parent-session-id`: the lead's session, which is what named the
    /// team's directory.
    pub parent_session_id: String,
}

/// Who this process is, once the flags have been checked against the grammar
/// the lead resolved them under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Membership {
    name: MemberName,
    team: TeamName,
    lead: MemberName,
    root: TeamsRoot,
    color: Option<String>,
    parent_session_id: String,
    surface: Surface,
    posture: Posture,
}

impl Membership {
    /// Checks the launch line and resolves where the team's documents live.
    ///
    /// The teams root is asked of the registry a lead would build for the same
    /// session, rather than composed here from a directory name of this
    /// crate's own: the directory's name is `ganja-core`'s constant, and a
    /// pane that spelled it itself would one day read a different directory
    /// from the one its lead wrote into. Building the registry touches
    /// nothing on disk.
    ///
    /// The `--agent-id` is checked against the name and the team it should be
    /// derived from, and a mismatch is refused: the id is what the lead
    /// recorded, and a pane running under a name its record does not carry
    /// would answer to one member and be shut down as another.
    ///
    /// The surface is `pane` exactly when `TMUX_PANE` names one, so the
    /// `shutdown_approved` this process eventually writes tells the lead which
    /// pane to kill; a process launched with these flags outside tmux reports
    /// no pane, which is the truth about it.
    ///
    /// The posture is [`Posture::ForwardToLead`], and nothing on the line can
    /// say otherwise: the record carries no posture of its own (Claude's shape
    /// holds `planModeRequired` and nothing else about it), and since **D513**
    /// a lead composes no posture onto the launch line either — the bypass trio
    /// that once rode it was the retired `--bypass`'s only carrier.
    ///
    /// # Errors
    ///
    /// A name or team the grammar refuses, or an id that does not name this
    /// member.
    pub fn resolve(
        flags: Flags,
        config_home: &Path,
        cwd: &Path,
        pane: Option<String>,
    ) -> Result<Self> {
        let name = MemberName::parse(&flags.name)
            .with_context(|| format!("--agent-name {:?} is refused", flags.name))?;
        let team = TeamName::parse(&flags.team)
            .with_context(|| format!("--team-name {:?} is refused", flags.team))?;
        let expected = name.agent_id(&team);
        if flags.agent_id != expected {
            bail!(
                "--agent-id {:?} does not name --agent-name {:?} of --team-name {:?} (expected {expected:?})",
                flags.agent_id,
                flags.name,
                flags.team,
            );
        }
        let root = TeammateRegistry::for_session(config_home, &flags.parent_session_id, cwd)
            .root()
            .clone();
        let surface = match pane.filter(|id| !id.is_empty()) {
            Some(id) => Surface::Pane { id },
            None => Surface::InProcess,
        };

        Ok(Self {
            name,
            team,
            lead: MemberName::lead(),
            root,
            color: flags.color,
            parent_session_id: flags.parent_session_id,
            surface,
            posture: Posture::ForwardToLead,
        })
    }

    /// Where an ask this member's rules raise is answered (D-5).
    #[must_use]
    pub fn posture(&self) -> Posture {
        self.posture
    }

    /// The team file's record of this member, or [`None`] when the document does
    /// not name it — including when there is no document at all.
    ///
    /// An absent file is [`None`] rather than an error, and that tolerance is
    /// **defensive** rather than the ordinary path: the lead writes the record
    /// before it launches this process (see [`Membership::await_record`]). What it
    /// answers for is a lead that died in between, or a `ganja` started with these
    /// flags by hand — cases where "no record" is the honest answer and a failure
    /// would name the wrong thing.
    ///
    /// # Errors
    ///
    /// A team file that is there and does not decode: that is somebody else's
    /// document being wrong, not a record still on its way.
    fn record(&self) -> Result<Option<MemberRecord>> {
        let path = self.root.config_path(&self.team);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("the team file {} could not be read", path.display())
                });
            }
        };
        let team: TeamFile = serde_json::from_str(&text)
            .with_context(|| format!("the team file {} does not decode", path.display()))?;

        Ok(team.member(self.name.as_str()).cloned())
    }

    /// Waits for the lead to write this member's record, and hands it back.
    ///
    /// Bounded by `limit`, polled rather than watched, and expected to answer on
    /// its **first** look: the lead writes the record before it types this
    /// process's launch line (see the module doc), so the row is already there.
    /// The loop is what covers the cases where it is not — a lead that died
    /// between the record and the launch, a rename not yet visible here, or a
    /// `ganja` somebody started with these flags by hand — and the bound is what
    /// turns all three into a sentence naming the file rather than a wait nobody
    /// can end.
    ///
    /// # Errors
    ///
    /// The record was not written within `limit`, naming the file a lead
    /// would have written it into; or the file is there and will not decode.
    pub async fn await_record(&self, limit: Duration) -> Result<MemberRecord> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            if let Some(record) = self.record()? {
                return Ok(record);
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "no lead wrote a record for teammate {:?} into {} within {limit:?}",
                    self.name.as_str(),
                    self.root.config_path(&self.team).display(),
                );
            }
            tokio::time::sleep(RECORD_POLL).await;
        }
    }

    /// The name this process answers to.
    #[must_use]
    pub fn name(&self) -> &MemberName {
        &self.name
    }

    /// The team it is a member of.
    #[must_use]
    pub fn team(&self) -> &TeamName {
        &self.team
    }

    /// Where the team's documents live — the lead's own teams root.
    #[must_use]
    pub fn root(&self) -> &TeamsRoot {
        &self.root
    }

    /// §4.3's colour, where the lead assigned one.
    #[must_use]
    pub fn color(&self) -> Option<&str> {
        self.color.as_deref()
    }

    /// The lead's session, which named the team.
    #[must_use]
    pub fn parent_session_id(&self) -> &str {
        &self.parent_session_id
    }

    /// The pane this process runs in, where it runs in one.
    #[must_use]
    fn surface(&self) -> &Surface {
        &self.surface
    }

    /// This member's own inbox.
    #[must_use]
    pub fn inbox(&self) -> PathBuf {
        self.root.inbox_path(&self.team, &self.name)
    }

    /// The lead's inbox — where this member's own frames go.
    #[must_use]
    pub fn lead_inbox(&self) -> PathBuf {
        self.root.inbox_path(&self.team, &self.lead)
    }
}

/// What one pass of §6.1 found and did.
///
/// Returned rather than only logged, for the runner's `Tick`'s reason: the
/// ordering — the shutdown ahead of everything, the frames acted on and never
/// queued, the plain messages batched — is the part of §6.1 that is the
/// contract, and a test drives one pass to assert it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    /// The request id of a `shutdown_request` this pass found, if it found
    /// one. A pass that carries this carries nothing else: the shutdown went
    /// ahead of everything and the rest of the inbox is left where it was.
    pub shutdown: Option<String>,
    /// The plain messages, oldest first, still owed a delivery — plus every
    /// `task_assignment` the lead wrote, rendered as a message from the lead.
    pub messages: Vec<Delivered>,
    /// The permission modes the lead set, in the order it set them (D-15).
    pub modes: Vec<PermissionMode>,
    /// The lead's answers to asks this member forwarded (D-5, AC-8), in the
    /// order they arrived — each the engine's own id and the reply the app
    /// sends it. A stale answer never reaches here: [`Asks::resolve`] ignores
    /// it, and this pass counts it among the ignored.
    pub answers: Vec<(PermissionId, PermissionReply)>,
    /// How many approvals were ignored as stale.
    pub ignored: usize,
    /// The frames this pass named and dropped, by kind.
    pub dropped: Vec<&'static str>,
}

impl Pass {
    /// Whether this pass found nothing at all, which is what almost every pass
    /// finds.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shutdown.is_none()
            && self.messages.is_empty()
            && self.modes.is_empty()
            && self.answers.is_empty()
            && self.ignored == 0
            && self.dropped.is_empty()
    }
}

/// This member's own mailbox, and the §6.1 pass over it.
#[derive(Debug)]
pub struct Inbox {
    membership: Membership,
    inbox: PathBuf,
    lead_inbox: PathBuf,
    /// The asks this member's engine raised and forwarded to the lead, still
    /// waiting on the answer (D-5) — `ganja-core`'s value, driven from here.
    asks: Asks,
    /// The mailbox identity behind a message this pass **rendered** rather
    /// than carried verbatim — a `task_assignment` — keyed by the identity of
    /// what the app holds. [`Delivered::identity`] derives from the three
    /// fields the value carries, and a rendered body is not the frame's text,
    /// so the app's identity would prune nothing; this is what maps it back.
    rendered: Mutex<HashMap<mailbox::Identity, mailbox::Identity>>,
}

impl Inbox {
    /// The inbox of the member `membership` names.
    #[must_use]
    pub fn new(membership: Membership) -> Self {
        let inbox = membership.inbox();
        let lead_inbox = membership.lead_inbox();
        let asks = Asks::new(membership.name.clone(), &membership.team, &membership.root);

        Self {
            membership,
            inbox,
            lead_inbox,
            asks,
            rendered: Mutex::new(HashMap::new()),
        }
    }

    /// The asks waiting on the lead.
    #[must_use]
    pub fn asks(&self) -> &Asks {
        &self.asks
    }

    /// Who this inbox belongs to.
    #[must_use]
    pub fn membership(&self) -> &Membership {
        &self.membership
    }

    /// One pass of §6.1, in its order.
    ///
    /// Control frames are pruned **here**, because acting on one is the whole
    /// of what it needed. Plain messages are not: whether one reached the
    /// conversation is the caller's fact, so they stay until
    /// [`Inbox::delivered`] says so.
    pub async fn poll(&self) -> Pass {
        let mut pass = Pass::default();
        let Some(contents) = runner::read_inbox(self.inbox.clone(), self.name()).await else {
            return pass;
        };
        if contents.valid.is_empty() {
            return pass;
        }

        // Step 1, and a step of its own because it goes first: a teammate
        // wedged behind a queue of messages stays reclaimable.
        let shutdown = contents
            .valid
            .iter()
            .enumerate()
            .find_map(|(position, message)| match message.frame() {
                Some(Frame::ShutdownRequest(request)) => Some((position, message, request)),
                _ => None,
            });
        if let Some((position, message, request)) = shutdown {
            tracing::info!(
                member = self.name(),
                request = request.request_id,
                jumped = position,
                "a shutdown request goes ahead of everything else in the inbox"
            );
            self.prune(vec![mailbox::identity(message)]).await;
            pass.shutdown = Some(request.request_id);

            return pass;
        }

        // Steps 2 and 3.
        let mut handled = Vec::new();
        for message in &contents.valid {
            let Some(kind) = Frame::reserved_kind(&message.text) else {
                pass.messages.push(plain(message));
                continue;
            };
            match self.apply(kind, message) {
                Verdict::Mode(mode) => {
                    pass.modes.push(mode);
                    handled.push(mailbox::identity(message));
                }
                Verdict::Answer(id, reply) => {
                    pass.answers.push((id, reply));
                    handled.push(mailbox::identity(message));
                }
                Verdict::Ignored => {
                    pass.ignored += 1;
                    handled.push(mailbox::identity(message));
                }
                Verdict::Dropped(name) => {
                    pass.dropped.push(name);
                    handled.push(mailbox::identity(message));
                }
                Verdict::Tell(delivered) => {
                    self.rendered
                        .lock()
                        .expect("the rendering map is never poisoned")
                        .insert(delivered.identity(), mailbox::identity(message));
                    pass.messages.push(delivered);
                }
            }
        }
        if !handled.is_empty() {
            self.prune(handled).await;
        }

        pass
    }

    /// Takes the messages the app really delivered out of the inbox.
    ///
    /// Separate from [`Inbox::poll`] because only the app knows: a batch the
    /// engine refused is left where it was and offered again next pass, which
    /// is a delivery delayed rather than a delivery lost.
    pub async fn delivered(&self, messages: &[Delivered]) {
        if messages.is_empty() {
            return;
        }
        let identities = {
            let mut rendered = self
                .rendered
                .lock()
                .expect("the rendering map is never poisoned");

            messages
                .iter()
                .map(|message| {
                    let held = message.identity();
                    rendered.remove(&held).unwrap_or(held)
                })
                .collect()
        };
        self.prune(identities).await;
    }

    /// Tells the lead this member's turn ended, and how (§10.3-3).
    ///
    /// The `from` is **this member's own name**, taken from the value it was
    /// constructed with: a frame that carried a sender of its own would let
    /// whoever wrote it choose whose name the lead reads. `failure_reason` is
    /// capped at write: it is a notification, and the reader caps it again at
    /// §5.3's second cap anyway, so nothing a person reads is lost — where a
    /// provider's whole refusal body in a mailbox entry is a lot of bytes for
    /// one sentence of news.
    pub async fn report_idle(&self, reason: FinishReason, failure: Option<&str>) {
        let idle = Frame::IdleNotification(IdleNotification {
            from: self.name().to_owned(),
            timestamp: record::now_iso8601(),
            idle_reason: Some(idle_reason(reason)),
            summary: None,
            completed_task_id: None,
            completed_status: None,
            failure_reason: failure.map(|text| cap_for_display(text).to_owned()),
        });
        self.tell_lead(&idle, "an idle notification").await;
    }

    /// Answers a `shutdown_request`, naming the pane the lead has to kill.
    ///
    /// Written **before** the process exits and after its turn has ended,
    /// which is the app's to sequence: the lead retires the member on this
    /// frame, and a member that had gone quiet without writing it would sit
    /// in the roster forever.
    pub async fn approve_shutdown(&self, request_id: &str) {
        let pane = match self.membership.surface() {
            Surface::Pane { id } => Some(id.clone()),
            // A shim member has no `ganja` frontend of its own to reach this
            // code — the shimmed CLI is what runs in its place, and this is an
            // in-process member's own `approve_shutdown`. A shim member in a
            // pane (P28, D512) reads back through `surface()` as `Surface::Pane`
            // rather than `Surface::Shim` (its `tmuxPaneId` holds the real
            // `%N`), so the `Shim` arm is unreachable here — but that path
            // belongs to the shim runtime's own teardown, not to this frontend,
            // so nothing routes such a member through here anyway.
            Surface::Leader | Surface::InProcess | Surface::Shim { .. } => None,
        };
        let approved = Frame::ShutdownApproved(ShutdownApproved {
            request_id: request_id.to_owned(),
            from: self.name().to_owned(),
            timestamp: record::now_iso8601(),
            backend_type: pane
                .is_some()
                .then(|| self.membership.surface().backend_type().to_owned()),
            pane_id: pane,
        });
        self.tell_lead(&approved, "a shutdown answer").await;
    }

    /// The name this inbox's frames are stamped with.
    fn name(&self) -> &str {
        self.membership.name.as_str()
    }

    /// What one frame is worth, once it is known to be one.
    fn apply(&self, kind: &'static str, message: &MailboxMessage) -> Verdict {
        // Undecodable, or from anybody but the lead: both are the same answer,
        // and the second is the whole of §7-2. `LeadFrame` cannot be built
        // from a peer's frame, so the three lead-only handlers below are
        // unreachable for one by construction rather than by a check.
        let Some(frame) = message.frame() else {
            return self.drop_it(kind, message);
        };
        let Some(lead) = LeadFrame::parse(&message.from, self.membership.lead.as_str(), frame)
        else {
            return self.drop_it(kind, message);
        };
        // The lead answering an ask this member forwarded, resolved by the
        // value that holds the wait — handed the proof itself, so a peer's
        // answer cannot even be passed in (§7-2), and a stale one is ignored
        // rather than applied (§7-3). Either way it leaves the inbox.
        if matches!(lead.frame(), Frame::PermissionResponse(_)) {
            return match self.asks.resolve(lead) {
                Resolved::Answered { id, reply } => Verdict::Answer(id, reply),
                Resolved::Stale { .. } | Resolved::NotAnAnswer { .. } => Verdict::Ignored,
            };
        }

        match lead.into_inner() {
            Frame::PlanApprovalResponse(response) => {
                // Nothing in this build raises a `plan_approval_request`, so
                // every answer is to a question this member never asked. It
                // still leaves the inbox: leaving it would be reading it again
                // on every pass forever.
                tracing::info!(
                    member = self.name(),
                    request = response.request_id,
                    "{IGNORED_STALE}"
                );

                Verdict::Ignored
            }
            Frame::ModeSetRequest(request) => self.mode_set(&request, message),
            Frame::TaskAssignment(assignment) => Verdict::Tell(assigned(message, assignment)),
            _ => self.drop_it(kind, message),
        }
    }

    /// The lead setting this member's permission mode.
    ///
    /// Claude's mode vocabulary is not ganja's, and the mapping has a refusal
    /// in it (**D496**): a mode this build has no posture for is dropped by
    /// name rather than rounded to the nearest one.
    fn mode_set(&self, request: &ModeSetRequest, message: &MailboxMessage) -> Verdict {
        match PermissionMode::from_claude_name(&request.mode) {
            Ok(mode) => Verdict::Mode(mode),
            Err(refusal) => {
                tracing::warn!(
                    member = self.name(),
                    from = message.from,
                    %refusal,
                    "{DROPPED_FRAME}: mode_set_request"
                );

                Verdict::Dropped("mode_set_request")
            }
        }
    }

    /// Names a frame nobody here handles — the runner's own account of it,
    /// under this member's name.
    fn drop_it(&self, kind: &'static str, message: &MailboxMessage) -> Verdict {
        runner::drop_frame(self.name(), kind, message);

        Verdict::Dropped(kind)
    }

    /// Writes one frame into the lead's inbox, stamped with this member's own
    /// name — the runner's writer, which shouts when it could not.
    async fn tell_lead(&self, frame: &Frame, what: &'static str) {
        runner::write_frame(self.lead_inbox.clone(), self.name(), frame, what).await;
    }

    /// Takes entries out of the inbox, in one write.
    async fn prune(&self, handled: Vec<mailbox::Identity>) {
        runner::prune_inbox(self.inbox.clone(), handled, self.name()).await;
    }
}

/// What one frame turned out to be worth.
enum Verdict {
    /// A permission mode the lead set, for the app to send the engine.
    Mode(PermissionMode),
    /// The lead's answer to a forwarded ask, for the app to send the engine.
    Answer(PermissionId, PermissionReply),
    /// Stale, or not this member's to act on. It still leaves the inbox.
    Ignored,
    /// Named and dropped.
    Dropped(&'static str),
    /// Rendered as something the member reads, so it leaves the inbox with
    /// the batch it travels in.
    Tell(Delivered),
}

/// A message nobody has to interpret, as the app takes it.
///
/// [`Delivery::Acknowledged`], and it is the member's own engine that
/// acknowledges: the strip shows the entry pending until `SteerConsumed` says
/// the turn took it, and the mailbox holds it until then.
fn plain(message: &MailboxMessage) -> Delivered {
    Delivered {
        from: message.from.clone(),
        timestamp: message.timestamp.clone(),
        summary: message.summary.clone(),
        color: message.color.clone(),
        body: message.text.clone(),
        delivery: Delivery::Acknowledged,
    }
}

/// A `task_assignment` the lead wrote, as the message it becomes.
///
/// From the envelope's sender rather than the frame's `assignedBy`, for the
/// runner's reason on the answering side: the envelope is the half the lead
/// wrote the inbox path from, and a frame that could name somebody else there
/// could put words in that somebody's mouth. The subject is the summary the
/// strip shows and the first line the model reads.
fn assigned(message: &MailboxMessage, assignment: TaskAssignment) -> Delivered {
    Delivered {
        from: message.from.clone(),
        timestamp: message.timestamp.clone(),
        summary: Some(assignment.subject.clone()),
        color: message.color.clone(),
        body: format!("{}\n\n{}", assignment.subject, assignment.description),
        delivery: Delivery::Acknowledged,
    }
}

/// §10.3-3's mapping, in one place.
const fn idle_reason(reason: FinishReason) -> IdleReason {
    match reason {
        FinishReason::Completed => IdleReason::Available,
        FinishReason::Cancelled => IdleReason::Interrupted,
        FinishReason::Failed => IdleReason::Failed,
    }
}

/// A `shutdown_request` from the lead, as a test writes one into an inbox.
#[cfg(test)]
pub(crate) fn shutdown_request(request_id: &str) -> Frame {
    Frame::ShutdownRequest(ganja_protocol::team::ShutdownRequest {
        request_id: request_id.to_owned(),
        from: MemberName::lead().into_inner(),
        reason: None,
        timestamp: record::now_iso8601(),
    })
}

/// §2.1's own example session, so the team a test's fixtures name on disk is
/// `session-224cbeab` and a reader can find it by hand.
#[cfg(test)]
pub(crate) const PARENT: &str = "224cbeab-4e62-497c-aa8f-d05cc33ce7ba";

/// §4.1's launch line for member `name` of §2.1's example team.
#[cfg(test)]
fn flags(name: &str) -> Flags {
    Flags {
        agent_id: format!("{name}@session-224cbeab"),
        name: name.to_owned(),
        team: "session-224cbeab".to_owned(),
        color: Some("blue".to_owned()),
        parent_session_id: PARENT.to_owned(),
    }
}

/// Teammate `w1` of §2.1's example team, resolved under `root` — the one
/// membership fixture this crate's tests share.
#[cfg(test)]
pub(crate) fn membership(root: &Path, pane: Option<&str>) -> Membership {
    Membership::resolve(flags("w1"), root, root, pane.map(str::to_owned))
        .expect("the flags resolve")
}

/// Writes one plain message into `inbox`, as a test's lead or peer would.
#[cfg(test)]
pub(crate) fn write(inbox: &Path, from: &str, text: &str) {
    mailbox::write(
        inbox,
        MailboxMessage::new(from, text, record::now_iso8601()),
    )
    .expect("the inbox takes a message");
}

/// Writes one frame into `inbox`, from `from`.
#[cfg(test)]
pub(crate) fn write_frame(inbox: &Path, from: &str, frame: &Frame) {
    let message =
        MailboxMessage::from_frame(from, frame, record::now_iso8601()).expect("the frame encodes");
    mailbox::write(inbox, message).expect("the inbox takes a frame");
}

/// Every valid message `inbox` holds.
#[cfg(test)]
pub(crate) fn held(inbox: &Path) -> Vec<MailboxMessage> {
    mailbox::read(inbox).expect("the inbox reads").valid
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ganja_core::{
        team::{MemberName, TeamName, record},
        teammate::posture::Posture,
    };
    use ganja_protocol::{
        Event, FinishReason, PermissionId, PermissionMode, PermissionReply, SessionId,
        team::{
            Frame, IdleReason, ModeSetRequest, PermissionResponse, PermissionResponseBody,
            PlanApprovalResponse, TaskAssignment,
        },
    };
    use tempfile::TempDir;

    use super::{
        Flags, Inbox, Membership, PARENT, RECORD_POLL, flags, held, membership, shutdown_request,
        write, write_frame,
    };

    /// The `w1` fixture, under a temporary home.
    fn member(home: &TempDir, pane: Option<&str>) -> Membership {
        membership(home.path(), pane)
    }

    /// The root the flags resolve to is the one a lead of the same session
    /// writes into — asked of the registry, never spelled here.
    #[test]
    fn a_member_reads_the_directory_its_lead_wrote_into() {
        let home = tempfile::tempdir().expect("a temporary home");
        let member = member(&home, Some("%7"));
        let lead =
            ganja_core::teammate::TeammateRegistry::for_session(home.path(), PARENT, home.path());

        assert_eq!(member.lead_inbox(), lead.lead_inbox());
        assert_eq!(
            member.inbox(),
            lead.root()
                .inbox_path(lead.team(), &MemberName::parse("w1").expect("a name")),
        );
        assert_eq!(
            member.team(),
            &TeamName::parse("session-224cbeab").expect("a team")
        );
        assert_eq!(member.color(), Some("blue"));
        assert_eq!(member.parent_session_id(), PARENT);
        assert_eq!(
            member.surface(),
            &ganja_core::team::Surface::Pane {
                id: "%7".to_owned()
            }
        );
    }

    /// The id the lead recorded has to be the one these flags describe.
    #[test]
    fn an_agent_id_naming_another_member_is_refused_before_anything_runs() {
        let home = tempfile::tempdir().expect("a temporary home");
        let mut wrong = flags("w1");
        wrong.agent_id = "w2@session-224cbeab".to_owned();

        let refused = Membership::resolve(wrong, home.path(), home.path(), None)
            .expect_err("a mismatched id is refused");

        assert!(
            refused.to_string().contains("w1@session-224cbeab"),
            "the refusal names what was expected: {refused}"
        );
        assert!(
            Membership::resolve(
                Flags {
                    name: "main".to_owned(),
                    ..flags("main")
                },
                home.path(),
                home.path(),
                None,
            )
            .is_err(),
            "and the reserved name is refused by the grammar"
        );
    }

    /// The seeded message — the preamble around the task — is the first message the first pass finds, and it is
    /// still owed until the app says it landed (§10.3-2).
    #[tokio::test]
    async fn the_seeded_prompt_is_a_plain_message_that_stays_until_delivered() {
        let home = tempfile::tempdir().expect("a temporary home");
        let inbox = Inbox::new(member(&home, None));
        write(&inbox.inbox, "team-lead", "start on the parser");

        let pass = inbox.poll().await;

        assert_eq!(pass.messages.len(), 1);
        assert_eq!(pass.messages[0].from, "team-lead");
        assert_eq!(pass.messages[0].body, "start on the parser");
        assert_eq!(held(&inbox.inbox).len(), 1, "not pruned by the read");

        inbox.delivered(&pass.messages).await;

        assert!(held(&inbox.inbox).is_empty(), "delivered means gone");
    }

    /// §6.1's first step: a shutdown request goes ahead of everything.
    #[tokio::test]
    async fn a_shutdown_request_goes_ahead_of_everything_else() {
        let home = tempfile::tempdir().expect("a temporary home");
        let inbox = Inbox::new(member(&home, Some("%3")));
        write(&inbox.inbox, "team-lead", "one");
        write(&inbox.inbox, "w2", "two");
        write_frame(&inbox.inbox, "team-lead", &shutdown_request("req-9"));

        let pass = inbox.poll().await;

        assert_eq!(pass.shutdown.as_deref(), Some("req-9"));
        assert!(
            pass.messages.is_empty(),
            "nothing else is delivered: {pass:?}"
        );
        assert_eq!(
            held(&inbox.inbox).len(),
            2,
            "the request left the inbox, the messages it jumped did not"
        );
    }

    /// The answer names the pane and the backend, stamped with this member's
    /// own name, in the lead's inbox.
    #[tokio::test]
    async fn a_shutdown_answer_names_the_pane_and_reaches_the_lead() {
        let home = tempfile::tempdir().expect("a temporary home");
        let inbox = Inbox::new(member(&home, Some("%3")));

        inbox.approve_shutdown("req-9").await;

        let written = held(&inbox.lead_inbox);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].from, "w1");
        match written[0].frame() {
            Some(Frame::ShutdownApproved(approved)) => {
                assert_eq!(approved.request_id, "req-9");
                assert_eq!(approved.from, "w1");
                assert_eq!(approved.pane_id.as_deref(), Some("%3"));
                assert_eq!(approved.backend_type.as_deref(), Some("tmux"));
            }
            other => panic!("a shutdown_approved was expected, got {other:?}"),
        }
    }

    /// Outside tmux there is no pane to name, and none is invented.
    #[tokio::test]
    async fn a_member_with_no_pane_names_none() {
        let home = tempfile::tempdir().expect("a temporary home");
        let inbox = Inbox::new(member(&home, Some("")));

        inbox.approve_shutdown("req-1").await;

        match held(&inbox.lead_inbox)[0].frame() {
            Some(Frame::ShutdownApproved(approved)) => {
                assert_eq!(approved.pane_id, None);
                assert_eq!(approved.backend_type, None);
            }
            other => panic!("a shutdown_approved was expected, got {other:?}"),
        }
    }

    /// §10.3-3's mapping, and the frame's own `from`.
    #[tokio::test]
    async fn the_turns_end_maps_onto_the_three_idle_reasons() {
        let home = tempfile::tempdir().expect("a temporary home");
        let inbox = Inbox::new(member(&home, None));

        inbox.report_idle(FinishReason::Completed, None).await;
        inbox.report_idle(FinishReason::Cancelled, None).await;
        inbox
            .report_idle(FinishReason::Failed, Some("the provider hung up"))
            .await;

        let reasons: Vec<_> = held(&inbox.lead_inbox)
            .iter()
            .map(|message| {
                assert_eq!(message.from, "w1");
                match message.frame() {
                    Some(Frame::IdleNotification(idle)) => {
                        assert_eq!(idle.from, "w1");
                        (idle.idle_reason, idle.failure_reason)
                    }
                    other => panic!("an idle_notification was expected, got {other:?}"),
                }
            })
            .collect();

        assert_eq!(
            reasons,
            [
                (Some(IdleReason::Available), None),
                (Some(IdleReason::Interrupted), None),
                (
                    Some(IdleReason::Failed),
                    Some("the provider hung up".to_owned())
                ),
            ]
        );
    }

    /// §7-2 as a type: the lead's mode is applied, a peer's identical frame is
    /// dropped by name, and a mode this build cannot hold is refused rather
    /// than rounded (**D496**). Every one of them leaves the inbox.
    #[tokio::test]
    async fn a_mode_is_taken_from_the_lead_only_and_refused_by_name_when_unknown() {
        let home = tempfile::tempdir().expect("a temporary home");
        let inbox = Inbox::new(member(&home, None));
        let bypass = |from: &str| {
            Frame::ModeSetRequest(ModeSetRequest {
                mode: "bypassPermissions".to_owned(),
                from: from.to_owned(),
            })
        };
        write_frame(&inbox.inbox, "team-lead", &bypass("team-lead"));
        write_frame(&inbox.inbox, "w2", &bypass("team-lead"));
        write_frame(
            &inbox.inbox,
            "team-lead",
            &Frame::ModeSetRequest(ModeSetRequest {
                mode: "plan".to_owned(),
                from: "team-lead".to_owned(),
            }),
        );

        let pass = inbox.poll().await;

        assert_eq!(pass.modes, [PermissionMode::Bypass]);
        assert_eq!(pass.dropped, ["mode_set_request", "mode_set_request"]);
        assert!(pass.messages.is_empty());
        assert!(held(&inbox.inbox).is_empty(), "every frame left the inbox");
    }

    /// Nothing here asks for a plan, so every approval is stale — and a peer's
    /// approval is not even that, it is dropped.
    #[tokio::test]
    async fn a_plan_approval_is_stale_by_definition_and_leaves_the_inbox() {
        let home = tempfile::tempdir().expect("a temporary home");
        let inbox = Inbox::new(member(&home, None));
        // No `from` on this frame at all: the envelope is the only sender it
        // has, which is exactly why the peer's copy below is dropped on the
        // envelope alone.
        let approval = Frame::PlanApprovalResponse(PlanApprovalResponse {
            request_id: "plan-1".to_owned(),
            approved: true,
            feedback: None,
            timestamp: record::now_iso8601(),
            permission_mode: None,
        });
        write_frame(&inbox.inbox, "team-lead", &approval);
        write_frame(&inbox.inbox, "w2", &approval);

        let pass = inbox.poll().await;

        assert_eq!(pass.ignored, 1);
        assert_eq!(pass.dropped, ["plan_approval_response"]);
        assert!(held(&inbox.inbox).is_empty());
    }

    /// A `task_assignment` from the lead becomes this member's next turn, and
    /// is pruned by the identity the app holds even though the body it holds
    /// is the rendering rather than the frame.
    #[tokio::test]
    async fn a_task_assignment_from_the_lead_becomes_a_message_and_prunes_by_the_rendered_identity()
    {
        let home = tempfile::tempdir().expect("a temporary home");
        let inbox = Inbox::new(member(&home, None));
        let assignment = |assigned_by: &str| {
            Frame::TaskAssignment(TaskAssignment {
                task_id: "t-1".to_owned(),
                subject: "look at the parser".to_owned(),
                description: "the whole of it".to_owned(),
                assigned_by: assigned_by.to_owned(),
                timestamp: record::now_iso8601(),
            })
        };
        write_frame(&inbox.inbox, "team-lead", &assignment("team-lead"));
        write_frame(&inbox.inbox, "w2", &assignment("team-lead"));

        let pass = inbox.poll().await;

        assert_eq!(pass.messages.len(), 1);
        assert_eq!(pass.messages[0].from, "team-lead");
        assert_eq!(
            pass.messages[0].summary.as_deref(),
            Some("look at the parser")
        );
        assert_eq!(
            pass.messages[0].body,
            "look at the parser\n\nthe whole of it"
        );
        assert_eq!(
            pass.dropped,
            ["task_assignment"],
            "a peer cannot assign work"
        );
        assert_eq!(
            held(&inbox.inbox).len(),
            1,
            "the lead's stays until delivered"
        );

        inbox.delivered(&pass.messages).await;

        assert!(
            held(&inbox.inbox).is_empty(),
            "and the rendered identity found it"
        );
    }

    /// The other frames the harness may write are named and dropped, never
    /// read as prose.
    #[tokio::test]
    async fn an_unhandled_frame_is_dropped_by_name_and_never_delivered() {
        let home = tempfile::tempdir().expect("a temporary home");
        let inbox = Inbox::new(member(&home, None));
        write_frame(
            &inbox.inbox,
            "team-lead",
            &Frame::TeammateTerminated(ganja_protocol::team::TeammateTerminated {
                message: "w2 is gone".to_owned(),
            }),
        );

        let pass = inbox.poll().await;

        assert_eq!(pass.dropped, ["teammate_terminated"]);
        assert!(pass.messages.is_empty());
        assert!(held(&inbox.inbox).is_empty());
    }

    /// A pane's asks go to the lead's own dialog (D-5), and since **D513** the
    /// launch line carries nothing that could say otherwise.
    #[test]
    fn a_pane_forwards_its_asks_to_its_lead() {
        let home = tempfile::tempdir().expect("a temporary home");

        assert_eq!(member(&home, None).posture(), Posture::ForwardToLead);
        assert_eq!(
            membership(home.path(), Some("%5")).posture(),
            Posture::ForwardToLead,
            "a pane id changes the surface, never the posture"
        );
    }

    /// The record is the lead's to write before it types the launch line, so
    /// the wait is defensive — and a lead that never writes one is refused
    /// naming the file rather than waited on forever.
    #[tokio::test]
    async fn a_member_waits_for_its_record_and_refuses_when_no_lead_writes_one() {
        let home = tempfile::tempdir().expect("a temporary home");
        let member = member(&home, None);
        assert_eq!(member.record().expect("no file is no record"), None);

        let refused = member
            .await_record(RECORD_POLL)
            .await
            .expect_err("nothing wrote a record");
        assert!(
            refused.to_string().contains("config.json"),
            "the refusal names the file: {refused}"
        );

        // A lead of the same session writes it a moment later, exactly as the
        // registry does — the record after the spawn — and the wait finds it.
        let lead =
            ganja_core::teammate::TeammateRegistry::for_session(home.path(), PARENT, home.path());
        let path = lead.root().config_path(lead.team());
        let mut team = ganja_core::team::TeamFile::new(
            lead.team(),
            PARENT,
            home.path().display().to_string(),
            record::now_millis(),
        );
        team.members.push(ganja_core::team::MemberRecord::teammate(
            member.name(),
            lead.team(),
            ganja_core::team::Spawn {
                agent_type: "general".to_owned(),
                model: "recorder-model".to_owned(),
                color: "blue".to_owned(),
                prompt: "start on the parser".to_owned(),
                plan_mode_required: false,
                surface: ganja_core::team::Surface::Pane {
                    id: "%7".to_owned(),
                },
                cwd: home.path().display().to_string(),
            },
            record::now_millis(),
        ));
        let writer = tokio::spawn(async move {
            tokio::time::sleep(RECORD_POLL * 2).await;
            std::fs::create_dir_all(path.parent().expect("a team dir")).expect("the dir");
            std::fs::write(
                &path,
                ganja_core::team::record::document(&team).expect("the team encodes"),
            )
            .expect("the team file is written");
        });

        let found = member
            .await_record(Duration::from_secs(5))
            .await
            .expect("the record arrived within the wait");
        writer.await.expect("the writer finished");

        assert_eq!(found.name, "w1");
        assert_eq!(found.model.as_deref(), Some("recorder-model"));
    }

    /// The ask an engine raises, as the app hands it over.
    fn asked(id: &str) -> Event {
        Event::PermissionRequested {
            session_id: SessionId::from("ses_fixture".to_owned()),
            id: PermissionId::from(id.to_owned()),
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            title: "rm -rf build".to_owned(),
            args: serde_json::json!({"command": "rm -rf build"}),
            directories: vec!["/tmp/elsewhere".to_owned()],
        }
    }

    /// An ask travels to the lead's inbox as §5's `permission_request`, from
    /// this member's own name, and is remembered as waiting until the engine's
    /// own reply forgets it. The frame's fields are `Asks::forward`'s to fill,
    /// and core's own tests pin them.
    #[tokio::test]
    async fn a_forwarded_ask_reaches_the_lead_as_a_permission_request() {
        let home = tempfile::tempdir().expect("a temporary home");
        let inbox = Inbox::new(member(&home, None));

        inbox
            .asks()
            .forward(&asked("perm-1"))
            .await
            .expect("the lead's inbox takes the ask");

        assert_eq!(inbox.asks().waiting(), 1);
        let written = held(&inbox.lead_inbox);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].from, "w1");
        assert!(
            matches!(written[0].frame(), Some(Frame::PermissionRequest(_))),
            "a permission_request was expected, got {:?}",
            written[0].frame()
        );
        assert!(
            inbox
                .asks()
                .retire(&PermissionId::from("perm-1".to_owned())),
            "an engine's own reply forgets the wait"
        );
        assert_eq!(inbox.asks().waiting(), 0);
    }

    /// The lead's answer comes back through the pass as the reply the dialog
    /// gave — an "always" honored, since the person at the lead's dialog
    /// answered this member's own open ask — a peer's copy is dropped on the
    /// envelope, and an answer to nothing waited on is ignored. Every one
    /// leaves the inbox.
    #[tokio::test]
    async fn a_leads_answer_resolves_a_waiting_ask_and_a_peers_or_a_stale_one_does_not() {
        let home = tempfile::tempdir().expect("a temporary home");
        let inbox = Inbox::new(member(&home, None));
        inbox
            .asks()
            .forward(&asked("perm-1"))
            .await
            .expect("the ask is forwarded");
        inbox
            .asks()
            .forward(&asked("perm-2"))
            .await
            .expect("the ask is forwarded");
        let allowed = Frame::PermissionResponse(PermissionResponse::success(
            "perm-1",
            PermissionResponseBody {
                updated_input: serde_json::json!({"command": "rm -rf build"}),
                permission_updates: vec![serde_json::json!({
                    "type": "addRules",
                    "behavior": "allow",
                    "rules": [{"toolName": "bash"}],
                    "destination": "projectSettings",
                })],
            },
        ));
        let refused = Frame::PermissionResponse(PermissionResponse::error("perm-2", "no"));
        let stale = Frame::PermissionResponse(PermissionResponse::error("perm-9", "no"));
        write_frame(&inbox.inbox, "w2", &allowed);
        write_frame(&inbox.inbox, "team-lead", &allowed);
        write_frame(&inbox.inbox, "team-lead", &refused);
        write_frame(&inbox.inbox, "team-lead", &stale);

        let pass = inbox.poll().await;

        assert_eq!(
            pass.answers,
            [
                (
                    PermissionId::from("perm-1".to_owned()),
                    PermissionReply::Always
                ),
                (
                    PermissionId::from("perm-2".to_owned()),
                    PermissionReply::Reject
                ),
            ]
        );
        assert_eq!(
            pass.dropped,
            ["permission_response"],
            "a peer cannot answer"
        );
        assert_eq!(pass.ignored, 1, "an answer to nothing waited on is stale");
        assert_eq!(inbox.asks().waiting(), 0);
        assert!(held(&inbox.inbox).is_empty());
    }
}
