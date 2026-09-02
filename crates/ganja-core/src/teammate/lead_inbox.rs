//! What the lead does with its own inbox (§6.2).
//!
//! Upstream opencode has **no counterpart**, for [`crate::teammate`]'s reason: nothing
//! there outlives the call that started it, so no conversation has a mailbox
//! anybody writes into. What is ported is Claude Code's lead-side
//! `InboxPoller` — a *UI hook rather than a background service*, which is the
//! sentence that decides this module's shape: the pass below does the reading,
//! the classifying and the pruning, and hands back what a frontend has to act
//! on. It starts no task and owns no cadence; the tick that calls it does.
//!
//! # Why this is not in the frontend
//!
//! The plan puts `poll_team()` on the TUI's tick, and it still is — this is
//! what that tick calls. What could not live there is the *reading*:
//! `ganja-tui` does not depend on `ganja-team` and must not, since a frontend
//! that decoded Claude's documents would be a second copy of [`crate::teammate::runner`]'s
//! §6.1 pass in a crate with no registry to test it against. So the bytes are
//! read here, beside the loop that answers them, and the frontend is handed
//! values it only has to render and deliver.
//!
//! # The three frames a lead answers
//!
//! `shutdown_approved` retires the member — [`crate::teammate::TeammateRegistry::retire`]
//! drops it from the roster and takes its record out of the team file —
//! `idle_notification` is recorded as what it is, a teammate reporting itself
//! available, and `permission_request` is **routed**: a pane teammate's
//! dialog, put in front of the same person the in-process ones reach
//! (**D-5**, the pane half). Everything else frame-shaped is **dropped by
//! name** with the head of it, which is §6.1's own posture pointed the other
//! way: a frame this side has no handler for is named rather than acted on by
//! guess.
//!
//! The routing rides the one dialog channel the frontend already drains.
//! An in-process teammate's ask arrives on
//! [`crate::teammate::posture::Forwarded`]'s channel with a reply oneshot;
//! a pane's arrives in this file as §5's `permission_request`, and this pass
//! wraps it in **the same** [`crate::teammate::posture::Forwarded`] — the
//! frame's fields read back into the `PermissionRequested` a dialog is drawn
//! from ([`crate::teammate::member::dialog_of`]) — and offers it on that same
//! channel, so a frontend that answers one answers the other with no code of
//! its own. What comes back on the oneshot is written to the asker's inbox as
//! §5's `permission_response`; a channel nobody claimed, or one that is full,
//! is the refusal it is for an in-process ask (`try_send`, never a wait), and
//! that refusal is written back too, because a pane whose ask vanished into a
//! file would wait on it forever.
//!
//! What a routed ask proves, and what it does not: that a member's **process**
//! asked, not that its harness did — `permission_request` is agent-sendable
//! (§5.1), so a member's model can compose one through `send_message` and
//! its text reaches the lead's dialog. The teammate-name prefix the frontend
//! puts on the title is the mitigation on the reading side, and a fabricated
//! ask is harmless on the answering side only because the member's
//! [`crate::teammate::member::Asks::resolve`] applies an answer solely to a
//! request its own engine is still waiting on — a made-up id is answered into
//! a mailbox and ignored there as stale.
//!
//! Two drops are deliberate rather than unimplemented, and §7-1 is why:
//! `team_permission_update` never travels a mailbox in this build — the
//! mailbox is not an escalation channel — and `permission_response` is the
//! *answer* to a question, which the lead never asks over a frame. Both are
//! named and pruned rather than left to be read again every second.
//!
//! # Two roots, because a real `claude` reads only one of them
//!
//! A lead's replies do not all arrive in the same directory. A `ganja` teammate —
//! in-process or in a pane — writes into the team under ganja's own config home,
//! which is where [`crate::teammate::TeammateRegistry`] keeps its documents. A
//! real `claude` writes into `$CLAUDE_CONFIG_DIR/teams` and nothing will
//! persuade it otherwise (§2.1), which is why
//! [`crate::teammate::teams_root`] sits in core rather than in the backend
//! that spawns one. So a lead whose roster
//! holds a claude-backed member reads **both** `team-lead` inboxes each pass
//! (`LeadInbox::inboxes`), and a lead whose roster holds none reads only its
//! own — reading, and on a delivery writing, inside another program's config
//! directory is not something to do speculatively. When the two roots name one
//! directory (AC-13 points the lead's own root at claude's) it is one read, not
//! two: the same file twice in a pass would hand the same message out twice.
//!
//! # One write, not two
//!
//! [`crate::teammate::runner`]'s note applies unchanged: everything one pass finished
//! leaves the inbox in a single [`mailbox::prune_delivered`], because each
//! write is a full read-modify-write under `ganja-team`'s lock and two of them
//! are two chances to wait out a peer where one will do. What a frontend
//! **refuses** to deliver is not this module's to know — the messages come back
//! in the pass and are pruned by the caller once they have really landed
//! ([`crate::teammate::lead_inbox::LeadInbox::delivered`]).
//!
//! # The mailbox door of the admission gate (**D523**–**D525**)
//!
//! Since the gate landed, a gated pass
//! ([`crate::teammate::lead_inbox::LeadInbox::gated`]) classifies every entry
//! before anything above happens to it: a roster member's mail is ungated as
//! ever, and everything else is the gate's — admitted delivers without
//! re-gating, held skips (the entry stays durable, C1), and an unknown writer
//! is demoted to a peer from `unknown` and run through the normal peer gate,
//! its frames dropped by name rather than applied. The pass's own doc
//! (`poll_one`) carries the full table; the gate itself, its buffer and both
//! sets are [`crate::teammate::inbound`]'s.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ganja_protocol::team::{
    Frame, IdleNotification, MemberBackend, PermissionRequest, PermissionResponse, ShutdownApproved,
};
use ganja_team::{MailboxMessage, MemberName, Surface, TeamsRoot, mailbox, record};
use tokio::sync::oneshot;

use super::inbound::{Inbound, MailboxAdmission, PassDisposition, ReceiverClass};
use super::posture::{Forwarded, Undelivered};
use super::runner::{drop_frame, prune_inbox, read_inbox};
use super::{Delivery, Exited, REFUSED_NO_CONFIG_DIR, TeammateRegistry, member, teams_root};
use crate::protocol::{PermissionReply, SessionId};

/// §6's lead cadence, and deliberately half the teammate's own
/// ([`crate::teammate::runner::POLL`]): the teammate is the side that has to notice a
/// shutdown promptly, and the lead is the side a person is watching anyway.
pub const POLL: Duration = Duration::from_millis(1000);

/// What is logged when a pane's ask is answered with a refusal because nobody
/// could be shown it — [`crate::teammate::posture::Forwarding`]'s own line,
/// for the same two reasons it gives.
const REFUSED_ASK: &str = "a pane's permission dialog was refused rather than made to wait";

/// The error a refused ask carries back when this lead has no dialog surface
/// at all — a headless lead, or a session nothing attached one to.
const NO_DIALOG_SURFACE: &str = "the lead has no dialog to put this ask in front of anybody";

/// The same, when the surface is there and its queue is full.
const DIALOG_QUEUE_FULL: &str = "the lead's dialog queue is full";

/// The same, when the lead's side of the channel has gone.
const LEAD_GONE: &str = "the lead's side is gone";

/// One plain message on its way into the lead's conversation.
///
/// Everything a frontend needs to render it and to decide how long to render it
/// for. The three fields the identity is composed of are all here, which is
/// what makes [`crate::teammate::lead_inbox::Delivered::new`] safe to expose: §2.3's identity key is
/// `from|timestamp|text` precisely so that **any** reader can derive it, and a
/// value built from those three names the one message they came from and no
/// other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    /// Which member wrote it.
    pub from: String,
    /// When it was written, ISO-8601 — the second third of its identity.
    pub timestamp: String,
    /// The sender's one-line summary, where it wrote one.
    pub summary: Option<String>,
    /// The sender's assigned colour, where the team file gave it one.
    pub color: Option<String>,
    /// What it said, verbatim.
    pub body: String,
    /// What the sender's backend can tell the lead about a delivery
    /// (**D503**) — [`crate::teammate::Delivery::FireAndForget`] for a member this registry does
    /// not hold, since a peer nothing here started is a peer nothing here can
    /// watch consume anything.
    pub delivery: Delivery,
}

impl Delivered {
    /// One message, as a frontend would name it back.
    #[must_use]
    pub fn new(
        from: impl Into<String>,
        timestamp: impl Into<String>,
        body: impl Into<String>,
        delivery: Delivery,
    ) -> Self {
        Self {
            from: from.into(),
            timestamp: timestamp.into(),
            summary: None,
            color: None,
            body: body.into(),
            delivery,
        }
    }

    /// The identity [`crate::teammate::lead_inbox::LeadInbox::delivered`] prunes it by.
    ///
    /// Public because **delivery is not idempotent and only pruning is**: a
    /// plain message stays in the inbox until a caller says it landed, so
    /// every pass in between hands the same message out again, and a caller
    /// holding one in flight across passes has nothing but this key to
    /// recognise it by. Deriving it costs the three fields §2.3 composes it
    /// from, all of which are on this value.
    #[must_use]
    pub fn identity(&self) -> mailbox::Identity {
        mailbox::identity(&MailboxMessage::new(
            self.from.clone(),
            self.body.clone(),
            self.timestamp.clone(),
        ))
    }
}

/// A teammate whose `shutdown_approved` this pass read.
///
/// The pane fields are **carried rather than acted on**, and now that two pane
/// backends ship that is a rule rather than a phase: the pane is ended by
/// [`crate::teammate::TeammateRegistry::retire`], through the backend that
/// spawned it and against the `(pane_id, birth)` pair recorded then — never
/// against the `paneId` this frame names, because a member that could name
/// somebody else's there could have the lead kill a stranger's window. So these
/// two fields are for the record and for a log line, and nothing reads them to
/// decide anything. The retire has already happened by the time a caller sees
/// this ([`crate::teammate::lead_inbox::LeadInbox::poll`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retired {
    /// Who shut down.
    pub name: String,
    /// The pane it named, where it named one.
    pub pane_id: Option<String>,
    /// Claude's own backend word for it, which is not [`ganja_protocol::team::MemberBackend`].
    pub backend_type: Option<String>,
}

/// A teammate reporting itself available again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Idle {
    /// Who went idle.
    pub name: String,
    /// What it said it had been doing, where it said anything.
    pub summary: Option<String>,
}

/// A pane teammate's permission ask this pass read (**D-5**).
///
/// Carried for the record rather than for the caller to act on: the routing
/// is done by the time a caller sees this — the dialog is on the channel, or
/// the refusal is in the asker's inbox — and what a frontend does with a
/// forwarded ask it already does for the in-process ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    /// Who asked.
    pub name: String,
    /// The request id, which is the asking engine's own dialog id.
    pub request_id: String,
    /// The tool it asked about.
    pub tool: String,
    /// Whether it reached the dialog channel, or was refused on the spot
    /// because nothing could show it.
    pub raised: bool,
}

/// What one pass of §6.2 found and did.
///
/// Returned rather than only logged, for [`crate::teammate::runner::Tick`]'s reason: the
/// ordering — control frames acted on and never queued, plain messages batched
/// — is the part of §6.2 that is the contract, and a test drives one pass to
/// assert it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    /// The plain messages, oldest first, still owed a delivery.
    pub messages: Vec<Delivered>,
    /// The members this pass forgot — on their own `shutdown_approved`, or
    /// because their pane stopped running (those are also under `exited`).
    ///
    /// One member may appear twice in one pass: a TUI member whose inbox held
    /// a `shutdown_request` at the moment its pane died approves it as it
    /// exits *and* is reported exited, and each road retires it — harmless,
    /// since the second retire finds nothing to remove and the one consumer
    /// recounts the roster rather than subtracting.
    pub retired: Vec<Retired>,
    /// The TUI members whose panes stopped running after readiness, retired
    /// by this pass (**D512** as amended for bead g9u); what a frontend says
    /// about each is [`Exited::notice`].
    pub exited: Vec<Exited>,
    /// The teammates that reported themselves available.
    pub idle: Vec<Idle>,
    /// The pane asks this pass routed, or refused because it could not.
    pub asked: Vec<Asked>,
    /// The frames this pass named and dropped, by kind.
    pub dropped: Vec<&'static str>,
}

impl Pass {
    /// Whether this pass found nothing at all, which is what almost every pass
    /// finds.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
            && self.retired.is_empty()
            && self.exited.is_empty()
            && self.idle.is_empty()
            && self.asked.is_empty()
            && self.dropped.is_empty()
    }
}

/// The admission gate's half of the lead's pass (**D523**–**D525**): the
/// engine-owned [`Inbound`] this pass shares with the socket door, and the
/// read of this session's receiver class each pass decides under.
///
/// A closure rather than the engine, because this module must not hold the
/// engine that holds the registry this holds — the same cycle rule every
/// postbox keeps — and a class, not a mode: the D479 trio seed is half of
/// the classification and only the engine has both halves
/// (`Engine::receiver_class`).
struct Gate {
    inbound: Arc<Inbound>,
    receiver: Box<dyn Fn() -> Option<ReceiverClass> + Send + Sync>,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Gate").finish_non_exhaustive()
    }
}

/// The lead's own mailbox, and the §6.2 pass over it.
#[derive(Debug)]
pub struct LeadInbox {
    registry: Arc<TeammateRegistry>,
    /// Where a real `claude` teammate answers, when this machine resolves such a
    /// root at all ([`teams_root`]).
    ///
    /// Resolved once, at construction, because `CLAUDE_CONFIG_DIR` does not
    /// change under a running process — and *held* rather than re-read per pass
    /// so a tick does not touch the environment a thousand times an hour.
    /// [`None`] only when neither that variable nor a home directory resolves,
    /// which is [`REFUSED_NO_CONFIG_DIR`]'s own case and one in which no
    /// claude teammate could have been spawned either.
    claude: Option<TeamsRoot>,
    /// The admission gate, once [`LeadInbox::gated`] installed it. [`None`]
    /// keeps the pre-gate pass — every entry delivered, every frame applied —
    /// which is the **bridge** the TUI's construction still stands on until
    /// its own lane moves it (W3-L3b); the gated pass is the production
    /// shape, and every admission test drives it.
    gate: Option<Gate>,
}

impl LeadInbox {
    /// The inbox of the team `registry` leads, and the root a `claude` teammate
    /// answers into as this machine resolves it.
    #[must_use]
    pub fn new(registry: Arc<TeammateRegistry>) -> Self {
        Self::reading(registry, teams_root())
    }

    /// The same, over a claude root given as a **value**.
    ///
    /// [`LeadInbox::new`] reads that root off the environment; this takes it,
    /// which is the split [`teams_root`] and its own `claude_root_under` already
    /// keep — a caller that holds a root of its own (a test, or a lead told where
    /// to look) does not have to mutate the process it runs in to be heard.
    #[must_use]
    pub fn reading(registry: Arc<TeammateRegistry>, claude: Option<TeamsRoot>) -> Self {
        Self { registry, claude, gate: None }
    }

    /// Installs the admission gate (**D523**): the engine's own [`Inbound`] —
    /// `Engine::inbound`, so the pass and the socket door share one buffer
    /// and two sets — and the engine's receiver-class read,
    /// `Engine::receiver_class`, carried as a closure over the shared engine
    /// handle.
    ///
    /// A builder beside [`LeadInbox::new`] rather than a parameter on it, so
    /// the ungated construction keeps compiling where it stands while the
    /// frontend lane moves to this; once every construction is gated the
    /// bridge form retires.
    #[must_use]
    pub fn gated(
        mut self,
        inbound: Arc<Inbound>,
        receiver: impl Fn() -> Option<ReceiverClass> + Send + Sync + 'static,
    ) -> Self {
        self.gate = Some(Gate { inbound, receiver: Box::new(receiver) });

        self
    }

    /// Every teams **root** this lead's replies can arrive under, in the order
    /// they are read.
    ///
    /// Its own, always — and **claude's** for as long as the roster holds a
    /// claude-backed member. That second one is the whole of the fix: a real
    /// `claude` writes where a real `claude` reads (§2.1), and a lead that polled
    /// only `<config home>/teams` never saw the answer to anything it had asked.
    /// Both conditions are load-bearing — without the roster check a lead with no
    /// claude teammate would be reading, and on a delivery *writing*, inside
    /// somebody else's config directory for no reason.
    ///
    /// One root when the two are the same directory, which is AC-13's own
    /// configuration: the same file read twice in a pass would hand the same
    /// message out twice.
    ///
    /// **Roots rather than the inbox paths they resolve to**, and that is the
    /// half a reply *out of* this module needs: a routed ask is answered into the
    /// asker's own inbox, and which directory that is follows from where the ask
    /// was found. Carrying only the path a pass read would leave the write with
    /// nothing but a guess ([`LeadInbox::route`]).
    fn roots(&self) -> Vec<TeamsRoot> {
        let own = self.registry.root().clone();
        let mut roots = vec![own.clone()];
        if !self.registry.holds_backend(MemberBackend::Claude) {
            return roots;
        }
        let Some(claude) = self.claude.as_ref() else {
            // Unreachable through a spawn — `ClaudePane::spawn` refuses before a
            // member exists when this root cannot be had — so it is said once at
            // debug rather than warned about on every tick.
            tracing::debug!(
                reason = REFUSED_NO_CONFIG_DIR,
                "a claude teammate is in the roster but its root cannot be resolved"
            );

            return roots;
        };
        if *claude != own {
            roots.push(claude.clone());
        }

        roots
    }

    /// The lead's own inbox under `root`.
    fn lead_inbox_in(&self, root: &TeamsRoot) -> PathBuf {
        root.inbox_path(self.registry.team(), self.registry.lead())
    }

    /// One pass: read, act on the control frames, hand back the rest.
    ///
    /// Over every root `LeadInbox::roots` names, in one [`Pass`]: which directory
    /// a teammate's answer arrived in is a fact about that teammate's backend and
    /// nothing a frontend should have to know.
    ///
    /// A gated pass ends by **reconciling** the gate against everything it
    /// read (**D525**): consumed admitted identities leave the set — the
    /// caller's [`LeadInbox::delivered`] prune is the consumption signal —
    /// and a held identity gone from every inbox settles its record
    /// `expired`, because a review offer cannot outlive the bytes it
    /// reviews. Once per poll rather than per root, since an identity's
    /// residence is whichever root holds it and a per-root reconcile would
    /// read absence where there is none.
    pub async fn poll(&self) -> Pass {
        let mut pass = Pass::default();
        let mut present = HashSet::new();
        for root in self.roots() {
            self.poll_one(&root, &mut pass, &mut present).await;
        }
        if let Some(gate) = &self.gate {
            gate.inbound.reconcile(&present);
        }
        self.retire_exited(&mut pass).await;

        pass
    }

    /// Retires every member whose own loop saw its pane stop running since the
    /// last pass (**D512** as amended for bead g9u; **D541** for the `ganja`
    /// and `claude` panes).
    ///
    /// The same door a `shutdown_approved` takes ([`LeadInbox::retire`]):
    /// [`TeammateRegistry::retire`] ends the surface — whose `end` answers the
    /// fate the loop's own call already recorded, touching nothing — and takes
    /// the record out of the team file. Reported under both `retired`, so a
    /// frontend's roster shrinks the way it does for any retirement, and
    /// `exited`, so it can say what the pane said. The loop that noticed has
    /// already told the lead's model in prose; this is the harness's half.
    ///
    /// The surface is rebuilt from [`Exited::cli`] rather than named, which is
    /// what keeps `backend_type` the word the roster already holds for this
    /// member: a shim's is its CLI's own name whether or not it ran in a pane,
    /// and a `ganja` or `claude` pane's is [`ganja_team::record::BACKEND_TMUX`] — the
    /// same string [`ganja_team::Surface::backend_type`] gave the spawn that
    /// wrote the record, and the same one that member's own
    /// `shutdown_approved` would have carried. Deliberately not
    /// [`crate::teammate::backend_name`], which spells those two `ganja` and
    /// `claude`: that is ganja's word for *which backend*, and this field is
    /// Claude Code's for *which surface*, which is why the record has only
    /// ever held the latter.
    async fn retire_exited(&self, pass: &mut Pass) {
        for exited in self.registry.take_exited() {
            if let Err(error) = self.registry.retire(&exited.name).await {
                tracing::warn!(
                    teammate = exited.name,
                    %error,
                    "a teammate's TUI exited but its record could not be taken out of the team file"
                );
            }
            tracing::info!(
                teammate = exited.name,
                pane = exited.pane_id,
                last = ?exited.last_words,
                "a teammate's TUI exited and the lead has forgotten it"
            );
            let surface = match exited.cli {
                Some(cli) => Surface::Shim { cli, pane: Some(exited.pane_id.clone()) },
                None => Surface::Pane { id: exited.pane_id.clone() },
            };
            pass.retired.push(Retired {
                name: exited.name.clone(),
                pane_id: Some(exited.pane_id.clone()),
                backend_type: Some(surface.backend_type().to_owned()),
            });
            pass.exited.push(exited);
        }
    }

    /// One pass over the lead's inbox under one root.
    ///
    /// Control frames are pruned **here**, because acting on one is the whole
    /// of what it needed and leaving it would be acting on it again a second
    /// later. Plain messages are not: whether one reached the conversation is
    /// the caller's fact, so they stay in the inbox until
    /// [`crate::teammate::lead_inbox::LeadInbox::delivered`] says so — a lead that quit between the read and
    /// the delivery loses nothing, which is the property a durable mailbox
    /// exists for.
    ///
    /// # The gated classification (**D523**)
    ///
    /// A **roster member's** entry is ungated — frames apply, plain mail
    /// delivers, exactly the pre-gate pass: the roster is the trust the team
    /// itself established, and the gate exists for what is outside it. Every
    /// other writer is answered by the gate's two sets first
    /// ([`Inbound::disposition`]): an **admitted** identity delivers with no
    /// re-run of policy or guards — accepted is final, and the 1 s re-offer
    /// loop must not drain a bucket — carrying H1's hold-time summary
    /// snapshot where a release reviewed one; a **held** identity is skipped,
    /// its entry left durable and its review copy in the buffer (C1); and an
    /// identity the gate has never met is **demoted** — a peer from
    /// `unknown`. A demoted *frame* is dropped by name and pruned, never
    /// acted on: structure does not cross a trust boundary, the rule the
    /// socket already enforces (v2 §"Rejection of structured protocol
    /// frames", evidence 623036), and the hardening that closes the
    /// fabricated-dialog hole a non-roster `permission_request` once had.
    /// Demoted *plain* mail runs the normal peer gate
    /// ([`Inbound::admit_mailbox`]): accept delivers and joins the admitted
    /// set, hold leaves the entry in place under the held-index, refuse — and
    /// every guard drop — prunes, traced. A capacity eviction's mailbox-door
    /// victim is pruned best-effort in this inbox's own batch; a victim
    /// living under the other root re-gates as a fresh hold next pass —
    /// fail-closed, never delivered.
    ///
    /// Ungated construction ([`LeadInbox::new`] with no [`LeadInbox::gated`])
    /// keeps the pre-gate pass whole, the bridge the TUI stands on until its
    /// lane installs the gate.
    async fn poll_one(
        &self,
        root: &TeamsRoot,
        pass: &mut Pass,
        present: &mut HashSet<mailbox::Identity>,
    ) {
        let inbox = self.lead_inbox_in(root);
        let Some(contents) = read_inbox(inbox.clone(), self.registry.lead().as_str()).await else {
            return;
        };
        if contents.valid.is_empty() {
            return;
        }

        // Once per pass, not per entry: a mode change mid-pass is the next
        // pass's fact, exactly as it is the next turn's on the engine.
        let receiver = self.gate.as_ref().map(|gate| (gate.receiver)());
        let mut handled = Vec::new();
        for message in &contents.valid {
            let identity = mailbox::identity(message);
            present.insert(identity.clone());
            let kind = Frame::reserved_kind(&message.text);
            // The roster check the classification stands on: the registry
            // answers for the members it holds, and `None` — a retired
            // member's late mail included — is the demoted class, fail-closed.
            let roster = self.registry.delivery_of(&message.from).is_some();

            let Some(gate) = self.gate.as_ref().filter(|_| !roster) else {
                match kind {
                    Some(kind) => {
                        self.apply(kind, message, pass, root).await;
                        handled.push(identity);
                    }
                    None => pass.messages.push(self.plain(message)),
                }
                continue;
            };
            match gate.inbound.disposition(&identity) {
                PassDisposition::Deliver => pass.messages.push(self.plain(message)),
                PassDisposition::DeliverReviewed { summary } => {
                    // H1: the summary reviewed at hold time, never the durable
                    // entry's current one — `summary` sits outside §2.3's
                    // identity key, so a same-uid writer could have swapped it
                    // under an unchanged identity between review and delivery.
                    // Even a `None` snapshot overrides.
                    let mut delivered = self.plain(message);
                    delivered.summary = summary.map(|snapshot| snapshot.as_str().to_owned());
                    pass.messages.push(delivered);
                }
                PassDisposition::Skip => {}
                PassDisposition::Classify => match kind {
                    Some(kind) => {
                        self.drop_it(kind, message, pass);
                        handled.push(identity);
                    }
                    None => match gate.inbound.admit_mailbox(receiver.flatten(), message) {
                        MailboxAdmission::Deliver => pass.messages.push(self.plain(message)),
                        MailboxAdmission::Held { cause, evicted_prune } => {
                            tracing::info!(
                                from = message.from,
                                ?cause,
                                "a non-roster inbox entry was held for review"
                            );
                            if let Some(evicted) = evicted_prune {
                                handled.push(evicted);
                            }
                        }
                        MailboxAdmission::Drop(reason) => {
                            tracing::info!(
                                from = message.from,
                                ?reason,
                                "a non-roster inbox entry was dropped"
                            );
                            handled.push(identity);
                        }
                    },
                },
            }
        }
        if !handled.is_empty() {
            self.prune(&inbox, handled).await;
        }
    }

    /// Takes the messages a caller really delivered out of the inbox.
    ///
    /// Separate from [`crate::teammate::lead_inbox::LeadInbox::poll`] because only the caller knows: a batch
    /// the engine refused is left where it was and offered again next pass,
    /// which is a delivery delayed rather than a delivery lost.
    ///
    /// Every inbox the pass read, because a [`Delivered`] does not carry which
    /// one it came from — §2.3's identity is a function of the message, so
    /// pruning it from an inbox that never held it is a rewrite that changes
    /// nothing rather than a wrong answer. That costs one extra write per
    /// delivered batch, and only for a lead that really has a `claude` teammate.
    pub async fn delivered(&self, messages: &[Delivered]) {
        if messages.is_empty() {
            return;
        }
        let identities: Vec<mailbox::Identity> = messages.iter().map(Delivered::identity).collect();
        for root in self.roots() {
            self.prune(&self.lead_inbox_in(&root), identities.clone()).await;
        }
    }

    /// A message nobody has to interpret, as the frontend takes it.
    fn plain(&self, message: &MailboxMessage) -> Delivered {
        Delivered {
            from: message.from.clone(),
            timestamp: message.timestamp.clone(),
            summary: message.summary.clone(),
            color: message.color.clone(),
            body: message.text.clone(),
            delivery: self.registry.delivery_of(&message.from).unwrap_or(Delivery::FireAndForget),
        }
    }

    /// What one frame is worth, once it is known to be one.
    ///
    /// No [`ganja_protocol::team::LeadFrame`] check here, and the asymmetry is
    /// the point: §7-2's rule is that a *teammate* obeys only its lead, where
    /// the lead is the one everybody reports to. What guards this side instead
    /// is that none of the frames it acts on can be forged into authority — a
    /// `shutdown_approved` only ever makes the lead forget a member, a
    /// `permission_request` only ever *asks* a person, and the name each acts
    /// on is the message's own sender, never a name the frame carries.
    ///
    /// `root` is the teams root this frame was **found** under, carried because
    /// one of the three answers back and has to answer into the same directory —
    /// see [`LeadInbox::route`].
    async fn apply(
        &self,
        kind: &'static str,
        message: &MailboxMessage,
        pass: &mut Pass,
        root: &TeamsRoot,
    ) {
        let Some(frame) = message.frame() else {
            self.drop_it(kind, message, pass);

            return;
        };
        match frame {
            Frame::ShutdownApproved(approved) => self.retire(message, approved, pass).await,
            Frame::IdleNotification(idle) => Self::idle(message, idle, pass),
            Frame::PermissionRequest(request) => self.route(message, request, pass, root).await,
            // `permission_response` and `team_permission_update` land here on
            // purpose (§7-1), beside everything else this side has no handler
            // for; see the module doc.
            _ => self.drop_it(kind, message, pass),
        }
    }

    /// A pane teammate's ask, put in front of the person at this lead's dialog
    /// (**D-5**), with the answer arranged to be written back.
    ///
    /// The answer goes to the **sender's** inbox — the envelope's `from`, the
    /// same name [`LeadInbox::retire`] trusts — never to whatever the frame's
    /// own `agent_id` claims: a member that could name somebody else there
    /// could have another pane's asks answered on its behalf. A sender the
    /// name grammar refuses cannot be written back to at all, so its ask is
    /// dropped by name rather than raised for a person to answer into nowhere.
    ///
    /// **Under the root the ask was found beneath**, which is `root`, and not
    /// this registry's own. A pass reads two of them ([`LeadInbox::roots`]), so a
    /// real `claude` teammate's ask arrives from `$CLAUDE_CONFIG_DIR/teams`, and
    /// an answer written into ganja's root instead would land in a file that
    /// member never reads — a pane waiting forever on a dialog a person had
    /// already answered. The origin is the only honest source for it: nothing in
    /// the frame says which directory it came from, and the sender's *name* is
    /// the same in both.
    ///
    /// **The handover never waits.** A `try_send`, exactly as
    /// [`crate::teammate::posture::Forwarding`]'s is and for its reason: an
    /// awaited send on a channel nobody claimed would park this pass — and the
    /// frontend's tick behind it — forever. No surface, a full queue and a
    /// closed one are one answer with three reasons, and the pane reads that
    /// answer as a refusal rather than waiting on a dialog nobody will see.
    async fn route(
        &self,
        message: &MailboxMessage,
        request: PermissionRequest,
        pass: &mut Pass,
        root: &TeamsRoot,
    ) {
        let Ok(asker) = MemberName::parse(&message.from) else {
            tracing::warn!(
                from = message.from,
                request = request.request_id,
                "a permission ask came from a name that cannot be answered, and was dropped"
            );
            pass.dropped.push("permission_request");

            return;
        };
        let inbox = root.inbox_path(self.registry.team(), &asker);
        let asked = Asked {
            name: message.from.clone(),
            request_id: request.request_id.clone(),
            tool: request.tool_name.clone(),
            raised: false,
        };
        // What the answer needs, taken before the frame is spent on the
        // dialog: the id it names, the tool an "always" is stored for, and
        // the input echoed back as what the call may run with.
        let answer = Answer {
            lead: self.registry.lead().as_str().to_owned(),
            inbox,
            request_id: request.request_id.clone(),
            tool: request.tool_name.clone(),
            input: request.input.clone(),
        };
        let dialog =
            member::dialog_of(SessionId::from(self.registry.lead_session_id().to_owned()), request);

        let Some(surface) = self.registry.dialog_surface() else {
            tracing::warn!(
                teammate = message.from,
                request = asked.request_id,
                reason = NO_DIALOG_SURFACE,
                "{REFUSED_ASK}"
            );
            answer.refuse(NO_DIALOG_SURFACE).await;
            pass.asked.push(asked);

            return;
        };
        let (reply, waiting) = oneshot::channel();
        let raised = match surface.hand_over(Forwarded {
            teammate: message.from.clone(),
            request: dialog,
            reply,
        }) {
            Ok(raised) => raised,
            Err(undelivered) => {
                let reason = match undelivered {
                    Undelivered::Full => DIALOG_QUEUE_FULL,
                    Undelivered::Closed => LEAD_GONE,
                };
                tracing::warn!(
                    teammate = message.from,
                    request = asked.request_id,
                    reason,
                    "{REFUSED_ASK}"
                );
                answer.refuse(reason).await;
                pass.asked.push(asked);

                return;
            }
        };
        tracing::info!(
            teammate = message.from,
            request = asked.request_id,
            tool = asked.tool,
            "a pane's permission ask was put in front of the lead"
        );
        // The wait for the answer runs in a task of its own so the pass keeps
        // reading, exactly as the in-process handover does: a person takes as
        // long as they take, and the inbox has other frames in it.
        let cancel = self.registry.cancellation();
        tokio::spawn(async move {
            // Moved in with the wait, because that is exactly how long this
            // question is in front of the person: the lead's own turn loop
            // reads the count and refuses to talk over an open dialog.
            let _raised = raised;
            let reply = tokio::select! {
                // The team going down ends the wait, the same arm the
                // in-process carrier has had since D-5
                // (`crate::teammate::posture::hand_over`). A dropped frontend
                // already resolves the wait on its own — the receiver goes,
                // the sender with it — so this is not the load-bearing path
                // today; it is here because the count this guard holds is now
                // read by the lead's continuation blocker, and a wait that
                // never ended would disable that blocker for the rest of the
                // session with nothing said (bead xysf). Symmetry is cheaper
                // than a comment asking the next reader to re-derive that.
                () = cancel.cancelled() => PermissionReply::Reject,
                // A dropped sender is a lead that gave up on the dialog, which
                // is the refusal it looks like.
                reply = waiting => reply.unwrap_or(PermissionReply::Reject),
            };
            answer.write(reply).await;
        });
        pass.asked.push(Asked { raised: true, ..asked });
    }

    /// A teammate saying it is done, which is the lead's cue to forget it.
    ///
    /// The name forgotten is **the sender's**, taken off the envelope rather
    /// than off the frame's own `from`: a member that could name somebody else
    /// there could retire a teammate that is still running, and the envelope is
    /// the half [`crate::teammate::TeammateRegistry`] wrote the inbox path from.
    async fn retire(&self, message: &MailboxMessage, approved: ShutdownApproved, pass: &mut Pass) {
        if let Err(error) = self.registry.retire(&message.from).await {
            // Not fatal and not retried: the member is out of the roster
            // either way, and a team file that would not be rewritten is a
            // stale row in a listing rather than a teammate that keeps running.
            tracing::warn!(
                teammate = message.from,
                %error,
                "a teammate shut down but its record could not be taken out of the team file"
            );
        }
        tracing::info!(
            teammate = message.from,
            request = approved.request_id,
            "a teammate shut down and the lead has forgotten it"
        );
        pass.retired.push(Retired {
            name: message.from.clone(),
            pane_id: approved.pane_id,
            backend_type: approved.backend_type,
        });
    }

    /// A teammate reporting itself available.
    ///
    /// Recorded and logged rather than delivered: what a teammate did is
    /// already under its row in `/teammate`, and putting the harness's own
    /// bookkeeping into the model's context would be telling it something
    /// nobody said.
    ///
    /// **A teammate's own model cannot raise one**, and that is the part worth
    /// keeping: `idle_notification` is harness-only, written by the frontend at
    /// a turn's end and never composable through `send_message`. The asking half
    /// ships — `ganja-tui`'s `member::Inbox::report_idle` writes this frame every
    /// time a pane teammate's turn ends, mapping completed / cancelled / failed
    /// onto `available` / `interrupted` / `failed` — so what a lead reads here
    /// arrives from a real pane on every turn rather than from nothing.
    fn idle(message: &MailboxMessage, idle: IdleNotification, pass: &mut Pass) {
        tracing::info!(
            teammate = message.from,
            reason = ?idle.idle_reason,
            "a teammate reported itself available"
        );
        pass.idle.push(Idle { name: message.from.clone(), summary: idle.summary });
    }

    /// Names a frame nobody here handles ([`drop_frame`]).
    fn drop_it(&self, kind: &'static str, message: &MailboxMessage, pass: &mut Pass) {
        drop_frame(self.registry.lead().as_str(), kind, message);
        pass.dropped.push(kind);
    }

    /// Takes entries out of one inbox, in one write.
    async fn prune(&self, inbox: &Path, handled: Vec<mailbox::Identity>) {
        prune_inbox(inbox.to_path_buf(), handled, self.registry.lead().as_str()).await;
    }
}

/// Everything the answer to one routed ask needs, held apart from the pass
/// that read the ask so a task can carry it past the pass's own lifetime.
///
/// Owned paths and strings rather than the registry: a task waiting on a
/// person must not keep the team alive, and needs nothing of it but where to
/// write.
struct Answer {
    /// Whose name the response is stamped with — the lead's, by construction.
    lead: String,
    /// The asker's inbox.
    inbox: PathBuf,
    request_id: String,
    tool: String,
    input: serde_json::Value,
}

impl Answer {
    /// Writes the person's decision back as a `permission_response`.
    async fn write(&self, reply: PermissionReply) {
        let response = member::response_of(&self.request_id, &self.tool, &self.input, reply);
        self.deliver(Frame::PermissionResponse(response), "answered").await;
    }

    /// Writes a refusal back, for an ask nobody could be shown — a dialog
    /// that could not be raised, rather than one answered "no".
    async fn refuse(&self, reason: &str) {
        let response = PermissionResponse::error(&self.request_id, reason);
        self.deliver(Frame::PermissionResponse(response), "refused").await;
    }

    /// One write into the asker's inbox, said out loud when it fails: a pane
    /// whose answer never landed is a pane still waiting, which is the thing
    /// worth shouting about (§6.2's own posture for a plan approval).
    async fn deliver(&self, frame: Frame, what: &'static str) {
        let message = match MailboxMessage::from_frame(&self.lead, &frame, record::now_iso8601()) {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(
                    request = self.request_id,
                    %error,
                    "a permission answer would not encode; the pane is still waiting on it"
                );

                return;
            }
        };
        let path = self.inbox.clone();
        let written = crate::teammate::blocking_io(move || {
            mailbox::write_bounded(&path, message, Some(super::postbox::INBOX_CEILING))
        })
        .await;
        match written {
            Ok(_) => tracing::info!(
                request = self.request_id,
                inbox = %self.inbox.display(),
                "a pane's permission ask was {what} and the answer written back"
            ),
            Err(error) => tracing::warn!(
                request = self.request_id,
                inbox = %self.inbox.display(),
                %error,
                "FAILED to write a permission answer; the pane is still waiting on it"
            ),
        }
    }
}

#[cfg(test)]
#[path = "lead_inbox_tests.rs"]
mod tests;
