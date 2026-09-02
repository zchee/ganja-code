//! The two guards a lead's own turn loop runs under while it holds a team.
//!
//! Both exist because neither can be a sentence in a prompt and still be true.
//! `/team`'s instruction template can *ask* the model to keep going until the
//! list is drained and to give continuing work a named teammate; what it
//! cannot do is notice when the model stopped anyway. So the noticing is
//! engine-native, made of facts this build can type — a registry that holds a
//! live member, a task document that is still pending, a `task` call whose
//! arguments carry no `name` — and never of the state JSON the model writes
//! for itself, which is exactly as reliable as the behavior it is supposed to
//! be checking.
//!
//! Nothing here is ported from either upstream: opencode has no teams, and
//! Claude Code's own equivalents are a Stop hook's block decision and a
//! system reminder rather than anything this port can read. The specification
//! is decisions 11 and 12 of `.omc/plans/2026-09-02-team-orchestration.md`,
//! and both texts below are ganja's own prose.
//!
//! # Where each one sits
//!
//! The **continuation blocker** ([`Discipline::should_continue`]) sits at the
//! one point the agent loop calls a turn over: the model returned a step with
//! no tool calls, and no steer was waiting. It is checked *after* the steer
//! drain, which is the whole of "it loses to steering" — a person who typed
//! while the model was finishing is answered, and their message resets the
//! counter, because a turn that continued because somebody asked it to is not
//! a turn that continued by itself.
//!
//! A *teammate's* message arrives at that same mailbox and keeps the turn
//! going the same way, and resets nothing ([`Discipline::user_took_over`]):
//! the traffic a live team generates is what the breaker exists to count, so
//! a team with something to say every step would otherwise refill the budget
//! forever. What that does not bound is the turn itself: a team that speaks
//! every step keeps it going one drained message at a time, and nothing here
//! counts those, because the breaker counts what the model did on its own.
//!
//! The **name nag** ([`Discipline::note_anonymous_delegation`]) sits one step
//! earlier, over the calls a step produced and before any of them runs.
//! Scanning the batch is what makes "once per assistant step" structural
//! rather than a flag that happens to collapse: five anonymous `task` calls in
//! one fan-out are one scan and one block. It is informational and blocks
//! nothing — an anonymous subagent stays first-class (**D462**) — because the
//! model is often right to want one.
//!
//! What makes a step *about a team* is two facts, either of which is enough
//! (bead s8rw): the registry holds somebody live at the moment of the scan, or
//! this very batch names somebody in a `task` call of its own
//! ([`delegates_named`]). The second exists because the first cannot see the
//! pipeline's own opening step, which spawns its named members and an
//! anonymous helper in one batch: the registry is empty right up until those
//! calls run, so a registry-only trigger answers about the instant before the
//! delegation it is supposed to be about. A turn that leads nobody and names
//! nobody is still silent, which is D462 intact.
//!
//! # Why the counter is a turn's and not a session's
//!
//! "Five *consecutive* auto-continuations" is exactly a turn's own count: the
//! only way a sixth turn happens is that somebody prompted or steered, and
//! either resets it. A cancel ends the loop before this is consulted at all.
//! So the breaker needs no engine cell, nothing to clear on `NewSession`, and
//! no way to disagree with itself across a resume.

use std::fmt::Write as _;

use ganja_tool::tasklist::{Status, Summary};

/// How many auto-continuations a turn may inject before it stops and returns
/// to the user.
///
/// A constant rather than a config key, deliberately (plan non-goal): what a
/// person tunes when this annoys them is whether the team is running at all,
/// and a knob here would be a way to make the annoying case worse rather than
/// to fix it. Five is enough to carry a stalled reconciliation over a few
/// members reporting in, and short enough that a model looping on a task it
/// cannot finish hands the session back inside a minute.
pub(crate) const MAX_CONTINUATIONS: usize = 5;

/// What the model reads when a turn was about to end with the team's list
/// still holding work.
const CONTINUATION: &str = "<team_still_working>\nYour team is still running and its task list \
                            still holds work that is neither completed nor abandoned. Do not stop \
                            here. Reconcile the list with `task_list`, then do whichever of these \
                            applies: take or reassign a task nobody is making progress on, unblock \
                            one that is waiting on something already done, give an idle member its \
                            next piece of work, or — if the work really is finished — shut the \
                            team down by sending each member a shutdown_request and confirming it \
                            was approved. Ending the turn with members running and tasks open \
                            leaves the team stranded.\n</team_still_working>";

/// What the model reads after a step delegated to an unnamed subagent while
/// the team was live.
const NAG: &str = "<teammate_naming>\nA `task` call in your last step carried no `name`, so it ran \
                   as a disposable subagent: it works once, reports back, and is gone. That is the \
                   right shape for a self-contained question. It is the wrong shape for work that \
                   belongs to this team — a subagent cannot claim a task, cannot be messaged, \
                   cannot be reassigned and does not appear on the roster. Give continuing work to \
                   a named teammate instead.\n</teammate_naming>";

/// The typed facts the continuation blocker decides on, gathered by the turn
/// loop and never inferred from anything the model wrote.
///
/// A struct rather than three `bool` parameters because three of those at one
/// call site is an invitation to pass them in the wrong order, and each of
/// these is a different question about a different subsystem.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Facts {
    /// Whether the teammate registry holds somebody still running.
    ///
    /// Read at the moment the question is asked rather than at the turn's
    /// start: the pipeline's own first step spawns the members, so a turn that
    /// began leading nobody is exactly the turn this is about.
    pub(crate) live_team: bool,
    /// Whether a permission dialog or a question is still waiting for an
    /// answer — **this turn's own, or one a teammate forwarded** to the same
    /// person (bead xysf).
    ///
    /// A continuation queued behind either would put a synthetic instruction
    /// in front of something the person has not answered yet, and both land on
    /// one screen: a teammate's dialog is carried to whoever is sitting at the
    /// lead, which is exactly who the block would be talking over.
    pub(crate) dialog_open: bool,
    /// Whether the shared list holds a pending or in-progress task
    /// ([`holds_unfinished_work`]).
    pub(crate) unfinished_work: bool,
}

impl Facts {
    /// Whether everything but the breaker says to keep going.
    ///
    /// Named because the gathering site needs the same question the decision
    /// asks, minus the one clause that is about this turn rather than about
    /// the team: the breaker's log line claims the turn was *handed back*,
    /// which is only true when these three would have continued it (bead
    /// lymf). Said once here rather than spelled twice, so the claim and the
    /// decision cannot come apart.
    pub(crate) fn would_continue(self) -> bool {
        self.live_team && !self.dialog_open && self.unfinished_work
    }
}

/// One turn's own state for both guards.
///
/// Lives on the turn rather than on the engine for the reason
/// [`MAX_CONTINUATIONS`] is a constant: every question either answers is about
/// this turn, and a session-lifetime cell would only create ways for the two
/// to disagree.
#[derive(Debug, Default)]
pub(crate) struct Discipline {
    /// Whether the next request carries the nag block.
    ///
    /// Set by [`Discipline::note_anonymous_delegation`] over a whole step's
    /// calls, cleared by the render that spends it.
    nag: bool,
    /// Whether the next request carries the continuation block.
    continuation: bool,
    /// How many auto-continuations this turn has already spent.
    continued: usize,
}

impl Discipline {
    /// Records that this step delegated without naming anybody, so the next
    /// request says so once.
    ///
    /// Idempotent within a step by construction: the caller scans the step's
    /// whole batch and calls this at most once, and a second call would set an
    /// already-set flag anyway.
    pub(crate) fn note_anonymous_delegation(&mut self) {
        self.nag = true;
    }

    /// Whether the breaker still allows an auto-continuation.
    ///
    /// Read before the expensive fact is gathered, because gathering that one
    /// means listing the task documents off the disk and a turn that has spent
    /// its budget has no use for the answer.
    pub(crate) fn may_continue(&self) -> bool {
        self.continued < MAX_CONTINUATIONS
    }

    /// Whether a turn about to end should keep going instead.
    ///
    /// The blocker's whole truth table, over facts the caller has already
    /// gathered — which is why it is here rather than spread through the
    /// gathering site: every clause is then one expression a test can walk
    /// exhaustively, including the two that are hard to reach through a live
    /// engine. Every one of the four must hold.
    pub(crate) fn should_continue(&self, facts: Facts) -> bool {
        facts.would_continue() && self.may_continue()
    }

    /// Spends one auto-continuation, so the next request carries the block,
    /// and answers with how many this turn has now spent.
    pub(crate) fn continue_turn(&mut self) -> usize {
        self.continued += 1;
        self.continuation = true;

        self.continued
    }

    /// Forgets the continuations spent so far, because something a person did
    /// carried the turn instead.
    ///
    /// "Consecutive" is the whole of it: a steer that keeps the turn alive is
    /// the user still driving, and the budget that exists to stop a model
    /// talking to itself should not be spent by a conversation.
    ///
    /// Called for a message somebody **typed** and for no other kind. A
    /// teammate's message reaches the turn through the same mailbox and is
    /// deliberately not that: it is the team running, which is the state this
    /// breaker is counting rather than an interruption of it.
    ///
    /// It resets the counter and nothing else, because there is never a queued
    /// continuation here to withdraw: the loop spends one the moment it takes
    /// it — [`Discipline::continue_turn`] is followed by the request assembly
    /// that calls [`Discipline::take_blocks`] before the steer drain can be
    /// reached again — so a "queued but unrendered" continuation is a state
    /// this loop cannot produce (bead lymf). An arm clearing it would be dead
    /// code that reads as though the state were ordinary.
    pub(crate) fn user_took_over(&mut self) {
        self.continued = 0;
    }

    /// The blocks the request being assembled should carry, each spent as it
    /// is taken.
    ///
    /// Rendered and cleared in one call, which is what makes each block
    /// appear exactly once however many steps follow it — the D492 deferred
    /// listing's opposite property, and for the opposite reason: that block
    /// describes a state that persists, and these two describe a thing that
    /// just happened.
    pub(crate) fn take_blocks(&mut self) -> Vec<String> {
        let mut blocks = Vec::new();
        if std::mem::take(&mut self.nag) {
            blocks.push(NAG.to_owned());
        }
        if std::mem::take(&mut self.continuation) {
            blocks.push(CONTINUATION.to_owned());
        }

        blocks
    }
}

/// Whether one task is work that is neither finished nor abandoned.
///
/// Half of the continuation blocker's typed condition, and the half worth
/// naming: `completed` is done and a deleted task is not on the list at all,
/// so what is left is precisely pending and in-progress. Named rather than
/// inlined because the caller counts what this answers `true` for and reports
/// the count in the log, and a guard whose sentence disagrees with its own
/// decision is worse than no sentence.
pub(crate) fn is_unfinished(task: &Summary) -> bool {
    matches!(task.status, Status::Pending | Status::InProgress)
}

/// Whether a task list holds any [unfinished work](is_unfinished).
///
/// A list of nothing is not unfinished work, which is what makes a team that
/// filed no tasks — or drained them all — end its turn like any other session.
pub(crate) fn holds_unfinished_work(tasks: &[Summary]) -> bool {
    tasks.iter().any(is_unfinished)
}

/// Whether one `task` call names a teammate, or [`None`] for arguments that
/// will not parse at all.
///
/// Arguments arrive as the raw JSON the model produced, because that is what
/// the loop holds before a call is parsed — and a `name` that is absent, not a
/// string, or blank is all the same answer. The unparseable case is a third
/// answer rather than either of the other two: such a call is about to fail
/// with a message of its own, and it is evidence of nothing about how this
/// model delegates. One function so the two questions below cannot disagree
/// about which is which.
fn names_a_teammate(arguments: &str) -> Option<bool> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(arguments)
    else {
        return None;
    };

    Some(
        map.get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| !name.trim().is_empty()),
    )
}

/// Whether this `task` call delegates without naming anybody — the state the
/// nag is about.
///
/// A call whose arguments will not parse is not counted: telling the model to
/// name a teammate on a call that never ran would be advice about the wrong
/// thing.
pub(crate) fn delegates_anonymously(arguments: &str) -> bool {
    names_a_teammate(arguments) == Some(false)
}

/// Whether this `task` call names somebody — the counterpart question, and the
/// second of the nag's two triggers (bead s8rw).
///
/// A step that spawns named teammates *and* an anonymous helper in the same
/// batch is delegating into a team whether or not the registry holds anybody
/// yet, which is precisely what the pipeline's own first step looks like: it
/// spawns its members in the batch the nag is scanning, so a trigger that only
/// read the registry answered about the moment before that batch ran and said
/// nothing. Reading it off the calls themselves makes the answer independent of
/// *when* anything is read.
pub(crate) fn delegates_named(arguments: &str) -> bool {
    names_a_teammate(arguments) == Some(true)
}

/// The one-line note the log carries when a turn spends a continuation.
///
/// Here rather than inline so the two guards' vocabulary stays in one file,
/// and so a reader grepping for what "auto-continued" means lands on the
/// module that decides it.
pub(crate) fn continuation_note(spent: usize, open: usize) -> String {
    let mut note = String::new();
    let _ = write!(note, "auto-continuation {spent} of {MAX_CONTINUATIONS}, {open} task(s) open");

    note
}

#[cfg(test)]
#[path = "discipline_tests.rs"]
mod tests;
