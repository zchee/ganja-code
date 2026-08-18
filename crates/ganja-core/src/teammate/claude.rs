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
//! [`crate::teammate::pane`]'s are: [`TeammateBackend::spawn`] makes the
//! surface, the registry writes the member record, and
//! [`TeammateBackend::launch`] runs afterwards — which is why the inbox work
//! sits *there* rather than in `spawn`. That ordering is §4.1's own —
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
//!   what D-1 buys. [`teams_root`](crate::teammate::claude::teams_root) is
//!   that value, and it is public because the lead side needs it too: a lead
//!   that polls only its own root never sees what a `claude` pane wrote. See
//!   "the shared inbox" below.
//! - **the binary**. `claude` is resolved on `PATH`, deliberately unlike the
//!   `ganja` pane's `current_exe()`: §10.10's rule exists because a
//!   PATH-resolved *ganja* could be a different build joining a different
//!   session store, and this process has no `claude` build to be consistent
//!   with. It is resolved **before** the pane is split, so a machine without
//!   the binary gets one sentence instead of a window that closes.
//! - **the preamble**. §5.5.1: `"main"` names *the sender's own parent
//!   conversation*, and a pane-backed teammate is the main conversation of its
//!   own session — so it has no parent and a send to `main` fails. The seeded
//!   message therefore names the lead, in ganja's own words (**D497**: no
//!   Claude Code prose is copied here).
//!
//! # The shared inbox, and the three things that follow from two roots
//!
//! A round trip needs *one* directory: this backend seeds
//! `$CLAUDE_CONFIG_DIR/teams/<team>/inboxes/<name>.json` and the pane answers
//! into `…/inboxes/<lead>.json` beside it. Three consequences, all of them
//! stated rather than hidden — the first two are why
//! [`teams_root`](crate::teammate::claude::teams_root) is public.
//!
//! 1. **The lead has to read that root too.** A lead polling only
//!    `<ganja config home>/teams` would never see the answer, because the answer
//!    is in the other directory. So it reads both:
//!    [`crate::teammate::lead_inbox::LeadInbox`] adds the claude root's
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
//!    ([`crate::teammate::TeammateBackend::owns_inbox`]): with the roots
//!    collapsed, its bare-prompt message landed *ahead* of
//!    [`preamble`](crate::teammate::claude::preamble) in the same file and a real
//!    `claude` read the one message that does not name its lead; with the roots
//!    apart, that copy rotted under the ganja root where nothing reads. So this
//!    backend writes the teammate's inbox, and it prunes its own write when its
//!    launch line cannot be typed.
//! 3. **The member record is written under the ganja root only.** The team file
//!    is [`crate::teammate::TeammateRegistry`]'s own document, written where that
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
//! §4.1's own flags: the five that identify the teammate, `--plan-mode-required`
//! when the spawn asked for plan mode, and `--permission-mode bypassPermissions`
//! when it asked for bypass — the spelling the reference records twice, once as
//! the `permissionMode: "bypassPermissions"` a real pane's transcript opens
//! with (§4.1) and once as the flag a Claude-compatible CLI advertises
//! (§10.7). What is deliberately absent is §4.1's other two optionals:
//!
//! - **not `--model`.** A [`SpawnSpec`]'s model is the id ganja's own catalog
//!   names for whichever provider *this* session selected — `gpt-5`, a
//!   gateway's slug, the fake provider's recorder id — and `claude --model`
//!   names a model that account serves. Passing one as the other is a guess,
//!   and the pane's own default is the honest answer.
//! - **not `--agent-type`.** The same argument about the same words: the
//!   `subagent_type` a `task` call named is a name off *ganja's* agent roster,
//!   and claude's subagent types are claude's.
//!
//! Both refusals mirror [`crate::teammate::pane`]'s, which drops the same two
//! flags for the same reason.
//!
//! **The prompt never rides the line** (§4.1 step 5). `argv` is `ps(1)`-visible
//! to every user on the machine and a prompt is a place credentials get
//! pasted; the inbox is neither, and it is the same channel every follow-up
//! arrives on — one ordering, one lock, one audit point.
//!
//! # The environment, plus exactly one name
//!
//! This is the claude half of **D502** (minted in [`crate::teammate::pane`], the
//! way [`crate::teammate::tmux`]'s own environment note points at the same one
//! mint): the ruling is that backend's, and what follows is the one thing this
//! one adds to it.
//!
//! A tmux pane inherits the **tmux server's** environment (§10.10), so the
//! launch carries [`crate::teammate::pane::CARRIED_ENV`] — a closed list of
//! directory names, never a filter over the parent's environment — with
//! [`CONFIG_DIR_ENV`](crate::teammate::claude::CONFIG_DIR_ENV) added, and
//! nothing else. That one addition is what keeps
//! the two sides honest: the root this process computed and the root the pane
//! resolves are the same function of the same one variable, so a lead started
//! with a `CLAUDE_CONFIG_DIR` of its own spawns a pane that joins the team it
//! meant rather than the one under `~/.claude`.
//!
//! # Delivery
//!
//! [`Delivery::FireAndForget`], and not shared with the `ganja` pane: a real
//! `claude` marks a message read when it *reads* it, not when a turn takes it
//! on (§3.1), so there is no consumption signal to wait for. The lead retires
//! such a queue entry at write time; without the split a claude peer's message
//! sits pending in the lead's UI forever.

use std::{ffi::OsString, path::PathBuf};

use async_trait::async_trait;
use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use ganja_protocol::team::MemberBackend;
use ganja_team::{TeamsRoot, mailbox};

use crate::teammate::{
    Delivery, Handle, SpawnSpec, TeammateBackend, Unsupported, pane,
    tmux::{self, Server, TmuxError},
};

/// The variable naming the directory a real `claude` keeps its own things in,
/// and therefore the parent of the teams directory it reads (§2.1).
///
/// It reaches further than the teams directory, and a caller that sets one for
/// a session should know it: a real `claude` derives the identity of its
/// **credential store** from this path too — on macOS the keychain service is
/// `Claude Code-credentials` under the default home and
/// `Claude Code-credentials-<eight hex of the path>` under any other — which is
/// how one variable serves several accounts. Nothing here needs to act on that,
/// because a pane under the user's own config home reads the store that user
/// logged into; it is recorded because a *fresh* config home is a fresh login,
/// and a pane that starts, reads its inbox and then refuses to take a turn looks
/// nothing like an authentication problem until somebody knows this.
pub const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Where a `claude` with no [`CONFIG_DIR_ENV`] keeps them, under the user's
/// home.
///
/// **This plan's assumption, not the reference's**: §2.1 spells the root as
/// `$CLAUDE_CONFIG_DIR/teams` and never says what an unset variable falls back
/// to. It is recorded as a constant so that being wrong costs one line, and so
/// that a reader can see it is a guess rather than a citation.
pub const CONFIG_HOME_DIRECTORY: &str = ".claude";

/// The directory holding teams under a config home (§2.1).
pub const TEAMS_DIRECTORY: &str = "teams";

/// The binary a `claude` pane runs, resolved on `PATH` — see the module doc for
/// why this one and not `current_exe()`.
pub const BINARY: &str = "claude";

/// §4.1's optional flag for a teammate that must start in plan mode.
pub const PLAN_MODE_REQUIRED: &str = "--plan-mode-required";

/// §4.1's permission-mode flag.
pub const PERMISSION_MODE: &str = "--permission-mode";

/// The permission mode a bypassing spawn asks for — the value a real pane's own
/// transcript opens with (§4.1 `[OBS]`).
pub const BYPASS_PERMISSIONS: &str = "bypassPermissions";

/// What a claude spawn says when the binary is not on `PATH`.
///
/// Names the binary and the variable, because between them they are the whole
/// of what somebody reading this can act on.
pub const REFUSED_NO_BINARY: &str = "no `claude` on PATH, and ganja will not guess at where one \
     might be; install it, put it on this session's PATH, or spawn this teammate in-process";

/// What a claude spawn says when there is no directory to put the team in.
pub const REFUSED_NO_CONFIG_DIR: &str = "there is no directory to reach claude's teams through: neither CLAUDE_CONFIG_DIR nor a home \
     directory could be resolved";

/// Where a real `claude` reads and writes its teams (§2.1).
///
/// `$CLAUDE_CONFIG_DIR/teams`, else `~/.claude/teams`. Public because the two
/// sides of a round trip have to agree about it and only one of them is this
/// backend: a lead that wants to hear from a `claude` teammate reads the team
/// under what this answers. [`None`] when neither the variable nor a home
/// directory can be had, which is what [`REFUSED_NO_CONFIG_DIR`] says out loud.
#[must_use]
pub fn teams_root() -> Option<TeamsRoot> {
    // The home comes off the same strategy `config::config_home` asks, rather
    // than off `$HOME` directly: one answer about where this machine's home is,
    // whichever of the two directories is being resolved.
    let home = Xdg::new().ok().map(|base| base.home_dir().to_path_buf());

    root_under(std::env::var_os(CONFIG_DIR_ENV), home)
}

/// [`teams_root`]'s decision, over values rather than over the environment, so
/// a test can hold both cases without touching the process it runs in.
///
/// An empty variable is treated as unset — the shape every other environment
/// read in this tree keeps (`config_home`'s `CONFIG_HOME_ENV`), because
/// `CLAUDE_CONFIG_DIR=` in a shell profile means "I did not set this" far more
/// often than it means "the root directory".
fn root_under(config_dir: Option<OsString>, home: Option<PathBuf>) -> Option<TeamsRoot> {
    let named = config_dir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);

    named
        .or_else(|| home.map(|home| home.join(CONFIG_HOME_DIRECTORY)))
        .map(|home| TeamsRoot::new(home.join(TEAMS_DIRECTORY)))
}

/// The environment a `claude` pane is started with, by name (**D502**).
///
/// The `ganja` pane's closed list plus [`CONFIG_DIR_ENV`], for the reason in
/// the module doc, and still a **list of names** rather than a filter: what is
/// not spelled here does not travel, however harmless it looks.
#[must_use]
pub fn carried_env() -> Vec<&'static str> {
    pane::CARRIED_ENV
        .iter()
        .copied()
        .chain([CONFIG_DIR_ENV])
        .collect()
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
#[must_use]
pub fn preamble(spec: &SpawnSpec) -> String {
    format!(
        "You are {name}, a teammate on the team {team}. Your lead is {lead}.\n\n\
         Address the lead by that name — `SendMessage(to: \"{lead}\")`. Do **not** address \
         \"main\": you are the main conversation of your own session, so it has no parent for \
         \"main\" to name and the send fails. Everything after this arrives the same way this \
         did, through your inbox.\n\n\
         Your task:\n\n{prompt}",
        name = spec.name.as_str(),
        team = spec.team.as_str(),
        lead = spec.lead.as_str(),
        prompt = spec.prompt,
    )
}

/// The arguments a `claude` pane is launched with, after the binary.
///
/// §4.1's five identifying flags in its own order, then its two postures —
/// plan mode and bypass — each only when this spawn asked for it. What is
/// deliberately absent is in the module doc. Pure, so the composed line is a
/// thing a test can hold in its hand.
#[must_use]
pub fn arguments(spec: &SpawnSpec) -> Vec<OsString> {
    let mut argv = pane::identity_flags(spec);
    if spec.plan_mode_required {
        argv.push(OsString::from(PLAN_MODE_REQUIRED));
    }
    if spec.bypass {
        argv.push(OsString::from(PERMISSION_MODE));
        argv.push(OsString::from(BYPASS_PERMISSIONS));
    }

    argv
}

/// `binary` as `PATH` resolves it for this process, or [`None`].
///
/// `which` asks the operating system whether this process may execute each
/// candidate, which is the question the later spawn needs answered. The old
/// mode-bit walk instead asked whether somebody could execute the file, so a
/// binary executable only by another owner was reported as runnable and failed
/// later with `EACCES` instead of [`REFUSED_NO_BINARY`].
fn on_path(binary: &str) -> Option<PathBuf> {
    resolve(&std::env::var_os("PATH")?, binary)
}

/// [`on_path`]'s decision over an explicit path list.
///
/// The same split [`teams_root`] and [`root_under`] keep, for the same reason: a
/// test can hold a `PATH` of its own without mutating the process it runs in,
/// which is what would otherwise cost this one function its own test binary.
/// Empty and relative components are removed before `which` sees the list
/// because its Unix behavior follows `which(1)` and can resolve them against
/// the working directory, while this backend refuses to discover a teammate
/// binary from a turn's incidental directory.
fn resolve(path: &std::ffi::OsStr, binary: &str) -> Option<PathBuf> {
    let mut directories = std::env::split_paths(path)
        .filter(|directory| !directory.as_os_str().is_empty() && directory.is_absolute())
        .peekable();
    directories.peek()?;
    let search_path = std::env::join_paths(directories).ok()?;

    which::which_in_global(binary, Some(search_path))
        .ok()?
        .next()
}

/// The real-`claude` pane backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudePane;

impl ClaudePane {
    /// A tmux failure as the trait's refusal: this session cannot have the
    /// surface, and here is why. For [`TmuxError::NotHosted`] the reason is
    /// exactly [`tmux::REFUSED_NO_TMUX`], the D501 sentence — the same one
    /// [`crate::teammate::pane`] refuses in, because one door must not say two
    /// things about one missing session.
    fn refused(error: &TmuxError) -> Unsupported {
        Unsupported {
            backend: MemberBackend::Claude,
            reason: error.to_string(),
        }
    }

    /// Anything else this backend cannot do, in one sentence.
    fn cannot(reason: impl Into<String>) -> Unsupported {
        Unsupported {
            backend: MemberBackend::Claude,
            reason: reason.into(),
        }
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
    /// ([`crate::teammate::lead_inbox`]), so it has to exist before the pane
    /// can write. Seeding it
    /// here, from the side that already knows the team, makes it exist before
    /// anybody needs it — `mailbox::seed` tolerates one that is already there
    /// (§2.5).
    ///
    /// Answers with what identifies the entry, so a launch that fails after this
    /// can take it back out ([`crate::teammate::unseed_inbox`], under this
    /// backend's own root — the unwind the registry cannot do for it, since it
    /// never wrote the entry and does not know the root it went to).
    async fn seed(spec: &SpawnSpec, root: &TeamsRoot) -> Result<mailbox::Identity, Unsupported> {
        let lead = root.inbox_path(&spec.team, &spec.lead);
        if let Err(reason) = crate::teammate::blocking_io(move || {
            mailbox::seed(&lead).map_err(|error| format!("{}: {error}", lead.display()))
        })
        .await
        {
            return Err(Self::cannot(format!(
                "the teammate's inbox under claude's own teams directory could not be written — \
                 {reason}"
            )));
        }

        crate::teammate::seed_inbox(
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

    async fn spawn(&self, spec: &SpawnSpec) -> Result<Handle, Unsupported> {
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
            MemberBackend::Claude,
            "claude teammate",
        )
        .await?;

        Ok(Handle::Pane(pane))
    }

    async fn launch(&self, spec: &SpawnSpec, handle: &Handle) -> Result<(), Unsupported> {
        let Handle::Pane(pane) = handle else {
            // Not reachable through the registry, which hands back the handle
            // this backend's own `spawn` returned — but a handle of the other
            // shape arriving here would mean a registry had crossed two
            // backends, and that is worth a refusal rather than a silent skip.
            return Err(Self::cannot(
                "this backend was asked to launch something it did not make",
            ));
        };
        let server = Server::current().map_err(|error| Self::refused(&error))?;
        let binary = on_path(BINARY).ok_or_else(|| Self::cannot(REFUSED_NO_BINARY))?;
        let root = teams_root().ok_or_else(|| Self::cannot(REFUSED_NO_CONFIG_DIR))?;
        // Composed before the seed, so its one refusal — a word no shell
        // quoting can carry — leaves no inbox to unseed.
        let line =
            tmux::launch_line(&binary, &arguments(spec)).map_err(|error| Self::refused(&error))?;

        // §4.1 steps 4 and 5 before step 6, which is the order that matters:
        // the pane reads its inbox on its way up, so a process launched before
        // the task was in it would either idle or ask what it is for.
        let seeded = Self::seed(spec, &root).await?;

        if let Err(error) = server.type_line(&pane.id, &line).await {
            // The one failing path past the seed, and this backend's to unwind:
            // the registry seeded nothing here and cannot prune what it does not
            // know the root of (`TeammateBackend::owns_inbox`). A prompt left in
            // an inbox nothing will read is the half of a failed spawn still
            // visible tomorrow.
            crate::teammate::unseed_inbox(
                root.inbox_path(&spec.team, &spec.name),
                Some(seeded),
                spec.name.as_str(),
            )
            .await;

            return Err(Self::refused(&error));
        }
        tracing::info!(
            teammate = spec.name.as_str(),
            pane = pane.id,
            "a claude teammate's pane was launched"
        );

        Ok(())
    }

    async fn kill(&self, handle: &Handle) {
        pane::kill_pane(handle, "claude", "claude teammate").await;
    }

    fn delivery(&self) -> Delivery {
        Delivery::FireAndForget
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use ganja_protocol::team::MemberBackend;
    use ganja_team::{MemberName, TeamName, TeamsRoot, mailbox};

    use super::{
        BINARY, BYPASS_PERMISSIONS, ClaudePane, PERMISSION_MODE, PLAN_MODE_REQUIRED,
        TEAMS_DIRECTORY, arguments, carried_env, preamble, resolve, root_under,
    };
    use crate::teammate::{
        SpawnSpec, TeammateBackend as _,
        pane::CARRIED_ENV,
        tmux::{REFUSED_NO_TMUX, TmuxError},
    };

    /// A spawn with every field a launch could be tempted to put on the line,
    /// and a prompt wearing a canary.
    fn spec() -> SpawnSpec {
        SpawnSpec {
            name: MemberName::parse("worker").expect("a member name"),
            team: TeamName::parse("session-abcd1234").expect("a team name"),
            lead: MemberName::lead(),
            root: TeamsRoot::new("/nowhere/teams"),
            backend: MemberBackend::Claude,
            agent_type: "general".to_owned(),
            model: "recorder-model".to_owned(),
            color: "blue".to_owned(),
            prompt: "sk-ant-CANARY-a-prompt-is-not-argv".to_owned(),
            cwd: PathBuf::from("/nowhere/project"),
            plan_mode_required: false,
            bypass: false,
            parent_session_id: "01998ad0-0000-7000-8000-000000000000".to_owned(),
        }
    }

    fn strings(argv: Vec<OsString>) -> Vec<String> {
        argv.into_iter()
            .map(|argument| argument.into_string().expect("ascii"))
            .collect()
    }

    /// §4.1's five, then the two postures only when the spawn asked for them —
    /// and never the prompt, the model or the agent type, for the reasons in
    /// the module doc.
    #[test]
    fn the_launch_line_is_the_spawn_flags_and_the_postures_that_were_asked_for() {
        let five = [
            "--agent-id",
            "worker@session-abcd1234",
            "--agent-name",
            "worker",
            "--team-name",
            "session-abcd1234",
            "--agent-color",
            "blue",
            "--parent-session-id",
            "01998ad0-0000-7000-8000-000000000000",
        ];

        assert_eq!(strings(arguments(&spec())), five);

        let posturing = strings(arguments(&SpawnSpec {
            plan_mode_required: true,
            bypass: true,
            ..spec()
        }));
        assert_eq!(posturing[..five.len()], five);
        assert_eq!(
            posturing[five.len()..],
            [PLAN_MODE_REQUIRED, PERMISSION_MODE, BYPASS_PERMISSIONS]
        );

        let line = posturing.join(" ");
        assert!(
            !line.contains("CANARY"),
            "the prompt rides the mailbox: {line}"
        );
        assert!(!line.contains("recorder-model"), "no model guess: {line}");
        assert!(!line.contains("general"), "no agent-type guess: {line}");
    }

    /// The composed line, as tmux is handed it: `exec`, the binary — bare,
    /// because no byte of that path needs quoting — and never the prompt.
    /// (The one-word login-shell hazard is a property of the *idle* argv,
    /// pinned at `pane::SHELL`; this line is typed with `send-keys -l`,
    /// which no shell re-reads.)
    #[test]
    fn the_composed_line_execs_the_binary_and_the_prompt_stays_off_it() {
        let line = crate::teammate::tmux::launch_line(
            &PathBuf::from("/usr/local/bin/claude"),
            &arguments(&spec()),
        )
        .expect("no NUL rides the spawn flags")
        .into_string()
        .expect("ascii");
        assert!(line.starts_with("exec /usr/local/bin/claude "), "{line}");
        // The canary again, on the *composed* line rather than on `arguments`
        // alone: the line is what tmux is handed and what `ps`
        // would print, so it is the value the §4.1-step-5 rule is really about.
        assert!(
            !line.contains("CANARY"),
            "the prompt rides the mailbox: {line}"
        );
    }

    /// The carried set is the `ganja` pane's closed list plus the one variable
    /// this backend's root is a function of — and still no credential name.
    #[test]
    fn the_carried_environment_adds_the_claude_config_dir_and_nothing_else() {
        let mut expected: Vec<&str> = CARRIED_ENV.to_vec();
        expected.push("CLAUDE_CONFIG_DIR");
        assert_eq!(carried_env(), expected);

        for name in carried_env() {
            assert!(
                !name.contains("KEY") && !name.contains("PASSWORD") && !name.contains("TOKEN"),
                "{name} has no business on a pane's launch"
            );
        }
    }

    /// The root is the variable when there is one, the home when there is not,
    /// and nothing at all when there is neither — with an empty variable read
    /// as unset.
    #[test]
    fn the_teams_root_follows_the_config_dir_and_falls_back_to_the_home() {
        let named = root_under(
            Some(OsString::from("/tmp/claude-home")),
            Some(PathBuf::from("/home/somebody")),
        )
        .expect("a named config dir is a root");
        assert_eq!(
            named.inbox_path(
                &TeamName::parse("session-abcd1234").expect("a team name"),
                &MemberName::lead(),
            ),
            PathBuf::from("/tmp/claude-home")
                .join(TEAMS_DIRECTORY)
                .join("session-abcd1234")
                .join("inboxes")
                .join("team-lead.json")
        );

        let fallen = root_under(None, Some(PathBuf::from("/home/somebody")))
            .expect("a home is a root when the variable is unset");
        assert_eq!(
            fallen.config_path(&TeamName::parse("session-abcd1234").expect("a team name")),
            PathBuf::from("/home/somebody/.claude/teams/session-abcd1234/config.json")
        );

        assert_eq!(
            root_under(Some(OsString::new()), Some(PathBuf::from("/home/somebody"))),
            Some(fallen),
            "an empty variable is unset, not the root directory"
        );
        assert!(root_under(None, None).is_none());
    }

    /// §5.5.1, as the thing a worker actually reads: its lead by name, and
    /// `main` named as the address that will not work.
    #[test]
    fn the_preamble_names_the_lead_and_says_main_is_not_an_address() {
        let seeded = preamble(&spec());
        assert!(seeded.contains("team-lead"), "{seeded}");
        assert!(seeded.contains("main"), "{seeded}");
        assert!(
            seeded.ends_with("sk-ant-CANARY-a-prompt-is-not-argv"),
            "the task is what the message ends with: {seeded}"
        );
    }

    /// A session with no tmux refuses in the sentence AC-16 asserts — the same
    /// one the `ganja` pane refuses in, because one door must not say two
    /// things about one missing session.
    #[test]
    fn a_session_without_tmux_is_refused_in_the_sentence_the_other_pane_uses() {
        let refused = ClaudePane::refused(&TmuxError::NotHosted);
        assert_eq!(refused.backend, MemberBackend::Claude);
        assert_eq!(refused.reason, REFUSED_NO_TMUX);
        assert!(
            refused.to_string().contains("claude"),
            "and still names the surface asked for: {refused}"
        );
    }

    /// The delivery and backend answers are pinned beside the other backends'
    /// in `tests/teammate_backends.rs`; what is this file's alone is the inbox
    /// ownership the registry's seed-skip reads.
    #[test]
    fn a_claude_pane_owns_its_inbox_so_the_registry_must_not_seed_it() {
        assert!(
            ClaudePane.owns_inbox(),
            "the registry must not write a second message into this inbox"
        );
    }

    /// **One** message in the teammate's inbox, and it is the preamble.
    ///
    /// The defect this pins: with the registry seeding too, the bare
    /// prompt landed here first and a real `claude` read the one message that
    /// does not tell it how to address its lead. Drivable without a tmux server
    /// or a `claude` on the machine, because seeding is file work and nothing
    /// else — which is why it had no coverage at all and now does.
    #[tokio::test]
    async fn seeding_leaves_exactly_one_message_and_it_is_the_preamble() {
        let home = tempfile::tempdir().expect("a temporary claude config home");
        let root = TeamsRoot::new(home.path().join(TEAMS_DIRECTORY));
        let spec = spec();

        let seeded = ClaudePane::seed(&spec, &root)
            .await
            .expect("the seed lands");

        let inbox = root.inbox_path(&spec.team, &spec.name);
        let held = mailbox::read(&inbox).expect("the inbox reads").valid;
        assert_eq!(held.len(), 1, "one writer, one message: {held:?}");
        assert_eq!(held[0].from, spec.lead.as_str());
        assert_eq!(held[0].text, preamble(&spec));
        assert!(
            held[0].text.contains("Do **not** address"),
            "and it is the message that says so: {}",
            held[0].text
        );
        assert_eq!(
            mailbox::identity(&held[0]),
            seeded,
            "the identity handed back names the entry that landed"
        );
        // The lead's inbox exists before the pane can answer into it, which is
        // what keeps two processes from racing to create it — and what the
        // lead's own pass over this root reads.
        assert!(
            mailbox::read(&root.inbox_path(&spec.team, &spec.lead))
                .expect("the lead's inbox reads")
                .valid
                .is_empty(),
            "seeded, and empty"
        );
    }

    /// A launch refused after the seed leaves nothing behind — the claude root's
    /// inbox included, which the registry's own unwind cannot reach.
    #[tokio::test]
    async fn a_refused_launch_takes_the_seeded_prompt_back_out() {
        let home = tempfile::tempdir().expect("a temporary claude config home");
        let root = TeamsRoot::new(home.path().join(TEAMS_DIRECTORY));
        let spec = spec();
        let seeded = ClaudePane::seed(&spec, &root)
            .await
            .expect("the seed lands");

        crate::teammate::unseed_inbox(
            root.inbox_path(&spec.team, &spec.name),
            Some(seeded),
            spec.name.as_str(),
        )
        .await;

        let inbox = root.inbox_path(&spec.team, &spec.name);
        assert!(
            mailbox::read(&inbox)
                .expect("the inbox reads")
                .valid
                .is_empty(),
            "a prompt nothing will read does not stay in a mailbox"
        );
        assert!(inbox.exists(), "the inbox itself is left where it was");
    }

    /// The `PATH` search returns the first runnable file, skips directories and
    /// candidates this process cannot execute, and never interprets an empty
    /// entry as the working directory.
    ///
    /// Unix-only because the fixtures use Unix permission classes to establish
    /// which candidate the test process may execute.
    #[cfg(unix)]
    #[test]
    fn the_binary_is_the_first_path_entry_holding_something_runnable() {
        use std::os::unix::fs::PermissionsExt as _;

        let home = tempfile::tempdir().expect("a temporary PATH");
        // A directory by the right name, then a file nobody may run, then the
        // one a resolve should find.
        let shadow = home.path().join("shadow");
        let unrunnable = home.path().join("unrunnable");
        let real = home.path().join("real");
        std::fs::create_dir_all(shadow.join(BINARY)).expect("a directory in the way");
        for directory in [&unrunnable, &real] {
            std::fs::create_dir_all(directory).expect("a PATH entry");
        }
        let decoy = unrunnable.join(BINARY);
        let found = real.join(BINARY);
        for (path, mode) in [(&decoy, 0o644), (&found, 0o755)] {
            std::fs::write(path, "#!/bin/sh\n").expect("a candidate is written");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .expect("its mode is set");
        }

        let path = std::env::join_paths([
            std::path::Path::new(""),
            shadow.as_path(),
            unrunnable.as_path(),
            real.as_path(),
        ])
        .expect("a PATH joins");

        assert_eq!(resolve(&path, BINARY).as_deref(), Some(found.as_path()));
        assert!(
            resolve(std::ffi::OsStr::new(""), BINARY).is_none(),
            "an empty PATH resolves nothing rather than the working directory"
        );

        let shadow_only = std::env::join_paths([shadow.as_path()]).expect("a PATH joins");
        assert!(
            resolve(&shadow_only, BINARY).is_none(),
            "a directory is not a file"
        );

        let unrunnable_only = std::env::join_paths([unrunnable.as_path()]).expect("a PATH joins");
        assert!(
            resolve(&unrunnable_only, BINARY).is_none(),
            "a file this process may not run is skipped"
        );

        let home_only = std::env::join_paths([home.path()]).expect("a PATH joins");
        assert!(resolve(&home_only, "absent").is_none());
    }

    /// An execute bit for another permission class does not make a binary
    /// runnable by the process that owns it.
    #[cfg(unix)]
    #[test]
    fn an_execute_bit_for_another_permission_class_does_not_make_the_binary_runnable() {
        use std::os::unix::fs::PermissionsExt as _;

        // SAFETY: `geteuid` only reads the process credentials and has no
        // memory-safety preconditions.
        if unsafe { libc::geteuid() } == 0 {
            // POSIX gives root special X_OK handling: any execute bit suffices,
            // so this permission-class discriminator does not exist for root.
            return;
        }

        let home = tempfile::tempdir().expect("a temporary PATH");
        let candidate = home.path().join(BINARY);
        std::fs::write(&candidate, "#!/bin/sh\n").expect("a candidate is written");
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o001))
            .expect("only another permission class may execute it");

        let mode = std::fs::metadata(&candidate)
            .expect("the candidate has metadata")
            .permissions()
            .mode();
        assert_ne!(
            mode & 0o111,
            0,
            "the old any-execute-bit check would accept this candidate"
        );

        let path = std::env::join_paths([home.path()]).expect("a PATH joins");
        assert!(
            resolve(&path, BINARY).is_none(),
            "access(2) rejects another class's execute permission for the owner"
        );
    }
}
