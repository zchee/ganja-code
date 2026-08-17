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
//! sits *there* rather than in `spawn`. That ordering is not a compromise, it
//! is §4.1's own: surface (1), member record (2), pane title (3),
//! `ensureInboxDirectory` (4), the prompt written into the inbox (5), the
//! launch line (6). The `ganja` pane watches for its record because its body
//! predates the hook; this one uses the hook, so a launch that cannot be made
//! is unwound by the registry — the pane killed, the record taken back out,
//! the name given back — instead of leaving a member holding an idle shell.
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
//! # The shared inbox
//!
//! A round trip needs *one* directory: this backend seeds
//! `$CLAUDE_CONFIG_DIR/teams/<team>/inboxes/<name>.json` and the pane answers
//! into `…/inboxes/<lead>.json` beside it, so a lead reading its own
//! `<config home>/teams` sees nothing. That is stated rather than hidden: it is
//! why [`teams_root`](crate::teammate::claude::teams_root) is public, and it is
//! what AC-13's live test means by *the shared inbox* — it points the lead's own
//! [`ganja_team::TeamsRoot`] at what that function answers, which is the one
//! configuration in which a `claude` teammate and a `ganja` lead are members of
//! the same team.
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
//! # D502 — the environment, plus exactly one name
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

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use ganja_protocol::team::MemberBackend;
use ganja_team::{MailboxMessage, TeamsRoot, mailbox, record};

use crate::teammate::{
    Delivery, Handle, SpawnSpec, TeammateBackend, Unsupported,
    pane::{self, AGENT_COLOR, AGENT_ID, AGENT_NAME, PARENT_SESSION_ID, TEAM_NAME},
    reaper::Pane,
    tmux::{self, Killed, Launch, Server, TmuxError},
};

/// The variable naming the directory a real `claude` keeps its own things in,
/// and therefore the parent of the teams directory it reads (§2.1).
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
    let mut argv: Vec<OsString> = [
        (AGENT_ID, spec.agent_id()),
        (AGENT_NAME, spec.name.as_str().to_owned()),
        (TEAM_NAME, spec.team.as_str().to_owned()),
        (AGENT_COLOR, spec.color.clone()),
        (PARENT_SESSION_ID, spec.parent_session_id.clone()),
    ]
    .into_iter()
    .flat_map(|(flag, value)| [OsString::from(flag), OsString::from(value)])
    .collect();
    if spec.plan_mode_required {
        argv.push(OsString::from(PLAN_MODE_REQUIRED));
    }
    if spec.bypass {
        argv.push(OsString::from(PERMISSION_MODE));
        argv.push(OsString::from(BYPASS_PERMISSIONS));
    }

    argv
}

/// The line typed into the pane's shell: `exec` the binary with
/// [`arguments`], every word quoted for `sh`.
///
/// `exec`, so the shell is replaced rather than parented — the pane's process
/// keeps the pid tmux forked, which is the `birth` half of its recorded
/// identity and what an identity-checked kill compares against. Composed here
/// rather than shared with [`crate::teammate::pane`]'s: the two argv differ,
/// and the only thing they have in common is [`tmux::shell_quote`], which is
/// where the quoting rule already lives.
#[must_use]
pub fn launch_line(binary: &Path, spec: &SpawnSpec) -> OsString {
    let mut line = OsString::from("exec ");
    line.push(tmux::shell_quote(binary.as_os_str()));
    for argument in arguments(spec) {
        line.push(" ");
        line.push(tmux::shell_quote(&argument));
    }

    line
}

/// `binary` as `PATH` resolves it, or [`None`].
///
/// Hand-written rather than reached for: the whole of it is "the first entry of
/// `PATH` holding an executable file by that name", and a crate for that would
/// be a dependency carrying a `which` implementation nobody here would read.
fn on_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    std::env::split_paths(&path)
        // An empty `PATH` entry means the working directory to a shell, and a
        // binary picked up out of whatever directory a turn happens to be in
        // is not a binary anybody asked for.
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| directory.join(binary))
        .find(|candidate| executable(candidate))
}

/// Whether `path` is a file somebody could run — following links, because a
/// `PATH` entry is very often one.
fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
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
    /// The lead's inbox is seeded too, and that is not tidiness: the pane
    /// answers into it, and an inbox created by whoever writes first is an
    /// inbox two processes can race to create. Seeding it here, from the side
    /// that already knows the team, makes it exist before anybody needs it —
    /// `mailbox::seed` tolerates one that is already there (§2.5).
    async fn seed(spec: &SpawnSpec, root: &TeamsRoot) -> Result<(), Unsupported> {
        let inbox = root.inbox_path(&spec.team, &spec.name);
        let lead = root.inbox_path(&spec.team, &spec.lead);
        let message =
            MailboxMessage::new(spec.lead.as_str(), preamble(spec), record::now_iso8601());

        let seeding = tokio::task::spawn_blocking(move || -> Result<(), String> {
            for path in [&lead, &inbox] {
                mailbox::seed(path).map_err(|error| format!("{}: {error}", path.display()))?;
            }
            mailbox::write(&inbox, message)
                .map(|_| ())
                .map_err(|error| format!("{}: {error}", inbox.display()))
        })
        .await;

        match seeding {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => Err(Self::cannot(format!(
                "the teammate's inbox under claude's own teams directory could not be written — \
                 {reason}"
            ))),
            Err(error) => Err(Self::cannot(format!(
                "the write of the teammate's inbox was lost: {error}"
            ))),
        }
    }
}

#[async_trait]
impl TeammateBackend for ClaudePane {
    fn backend(&self) -> MemberBackend {
        MemberBackend::Claude
    }

    async fn spawn(&self, spec: &SpawnSpec) -> Result<Handle, Unsupported> {
        // D501's capability check, at the moment of asking rather than at
        // install: whether there is a server to put a pane in.
        let server = Server::current().map_err(|error| Self::refused(&error))?;
        // Both of these are resolved *before* the pane exists, so a machine
        // that cannot run a `claude` at all makes no window it would then have
        // to unmake.
        let binary = on_path(BINARY).ok_or_else(|| Self::cannot(REFUSED_NO_BINARY))?;
        teams_root().ok_or_else(|| Self::cannot(REFUSED_NO_CONFIG_DIR))?;

        // §4.1 step 1: the surface, holding an idle shell. The environment
        // travels here (D502), through tmux's own door; the launch line comes
        // later, once the record this pane's process reads exists.
        let environment = tmux::environment(carried_env());
        let shell: Vec<OsString> = pane::SHELL.iter().map(OsString::from).collect();
        let pane = server
            .split(Launch {
                cwd: &spec.cwd,
                environment: &environment,
                argv: &shell,
            })
            .await
            .map_err(|error| Self::refused(&error))?;
        tracing::info!(
            teammate = spec.name.as_str(),
            pane = pane.id,
            birth = pane.birth,
            binary = %binary.display(),
            "a pane was split for a claude teammate"
        );

        // §4.1 step 3, cosmetic and treated as such by every caller: a title
        // that would not stick is a pane without a name on it, not a teammate
        // that did not start. Named rather than swallowed, because a tmux that
        // refuses a cosmetic call is worth a line in the log.
        if let Err(error) = server.title(&pane.id, spec.name.as_str()).await {
            tracing::warn!(
                teammate = spec.name.as_str(),
                pane = pane.id,
                %error,
                "the teammate's pane could not be titled"
            );
        }

        Ok(Handle::Pane {
            pane_id: pane.id,
            birth: pane.birth,
        })
    }

    async fn launch(&self, spec: &SpawnSpec, handle: &Handle) -> Result<(), Unsupported> {
        let Some(pane) = Pane::of(handle) else {
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

        // §4.1 steps 4 and 5 before step 6, which is the order that matters:
        // the pane reads its inbox on its way up, so a process launched before
        // the task was in it would either idle or ask what it is for.
        Self::seed(spec, &root).await?;

        let line = launch_line(&binary, spec);
        server
            .type_line(&pane.id, &line)
            .await
            .map_err(|error| Self::refused(&error))?;
        tracing::info!(
            teammate = spec.name.as_str(),
            pane = pane.id,
            "a claude teammate's pane was launched"
        );

        Ok(())
    }

    async fn kill(&self, handle: &Handle) {
        let Some(pane) = Pane::of(handle) else {
            tracing::warn!(
                ?handle,
                "a claude backend was asked to end something it did not start"
            );
            return;
        };
        let server = match Server::current() {
            Ok(server) => server,
            Err(error) => {
                // A lead that had a pane to spawn into and now has no `$TMUX`
                // is not a case this build makes: the pane outlives this
                // process either way, and the reaper is what finds it.
                tracing::warn!(pane = pane.id, %error, "a pane could not be ended");
                return;
            }
        };
        match server.kill(&pane).await {
            Ok(Killed::Yes) => tracing::info!(pane = pane.id, "a claude teammate's pane was ended"),
            Ok(Killed::AlreadyGone) => {
                tracing::debug!(pane = pane.id, "a claude teammate's pane was already gone");
            }
            Ok(Killed::Recycled) => tracing::warn!(
                pane = pane.id,
                birth = pane.birth,
                "a claude teammate's pane id now names somebody else's pane; left alone"
            ),
            Err(error) => tracing::warn!(pane = pane.id, %error, "a pane could not be ended"),
        }
    }

    fn delivery(&self) -> Delivery {
        Delivery::FireAndForget
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use ganja_protocol::team::MemberBackend;
    use ganja_team::{MemberName, TeamName, TeamsRoot};

    use super::{
        BYPASS_PERMISSIONS, ClaudePane, PERMISSION_MODE, PLAN_MODE_REQUIRED, TEAMS_DIRECTORY,
        arguments, carried_env, launch_line, preamble, root_under,
    };
    use crate::teammate::{
        Delivery, SpawnSpec, TeammateBackend as _,
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

    /// The composed line is the binary and then at least one word, which is the
    /// §10.10 hazard tmux itself creates: a *one*-word command goes through the
    /// person's login shell, and a `.zshenv` that exports credentials would put
    /// straight back what D502 carefully withheld.
    #[test]
    fn the_launch_is_never_one_word() {
        let line = launch_line(&PathBuf::from("/usr/local/bin/claude"), &spec())
            .into_string()
            .expect("ascii");
        assert!(line.starts_with("exec "), "{line}");
        assert!(
            line.split_whitespace().count() >= 2,
            "a one-word command would be re-read by a login shell: {line}"
        );
        assert!(line.contains("'/usr/local/bin/claude'"), "quoted: {line}");
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

    /// Handing the message over is all there is to see: a real `claude` marks a
    /// message read when it reads it, so there is no consumption to wait for.
    #[test]
    fn a_claude_pane_can_only_report_that_it_handed_a_message_over() {
        assert_eq!(ClaudePane.delivery(), Delivery::FireAndForget);
        assert_eq!(ClaudePane.backend(), MemberBackend::Claude);
    }
}
