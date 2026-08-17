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
//! # The two frames a lead answers
//!
//! `shutdown_approved` retires the member — [`crate::teammate::TeammateRegistry::retire`]
//! drops it from the roster and takes its record out of the team file — and
//! `idle_notification` is recorded as what it is, a teammate reporting itself
//! available. Everything else frame-shaped is **dropped by name** with the
//! head of it, which is §6.1's own posture pointed the other way: a frame this
//! side has no handler for is named rather than acted on by guess.
//!
//! Two of those drops are deliberate rather than unimplemented, and §7-1 is
//! why: the permission family never travels the lead's mailbox in this build.
//! An in-process teammate's dialog crosses on
//! [`crate::teammate::posture::Forwarded`]'s channel, where it keeps the reply oneshot
//! that makes a refusal expressible; a `permission_request` in a file is a
//! question nothing could answer, so it is named and pruned rather than left
//! to be read again every second.
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

use std::{path::PathBuf, sync::Arc, time::Duration};

use ganja_protocol::team::{Frame, IdleNotification, ShutdownApproved};
use ganja_team::{MailboxMessage, mailbox};

use super::{Delivery, TeammateRegistry, runner::FRAME_HEAD};

/// §6's lead cadence, and deliberately half the teammate's own
/// ([`crate::teammate::runner::POLL`]): the teammate is the side that has to notice a
/// shutdown promptly, and the lead is the side a person is watching anyway.
pub const POLL: Duration = Duration::from_millis(1000);

/// What is logged when a frame arrives that the lead has no handler for.
pub const DROPPED_FRAME: &str = "an inbox frame was dropped";

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
    fn identity(&self) -> mailbox::Identity {
        mailbox::identity(&MailboxMessage::new(
            self.from.clone(),
            self.body.clone(),
            self.timestamp.clone(),
        ))
    }
}

/// A teammate whose `shutdown_approved` this pass read.
///
/// The pane fields are carried rather than acted on: killing a pane is the
/// backend's, and in this phase there are none. What the lead does here and
/// now is forget the member, which [`crate::teammate::lead_inbox::LeadInbox::poll`] has already done by the
/// time a caller sees this.
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
    /// The members this pass forgot.
    pub retired: Vec<Retired>,
    /// The teammates that reported themselves available.
    pub idle: Vec<Idle>,
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
            && self.idle.is_empty()
            && self.dropped.is_empty()
    }
}

/// The lead's own mailbox, and the §6.2 pass over it.
#[derive(Debug)]
pub struct LeadInbox {
    registry: Arc<TeammateRegistry>,
    inbox: PathBuf,
}

impl LeadInbox {
    /// The inbox of the team `registry` leads.
    #[must_use]
    pub fn new(registry: Arc<TeammateRegistry>) -> Self {
        let inbox = registry.lead_inbox();

        Self { registry, inbox }
    }

    /// One pass: read, act on the control frames, hand back the rest.
    ///
    /// Control frames are pruned **here**, because acting on one is the whole
    /// of what it needed and leaving it would be acting on it again a second
    /// later. Plain messages are not: whether one reached the conversation is
    /// the caller's fact, so they stay in the inbox until
    /// [`crate::teammate::lead_inbox::LeadInbox::delivered`] says so — a lead that quit between the read and
    /// the delivery loses nothing, which is the property a durable mailbox
    /// exists for.
    pub async fn poll(&self) -> Pass {
        let mut pass = Pass::default();
        let path = self.inbox.clone();
        let contents = match tokio::task::spawn_blocking(move || mailbox::read(&path)).await {
            Ok(Ok(contents)) => contents,
            Ok(Err(error)) => {
                tracing::warn!(%error, "the lead's inbox could not be read");

                return pass;
            }
            Err(error) => {
                tracing::warn!(%error, "an inbox read was lost");

                return pass;
            }
        };
        for report in &contents.reports {
            tracing::warn!("{report}");
        }
        if contents.valid.is_empty() {
            return pass;
        }

        let mut handled = Vec::new();
        for message in &contents.valid {
            let Some(kind) = Frame::reserved_kind(&message.text) else {
                pass.messages.push(self.plain(message));
                continue;
            };
            self.apply(kind, message, &mut pass).await;
            handled.push(mailbox::identity(message));
        }
        if !handled.is_empty() {
            self.prune(handled).await;
        }

        pass
    }

    /// Takes the messages a caller really delivered out of the inbox.
    ///
    /// Separate from [`crate::teammate::lead_inbox::LeadInbox::poll`] because only the caller knows: a batch
    /// the engine refused is left where it was and offered again next pass,
    /// which is a delivery delayed rather than a delivery lost.
    pub async fn delivered(&self, messages: &[Delivered]) {
        if messages.is_empty() {
            return;
        }
        self.prune(messages.iter().map(Delivered::identity).collect())
            .await;
    }

    /// A message nobody has to interpret, as the frontend takes it.
    fn plain(&self, message: &MailboxMessage) -> Delivered {
        Delivered {
            from: message.from.clone(),
            timestamp: message.timestamp.clone(),
            summary: message.summary.clone(),
            color: message.color.clone(),
            body: message.text.clone(),
            delivery: self
                .registry
                .delivery_of(&message.from)
                .unwrap_or(Delivery::FireAndForget),
        }
    }

    /// What one frame is worth, once it is known to be one.
    ///
    /// No [`ganja_protocol::team::LeadFrame`] check here, and the asymmetry is
    /// the point: §7-2's rule is that a *teammate* obeys only its lead, where
    /// the lead is the one everybody reports to. What guards this side instead
    /// is that neither frame it acts on can be forged into authority — a
    /// `shutdown_approved` only ever makes the lead forget a member, and the
    /// name it forgets is the message's own sender, never a name the frame
    /// carries.
    async fn apply(&self, kind: &'static str, message: &MailboxMessage, pass: &mut Pass) {
        let Some(frame) = message.frame() else {
            self.drop_it(kind, message, pass);

            return;
        };
        match frame {
            Frame::ShutdownApproved(approved) => self.retire(message, approved, pass).await,
            Frame::IdleNotification(idle) => Self::idle(message, idle, pass),
            _ => self.drop_it(kind, message, pass),
        }
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
    /// already under its row in `/team`, and putting the harness's own
    /// bookkeeping into the model's context would be telling it something
    /// nobody said. Nothing in this build raises one — `idle_notification` is
    /// harness-only, so a teammate's own model cannot send it — which makes
    /// this the answering half of a `claude` pane's loop, complete and waiting
    /// on P25b's asking half.
    fn idle(message: &MailboxMessage, idle: IdleNotification, pass: &mut Pass) {
        tracing::info!(
            teammate = message.from,
            reason = ?idle.idle_reason,
            "a teammate reported itself available"
        );
        pass.idle.push(Idle {
            name: message.from.clone(),
            summary: idle.summary,
        });
    }

    /// Names a frame nobody here handles, with the head of it.
    ///
    /// [`crate::teammate::runner`]'s rule, for its reason: a plain message's body never
    /// reaches a log line, and a frame that would not decode is undiagnosable
    /// without seeing some of it.
    fn drop_it(&self, kind: &'static str, message: &MailboxMessage, pass: &mut Pass) {
        tracing::warn!(
            from = message.from,
            frame = head(&message.text),
            "{DROPPED_FRAME}: {kind}"
        );
        pass.dropped.push(kind);
    }

    /// Takes entries out of the inbox, in one write.
    async fn prune(&self, handled: Vec<mailbox::Identity>) {
        let path = self.inbox.clone();
        let outcome =
            tokio::task::spawn_blocking(move || mailbox::prune_delivered(&path, &handled))
                .await
                .map_err(|error| error.to_string())
                .and_then(|pruned| pruned.map_err(|error| error.to_string()));

        if let Err(error) = outcome {
            // [`crate::teammate::runner::Runner::prune`]'s posture: the next pass reads
            // the same entries again, and a redelivery is a cost this side can
            // pay where a lost message is not.
            tracing::warn!(%error, "the lead could not prune its inbox");
        }
    }
}

/// The first [`FRAME_HEAD`] characters, cut on a character boundary.
fn head(text: &str) -> &str {
    match text.char_indices().nth(FRAME_HEAD) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ganja_protocol::team::{Frame, IdleNotification, ShutdownApproved, TaskAssignment};
    use ganja_team::{MailboxMessage, mailbox, record};

    use super::{Delivered, LeadInbox};
    use crate::teammate::{Delivery, TeammateRegistry};

    /// A registry over a throwaway teams root, with the lead's inbox seeded.
    fn registry(home: &std::path::Path) -> Arc<TeammateRegistry> {
        Arc::new(TeammateRegistry::for_session(
            home,
            "224cbeab-4e62-497c-aa8f-d05cc33ce7ba",
            home,
        ))
    }

    fn write(inbox: &std::path::Path, from: &str, text: &str) {
        mailbox::write(
            inbox,
            MailboxMessage::new(from, text, record::now_iso8601()),
        )
        .expect("the inbox takes a message");
    }

    fn write_frame(inbox: &std::path::Path, from: &str, frame: &Frame) {
        let message = MailboxMessage::from_frame(from, frame, record::now_iso8601())
            .expect("the frame encodes");
        mailbox::write(inbox, message).expect("the inbox takes a frame");
    }

    #[tokio::test]
    async fn a_plain_message_is_carried_out_and_stays_until_it_is_delivered() {
        let home = tempfile::tempdir().expect("a temporary home");
        let registry = registry(home.path());
        let inbox = registry.lead_inbox();
        write(&inbox, "w1", "the parser is done");

        let lead = LeadInbox::new(Arc::clone(&registry));
        let pass = lead.poll().await;

        assert_eq!(pass.messages.len(), 1);
        assert_eq!(pass.messages[0].from, "w1");
        assert_eq!(pass.messages[0].body, "the parser is done");
        // A peer this registry never started gives no consumption signal, so
        // the lead may not render one.
        assert_eq!(pass.messages[0].delivery, Delivery::FireAndForget);
        assert_eq!(
            mailbox::read(&inbox).expect("the inbox reads").valid.len(),
            1,
            "a message the caller has not delivered yet is still owed"
        );

        lead.delivered(&pass.messages).await;

        assert!(
            mailbox::read(&inbox)
                .expect("the inbox reads")
                .valid
                .is_empty(),
            "a delivered message does not remain"
        );
    }

    #[tokio::test]
    async fn a_control_frame_is_acted_on_and_never_handed_out_to_be_queued() {
        let home = tempfile::tempdir().expect("a temporary home");
        let registry = registry(home.path());
        let inbox = registry.lead_inbox();
        write_frame(
            &inbox,
            "w1",
            &Frame::ShutdownApproved(ShutdownApproved {
                request_id: "req-1".to_owned(),
                from: "w1".to_owned(),
                timestamp: record::now_iso8601(),
                pane_id: Some("in-process".to_owned()),
                backend_type: Some("in-process".to_owned()),
            }),
        );
        write_frame(
            &inbox,
            "w2",
            &Frame::IdleNotification(IdleNotification {
                from: "w2".to_owned(),
                timestamp: record::now_iso8601(),
                idle_reason: None,
                summary: Some("waiting for review".to_owned()),
                completed_task_id: None,
                completed_status: None,
                failure_reason: None,
            }),
        );

        let pass = LeadInbox::new(registry).poll().await;

        assert!(
            pass.messages.is_empty(),
            "a control frame is acted on, never queued: {pass:?}"
        );
        assert_eq!(pass.retired.len(), 1);
        assert_eq!(pass.retired[0].name, "w1");
        assert_eq!(pass.idle.len(), 1);
        assert_eq!(pass.idle[0].summary.as_deref(), Some("waiting for review"));
        assert!(
            mailbox::read(&inbox)
                .expect("the inbox reads")
                .valid
                .is_empty(),
            "a frame the lead acted on leaves the inbox in the same pass"
        );
    }

    /// §7-1: the permission family does not travel this file. An in-process
    /// teammate's dialog crosses on the forwarding channel, which keeps the
    /// reply oneshot a refusal needs.
    #[tokio::test]
    async fn a_permission_frame_and_an_unhandled_one_are_both_dropped_by_name() {
        let home = tempfile::tempdir().expect("a temporary home");
        let registry = registry(home.path());
        let inbox = registry.lead_inbox();
        write_frame(
            &inbox,
            "w1",
            &Frame::TaskAssignment(TaskAssignment {
                task_id: "t-1".to_owned(),
                subject: "look at the parser".to_owned(),
                description: "the whole of it".to_owned(),
                assigned_by: "w1".to_owned(),
                timestamp: record::now_iso8601(),
            }),
        );

        let pass = LeadInbox::new(registry).poll().await;

        assert_eq!(pass.dropped, ["task_assignment"]);
        assert!(pass.messages.is_empty());
        assert!(
            mailbox::read(&inbox)
                .expect("the inbox reads")
                .valid
                .is_empty(),
            "a named drop leaves the inbox rather than being read again forever"
        );
    }

    #[test]
    fn a_frames_head_is_cut_on_a_character_boundary() {
        let wide: String = "あ".repeat(super::FRAME_HEAD * 2);

        assert_eq!(super::head(&wide).chars().count(), super::FRAME_HEAD);
        assert_eq!(super::head("{}"), "{}");
    }

    /// §2.3's identity is derivable by any reader, and this is that property
    /// holding: a value built from the three fields prunes the one message
    /// they came from.
    #[test]
    fn a_delivered_entry_derives_the_identity_it_will_be_pruned_by() {
        let message = MailboxMessage::new("w1", "done", "2026-08-17T00:00:00.000Z");
        let delivered = Delivered::new(
            "w1",
            "2026-08-17T00:00:00.000Z",
            "done",
            Delivery::Acknowledged,
        );

        assert_eq!(delivered.identity(), mailbox::identity(&message));
        assert_ne!(
            Delivered::new(
                "w2",
                "2026-08-17T00:00:00.000Z",
                "done",
                Delivery::Acknowledged
            )
            .identity(),
            mailbox::identity(&message),
            "the sender is part of what a message is"
        );
    }
}
