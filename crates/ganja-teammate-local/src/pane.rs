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
//! ([`ganja_core::teammate::TeammateRegistry`]'s `spawn`): the inbox is made and
//! the prompt written into it *before* this runs, the member record is written
//! *after* this returns with the pane's identity in hand, and a record that
//! cannot be written has the registry call [`Spawned::kill`](ganja_core::teammate::Spawned::kill) on what this made
//! — which is §4.1's failure-cleanup closure in the shape a backend that hands
//! back a live member needs. What is this backend's own is the surface and
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
//! exactly the names in [`CARRIED_ENV`](crate::pane::CARRIED_ENV) — where this build keeps its own
//! directories — through tmux's own `-e`, and nothing else. It is a **closed
//! list of names**, never a filter over the parent's environment, so every
//! credential-bearing variable (`*_API_KEY`, `GANJA_SERVER_PASSWORD`, an
//! `ANTHROPIC_BASE_URL` somebody put a token in) is excluded by construction:
//! a new secret in the world costs this list nothing, because it was never
//! consulted. `CLAUDE_CONFIG_DIR` joins the same helper for the claude backend,
//! whose store lives under it ([`crate::claude`]). What the pane
//! needs beyond these — its provider, its credentials — it resolves the way any
//! `ganja` session does: from the config files under that home, and from the
//! server's environment, which is the shell the person started tmux from.
//!
//! # What the launch line carries, and what it does *not*
//!
//! The five spawn flags — `--agent-id`, `--agent-name`, `--team-name`,
//! `--agent-color`, `--parent-session-id` — beside what tmux is told (`-c`,
//! `-e`), and nothing about posture at all: `plan_mode_required` reaches the
//! pane through the member record it finds by its own name and team, and its
//! asks forward to the lead, the one posture a spawn has. Until 2026-08-22 the
//! line also carried `--auto` when the spawn had asked for bypass; **D513**
//! retired that axis, so a pane's own `--auto` is now only ever a person's to
//! type (**D479**) and never a lead's to compose. Not `--model`: a [`SpawnSpec`](ganja_core::teammate::SpawnSpec) holds
//! the bare model id the lead's turn is asking, and the flag wants
//! `provider/model`, so a line composed here would be a guess about the
//! provider. Not `--agent`: the agent types a `task` call names (`general`,
//! `explore`) are subagent-mode and cannot head a session, and the pane's own
//! roster is the one that gets to say so.

use std::ffi::{OsStr, OsString};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ganja_core::config::CONFIG_HOME_ENV;
use ganja_core::teammate::{Delivery, Lent, SpawnSpec, Spawned, TeammateBackend, Unsupported};
use ganja_protocol::team::MemberBackend;
use ganja_team::{Surface, TeamFile};
use tokio::task::JoinHandle;

use crate::reaper::Pane;
use crate::tmux::{self, Killed, Launch, Placement, Server, TmuxError};

/// The environment a `ganja` pane is started with, by name (**D502**).
///
/// Where this build keeps its own directories, and nothing else: the config
/// home (the team file and the inbox are under it — a pane reading a different
/// one is a pane on a different team), and the three XDG bases the config
/// home, the data home (the session store, the credential store) and the
/// runtime directory resolve through when the first is unset. A closed list:
/// what is not named here does not travel, however harmless it looks, because
/// the day this list becomes a filter is the day a secret rides it.
pub const CARRIED_ENV: [&str; 4] =
    [CONFIG_HOME_ENV, "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR"];

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
///
/// The **default**: `teammates.shell` names another (**D520**), and
/// [`PaneShell`] is what carries whichever one a spawn got.
pub const SHELL: [&str; 2] = ["/bin/sh", "-s"];

/// The shell a spawn's fresh pane holds until its launch line arrives
/// (**D520**): [`SHELL`] unless `teammates.shell` named one.
///
/// A configured shell keeps [`SHELL`]'s one structural rule — the argv is two
/// words or more, so tmux execs it directly instead of handing one word to
/// the login shell — by appending `-s` to a lone program, the same flag the
/// default carries for the same reason. What it does **not** keep is the
/// default's other property: a shell somebody named runs its own startup
/// files, and what those export enters the pane past the enumerated
/// environment (**D502**). That is the person's choice, made in the config
/// key whose doc says so, and not a thing this type can prevent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneShell(Vec<String>);

/// How much of the window's width the teammates' column takes when the
/// first teammate opens it, in percent — the **default**: `| lead 35% |
/// teammates 65% |` (user directive, 2026-08-25; 70 from 2026-08-20 until
/// then). `teammates.pane_share` names another, and [`PaneShare`] carries
/// whichever one a spawn got.
pub const DEFAULT_SHARE: u8 = 65;

/// The teammates' column's share of the width, in percent
/// ([`DEFAULT_SHARE`] unless `teammates.pane_share` named one), handed to
/// [`Placement::Beside`] by the first spawn that opens the column. tmux's
/// `-l` sizes the **new** pane, so this is the teammates' share and the lead
/// keeps what is left — which reads backwards from the layout, and is why it
/// is a type with a name rather than a number in an argv. The config refuses
/// anything outside 1..=99 at load; this type only carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneShare(u8);

impl Default for PaneShare {
    fn default() -> Self {
        Self(DEFAULT_SHARE)
    }
}

impl PaneShare {
    /// The share `teammates.pane_share` named.
    #[must_use]
    pub fn configured(percent: u8) -> Self {
        Self(percent)
    }

    /// The percentage, for a split and for a test to read back.
    #[must_use]
    pub fn percent(self) -> u8 {
        self.0
    }
}

impl Default for PaneShell {
    fn default() -> Self {
        Self(SHELL.iter().map(|word| (*word).to_owned()).collect())
    }
}

impl PaneShell {
    /// The shell `teammates.shell` named, as its words — a lone program made
    /// two words by `-s`, and a longer line taken as it was written.
    #[must_use]
    pub fn configured(mut words: Vec<String>) -> Self {
        if words.is_empty() {
            return Self::default();
        }
        if words.len() == 1 {
            words.push(SHELL[1].to_owned());
        }
        Self(words)
    }

    /// The words, for a launch and for a test to read back.
    #[must_use]
    pub fn words(&self) -> &[String] {
        &self.0
    }

    /// The argv tmux is given.
    #[must_use]
    pub fn argv(&self) -> Vec<OsString> {
        self.0.iter().map(OsString::from).collect()
    }
}

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
            && file.member(spec.name.as_str()).is_some_and(|member| member.tmux_pane_id == pane_id)
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
/// The five identifying flags in §4.1's own order, each with its value from
/// `spec` — and nothing else; what is deliberately absent is in the module
/// doc. They are the whole of a `ganja` pane's argv and the prefix a `claude`
/// pane's opens with ([`crate::claude::arguments`]), so the reaper's
/// witness reads one composition wherever a pane came from. Pure, so the
/// composed line is a thing a test can hold in its hand: the argv-secrets pin
/// reads it, and the pane's own side parses exactly these spellings.
#[must_use]
pub fn arguments(spec: &SpawnSpec) -> Vec<OsString> {
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
/// Where the pane lands is [`Placement`]'s, chosen here off
/// [`Server::column_bottom`] so both backends stack into one column rather
/// than each opening a column of its own.
///
/// `refused_as` is the surface a failing split is refused as, and `whose` the
/// word the log knows the teammate by.
pub(super) async fn split_idle_shell(
    server: &Server,
    spec: &SpawnSpec,
    environment: &[OsString],
    shell: &PaneShell,
    share: PaneShare,
    refused_as: MemberBackend,
    whose: &'static str,
) -> Result<Pane, Unsupported> {
    let shell = shell.argv();
    // Where it goes is read off the screen, not remembered: the first
    // teammate opens a column beside the lead and every later one stacks
    // under that column's bottom. A listing that fails is not a spawn that
    // fails — where a pane sits is cosmetic — so it falls back to the
    // placement a lead with no column would have given it anyway.
    let beside = Placement::Beside { share: share.percent() };
    let placement = match server.column_bottom().await {
        Ok(Some(bottom)) => Placement::Under(bottom),
        Ok(None) => beside,
        Err(error) => {
            tracing::warn!(
                teammate = spec.name.as_str(),
                %error,
                "the teammates' column could not be read; opening beside the lead"
            );

            beside
        }
    };
    let pane = server
        .split(Launch { cwd: &spec.cwd, environment, argv: &shell, placement })
        .await
        .map_err(|error| Unsupported { backend: refused_as, reason: error.to_string() })?;
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
/// answers both backends log alike; `whose` is the word for the teammate whose
/// pane it was.
///
/// Takes the pane itself since **D538**: the other-shape guard this used to
/// open with was a `Handle` variant test, and there is no handle to be of
/// another shape any more — a backend is handed back exactly what its own
/// `spawn` made.
pub(super) async fn kill_pane(pane: &Pane, whose: &'static str) {
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
///
/// Carries the shell and the column share since **D538**: they are properties
/// of the *runtime* a frontend resolved once from `teammates.shell` and
/// `teammates.pane_share`, and they arrive here as this module's own value
/// types, so no backend names a config type (**D520**'s intent, kept while the
/// state moved off the registry).
#[derive(Clone, Debug, Default)]
pub struct GanjaPane {
    shell: PaneShell,
    share: PaneShare,
}

impl GanjaPane {
    /// The backend a frontend assembles, over the shell and share this session
    /// resolved.
    #[must_use]
    pub fn new(shell: PaneShell, share: PaneShare) -> Self {
        Self { shell, share }
    }

    /// A tmux failure as the trait's refusal: this session cannot have the
    /// surface, and here is why. For [`TmuxError::NotHosted`] the reason is
    /// exactly [`tmux::REFUSED_NO_TMUX`], the D501 sentence.
    fn refused(error: &TmuxError) -> Unsupported {
        Unsupported { backend: MemberBackend::Ganja, reason: error.to_string() }
    }

    /// §4.1 step 6: types `line` into the pane's idle shell.
    ///
    /// A launch that cannot be typed is a pane holding a shell nobody will
    /// use, so it is ended here, by identity — the one failure past the split
    /// that this backend can still clean up after itself. Not the
    /// [`Spawned::launch`] hook, which this backend does not use yet (bead
    /// `ganja-code-ipg`).
    async fn type_launch_line(spec: &SpawnSpec, pane: &Pane, line: &OsStr, server: &Server) {
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
        MemberBackend::Ganja
    }

    /// The native channel: a `ganja` pane is a `ganja` session that speaks
    /// through its member postbox, so it holds the same `send_message` the
    /// in-process teammate does.
    fn preamble(&self, spec: &SpawnSpec) -> String {
        ganja_core::teammate::preamble::native(
            ganja_core::teammate::preamble::Names::of(spec),
            &spec.prompt,
        )
    }

    async fn spawn(&self, spec: &SpawnSpec, _lent: Lent) -> Result<Arc<dyn Spawned>, Unsupported> {
        // D501's capability check, at the moment of asking rather than at
        // install: whether there is a server to put a pane in.
        let server = Server::current().map_err(|error| Self::refused(&error))?;
        // The binary is resolved *now*, before the pane exists, so a process
        // that cannot name itself makes no pane it would then have to unmake.
        let binary = std::env::current_exe().map_err(|error| Unsupported {
            backend: MemberBackend::Ganja,
            reason: format!("this build cannot name its own binary to run in the pane: {error}"),
        })?;
        // The launch line under the same rule: its one refusal — a word no
        // shell quoting can carry — makes no pane either.
        let line =
            tmux::launch_line(&binary, &arguments(spec)).map_err(|error| Self::refused(&error))?;

        // §4.1 step 1: the surface, holding an idle shell. The environment
        // travels here (D502), through tmux's own door; the launch line is
        // typed later, once the record this pane will read exists.
        let environment = tmux::environment(CARRIED_ENV);
        let pane = split_idle_shell(
            &server,
            spec,
            &environment,
            &self.shell,
            self.share,
            MemberBackend::Ganja,
            "teammate",
        )
        .await?;

        // §4.1 step 6, sequenced after step 2 by watching for step 2 itself:
        // the record is what the pane's process reads first, so the record's
        // arrival on disk is the moment the launch line is typed. Watched
        // rather than called, because this backend's body predates the
        // [`Spawned::launch`] hook; the wait is bounded, and a record that
        // never comes is a spawn the registry has already unwound — its own
        // kill takes the idle shell with it, and the identity-checked kill
        // below is what keeps a timed-out watcher from ending a pane that has
        // since been reissued.
        //
        // The gap this shape leaves, stated rather than hidden: a launch that
        // fails *after* the record was written — tmux gone between the split
        // and the send-keys — ends the pane but cannot unregister the member,
        // because nothing here can reach the registry back. What bounds it is
        // the reaper: a record over a pane that is not there is exactly what
        // it drops at the next lead's startup (D506). And the task is detached
        // from the registry's shutdown: a lead that dies inside the wait
        // leaves a pane holding an idle shell that no record ever named —
        // invisible to a reaper that walks the team file — until a person
        // closes it. Moving the send-keys onto [`Spawned::launch`], with the
        // registry's own unwind, is the follow-up that closes both (bead
        // ganja-code-ipg); the body here is that method's body already.
        let member = Arc::new(PaneMember::new(pane.clone(), "teammate"));
        let owned = spec.clone();
        tokio::spawn(async move {
            match wait_for_record(&owned, &pane.id, RECORD_WAIT).await {
                Ok(()) => Self::type_launch_line(&owned, &pane, &line, &server).await,
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

        Ok(member)
    }

    fn delivery(&self) -> Delivery {
        Delivery::Acknowledged
    }
}

/// One teammate in a pane of its own, as either pane backend holds it after
/// the split.
///
/// Nothing of this session's runs for it: the pane is a whole process with its
/// own loop, its own ring and its own liveness, so [`Spawned::start`] hands
/// back no task, [`Spawned::recent`] is empty and [`Spawned::alive`] is
/// [`true`] until the member is retired out of the map.
///
/// Its identity is a **pair** rather than an id: `%N` recycles, so a lead that
/// killed panes by id alone would eventually kill somebody else's window. What
/// tmux reports beside the id is `#{pane_pid}` — there is no `pane_start_time`
/// format, as [`crate::tmux`]'s module doc records against
/// `man tmux` and against a live server — so **birth is that pid**, and it is
/// what makes the identity stable for as long as the machine keeps running.
/// [`crate::reaper`] is where the comparison lives, and where the
/// cold-start case that pid cannot answer for is dealt with.
#[derive(Debug)]
pub struct PaneMember {
    pane: Pane,
    /// The word the kill's four log lines know this teammate by.
    whose: &'static str,
}

impl PaneMember {
    /// The member over a pane that is already split.
    #[must_use]
    pub fn new(pane: Pane, whose: &'static str) -> Self {
        Self { pane, whose }
    }

    /// The recorded `(pane_id, birth)` pair.
    #[must_use]
    pub fn pane(&self) -> &Pane {
        &self.pane
    }
}

#[async_trait]
impl Spawned for PaneMember {
    fn surface(&self) -> Surface {
        Surface::Pane { id: self.pane.id.clone() }
    }

    fn start(self: Arc<Self>) -> Vec<JoinHandle<()>> {
        Vec::new()
    }

    fn alive(&self) -> bool {
        true
    }

    fn recent(&self) -> Vec<String> {
        Vec::new()
    }

    async fn kill(&self) {
        kill_pane(&self.pane, self.whose).await;
    }
}

#[cfg(test)]
#[path = "pane_tests.rs"]
mod tests;
