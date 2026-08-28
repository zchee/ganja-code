//! What a teammate may do, and who answers when it asks (**D-5**).
//!
//! Upstream opencode has no counterpart at all: it has no teammates, so it has
//! no second conversation whose dialogs somebody has to own. What is ported is
//! Claude Code's §10.3 posture question — presented there as three unresolved
//! options — together with the guard §10.11-11 states around it, where a
//! teammate's directory passes the same external directory gate any other work
//! outside the project does. The plan is
//! `.omc/plans/2026-08-17-teammates-first-landing.md`. Its other guard,
//! §10.11-10 — a spawn that asks to skip dialogs is itself something to be
//! asked about — was built as P25's `/team spawn --bypass` and **retired on
//! 2026-08-22** (**D513**, user directive): there is one spawn request on both
//! doors, a `task` call's and a person's, and nothing on it about dialogs, so
//! there is no such spawn left to gate.
//!
//! # The rules are the lead's, and that is the whole of the posture
//!
//! D-8 gives a teammate a session of its own, and therefore a turn of its own.
//! That is what puts `ganja-permission`'s standing rule for delegated work —
//! "a subagent inherits the refusals and never the allows: nobody is watching
//! its turn" — out of reach by construction, and it opens one route worth
//! naming: a lead **launders** a call it was refused by having a teammate make
//! it instead.
//!
//! [`permissions_for`] closes it. A teammate engine is built from the lead's
//! own ruleset — the same project, the same store, the same answers a person
//! gave — with every refusal the lead is under appended *after* the teammate's
//! own agent rules, where last-match-wins puts them over anything an agent
//! could say. So a stored deny still denies, and a denied call raises **no
//! dialog**: a deny is not a question, and asking one would be inventing an
//! answer the person already gave.
//!
//! # Two postures, and neither of them is a rule
//!
//! | Posture | Who answers a dialog | Chosen by |
//! |---|---|---|
//! | [`ForwardToLead`] | the person sitting at the lead's dialog | **the default**, and today the only one selected |
//! | [`HumanAttended`] | the person sitting at the teammate's own terminal | a pane whose frontend forwards nothing — none does yet |
//!
//! Neither changes a single rule, which is why the posture is not an argument
//! of [`permissions_for`]: what it decides is where an *ask* is carried, and an
//! ask only ever happens where the rules already said "ask". The reference's
//! third option — nobody answers afterwards, because the spawn *was* the
//! answer — is the one **D513** retired: every ask a teammate's rules raise is
//! carried to a person, and no spawn can buy its way past that.
//!
//! [`ForwardToLead`] has two carriers, one per kind of teammate, and this
//! module holds the in-process one: [`Forwarding`] rides the dialog channel
//! and its reply oneshot. A **pane** has no channel into the lead's process,
//! so its asks ride §5's `permission_request`/`permission_response` frames
//! through the mailbox instead — [`crate::teammate::member::Asks`] on the
//! pane's side, [`crate::teammate::lead_inbox`] on the lead's — landing on
//! the very same channel from the file, so the person answering cannot tell
//! which kind of teammate asked. A pane would run [`HumanAttended`] only if its
//! frontend forwarded nothing at all — and no frontend in this build does:
//! `ganja-tui`'s member side resolves every pane to [`ForwardToLead`], so the
//! second row is the reference's option kept as a value, selected by nothing
//! yet.
//!
//! [`HumanAttended`] has no meaning in this process: an in-process teammate has
//! no terminal of its own to put a dialog on, so [`Forwarding`] takes no
//! posture at all and carries every ask to the lead. That is safe precisely
//! because the rules are identical either way — the only difference is which
//! screen the question lands on, and in this process there is one.
//!
//! Every link above is spelled out, because a module carrying both an outer doc
//! and this inner one has the merged text resolved in `teammate.rs`'s scope,
//! where none of these names exist. The house workaround, as `hook.rs` uses it.
//!
//! [`permissions_for`]: crate::teammate::posture::permissions_for
//! [`ForwardToLead`]: crate::teammate::posture::Posture::ForwardToLead
//! [`HumanAttended`]: crate::teammate::posture::Posture::HumanAttended
//! [`Forwarding`]: crate::teammate::posture::Forwarding

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt as _;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::engine::Evicted;
use crate::permission::{Decision, EXTERNAL_DIRECTORY, Permissions, Rule, matches, resolve};
use crate::protocol::team::MemberBackend;
use crate::protocol::{Command, Event, PermissionId, PermissionReply};
use crate::teammate::{Teammate, backend_name, posture_line};

/// The permission a spawn onto a foreign CLI is judged under (**D508(c)**).
///
/// Minted here rather than taken from `ganja-permission`'s own list because
/// nothing below the engine knows teammates exist. It is spelled like the
/// names that travel in a stored file — a config may write
/// `"teammate_foreign": {"grok": "deny"}` and have it mean what it looks like
/// — and judged against the **backend name**, not the `agentType`: what a
/// person is being asked to consent to here is which vendor's binary runs, and
/// two teammates of one agent type on two different CLIs are two different
/// grants.
///
/// # A stored allow cannot exist, and that is the mechanism
///
/// [`spawn_gate`] reads the lead's rules through
/// [`Permissions::inherited_by_subagent`], whose filter keeps a deny and an
/// `external_directory` rule and drops everything else — so a stored
/// `teammate_foreign: allow` never reaches `decide` and this clause answers
/// its [`Decision::Ask`] default anyway. Nothing writes such a rule either:
/// the spawn dialog's only consumer tests for a rejection and discards
/// `PermissionReply::Always`, because storing an answer here would mean
/// inventing a decision about a call that never happened.
///
/// The result is the property this clause exists for: **every** shim spawn
/// raises a dialog, every time, and there is no way to turn that off. That is
/// stronger than it first reads — a vendor's trust gate that cannot be
/// permanently pre-cleared is a gate that is never cleared silently — and it
/// is proportionate only because of what it gates: v1 composes each CLI's
/// most restrictive working posture, so the question a person is answering
/// repeatedly is about a read-only agent.
///
/// A stored **deny** does pass that filter, and refuses.
pub const FOREIGN: &str = "teammate_foreign";

/// The pattern a rule covering every teammate is written with.
const ANY: &str = "*";

/// Who answers when a teammate's turn asks for permission (**D-5**).
///
/// Two of the reference's three: what varies between a pane with a person
/// watching it and a teammate whose asks travel to the lead is exactly *where
/// the question goes*, and a type is the honest place to say so. The third —
/// nobody, because the spawn itself was the answer — was P25's `--bypass` and
/// retired with it (**D513**): a posture in which a person is asked nothing
/// is not one a spawn can choose any more, and so it is not a value either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Posture {
    /// The question travels to the lead's own dialog, and the answer travels
    /// back. The default, because a teammate nobody approved anything for is a
    /// teammate whose asks are the lead's to answer.
    #[default]
    ForwardToLead,
    /// The teammate's own engine asks, on the terminal a person is watching it
    /// on. A pane's posture; see the module doc for what it means here.
    HumanAttended,
}

/// The ruleset a teammate's engine runs under: the lead's, with `agent_rules`
/// beneath every refusal the lead is bound by.
///
/// The **attended** derivation ([`Permissions::derive`]) rather than the
/// unattended one a subagent gets, and the difference is the posture: a
/// teammate's asks reach the same person at the same dialog, so the answers
/// that person already gave are theirs to keep spending. What
/// [`Permissions::derive_subagent`] withholds it withholds because nobody is
/// watching, which is the one thing that is not true here.
///
/// The order is the anti-laundering rule, stated in one line:
/// [`Permissions::inherited_by_subagent`] — every deny in the lead's whole
/// ordered set, plus every `external_directory` rule whatever it says — goes
/// **after** the teammate's agent rules, and last-match-wins reads a baseline
/// backwards. So an agent whose ruleset allows what the lead's config denies
/// changes nothing, and a spawn cannot become a way around a standing "no".
///
/// What is deliberately *not* appended is `subagent_rules`'s `task`/`todowrite`
/// pair: those exist because a subagent must not delegate further and must not
/// keep a checklist nobody reads, and a teammate is neither — it is a root
/// session with a transcript somebody may open tomorrow (D-8).
#[must_use]
pub fn permissions_for(lead: &Permissions, agent_rules: Vec<Rule>) -> Permissions {
    let mut baseline = agent_rules;
    baseline.extend(lead.inherited_by_subagent());

    lead.derive(baseline)
}

/// What the lead's own rules say about a spawn, before the teammate exists.
///
/// Two questions, both asked on the **lead's** side and both answered out of
/// the lead's ruleset, because a teammate cannot be trusted to gate its own
/// creation. Neither is a question the permission engine knows how to derive
/// from a call — one is about a directory nothing has run in yet and the other
/// about which vendor's binary would run — so they are read here and handed to
/// the one door that spawns, which is where the dialog belongs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpawnGate {
    /// The teammate's own directory and what the rules say about working
    /// there. [`None`] when it is inside the project, which asks nothing.
    pub directory: Option<(PathBuf, Decision)>,
    /// The foreign CLI this spawn would run, and what the rules say about
    /// running it (**D508(c)**). [`None`] for P25's three surfaces, which is
    /// what keeps their spawns exactly as silent as they were.
    ///
    /// Carries the backend for the same reason [`SpawnGate::directory`]
    /// carries its path: a refusal a person cannot act on is a refusal that
    /// does not say which of the six was refused.
    pub foreign: Option<(MemberBackend, Decision)>,
}

impl SpawnGate {
    /// What the spawn as a whole needs: the strongest of what was asked.
    ///
    /// The same all-or-nothing rule a call's patterns get — one denied part
    /// refuses the spawn, one unfamiliar part puts it in front of somebody.
    #[must_use]
    pub fn action(&self) -> Decision {
        self.directory
            .as_ref()
            .map(|(_, decision)| *decision)
            .into_iter()
            .chain(self.foreign.map(|(_, decision)| decision))
            .max()
            .unwrap_or(Decision::Allow)
    }

    /// The directories a dialog has to name, which is at most the one.
    ///
    /// A person answering "may this teammate run" cannot answer it without
    /// "where", the same reason a call's own dialog discloses them.
    #[must_use]
    pub fn directories(&self) -> Vec<PathBuf> {
        self.directory.iter().map(|(directory, _)| directory.clone()).collect()
    }

    /// Why a refused spawn was refused, in the words whoever asked reads next.
    ///
    /// [`None`] unless something was actually denied: a spawn that only has to
    /// be asked about is not a refusal, and a sentence here would read like
    /// one.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        let mut refused = Vec::new();
        if let Some((directory, Decision::Deny)) = &self.directory {
            refused.push(format!(
                "a rule refuses work in {}; spawn it inside the project",
                directory.display()
            ));
        }
        if let Some((backend, Decision::Deny)) = self.foreign {
            refused.push(format!(
                "a rule refuses teammates on the {} backend; spawn it on one of this build's own",
                backend_name(backend)
            ));
        }

        (!refused.is_empty()).then(|| refused.join(", and "))
    }
}

/// Reads the lead's rules for the two things a spawn decides (§10.11-11, and
/// **D508(c)**).
///
/// `project_root` is the lead's own project root — what `Permissions::load`
/// resolved for the session doing the spawning, and emphatically not the
/// project the teammate's `cwd` happens to sit in. Resolving the teammate's
/// directory to its *own* project would be the laundering move in its purest
/// form: a teammate started in somebody else's checkout would be judged by
/// that checkout's rules, which are nobody's answers at all.
///
/// The window onto the lead's rules is [`Permissions::inherited_by_subagent`],
/// which is every deny and every `external_directory` rule and nothing else.
/// So a blanket `"permission": "allow"` is invisible here and a foreign
/// directory is still asked about: the error is towards asking, which is the
/// direction the permission layer errs in everywhere else.
///
/// That filter is not merely a safety margin for the foreign clause — it **is**
/// that clause's mechanism. A stored `teammate_foreign: allow` is dropped
/// before `decide` sees it, so [`FOREIGN`] answers [`Decision::Ask`]
/// whatever a config says, and a stored deny passes the filter and refuses.
/// The invariant that falls out is the one **D508(c)** wanted: every spawn
/// onto a foreign CLI raises a dialog, and there is no rule anybody can write
/// to stop it.
#[must_use]
pub fn spawn_gate(
    lead: &Permissions,
    project_root: &Path,
    cwd: &Path,
    backend: MemberBackend,
) -> SpawnGate {
    // The two arguments after the root are the two facts a spawn decides that
    // the rules have an opinion about — handed in bare rather than behind a
    // [`SpawnSpec`], because the real spec is built by the registry after
    // this gate answers and a placeholder-stuffed one here would be a value a
    // later reader might trust.
    //
    // Every deny the lead is under, and every `external_directory` rule
    // whatever it says — the same set a teammate's own engine is bound by, so
    // the gate and what it gates cannot disagree.
    let rules = lead.inherited_by_subagent();
    let directory = outside(project_root, cwd).map(|directory| {
        let pattern = covering(&directory);

        (directory, decide(&rules, EXTERNAL_DIRECTORY, &pattern))
    });
    // Read only for the shims, so P25's three surfaces keep answering exactly
    // what they answered before this clause existed.
    //
    // `posture_line` is the discriminator rather than a second list of which
    // backends are foreign, because the two questions have one answer: a
    // backend has a posture to disclose exactly when *ganja* can ask nothing
    // after its spawn — a headless child has no channel, and a CLI's native
    // TUI in a pane (**D512**) puts the CLI's own prompts in front of a
    // person under the CLI's rules, never this gate's. Two lists would be two
    // places to add a seventh backend to, and the one that got forgotten
    // would be this one.
    //
    // Never below `Ask`, though here that floor is belt and braces rather
    // than the mechanism — `inherited_by_subagent` has already dropped any
    // allow that could have lowered it (see [`FOREIGN`]).
    let foreign = posture_line(backend)
        .map(|_| (backend, decide(&rules, FOREIGN, backend_name(backend)).max(Decision::Ask)));

    SpawnGate { directory, foreign }
}

/// The teammate's directory, when the project does not reach it.
///
/// Resolved through the gate's own resolver rather than compared as text: two
/// spellings of one directory — a symlink, a relative path, a `..` — must not
/// be answered differently, and that rule is the permission layer's rather
/// than this module's to restate.
fn outside(project_root: &Path, cwd: &Path) -> Option<PathBuf> {
    let root = resolve(project_root);
    let directory = resolve(cwd);

    (!directory.starts_with(&root)).then_some(directory)
}

/// How a directory is named as a pattern: everything at or under it.
///
/// `Permissions::gate`'s own spelling for the rules an "always" answer to a
/// location dialog leaves behind, so a rule stored by answering one of those
/// dialogs is a rule this gate reads.
fn covering(directory: &Path) -> String {
    directory.join(ANY).to_string_lossy().into_owned()
}

/// What `rules` say about `permission`/`pattern`, asking when they say nothing.
///
/// The rules arrive already filtered to what a teammate inherits, so this is
/// last-match-wins over that set. Ask is the default because both permissions
/// read here are things a person should see once: `external_directory` asks by
/// default in the permission engine itself, and [`FOREIGN`] would be a name
/// nothing has an opinion about, which the engine's own default would read as
/// allow.
fn decide(rules: &[Rule], permission: &str, pattern: &str) -> Decision {
    rules
        .iter()
        .rev()
        .find(|rule| matches(permission, &rule.permission) && matches(pattern, &rule.pattern))
        // [`Action::decision`]'s own reading rather than a second copy of it:
        // what an action this build cannot carry out means is the permission
        // engine's answer to give, and two matches over those four variants
        // are two places for it to drift.
        .map_or(Decision::Ask, |rule| rule.action.decision())
}

/// One teammate's permission dialog, on its way to the lead.
///
/// The request travels as the teammate's engine published it, session id and
/// all. A teammate session is a **real** one — listed, resumable, its own root
/// row (D-8) — where a subagent's is invisible, so re-addressing the request to
/// the lead the way `subagent.rs`'s watcher does would be hiding a
/// conversation a person can already open. What the lead's side does with an
/// event naming a session it is not showing is the lead's side's decision.
#[derive(Debug)]
pub struct Forwarded {
    /// Which teammate is waiting.
    pub teammate: String,
    /// The `Event::PermissionRequested` the teammate's turn published.
    pub request: Event,
    /// Where the answer goes. Dropping it is an answer too — the refusal a
    /// dialog nobody could show means.
    pub reply: oneshot::Sender<PermissionReply>,
}

/// Carries one teammate's dialogs to the lead — [`Posture::ForwardToLead`]'s
/// in-process carrier, and the only carrier an in-process teammate has.
///
/// The answer travels back as a [`Command::ReplyPermission`] naming the
/// request's **own id**, which is the whole of the routing: the teammate
/// engine's pending-reply registry (**D462**) turns that id back into the call
/// that is waiting, and it already answers several open dialogs by id rather
/// than by taking whatever is there. Ids are UUIDv7, so two engines minting
/// them at once cannot collide and a reply cannot land in the wrong
/// conversation.
///
/// The subscription is **droppable** and is registered when this value is
/// built rather than when its task starts, for two reasons that are really one:
/// a lossless lane would let a lead that stopped reading backpressure the
/// teammate's turn, and a subscription registered inside the spawned task would
/// race the teammate's first dialog. An eviction ends the forwarding with a
/// warning, which is the honest outcome — the alternative is a bridge that
/// silently stops carrying questions.
///
/// Nothing on this path ever waits on the lead. The channel a question travels
/// to the lead on is bounded and is offered a question rather than made to
/// carry one (`hand_over`), so a lead that is behind — or that never claimed
/// its receiver at all — costs the teammate a refusal and never its turn.
pub struct Forwarding {
    teammate: Arc<Teammate>,
    lead: Option<mpsc::Sender<Forwarded>>,
    events: futures::stream::BoxStream<'static, Result<Event, Evicted>>,
}

impl std::fmt::Debug for Forwarding {
    /// Hand-written because an event stream has no [`std::fmt::Debug`], and
    /// what a reader wants here is whose dialogs go where anyway.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Forwarding")
            .field("teammate", &self.teammate.name())
            .field("attached", &self.lead.is_some())
            .finish_non_exhaustive()
    }
}

impl Forwarding {
    /// Prepares to carry `teammate`'s dialogs.
    ///
    /// `lead` is [`None`] when there is no dialog surface at all — a headless
    /// lead, or a session nothing attached one to. Every ask is then refused
    /// rather than left hanging: a question nobody can see has exactly one
    /// honest answer, and a refusal is information the model reads and carries
    /// on from. A `lead` whose queue is full is the same answer for the same
    /// reason (`hand_over`); the two arms differ only in whether the surface
    /// was ever there.
    #[must_use]
    pub fn new(teammate: Arc<Teammate>, lead: Option<mpsc::Sender<Forwarded>>) -> Self {
        let events = teammate.engine().subscribe_droppable();

        Self { teammate, lead, events }
    }

    /// Carries dialogs until `cancel` fires or the teammate's stream ends.
    ///
    /// Taken apart on the way in rather than borrowed through: an event stream
    /// is [`Send`] and not [`Sync`], so a loop that held `&self` across one of
    /// its own awaits would be a future nothing could spawn.
    pub async fn run(self, cancel: CancellationToken) {
        let Self { teammate, lead, mut events } = self;

        loop {
            let next = tokio::select! {
                () = cancel.cancelled() => return,
                next = events.next() => next,
            };
            let event = match next {
                Some(Ok(event)) => event,
                Some(Err(Evicted)) => {
                    tracing::warn!(
                        teammate = teammate.name(),
                        "a teammate's dialogs fell behind and stopped being carried"
                    );
                    return;
                }
                None => return,
            };
            // Everything else on a teammate's stream belongs to whoever is
            // rendering it; this loop is about the questions only.
            let Event::PermissionRequested { id, .. } = &event else {
                continue;
            };
            let request = id.clone();

            let Some(lead) = lead.clone() else {
                answer(&teammate, request, PermissionReply::Reject).await;
                continue;
            };
            hand_over(&teammate, lead, request, event, &cancel).await;
        }
    }
}

/// Hands one request to the lead and arranges for its answer.
///
/// **The handover never waits.** A [`mpsc::Sender::try_send`] rather than a
/// `send().await`, and the difference is the whole of the queue's contract: an
/// awaited send on a bounded channel whose receiver nobody claimed — or whose
/// claimant stopped draining — blocks here forever, and the teammate's turn is
/// blocked behind it with no timeout and nothing to cancel it. A full queue is
/// therefore answered the same way no queue at all is: a question that cannot
/// be put in front of anybody is refused, which is information the model reads
/// and carries on from. The queue is deliberately small
/// (`Engine::TEAMMATE_DIALOGS`) because a lead holding a dozen unanswered
/// dialogs is a lead nobody is sitting at.
///
/// The wait for the *answer* runs in a task of its own so the loop keeps
/// reading: a teammate whose turn opened two dialogs at once must not have the
/// second one held behind the first, which is the very collision the
/// pending-reply registry was made a registry for.
async fn hand_over(
    teammate: &Arc<Teammate>,
    lead: mpsc::Sender<Forwarded>,
    request: PermissionId,
    event: Event,
    cancel: &CancellationToken,
) {
    let (sender, receiver) = oneshot::channel();
    if let Err(undelivered) = lead.try_send(Forwarded {
        teammate: teammate.name().to_owned(),
        request: event,
        reply: sender,
    }) {
        // Full and closed are one answer with two reasons, and the reason is
        // worth a line: a lead that has gone is permanent, where a lead that
        // is behind is this teammate's asks being dropped while it catches up.
        tracing::warn!(
            teammate = teammate.name(),
            reason = match undelivered {
                mpsc::error::TrySendError::Full(_) => "the lead's dialog queue is full",
                mpsc::error::TrySendError::Closed(_) => "the lead's side is gone",
            },
            "a teammate's permission dialog was refused rather than made to wait"
        );
        answer(teammate, request, PermissionReply::Reject).await;

        return;
    }

    let teammate = Arc::clone(teammate);
    let cancel = cancel.clone();
    tokio::spawn(async move {
        let reply = tokio::select! {
            // A cancel is the registry shutting the team down, and the
            // teammate's turn is still sitting in this dialog: its command
            // channel is up until its engine goes, so the honest move is to
            // answer rather than to walk away and leave `Teammate::shutdown`
            // waiting out its settle on a turn nothing will ever unblock.
            () = cancel.cancelled() => PermissionReply::Reject,
            // A dropped sender is a lead that gave up on the dialog, which is
            // the refusal it looks like.
            reply = receiver => reply.unwrap_or(PermissionReply::Reject),
        };
        answer(&teammate, request, reply).await;
    });
}

/// Answers one of `teammate`'s open dialogs by the id it was published with.
async fn answer(teammate: &Teammate, id: PermissionId, reply: PermissionReply) {
    if let Err(error) = teammate.engine().send(Command::ReplyPermission { id, reply }).await {
        tracing::warn!(
            teammate = teammate.name(),
            %error,
            "a teammate's permission answer could not be delivered"
        );
    }
}

#[cfg(test)]
#[path = "posture_tests.rs"]
mod tests;
