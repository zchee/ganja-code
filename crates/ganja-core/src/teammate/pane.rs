//! A teammate with a `ganja` pane of its own.
//!
//! Upstream opencode has **no counterpart**; the sequence being ported is
//! Claude Code's §4.1, read step by step against this tree in §10.2, with
//! §10.3's finding that a pane teammate is a resident full `ganja` TUI rather
//! than a headless worker — its own process, its own session id, its own
//! transcript, which is what makes "message a teammate that finished and it
//! resumes with its context" possible.
//!
//! # The sequence, and which step lives where
//!
//! §4.1's six steps are shared between this backend and the registry that
//! calls it, and the split is the registry's to explain
//! ([`crate::teammate::TeammateRegistry`]'s `spawn`): the inbox is made and
//! the prompt written into it *before* this runs, the member record is written
//! *after* this returns with the pane's identity in hand, and a record that
//! cannot be written has the registry call [`TeammateBackend::kill`] on what
//! this made — which is §4.1's failure-cleanup closure in the shape a backend
//! that returns a handle needs. What is this backend's own is the surface and
//! the launch, in §4.1's own order: one `split-window` carrying the working
//! directory and the environment, holding an idle `sh`, answered with the
//! `(pane_id, pid)` pair the record and the reaper identify the pane by; the
//! cosmetic title; and **the launch line typed into that shell only once the
//! member record is on disk** — the record is the first thing the pane's own
//! process reads (its posture, its model), so a launch that ran before it
//! would be a race the pane had to wait out. `exec` on the line keeps the
//! shell's pid, so the pair recorded at the split is still the pane's when
//! `ganja` is what runs in it.
//!
//! **Not `respawn-pane -k`**, though it looks like the tidier way to replace
//! an idle shell with a program: it forks a *new* process into the pane, and
//! `#{pane_pid}` follows it — so the birth recorded at the split, already
//! written into the member record and already held by the registry, would
//! name a pid that no longer exists, and every identity-checked kill from
//! then on (`shutdown_approved`, the reaper) would find a "recycled" pane and
//! leave it alone. Typing `exec` into the shell tmux forked is what keeps the
//! pane's first pid its first pid.
//!
//! **The binary is `current_exe()`, never `PATH`** (§10.10, §10.13). A debug
//! build writes `sessions-dev.db` where a release build writes `sessions.db`,
//! so a pane resolved off `PATH` could be a *different build* joining a
//! *different store* — a teammate whose transcript the lead cannot see, and
//! nothing on either side would say so. The one binary this process can be
//! sure shares its store is the one it is running.
//!
//! **The prompt travels through the mailbox, never on the command line**
//! (§4.1 step 5). `argv` is `ps(1)`-visible to every user on the machine and a
//! prompt is a place credentials get pasted; the inbox is neither, and it is
//! also the *same channel* the task's follow-ups arrive on — one ordering, one
//! lock, one audit point.
//!
//! # D502 — the environment a pane inherits is enumerated, and non-secret
//!
//! A tmux pane inherits the **tmux server's** environment, not the environment
//! of the process that asked for the pane (§10.10). So a lead started as
//! `GANJA_CONFIG_HOME=/tmp/x ganja` in a server that predates that export
//! would spawn a pane reading a *different* config home, joining a *different*
//! team — and reading none of the lead's messages. The earlier draft's rule of
//! "no environment prefix at all" is therefore a functional bug, and the rule
//! that replaces it is **no secrets, not no environment**: the launch carries
//! exactly the names in [`CARRIED_ENV`](crate::teammate::pane::CARRIED_ENV) — where this build keeps its own
//! directories — through tmux's own `-e`, and nothing else. It is a **closed
//! list of names**, never a filter over the parent's environment, so every
//! credential-bearing variable (`*_API_KEY`, `GANJA_SERVER_PASSWORD`, an
//! `ANTHROPIC_BASE_URL` somebody put a token in) is excluded by construction:
//! a new secret in the world costs this list nothing, because it was never
//! consulted. `CLAUDE_CONFIG_DIR` joins the same helper for the claude backend,
//! whose store lives under it ([`crate::teammate::claude`]). What the pane
//! needs beyond these — its provider, its credentials — it resolves the way any
//! `ganja` session does: from the config files under that home, and from the
//! server's environment, which is the shell the person started tmux from.
//!
//! # What the launch line carries, and what it does *not*
//!
//! The five spawn flags — `--agent-id`, `--agent-name`, `--team-name`,
//! `--agent-color`, `--parent-session-id` — beside what tmux is told (`-c`,
//! `-e`), and **`--auto` when, and only when, the spawn asked for bypass**:
//! the lead's spawn ask has already gated that request (§10.11-10, D-5), so
//! what reaches the line is a decision a person made, and the pane starts
//! answering its own dialogs the way an interactive `ganja --auto` does. So the
//! posture splits: `plan_mode_required` from the member record the pane finds
//! by its own name and team, bypass from the line, and forward-to-lead the
//! default when neither says otherwise. Not `--model`: a [`SpawnSpec`] holds
//! the bare model id the lead's turn is asking, and the flag wants
//! `provider/model`, so a line composed here would be a guess about the
//! provider. Not `--agent`: the agent types a `task` call names (`general`,
//! `explore`) are subagent-mode and cannot head a session, and the pane's own
//! roster is the one that gets to say so.

use std::{
    ffi::{OsStr, OsString},
    time::Duration,
};

use async_trait::async_trait;
use ganja_protocol::team::MemberBackend;
use ganja_team::TeamFile;

use crate::{
    config::CONFIG_HOME_ENV,
    teammate::{
        Delivery, Handle, SpawnSpec, TeammateBackend, Unsupported,
        reaper::Pane,
        tmux::{self, Killed, Launch, Server, TmuxError},
    },
};

/// The environment a `ganja` pane is started with, by name (**D502**).
///
/// Where this build keeps its own directories, and nothing else: the config
/// home (the team file and the inbox are under it — a pane reading a different
/// one is a pane on a different team), and the three XDG bases the config
/// home, the data home (the session store, the credential store) and the
/// runtime directory resolve through when the first is unset. A closed list:
/// what is not named here does not travel, however harmless it looks, because
/// the day this list becomes a filter is the day a secret rides it.
pub const CARRIED_ENV: [&str; 4] = [
    CONFIG_HOME_ENV,
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
];

/// The flag carrying §2.2's derived `<name>@<team>` identity.
pub const AGENT_ID: &str = "--agent-id";
/// The flag carrying the teammate's own name — its mailbox's basename.
pub const AGENT_NAME: &str = "--agent-name";
/// The flag carrying the team the pane joins.
pub const TEAM_NAME: &str = "--team-name";
/// The flag carrying §4.3's assigned colour.
pub const AGENT_COLOR: &str = "--agent-color";
/// The flag carrying the lead's session id.
pub const PARENT_SESSION_ID: &str = "--parent-session-id";
/// The bypass flag, in the spelling `ganja` itself takes — appended only when
/// the spawn asked for it and the lead's gate let it through.
const AUTO: &str = "--auto";

/// The shell a fresh pane holds until its launch line arrives, as the argv
/// tmux is given.
///
/// The POSIX one, by absolute path, rather than the person's login shell: the
/// launch line is typed into it a moment after it starts, and a bare `sh`
/// reads it back exactly — no rc file, no line editor, no plugin between the
/// keys and the `exec`. It is gone the moment the line runs.
///
/// **Two words, and the second is load-bearing.** tmux runs a one-word command
/// through the person's login shell (`$SHELL -c <word>`), and *that* shell
/// sources its own startup files before exec'ing — which is exactly how a
/// credential this backend withheld from the pane came back into it, off a
/// `.zshenv` that exports it (measured, 2026-08-17). A command of two or more
/// words is exec'd directly (`tmux(1)`, "executed directly (without `sh -c`)"),
/// so `-s` — read commands from standard input, which for a tty is what `sh`
/// does anyway — is here to make the argv two words long.
pub const SHELL: [&str; 2] = ["/bin/sh", "-s"];

/// How long a pane's shell waits for its member record before the pane is
/// ended: the record is written by the same process a few milliseconds after
/// the split, so this is a bound on a machine in trouble, not a schedule.
const RECORD_WAIT: Duration = Duration::from_secs(5);

/// Waits until the team file names `spec.name` on `pane_id`, polling the
/// document the registry writes.
///
/// # Errors
///
/// A sentence, when `limit` passes without the record: whose, and how long.
async fn wait_for_record(spec: &SpawnSpec, pane_id: &str, limit: Duration) -> Result<(), String> {
    let path = spec.root.config_path(&spec.team);
    let started = tokio::time::Instant::now();
    loop {
        if let Ok(text) = tokio::fs::read_to_string(&path).await
            && let Ok(file) = serde_json::from_str::<TeamFile>(&text)
            && file
                .member(spec.name.as_str())
                .is_some_and(|member| member.tmux_pane_id == pane_id)
        {
            return Ok(());
        }
        if started.elapsed() >= limit {
            return Err(format!(
                "no member record for {} on {pane_id} after {limit:?}",
                spec.name.as_str()
            ));
        }
        tokio::time::sleep(RECORD_POLL).await;
    }
}

/// How often [`wait_for_record`] looks.
const RECORD_POLL: Duration = Duration::from_millis(20);

/// The arguments a `ganja` pane is launched with, after the binary.
///
/// The five spawn flags in §4.1's own order, each with its value from `spec`,
/// then `--auto` iff `spec.bypass` — and nothing else; what is deliberately
/// absent is in the module doc. Pure, so the composed line is a thing a test
/// can hold in its hand: the argv-secrets pin reads it, and the pane's own side
/// parses exactly these spellings.
#[must_use]
pub fn arguments(spec: &SpawnSpec) -> Vec<OsString> {
    let mut argv = identity_flags(spec);
    if spec.bypass {
        argv.push(OsString::from(AUTO));
    }

    argv
}

/// The five identifying flags in §4.1's own order, each with its value from
/// `spec` — the prefix both pane backends' argv open with, so the reaper's
/// witness reads one composition wherever a pane came from.
#[must_use]
pub fn identity_flags(spec: &SpawnSpec) -> Vec<OsString> {
    [
        (AGENT_ID, spec.agent_id()),
        (AGENT_NAME, spec.name.as_str().to_owned()),
        (TEAM_NAME, spec.team.as_str().to_owned()),
        (AGENT_COLOR, spec.color.clone()),
        (PARENT_SESSION_ID, spec.parent_session_id.clone()),
    ]
    .into_iter()
    .flat_map(|(flag, value)| [OsString::from(flag), OsString::from(value)])
    .collect()
}

/// §4.1 step 1 as both pane backends run it: one `split-window` at `spec`'s
/// working directory carrying `environment` (D502's closed lists, each
/// caller's own) and holding [`SHELL`] idle, answered with the identifying
/// pair — then the cosmetic title, warned about rather than failed on.
///
/// `refused_as` is the surface a failing split is refused as, and `whose` the
/// word the log knows the teammate by.
pub(super) async fn split_idle_shell(
    server: &Server,
    spec: &SpawnSpec,
    environment: &[OsString],
    refused_as: MemberBackend,
    whose: &'static str,
) -> Result<Pane, Unsupported> {
    let shell: Vec<OsString> = SHELL.iter().map(OsString::from).collect();
    let pane = server
        .split(Launch {
            cwd: &spec.cwd,
            environment,
            argv: &shell,
        })
        .await
        .map_err(|error| Unsupported {
            backend: refused_as,
            reason: error.to_string(),
        })?;
    tracing::info!(
        teammate = spec.name.as_str(),
        pane = pane.id,
        birth = pane.birth,
        "a pane was split for a {whose}"
    );

    // From here the pane exists and belongs to a teammate; a title that would
    // not stick is a pane without a name on it, not a teammate that did not
    // start. Named rather than swallowed, because a tmux that refuses a
    // cosmetic call is worth a line in the log.
    if let Err(error) = server.title(&pane.id, spec.name.as_str()).await {
        tracing::warn!(
            teammate = spec.name.as_str(),
            pane = pane.id,
            %error,
            "the teammate's pane could not be titled"
        );
    }

    Ok(pane)
}

/// Ends what a pane backend's `spawn` produced, identity-checked, in the four
/// answers both backends log alike: `backend` is the word for the backend
/// asked, `whose` the word for the teammate whose pane it was.
pub(super) async fn kill_pane(handle: &Handle, backend: &'static str, whose: &'static str) {
    let Handle::Pane(pane) = handle else {
        // Named rather than ignored, because a handle of the other shape
        // arriving here would mean a registry had crossed two backends.
        tracing::warn!(
            ?handle,
            "a {backend} backend was asked to end something it did not start"
        );
        return;
    };
    let server = match Server::current() {
        Ok(server) => server,
        Err(error) => {
            // A lead that had a pane to spawn into and now has no `$TMUX` is
            // not a case this build makes: the pane outlives this process
            // either way, and the reaper is what finds it.
            tracing::warn!(pane = pane.id, %error, "a pane could not be ended");
            return;
        }
    };
    match server.kill(pane).await {
        Ok(Killed::Yes) => tracing::info!(pane = pane.id, "a {whose}'s pane was ended"),
        Ok(Killed::AlreadyGone) => {
            tracing::debug!(pane = pane.id, "a {whose}'s pane was already gone");
        }
        Ok(Killed::Recycled) => tracing::warn!(
            pane = pane.id,
            birth = pane.birth,
            "a {whose}'s pane id now names somebody else's pane; left alone"
        ),
        Err(error) => tracing::warn!(pane = pane.id, %error, "a pane could not be ended"),
    }
}

/// The `ganja`-pane backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct GanjaPane;

impl GanjaPane {
    /// A tmux failure as the trait's refusal: this session cannot have the
    /// surface, and here is why. For [`TmuxError::NotHosted`] the reason is
    /// exactly [`tmux::REFUSED_NO_TMUX`], the D501 sentence.
    fn refused(error: &TmuxError) -> Unsupported {
        Unsupported {
            backend: MemberBackend::Pane,
            reason: error.to_string(),
        }
    }

    /// §4.1 step 6: types `line` into the pane's idle shell.
    ///
    /// A launch that cannot be typed is a pane holding a shell nobody will
    /// use, so it is ended here, by identity — the one failure past the split
    /// that this backend can still clean up after itself.
    async fn launch(self, spec: &SpawnSpec, pane: &Pane, line: &OsStr, server: &Server) {
        match server.type_line(&pane.id, line).await {
            Ok(()) => tracing::info!(
                teammate = spec.name.as_str(),
                pane = pane.id,
                "a teammate's pane was launched"
            ),
            Err(error) => {
                tracing::warn!(
                    teammate = spec.name.as_str(),
                    pane = pane.id,
                    %error,
                    "a teammate's launch line could not be typed; the pane is being ended"
                );
                if let Err(error) = server.kill(pane).await {
                    tracing::warn!(pane = pane.id, %error, "a pane could not be ended");
                }
            }
        }
    }
}

#[async_trait]
impl TeammateBackend for GanjaPane {
    fn backend(&self) -> MemberBackend {
        MemberBackend::Pane
    }

    async fn spawn(&self, spec: &SpawnSpec) -> Result<Handle, Unsupported> {
        // D501's capability check, at the moment of asking rather than at
        // install: whether there is a server to put a pane in.
        let server = Server::current().map_err(|error| Self::refused(&error))?;
        // The binary is resolved *now*, before the pane exists, so a process
        // that cannot name itself makes no pane it would then have to unmake.
        let binary = std::env::current_exe().map_err(|error| Unsupported {
            backend: MemberBackend::Pane,
            reason: format!("this build cannot name its own binary to run in the pane: {error}"),
        })?;

        // §4.1 step 1: the surface, holding an idle shell. The environment
        // travels here (D502), through tmux's own door; the launch line comes
        // later, once the record this pane will read exists.
        let environment = tmux::environment(CARRIED_ENV);
        let pane =
            split_idle_shell(&server, spec, &environment, MemberBackend::Pane, "teammate").await?;

        // §4.1 step 6, sequenced after step 2 by watching for step 2 itself:
        // the record is what the pane's process reads first, so the record's
        // arrival on disk is the moment the launch line is typed. Watched
        // rather than called, because the trait this backend implements has
        // no seam after the registry's record write; the wait is bounded, and
        // a record that never comes is a spawn the registry has already
        // unwound — its own kill takes the idle shell with it, and the
        // identity-checked kill below is what keeps a timed-out watcher from
        // ending a pane that has since been reissued.
        //
        // The gap this shape leaves, stated rather than hidden: a launch that
        // fails *after* the record was written — tmux gone between the split
        // and the send-keys — ends the pane but cannot unregister the member,
        // because nothing here can reach the registry back. What bounds it is
        // this wave's own reaper: a record over a pane that is not there is
        // exactly what it drops at the next lead's startup (D506). And the
        // task is detached from the registry's shutdown: a lead that dies
        // inside the wait leaves a pane holding an idle shell that no record
        // ever named — invisible to a reaper that walks the team file — until
        // a person closes it. Moving the send-keys into a
        // `TeammateBackend::launch` the registry calls after its record write,
        // with the registry's own unwind, is the follow-up that closes both
        // (bead ganja-code-ipg); the body here is that method's body already.
        let handle = Handle::Pane(pane.clone());
        let watched = Self;
        let owned = spec.clone();
        let line = tmux::launch_line(&binary, &arguments(spec));
        tokio::spawn(async move {
            match wait_for_record(&owned, &pane.id, RECORD_WAIT).await {
                Ok(()) => watched.launch(&owned, &pane, &line, &server).await,
                Err(reason) => {
                    tracing::warn!(
                        teammate = owned.name.as_str(),
                        pane = pane.id,
                        %reason,
                        "a pane teammate's record never arrived; the pane is being ended"
                    );
                    match server.kill(&pane).await {
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(pane = pane.id, %error, "a pane could not be ended")
                        }
                    }
                }
            }
        });

        Ok(handle)
    }

    async fn kill(&self, handle: &Handle) {
        kill_pane(handle, "pane", "teammate").await;
    }

    fn delivery(&self) -> Delivery {
        Delivery::Acknowledged
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ganja_protocol::team::MemberBackend;
    use ganja_team::{MemberName, TeamName, TeamsRoot};

    use super::{CARRIED_ENV, arguments};
    use crate::teammate::SpawnSpec;

    /// A spawn with every field a launch could be tempted to put on the line.
    fn spec() -> SpawnSpec {
        SpawnSpec {
            name: MemberName::parse("worker").expect("a member name"),
            team: TeamName::parse("session-abcd1234").expect("a team name"),
            lead: MemberName::lead(),
            root: TeamsRoot::new("/nowhere/teams"),
            backend: MemberBackend::Pane,
            agent_type: "general".to_owned(),
            model: "recorder-model".to_owned(),
            color: "blue".to_owned(),
            prompt: "sk-ant-CANARY-a-prompt-is-not-argv".to_owned(),
            cwd: PathBuf::from("/nowhere/project"),
            plan_mode_required: true,
            bypass: true,
            parent_session_id: "01998ad0-0000-7000-8000-000000000000".to_owned(),
        }
    }

    /// The five flags, in §4.1's order, each with its value — and **only**
    /// those: the prompt is not on the line, and neither are the model, the
    /// agent type or the plan posture, for the reasons in the module doc.
    /// Bypass is the one posture that rides the line, and only when asked.
    #[test]
    fn the_launch_line_is_the_five_spawn_flags_and_auto_only_when_bypass_was_asked() {
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
        let strings = |argv: Vec<std::ffi::OsString>| -> Vec<String> {
            argv.into_iter()
                .map(|argument| argument.into_string().expect("ascii"))
                .collect()
        };

        let plain = SpawnSpec {
            bypass: false,
            ..spec()
        };
        assert_eq!(strings(arguments(&plain)), five);

        let bypassing = strings(arguments(&spec()));
        assert_eq!(bypassing[..five.len()], five);
        assert_eq!(bypassing[five.len()..], ["--auto"]);

        let line = bypassing.join(" ");
        assert!(
            !line.contains("CANARY"),
            "the prompt rides the mailbox: {line}"
        );
        assert!(!line.contains("recorder-model"), "no model guess: {line}");
        assert!(!line.contains("general"), "no agent flag: {line}");
        assert!(!line.contains("plan"), "no plan-mode flag: {line}");
    }

    /// The closed list holds directory names and never a credential's.
    #[test]
    fn no_credential_name_is_in_the_carried_environment() {
        for name in CARRIED_ENV {
            assert!(
                !name.contains("KEY") && !name.contains("PASSWORD") && !name.contains("TOKEN"),
                "{name} has no business on a pane's launch"
            );
        }
    }
}
