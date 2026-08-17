//! The loop that drives one in-process teammate (§6.1).
//!
//! Upstream opencode has **no counterpart**: a delegated turn there is awaited
//! by the call that started it, so nothing outlives it and nothing has a
//! mailbox to poll. What is ported is Claude Code's `inProcessRunner`, whose
//! mailbox check is the clearest statement of teammate message semantics the
//! reference carries — and the order of its steps is the semantics:
//!
//! 1. **A `shutdown_request` is handled before anything else**, and how many
//!    unread messages it jumped ahead of is logged. A teammate wedged behind a
//!    hundred queued messages stays reclaimable.
//! 2. The rest is partitioned into protocol frames and plain messages.
//! 3. `plan_approval_response` and `mode_set_request` are applied **from the
//!    lead only** — a check the type system makes rather than the code, via
//!    [`ganja_protocol::team::LeadFrame`]. An approval nothing is waiting on is
//!    ignored as stale; anything else frame-shaped is dropped with a warning
//!    carrying the frame's head.
//! 4. Plain messages are drained as one batch into the teammate's own turn.
//!
//! # What this loop does not do
//!
//! Claude's runner also drains `pendingUserMessages` off a task record, keeps
//! an `evictAfter` deadline, and auto-claims assigned tasks. None of the three
//! is here: the first two are that harness's own task-table bookkeeping, and
//! auto-claiming `task_assignment` is an explicit scope decision of this
//! landing (O-6) rather than an omission — claiming stays inside the existing
//! delegation.
//!
//! # One write, not two
//!
//! The reference prunes handled frames in one write and delivered messages in
//! another. This does it in one, at the end of a pass, and the reason is the
//! lock: each write is a full read-modify-write under `ganja-team`'s inbox
//! lock, and two of them are two chances to wait out a peer where one will do.
//! What is pruned is what was really finished — a batch the engine refused is
//! left in the inbox and retried on the next pass, which is the behaviour the
//! two-write version would also have had.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::StreamExt as _;
use ganja_protocol::team::{
    Frame, LeadFrame, ModeSetRequest, PlanApprovalResponse, ShutdownApproved, ShutdownRequest,
};
use ganja_team::{MailboxMessage, MemberName, Surface, mailbox, record};

use crate::{
    EngineError,
    protocol::{Command, PermissionMode},
    teammate::{SETTLE, Teammate},
};

/// §6's teammate cadence. The lead's own poller runs at half this rate; the
/// difference is the reference's and is kept, because the teammate is the side
/// that has to notice a shutdown promptly.
pub const POLL: Duration = Duration::from_millis(500);

/// How much of a dropped frame reaches the warning about it (§6.1).
pub const FRAME_HEAD: usize = 80;

/// What is logged when an approval answers nothing.
pub const IGNORED_STALE: &str =
    "a plan approval answered a request this teammate is not waiting on, and was ignored";

/// What is logged when a frame arrives that this teammate has no handler for —
/// or that did not come from the lead, which for these two frames is the same
/// thing.
pub const DROPPED_FRAME: &str = "an inbox frame was dropped";

/// How a batch of messages is put to the teammate's model.
///
/// Deliberately plain: the `<teammate-message>` envelope belongs to the request
/// assembly, keyed on a peer *part* rather than on text, and building a second
/// spelling of it here would be two envelopes to keep in step. This is the one
/// place a delivered message becomes words, so there is one place for that
/// envelope to arrive.
fn envelope(from: &str, text: &str) -> String {
    format!("A message from {from}:\n{text}")
}

/// What one pass of the loop did.
///
/// Returned rather than only logged so a test can drive a single pass and
/// assert the ordering, which is the part of §6.1 that is the contract.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// The request id of a shutdown this pass answered, if it answered one.
    /// A pass that returns this has torn the teammate down and the loop ends.
    pub shutdown: Option<String>,
    /// How many unread messages the shutdown went ahead of.
    pub jumped: usize,
    /// How many plain messages reached the teammate's turn.
    pub delivered: usize,
    /// The frames this pass applied, by kind.
    pub applied: Vec<&'static str>,
    /// How many approvals were ignored as stale.
    pub ignored: usize,
    /// The frames this pass dropped, by kind — which is what "dropped by name"
    /// means: an unhandled frame is named rather than silently eaten.
    pub dropped: Vec<&'static str>,
}

/// Why the loop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// The registry's shutdown cancelled it.
    Cancelled,
    /// It answered a `shutdown_request` and tore the teammate down.
    ShutDown,
    /// The teammate's engine went away under it.
    Gone,
}

/// One teammate's mailbox loop.
pub struct Runner {
    teammate: Arc<Teammate>,
    /// The name a frame has to come from to be obeyed (§7-2).
    lead: MemberName,
    inbox: PathBuf,
    lead_inbox: PathBuf,
    /// What this teammate runs on, which is what its `shutdown_approved`
    /// reports so the lead knows whether there is a pane to kill.
    surface: Surface,
    poll: Duration,
    cancel: tokio_util::sync::CancellationToken,
    /// The plan approval this teammate is waiting on, if it is waiting on one.
    /// An answer to anything else is stale by definition.
    awaiting: Mutex<Option<String>>,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Runner")
            .field("teammate", &self.teammate.name())
            .field("lead", &self.lead)
            .field("surface", &self.surface)
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

impl Runner {
    /// Builds the loop for one teammate.
    #[must_use]
    pub fn new(
        teammate: Arc<Teammate>,
        lead: MemberName,
        inbox: PathBuf,
        lead_inbox: PathBuf,
        surface: Surface,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            teammate,
            lead,
            inbox,
            lead_inbox,
            surface,
            poll: POLL,
            cancel,
            awaiting: Mutex::new(None),
        }
    }

    /// Records that this teammate is waiting on `request_id`, so the lead's
    /// answer to it is applied rather than ignored as stale.
    ///
    /// What makes an approval *not* stale is that somebody is waiting, and this
    /// is where a waiter says so. It is reached through
    /// [`crate::teammate::TeammateRegistry::awaiting_plan_approval`], which is
    /// why [`Runner::run`] takes `&self`: a loop that consumed the value would
    /// leave this method with no live receiver at all, and every approval the
    /// lead ever sent would be ignored as answering nothing.
    ///
    /// **What does not exist yet is the asking side.** Nothing in this build
    /// raises a `plan_approval_request`, so nothing calls this except a test;
    /// the frame handler below and this seam are the answering half, complete
    /// and reachable, waiting on the half that asks.
    pub fn awaiting_plan_approval(&self, request_id: impl Into<String>) {
        *self.awaiting.lock().expect("the wait is never poisoned") = Some(request_id.into());
    }

    /// Runs until the registry cancels it, a `shutdown_request` is answered, or
    /// the teammate's engine goes away.
    ///
    /// **Subscribes before the first pass can prompt**, and that is not a
    /// nicety: the engine's birth queue is a lossless lane registered when it
    /// was built, and a lossless lane nobody drains fills and then makes the
    /// publisher — the teammate's own turn — wait. What this loop does with
    /// those events is nothing; what it does with the *lane* is keep it moving.
    /// The events somebody reads are the registry's droppable subscription.
    ///
    /// Borrows rather than consumes, so the value stays reachable while its
    /// loop runs — see [`Runner::awaiting_plan_approval`], which is worth
    /// nothing if the only thing holding a `Runner` is the task inside it.
    pub async fn run(&self) -> Stopped {
        let Ok(mut events) = self.teammate.engine().subscribe().await else {
            return Stopped::Gone;
        };
        let mut poll = tokio::time::interval(self.poll);
        // A pass that ran late is taken late rather than immediately again: a
        // teammate whose inbox read blocked on a peer's lock must not then spin
        // through the passes it missed.
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = self.cancel.cancelled() => return Stopped::Cancelled,
                // The first pass is taken at once: `Interval`'s first tick is
                // now, which is the reference's "skipped on the first pass".
                _ = poll.tick() => {
                    if self.tick().await.shutdown.is_some() {
                        return Stopped::ShutDown;
                    }
                }
                event = events.next() => {
                    if event.is_none() {
                        return Stopped::Gone;
                    }
                }
            }
        }
    }

    /// One pass of §6.1, in its order.
    pub async fn tick(&self) -> Tick {
        let mut tick = Tick::default();
        let path = self.inbox.clone();
        let contents = match tokio::task::spawn_blocking(move || mailbox::read(&path)).await {
            Ok(Ok(contents)) => contents,
            Ok(Err(error)) => {
                tracing::warn!(
                    teammate = self.teammate.name(),
                    %error,
                    "a teammate's inbox could not be read"
                );
                return tick;
            }
            Err(error) => {
                tracing::warn!(teammate = self.teammate.name(), %error, "an inbox read was lost");
                return tick;
            }
        };
        for report in &contents.reports {
            tracing::warn!(teammate = self.teammate.name(), "{report}");
        }
        if contents.valid.is_empty() {
            return tick;
        }

        // Step 1, and it is a step of its own because it goes first.
        let shutdown = contents
            .valid
            .iter()
            .enumerate()
            .find_map(|(position, message)| match message.frame() {
                Some(Frame::ShutdownRequest(request)) => Some((position, message, request)),
                _ => None,
            });
        if let Some((position, message, request)) = shutdown {
            tick.jumped = position;
            tracing::info!(
                teammate = self.teammate.name(),
                request = request.request_id,
                jumped = position,
                "a shutdown request goes ahead of everything else in the inbox"
            );
            self.tear_down(&request).await;
            self.prune(vec![mailbox::identity(message)]).await;
            tick.shutdown = Some(request.request_id);

            return tick;
        }

        // Steps 2 and 3.
        let mut handled = Vec::new();
        let mut batch = Vec::new();
        let mut carried = Vec::new();
        let mut clears = Vec::new();
        for message in &contents.valid {
            let Some(kind) = Frame::reserved_kind(&message.text) else {
                batch.push(envelope(&message.from, &message.text));
                carried.push(mailbox::identity(message));
                continue;
            };
            match self.apply(kind, message).await {
                Verdict::Applied(name) => {
                    tick.applied.push(name);
                    handled.push(mailbox::identity(message));
                }
                Verdict::Ignored => {
                    tick.ignored += 1;
                    handled.push(mailbox::identity(message));
                }
                Verdict::Dropped(name) => {
                    tick.dropped.push(name);
                    handled.push(mailbox::identity(message));
                }
                Verdict::Tell {
                    kind,
                    text,
                    clears: request,
                } => {
                    tick.applied.push(kind);
                    batch.push(text);
                    carried.push(mailbox::identity(message));
                    clears.extend(request);
                }
            }
        }

        // Step 4. A batch the engine would not take stays in the inbox, so the
        // next pass offers it again rather than losing it.
        if !batch.is_empty() && self.deliver(&batch.join("\n\n")).await {
            tick.delivered = batch.len();
            handled.extend(carried);
            let mut awaiting = self.awaiting.lock().expect("the wait is never poisoned");
            if awaiting.as_ref().is_some_and(|held| clears.contains(held)) {
                *awaiting = None;
            }
        }
        if !handled.is_empty() {
            self.prune(handled).await;
        }

        tick
    }

    /// What one frame is worth, once it is known to be one.
    async fn apply(&self, kind: &'static str, message: &MailboxMessage) -> Verdict {
        // Undecodable, or from anybody but the lead: both are the same answer,
        // and the second is the whole of §7-2. `LeadFrame` cannot be built
        // from a peer's frame, so the two lead-only handlers below are
        // unreachable for one by construction rather than by a check.
        let Some(frame) = message.frame() else {
            return self.drop_it(kind, message);
        };
        let Some(lead) = LeadFrame::parse(&message.from, self.lead.as_str(), frame) else {
            return self.drop_it(kind, message);
        };

        match lead.into_inner() {
            Frame::PlanApprovalResponse(response) => self.plan_approval(response),
            Frame::ModeSetRequest(request) => self.mode_set(&request).await,
            _ => self.drop_it(kind, message),
        }
    }

    /// The lead's answer to a plan this teammate asked about.
    ///
    /// An answer nothing is waiting on is **ignored**, not applied: a stale
    /// approval is an answer to a question this conversation has already moved
    /// past, and acting on one would let a late frame change a posture nobody
    /// asked about now.
    ///
    /// An answer that *is* waited on becomes something the teammate reads,
    /// which is what unblocks it — and only once it has really been read is the
    /// wait cleared, so a delivery the engine refused leaves the teammate still
    /// waiting rather than silently unblocked.
    fn plan_approval(&self, response: PlanApprovalResponse) -> Verdict {
        let awaited = self
            .awaiting
            .lock()
            .expect("the wait is never poisoned")
            .as_ref()
            .is_some_and(|held| *held == response.request_id);
        if !awaited {
            tracing::info!(
                teammate = self.teammate.name(),
                request = response.request_id,
                "{IGNORED_STALE}"
            );

            return Verdict::Ignored;
        }

        let verdict = if response.approved {
            "The lead approved your plan."
        } else {
            "The lead did not approve your plan."
        };
        let text = match response.feedback {
            Some(feedback) => format!("{verdict}\n{feedback}"),
            None => verdict.to_owned(),
        };

        Verdict::Tell {
            kind: "plan_approval_response",
            text: envelope(self.lead.as_str(), &text),
            clears: Some(response.request_id),
        }
    }

    /// The lead setting this teammate's permission mode.
    ///
    /// Claude's mode vocabulary is not ganja's, and the mapping has a refusal
    /// in it (**D496**): a mode this build has no posture for is dropped by
    /// name rather than rounded to the nearest one, because rounding
    /// `bypassPermissions` to anything is a decision nobody made.
    async fn mode_set(&self, request: &ModeSetRequest) -> Verdict {
        let mode = match PermissionMode::from_claude_name(&request.mode) {
            Ok(mode) => mode,
            Err(refusal) => {
                tracing::warn!(
                    teammate = self.teammate.name(),
                    %refusal,
                    "{DROPPED_FRAME}: mode_set_request"
                );

                return Verdict::Dropped("mode_set_request");
            }
        };
        if let Err(error) = self
            .teammate
            .engine()
            .send(Command::SetPermissionMode { mode })
            .await
        {
            tracing::warn!(
                teammate = self.teammate.name(),
                %error,
                "a permission mode the lead set was refused"
            );

            return Verdict::Dropped("mode_set_request");
        }

        Verdict::Applied("mode_set_request")
    }

    /// Names a frame nobody here handles, with the head of it.
    ///
    /// The *head*, and only of a frame: a plain message's body never reaches a
    /// log line, but a frame that would not decode is undiagnosable without
    /// seeing some of it, which is the trade §6.1 already makes.
    fn drop_it(&self, kind: &'static str, message: &MailboxMessage) -> Verdict {
        tracing::warn!(
            teammate = self.teammate.name(),
            from = message.from,
            frame = head(&message.text),
            "{DROPPED_FRAME}: {kind}"
        );

        Verdict::Dropped(kind)
    }

    /// Puts a batch to the teammate's model.
    ///
    /// Two attempts and no more, because there are exactly two states to race:
    /// a turn starts between the prompt and the steer, or ends between them.
    /// Anything still refused is left in the inbox for the next pass, which is
    /// a delivery delayed rather than a delivery lost.
    async fn deliver(&self, text: &str) -> bool {
        match self.prompt(text).await {
            Ok(()) => true,
            Err(EngineError::Busy) => match self.steer(text).await {
                Ok(()) => true,
                Err(EngineError::NotStreaming) => self.report(self.prompt(text).await),
                other => self.report(other),
            },
            other => self.report(other),
        }
    }

    async fn prompt(&self, text: &str) -> Result<(), EngineError> {
        self.teammate
            .engine()
            .send(Command::SendPrompt {
                text: text.to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
            })
            .await
    }

    async fn steer(&self, text: &str) -> Result<(), EngineError> {
        self.teammate
            .engine()
            .send(Command::Steer {
                id: ganja_protocol::uuidv7(),
                text: text.to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
            })
            .await
    }

    /// Whether a delivery landed, saying so in the log when it did not.
    fn report(&self, outcome: Result<(), EngineError>) -> bool {
        match outcome {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    teammate = self.teammate.name(),
                    %error,
                    "a teammate's messages stay in its inbox until its next pass"
                );

                false
            }
        }
    }

    /// Settles the teammate, ends what it owns, and tells the lead it is done.
    ///
    /// The `from` on that answer is **this teammate's own name**, taken from
    /// the value it was constructed with and never from the request being
    /// answered: a message that carried its own sender would let whoever wrote
    /// it choose whose name the lead reads.
    async fn tear_down(&self, request: &ShutdownRequest) {
        if !self.teammate.shutdown(SETTLE).await {
            tracing::warn!(
                teammate = self.teammate.name(),
                "a teammate was still working when it was asked to shut down"
            );
        }

        let approved = Frame::ShutdownApproved(ShutdownApproved {
            request_id: request.request_id.clone(),
            from: self.teammate.name().to_owned(),
            timestamp: record::now_iso8601(),
            pane_id: Some(self.surface.tmux_pane_id().to_owned()),
            backend_type: Some(self.surface.backend_type().to_owned()),
        });
        let message = match MailboxMessage::from_frame(
            self.teammate.name(),
            &approved,
            record::now_iso8601(),
        ) {
            Ok(message) => message,
            Err(error) => {
                tracing::error!(
                    teammate = self.teammate.name(),
                    %error,
                    "a shutdown answer could not be encoded, so the lead is not being told"
                );

                return;
            }
        };

        let path = self.lead_inbox.clone();
        let written = tokio::task::spawn_blocking(move || mailbox::write(&path, message))
            .await
            .map_err(|error| error.to_string())
            .and_then(|written| written.map_err(|error| error.to_string()));
        if let Err(error) = written {
            // Worth shouting about, for §6.2's reason in the other direction:
            // the lead is the side that kills a pane and retires a member, and
            // it is now not going to.
            tracing::error!(
                teammate = self.teammate.name(),
                %error,
                "a teammate shut down without being able to tell the lead"
            );
        }
    }

    /// Takes everything this pass finished out of the inbox, in one write.
    async fn prune(&self, handled: Vec<mailbox::Identity>) {
        let path = self.inbox.clone();
        let outcome =
            tokio::task::spawn_blocking(move || mailbox::prune_delivered(&path, &handled))
                .await
                .map_err(|error| error.to_string())
                .and_then(|pruned| pruned.map_err(|error| error.to_string()));

        if let Err(error) = outcome {
            // Not fatal, and deliberately not retried here: the next pass reads
            // the same messages again, and a redelivery is a cost this one can
            // pay where a lost message is not.
            tracing::warn!(
                teammate = self.teammate.name(),
                %error,
                "a teammate could not prune its inbox"
            );
        }
    }
}

/// What one frame turned out to be worth.
enum Verdict {
    /// Applied here and now; it leaves the inbox whatever else this pass does.
    Applied(&'static str),
    /// Stale, or not this teammate's to act on. It still leaves the inbox:
    /// leaving it would be reading it again on every pass forever.
    Ignored,
    /// Named and dropped.
    Dropped(&'static str),
    /// Applied only once the teammate has actually read it, so it leaves the
    /// inbox with the batch it travels in.
    Tell {
        /// What kind of frame it was, for the pass's own account of itself.
        kind: &'static str,
        /// What the teammate reads.
        text: String,
        /// The plan approval this clears, once the teammate has read it.
        clears: Option<String>,
    },
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
    use super::{FRAME_HEAD, envelope, head};

    #[test]
    fn a_frames_head_is_cut_on_a_character_boundary() {
        let wide: String = "あ".repeat(FRAME_HEAD * 2);
        let cut = head(&wide);

        assert_eq!(cut.chars().count(), FRAME_HEAD);
        assert!(wide.starts_with(cut));
        // A frame shorter than the cap is not touched at all.
        assert_eq!(
            head("{\"type\":\"idle_notification\"}"),
            "{\"type\":\"idle_notification\"}"
        );
    }

    #[test]
    fn a_delivered_message_says_who_wrote_it() {
        assert_eq!(
            envelope("team-lead", "have a look at the parser"),
            "A message from team-lead:\nhave a look at the parser"
        );
    }
}
