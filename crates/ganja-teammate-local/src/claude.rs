//! A teammate that is a real `claude` pane.
//!
//! Upstream opencode has **no counterpart**. This is the backend the shared
//! on-disk format exists for: a real `claude` process, in a pane of its own,
//! reading and writing a team directory through `ganja-team` — the whole of
//! D-1's interop claim, and the only backend that can falsify it.
//!
//! # The sequence, and where it differs from the `ganja` pane's
//!
//! §4.1's six steps, split between this backend and the registry exactly as
//! [`crate::pane`]'s are: [`TeammateBackend::spawn`](ganja_core::teammate::TeammateBackend::spawn) makes the
//! surface, the registry writes the member record, and [`Spawned::launch`](ganja_core::teammate::Spawned::launch)
//! runs afterwards — which is why the inbox work sits *there* rather than in
//! `spawn`. That ordering is §4.1's own —
//! surface (1), member record (2), pane title (3), `ensureInboxDirectory` (4),
//! the prompt written into the inbox (5), the launch line (6) — **with one
//! departure, and it is step 3**: the title is set inside `spawn`, so it lands
//! before the record rather than after it. The reason is that a title is
//! cosmetic and a `spawn` that has just made a window is the only place holding
//! the pane id without a second `tmux` round trip; nothing reads a pane's title,
//! and a person watching the split would rather see the name immediately than in
//! §4.1's order. Steps 4, 5 and 6 keep it exactly.
//!
//! The `ganja` pane watches for its record because its body predates the hook;
//! this one uses the hook, so a launch that cannot be made is unwound by the
//! registry — the pane killed, the record taken back out, the name given back —
//! instead of leaving a member holding an idle shell.
//!
//! Three things are this backend's own:
//!
//! - **the root**. Ganja's teams live under ganja's config home; a real
//!   `claude` reads `$CLAUDE_CONFIG_DIR/teams` (§2.1) and nothing will
//!   persuade it otherwise, so that is where this backend seeds the inbox —
//!   *the same `ganja-team` code, a different value*, which is the whole of
//!   what D-1 buys. [`teams_root`](ganja_core::teammate::teams_root) is
//!   that value, and it is public because the lead side needs it too: a lead
//!   that polls only its own root never sees what a `claude` pane wrote. See
//!   "the shared inbox" below.
//! - **the binary**. `claude` is resolved on `PATH`, deliberately unlike the
//!   `ganja` pane's `current_exe()`: §10.10's rule exists because a
//!   PATH-resolved *ganja* could be a different build joining a different
//!   session store, and this process has no `claude` build to be consistent
//!   with. It is resolved **before** the pane is split, so a machine without
//!   the binary gets one sentence instead of a window that closes.
//! - **the preamble's channel**. §5.5.1: `"main"` names *the sender's own
//!   parent conversation*, and a pane-backed teammate is the main conversation
//!   of its own session — so it has no parent and a send to `main` fails. The
//!   seeded message therefore names the lead, in ganja's own words (**D497**:
//!   no Claude Code prose is copied here). Since **D514** every backend seeds
//!   a preamble in [`ganja_core::teammate::preamble`]'s shared frame; what stays
//!   this backend's own is the paragraph naming a real `claude`'s
//!   `SendMessage`, and the fact that it seeds the message itself, under
//!   claude's root.
//!
//! # The shared inbox, and the three things that follow from two roots
//!
//! A round trip needs *one* directory: this backend seeds
//! `$CLAUDE_CONFIG_DIR/teams/<team>/inboxes/<name>.json` and the pane answers
//! into `…/inboxes/<lead>.json` beside it. Three consequences, all of them
//! stated rather than hidden — the first two are why
//! [`teams_root`](ganja_core::teammate::teams_root) is public.
//!
//! 1. **The lead has to read that root too.** A lead polling only
//!    `<ganja config home>/teams` would never see the answer, because the answer
//!    is in the other directory. So it reads both:
//!    [`ganja_core::teammate::lead_inbox::LeadInbox`] adds the claude root's
//!    `team-lead` inbox to its pass for as long as the roster holds a
//!    claude-backed member, and this backend seeds **both** inboxes under that
//!    root at launch so the file exists before either side needs it. AC-13's
//!    live test instead points the lead's own [`ganja_team::TeamsRoot`] at what
//!    `teams_root` answers, collapsing the two roots into one — the tightest
//!    configuration in which a `claude` teammate and a `ganja` lead are members
//!    of the same team, and the one that would hide a lead that read only its
//!    own root, which is why the production path does not depend on it.
//! 2. **Only one process may write an inbox.** The registry seeds §4.1's steps 4
//!    and 5 for every other backend and deliberately not for this one
//!    ([`ganja_core::teammate::TeammateBackend::owns_inbox`]): with the roots
//!    collapsed, its bare-prompt message landed *ahead* of
//!    [`preamble`](crate::claude::preamble) in the same file and a real
//!    `claude` read the one message that does not name its lead; with the roots
//!    apart, that copy rotted under the ganja root where nothing reads. So this
//!    backend writes the teammate's inbox, and it prunes its own write when its
//!    launch line cannot be typed.
//! 3. **The member record is written under the ganja root only.** The team file
//!    is [`ganja_core::teammate::TeammateRegistry`]'s own document, written where that
//!    registry lives; this backend writes no second one. So unless a lead has
//!    pointed its own root at claude's — the collapsed configuration point 1
//!    describes, where the two are one directory and the question does not arise
//!    — `$CLAUDE_CONFIG_DIR/teams/<team>/config.json` does not name this member.
//!    What that means for the pane in the ordinary split-root case: a real
//!    `claude` reading its own team's config finds no row for itself — no roster,
//!    no peers, and nothing to read the colour, `agentType` or `planModeRequired`
//!    this spawn recorded off. It does not stop the round trip, because a mailbox
//!    is a file per name rather than a roster lookup, and the flags on §4.1's
//!    launch line already carry the facts the pane needs about itself. Writing a
//!    second team file under claude's root would be inventing a document with two
//!    owners and no lock between them, so this build does not.
//!
//! # What rides the launch line, and what does not
//!
//! §4.1's own flags: the five that identify the teammate, and
//! `--plan-mode-required` when the spawn asked for plan mode. Until 2026-08-22
//! the line also carried `--permission-mode bypassPermissions` when the spawn
//! had asked for bypass — the spelling the reference records twice, once as the
//! `permissionMode: "bypassPermissions"` a real pane's transcript opens with
//! (§4.1) and once as the flag a Claude-compatible CLI advertises (§10.7) —
//! and **D513** retired that axis with the `--bypass` that fed it, so no
//! permission mode is composed here at all. What is deliberately absent is
//! §4.1's other two optionals:
//!
//! - **not `--model`.** A [`SpawnSpec`](ganja_core::teammate::SpawnSpec)'s model is the id ganja's own catalog
//!   names for whichever provider *this* session selected — `gpt-5`, a
//!   gateway's slug, the fake provider's recorder id — and `claude --model`
//!   names a model that account serves. Passing one as the other is a guess,
//!   and the pane's own default is the honest answer.
//! - **not `--agent-type`.** The same argument about the same words: the
//!   `subagent_type` a `task` call named is a name off *ganja's* agent roster,
//!   and claude's subagent types are claude's.
//!
//! Both refusals mirror [`crate::pane`]'s, which drops the same two
//! flags for the same reason.
//!
//! **The prompt never rides the line** (§4.1 step 5). `argv` is `ps(1)`-visible
//! to every user on the machine and a prompt is a place credentials get
//! pasted; the inbox is neither, and it is the same channel every follow-up
//! arrives on — one ordering, one lock, one audit point.
//!
//! # The environment, plus exactly one name
//!
//! This is the claude half of **D502** (minted in [`crate::pane`], the
//! way [`crate::tmux`]'s own environment note points at the same one
//! mint): the ruling is that backend's, and what follows is the one thing this
//! one adds to it.
//!
//! A tmux pane inherits the **tmux server's** environment (§10.10), so the
//! launch carries [`crate::pane::CARRIED_ENV`] — a closed list of
//! directory names, never a filter over the parent's environment — with
//! [`CONFIG_DIR_ENV`](crate::claude::CONFIG_DIR_ENV) added, and
//! nothing else. That one addition is what keeps
//! the two sides honest: the root this process computed and the root the pane
//! resolves are the same function of the same one variable, so a lead started
//! with a `CLAUDE_CONFIG_DIR` of its own spawns a pane that joins the team it
//! meant rather than the one under `~/.claude`.
//!
//! # Delivery
//!
//! [`Delivery::FireAndForget`](ganja_core::teammate::Delivery::FireAndForget), and not shared with the `ganja` pane: a real
//! `claude` marks a message read when it *reads* it, not when a turn takes it
//! on (§3.1), so there is no consumption signal to wait for. The lead retires
//! such a queue entry at write time; without the split a claude peer's message
//! sits pending in the lead's UI forever.

use std::ffi::OsString;
use std::sync::Arc;

use async_trait::async_trait;
// Where a real `claude` keeps its teams is core's own answer since D538: the
// lead's inbox pass and the engine's prune need it whether or not this backend
// ever runs. Re-exported under the names this module has always spelled them
// with, so nothing that reads them through `teammate::claude` had to move.
pub use ganja_core::teammate::{
    CLAUDE_CONFIG_DIR_ENV as CONFIG_DIR_ENV, CLAUDE_CONFIG_HOME_DIRECTORY as CONFIG_HOME_DIRECTORY,
    REFUSED_NO_CONFIG_DIR, TEAMS_DIR as TEAMS_DIRECTORY, teams_root,
};
use ganja_core::teammate::{Delivery, Lent, SpawnSpec, Spawned, TeammateBackend, Unsupported};
use ganja_protocol::team::MemberBackend;
use ganja_team::{Surface, TeamsRoot, mailbox};
use tokio::task::JoinHandle;

// `shim::on_path` is the `PATH` walk this module used to own: it moved to the
// shim core in P27, where four backends now need it, so that two of them cannot
// come to disagree about what counts as a runnable teammate binary.
use crate::pane::{self, PaneMember, PaneShare, PaneShell};
use crate::shim::on_path;
use crate::tmux::{self, Server, TmuxError};

/// The binary a `claude` pane runs, resolved on `PATH` — see the module doc for
/// why this one and not `current_exe()`.
pub const BINARY: &str = "claude";

/// §4.1's optional flag for a teammate that must start in plan mode.
pub const PLAN_MODE_REQUIRED: &str = "--plan-mode-required";

/// What a claude spawn says when the binary is not on `PATH`.
///
/// Names the binary and the variable, because between them they are the whole
/// of what somebody reading this can act on.
pub const REFUSED_NO_BINARY: &str = "no `claude` on PATH, and ganja will not guess at where one \
     might be; install it, put it on this session's PATH, or spawn this teammate in-process";

/// The environment a `claude` pane is started with, by name (**D502**).
///
/// The `ganja` pane's closed list plus [`CONFIG_DIR_ENV`], for the reason in
/// the module doc, and still a **list of names** rather than a filter: what is
/// not spelled here does not travel, however harmless it looks.
#[must_use]
pub fn carried_env() -> Vec<&'static str> {
    pane::CARRIED_ENV.iter().copied().chain([CONFIG_DIR_ENV]).collect()
}

/// What the teammate is told before its task, in ganja's own words (**D497**).
///
/// §5.5.1 is the whole reason this exists: `"main"` names the sender's own
/// parent conversation, and a pane-backed teammate has none — it *is* the main
/// conversation of its own session — so a worker preamble that says "answer
/// `main`" is broken for exactly the backend being spawned here. The lead is
/// therefore named, and `main` is named as the thing that will not work, since
/// a teammate that has read the habit somewhere else needs to be told it is
/// wrong rather than merely not told it is right.
///
/// Since **D514** every backend seeds a preamble and this is the `claude`
/// channel of [`ganja_core::teammate::preamble::frame`]: the shape is shared, the
/// paragraph about answering is this backend's own, and the message is byte
/// for byte what it was before the frame existed.
#[must_use]
pub fn preamble(spec: &SpawnSpec) -> String {
    let who = ganja_core::teammate::preamble::Names::of(spec);

    ganja_core::teammate::preamble::frame(
        who,
        &format!(
            "Address the lead by that name — `SendMessage(to: \"{lead}\")`. Do **not** address \
             \"main\": you are the main conversation of your own session, so it has no parent for \
             \"main\" to name and the send fails. Everything after this arrives the same way this \
             did, through your inbox.",
            lead = who.lead,
        ),
        &spec.prompt,
    )
}

/// The arguments a `claude` pane is launched with, after the binary.
///
/// §4.1's five identifying flags in its own order — [`pane::arguments`]'s
/// composition, so the reaper's witness reads one prefix wherever a pane came
/// from — then plan mode, only when this spawn asked for it. What is
/// deliberately absent is in the module doc. Pure, so the composed line is a
/// thing a test can hold in its hand.
#[must_use]
pub fn arguments(spec: &SpawnSpec) -> Vec<OsString> {
    let mut argv = pane::arguments(spec);
    if spec.plan_mode_required {
        argv.push(OsString::from(PLAN_MODE_REQUIRED));
    }

    argv
}

/// The real-`claude` pane backend.
///
/// Carries the shell and the column share for [`crate::pane`]'s
/// reason (**D538**): a frontend resolved them once, and they arrive as that
/// module's own value types rather than as a config.
#[derive(Clone, Debug, Default)]
pub struct ClaudePane {
    shell: PaneShell,
    share: PaneShare,
}

impl ClaudePane {
    /// The backend a frontend assembles, over the shell and share this session
    /// resolved.
    #[must_use]
    pub fn new(shell: PaneShell, share: PaneShare) -> Self {
        Self { shell, share }
    }

    /// A tmux failure as the trait's refusal: this session cannot have the
    /// surface, and here is why. For [`TmuxError::NotHosted`] the reason is
    /// exactly [`tmux::REFUSED_NO_TMUX`], the D501 sentence — the same one
    /// [`crate::pane`] refuses in, because one door must not say two
    /// things about one missing session.
    fn refused(error: &TmuxError) -> Unsupported {
        Unsupported { backend: MemberBackend::Claude, reason: error.to_string() }
    }

    /// Anything else this backend cannot do, in one sentence.
    fn cannot(reason: impl Into<String>) -> Unsupported {
        Unsupported { backend: MemberBackend::Claude, reason: reason.into() }
    }

    /// §4.1 steps 4 and 5, under **claude's** root: the inboxes exist, and the
    /// task — behind [`preamble`] — is in the teammate's.
    ///
    /// The **only** write into that inbox for this spawn, which is what
    /// [`TeammateBackend::owns_inbox`] buys: the registry's own seed is skipped
    /// for this backend, so what a real `claude` reads first is the preamble and
    /// there is no second copy of the prompt anywhere.
    ///
    /// The lead's inbox is seeded too, and that is not tidiness: the pane
    /// answers into it, and an inbox created by whoever writes first is an
    /// inbox two processes can race to create — and the lead *reads* it
    /// ([`ganja_core::teammate::lead_inbox`]), so it has to exist before the pane
    /// can write. Seeding it
    /// here, from the side that already knows the team, makes it exist before
    /// anybody needs it — `mailbox::seed` tolerates one that is already there
    /// (§2.5).
    ///
    /// Answers with what identifies the entry, so a launch that fails after this
    /// can take it back out ([`ganja_core::teammate::unseed_inbox`], under this
    /// backend's own root — the unwind the registry cannot do for it, since it
    /// never wrote the entry and does not know the root it went to).
    async fn seed(spec: &SpawnSpec, root: &TeamsRoot) -> Result<mailbox::Identity, Unsupported> {
        let lead = root.inbox_path(&spec.team, &spec.lead);
        if let Err(reason) = ganja_core::teammate::blocking_io(move || {
            mailbox::seed(&lead).map_err(|error| format!("{}: {error}", lead.display()))
        })
        .await
        {
            return Err(Self::cannot(format!(
                "the teammate's inbox under claude's own teams directory could not be written — \
                 {reason}"
            )));
        }

        ganja_core::teammate::seed_inbox(
            root.inbox_path(&spec.team, &spec.name),
            spec.lead.as_str().to_owned(),
            preamble(spec),
        )
        .await
        .map_err(|error| {
            Self::cannot(format!(
                "the teammate's inbox under claude's own teams directory could not be written — \
                 {error}"
            ))
        })
    }
}

#[async_trait]
impl TeammateBackend for ClaudePane {
    fn backend(&self) -> MemberBackend {
        MemberBackend::Claude
    }

    /// This backend's inbox is under claude's root and holds
    /// [`preamble`]'s message rather than the bare prompt, so the registry must
    /// not write one of its own — see the module doc's "the shared inbox".
    fn owns_inbox(&self) -> bool {
        true
    }

    /// The `claude` channel: a real `claude`'s `SendMessage`, and `main`
    /// named as the address that fails (§5.5.1). Seeded by this backend's own
    /// `ClaudePane::seed` rather than by the registry, because the inbox is
    /// under claude's root — the same function, so the two cannot drift.
    fn preamble(&self, spec: &SpawnSpec) -> String {
        preamble(spec)
    }

    async fn spawn(&self, spec: &SpawnSpec, _lent: Lent) -> Result<Arc<dyn Spawned>, Unsupported> {
        // D501's capability check, at the moment of asking rather than at
        // install: whether there is a server to put a pane in.
        let server = Server::current().map_err(|error| Self::refused(&error))?;
        // Both of these are resolved *before* the pane exists, so a machine
        // that cannot run a `claude` at all makes no window it would then have
        // to unmake; the binary itself is spent in `launch`, which resolves it
        // again at the moment it is typed.
        on_path(BINARY).ok_or_else(|| Self::cannot(REFUSED_NO_BINARY))?;
        teams_root().ok_or_else(|| Self::cannot(REFUSED_NO_CONFIG_DIR))?;

        // §4.1 steps 1 and 3: the surface, holding an idle shell, then the
        // cosmetic title. The environment travels here (D502), through tmux's
        // own door; the launch line comes later, once the record this pane's
        // process reads exists.
        let environment = tmux::environment(carried_env());
        let pane = pane::split_idle_shell(
            &server,
            spec,
            &environment,
            &self.shell,
            self.share,
            MemberBackend::Claude,
            "claude teammate",
        )
        .await?;

        Ok(Arc::new(ClaudeMember {
            pane: PaneMember::new(pane, "claude teammate"),
            spec: spec.clone(),
        }))
    }

    fn delivery(&self) -> Delivery {
        Delivery::FireAndForget
    }
}

/// One real `claude` in a pane of its own.
///
/// [`PaneMember`]'s behaviour throughout — a whole process runs in the pane,
/// so nothing of this session's watches it — with the one thing that is this
/// backend's own: §4.1's steps 4, 5 and 6, run on [`Spawned::launch`] once the
/// registry's record write has happened.
#[derive(Debug)]
struct ClaudeMember {
    pane: PaneMember,
    /// Held because the launch needs it: the flags, the inbox and the root are
    /// all read off the spec at the moment the line is typed.
    spec: SpawnSpec,
}

#[async_trait]
impl Spawned for ClaudeMember {
    fn surface(&self) -> Surface {
        self.pane.surface()
    }

    /// §4.1 steps 4, 5 and 6, in that order, after the record write.
    ///
    /// # Errors
    ///
    /// [`Unsupported`] when the inbox could not be seeded or the launch line
    /// could not be typed. The registry unwinds — the pane killed, the record
    /// taken back out, the name given back — and the one thing it cannot
    /// unwind, this backend's own inbox write, is unwound here.
    async fn launch(&self) -> Result<(), Unsupported> {
        let spec = &self.spec;
        let server = Server::current().map_err(|error| ClaudePane::refused(&error))?;
        let binary = on_path(BINARY).ok_or_else(|| ClaudePane::cannot(REFUSED_NO_BINARY))?;
        let root = teams_root().ok_or_else(|| ClaudePane::cannot(REFUSED_NO_CONFIG_DIR))?;
        // Composed before the seed, so its one refusal — a word no shell
        // quoting can carry — leaves no inbox to unseed.
        let line = tmux::launch_line(&binary, &arguments(spec))
            .map_err(|error| ClaudePane::refused(&error))?;

        // §4.1 steps 4 and 5 before step 6, which is the order that matters:
        // the pane reads its inbox on its way up, so a process launched before
        // the task was in it would either idle or ask what it is for.
        let seeded = ClaudePane::seed(spec, &root).await?;

        if let Err(error) = server.type_line(&self.pane.pane().id, &line).await {
            // The one failing path past the seed, and this backend's to unwind:
            // the registry seeded nothing here and cannot prune what it does not
            // know the root of (`TeammateBackend::owns_inbox`). A prompt left in
            // an inbox nothing will read is the half of a failed spawn still
            // visible tomorrow.
            ganja_core::teammate::unseed_inbox(
                root.inbox_path(&spec.team, &spec.name),
                Some(seeded),
                spec.name.as_str(),
            )
            .await;

            return Err(ClaudePane::refused(&error));
        }
        tracing::info!(
            teammate = spec.name.as_str(),
            pane = self.pane.pane().id,
            "a claude teammate's pane was launched"
        );

        Ok(())
    }

    fn start(self: Arc<Self>) -> Vec<JoinHandle<()>> {
        Vec::new()
    }

    fn alive(&self) -> bool {
        self.pane.alive()
    }

    fn recent(&self) -> Vec<String> {
        self.pane.recent()
    }

    async fn kill(&self) {
        self.pane.kill().await;
    }
}

#[cfg(test)]
#[path = "claude_tests.rs"]
mod tests;
