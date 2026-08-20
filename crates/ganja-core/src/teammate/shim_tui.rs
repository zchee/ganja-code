//! A shim teammate rendered in its CLI's own native TUI, in a tmux pane
//! (P28, **D512**).
//!
//! Upstream opencode has no counterpart and neither does Claude Code: neither
//! harness runs another vendor's agent as a teammate, let alone that vendor's
//! interactive TUI in a split beside the lead, so every sentence here is
//! ganja's own. It is the **second surface** the three shim CLIs can be run
//! on, and the one every spawn door reaches since P28: [`crate::teammate::shim`]
//! bridges the mailbox to a CLI's *headless* door through a pipe, while this
//! module bridges it to the CLI's *interactive* door through a pane — the
//! composer a person would type into, with the lead's messages pasted into it
//! instead. The headless machinery stays in the tree, unit-tested and
//! reachable by no spawn door in this build; `Engine::with_teammates`'s three
//! shim slots say so where they are wired.
//!
//! # What a pane changes, and what it does not
//!
//! What does **not** change is the whole of **D508**: the CLI runs at the same
//! pinned floors — [`TuiDriver::tui_argv`](crate::teammate::shim_tui::TuiDriver::tui_argv) is each driver's own spelling of
//! them, floors only, never a prompt and never an identity flag — and the
//! spawn raises the same always-ask `teammate_foreign` gate. A pane does not
//! widen what a person consented to; it changes who answers the CLI's *own*
//! prompts. A headless child answers nobody (the deadline was how a wedged one
//! was noticed), while a TUI's trust dialog, login screen and approval prompts
//! now render in front of a person who can answer them — which is exactly why
//! this shape has **no per-turn deadline**: **D509**'s own rationale for the
//! deadline was that a shim's progress was unobservable, and a TUI in a pane
//! is a thing a person can look at. `teammates.shim_turn_timeout` keeps
//! governing the headless machinery only, and nothing here reads it.
//!
//! What also does not change is **v1 is send-only**: the lead's words reach
//! the composer, and the lead hears nothing back. The CLI's replies render in
//! the pane for a person; no transcript is read back into the mailbox. The
//! posture row, the spawn dialog and the `/team` ring say so rather than
//! implying a conversation — those sentences are W4's, and this module's ring
//! lines are the facts they are built from.
//!
//! # The wire is bracketed paste, never `send-keys -l`
//!
//! Measured 2026-08-20, against codex's and agy's composers and then against
//! a stub that enables bracketed paste and records its input: `load-buffer -`
//! (the text on the client's **stdin**, so no byte of it is on an argv `ps`
//! shows to every user — the rule grok's prompt file already encodes) then
//! `paste-buffer -p` lands a multi-line body in the composer as **one**
//! message with its newlines intact, and `Enter` submits it. The stub
//! received exactly `\x1b[200~<body>\x1b[201~\n` — one bracketed body and one
//! Enter after the close bracket — which is what `tests/shim_tui.rs` asserts
//! byte for byte. [`crate::teammate::tmux::Server::paste_submit`] is that
//! sequence; it is not safe against concurrent calls to one pane, so the
//! runner here delivers **one message at a time per member**.
//!
//! Two rules make that wire safe to carry a peer's own words. First, the body
//! is control-neutralized before it is framed (`paste_body`): the paste's
//! own `ESC[201~` close is a control sequence, and a peer message that
//! contained one would otherwise end the paste early and land the rest as raw
//! keystrokes — so every control character but `\n` and `\t` is stripped, the
//! way `ganja-tui`'s notifier already strips them from text carried inside an
//! escape. Second, a delivery that fails is a ring note **and a word back to
//! the sender**, never a blind redelivery: the text may be sitting
//! pasted-but-unsubmitted in the composer, so pasting it again unseen is the
//! one thing forbidden (the lead's rulings 8(a) and 8(b)), and under
//! [`Delivery::FireAndForget`] the sender is told rather than left to assume it
//! landed.
//!
//! # Readiness is a poll with a ceiling, never a gate
//!
//! After the launch line the pane is captured every [`READY_POLL`](crate::teammate::shim_tui::READY_POLL) for up to
//! [`READY_WAIT`](crate::teammate::shim_tui::READY_WAIT), looking for the driver's own composer marker. Seeing it is
//! the ordinary case. **Not** seeing it is a ring note and a proceed, never a
//! spawn failure: a first spawn in an untrusted directory shows a trust
//! dialog before the composer, a logged-out CLI shows a login screen, and a
//! person answers both in the pane. What the timeout changes is not whether a
//! message is delivered but whether it is **submitted**: a spawn that saw its
//! marker presses Enter, one that timed out pastes the text and presses none,
//! because an Enter into a pane that may be holding that trust or login dialog
//! answers it unseen (HIGH-3, ruling F3). The text waits in the composer for
//! the person, who is looking at the pane. The one outcome that *is* a
//! failure is the pane's process **dying** inside the window — grok's every
//! spawn on this machine, refusing its sandbox profile and exiting 1 — and
//! that is refused **by the vendor's own sentence**: the pane is kept on exit
//! (`remain-on-exit`, set before the launch line so the words survive the
//! exit), its last words are captured *first*, and only then is the dead pane
//! closed, because a dead pane left on screen would halve the teammates'
//! column on every refusal (ruling 8(c)). The words travel in the refusal the
//! lead reads.
//!
//! # What ends a pane, and in what order
//!
//! A pane's process is its own process-group leader (tmux `setsid`s it;
//! measured), so ending the member is ending that group — and agy survives
//! the `SIGHUP` a bare `kill-pane` sends (measured, the plan's fact 7). So
//! the order is the lead's ruling F3: **TERM the group first, while the pane
//! still pins the pid** and the recorded `(pane_id, birth)` pair can vouch
//! that the pid is ours; wait for the pane to stop listing live; KILL if it
//! did not; and only then close what is left — a dead pane under
//! `remain-on-exit` — through the one door that closes dead panes and nothing
//! else. Signalling after the pane was killed would signal a pid the kernel
//! may already have handed to somebody else.
//!
//! A lead that dies leaves its panes: a foreign CLI's argv carries no
//! `--agent-id`/`--parent-session-id` pair, so [`crate::teammate::reaper`]'s
//! witness cannot match it and leaves the pane alone under "no owner proof,
//! no signal" — it is a visible TUI a person closes by hand. Nothing here
//! writes a shim orphan record either: the `.shims` sweep signals what it
//! proves is ours, and a pane whose process ganja did not fork is not that.
//!
//! # Where the vendor's spelling lives
//!
//! [`TuiDriver`](crate::teammate::shim_tui::TuiDriver) is a companion trait over the three driver types of the
//! sibling modules, defined and implemented **here** and delegating to the
//! inherent `tui_argv()` / `READY_MARKER` those modules shipped (the lead's
//! ruling 3): a local trait over sibling types reopens no driver file and
//! avoids a second `match MemberBackend` dispatch table. What each driver
//! answers is pinned against that CLI's own recording under
//! `tests/fixtures/*-tui-probe.txt`, beside the driver.

use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ganja_protocol::team::{Frame, MemberBackend, ShutdownApproved, ShutdownRequest, Tagged};
use ganja_team::{MailboxMessage, MemberName, ShimCli, Surface, mailbox, record};
use tokio_util::sync::CancellationToken;

use crate::teammate::{
    Delivery, Handle, SETTLE, SpawnSpec, TeammateBackend, Unsupported,
    agy::Agy,
    backend_name,
    codex::Codex,
    grok::Grok,
    pane,
    reaper::Pane,
    runner,
    shim::{self, Driver},
    tmux::{self, Closed, Killed, Server, TmuxError},
};

/// How long a spawn waits for the CLI's composer to show before it proceeds
/// without having seen it.
///
/// A ceiling on the ordinary case, not a timeout a spawn fails on: the
/// module doc says why a miss is a note and a proceed. Fifteen seconds is
/// longer than any of the three TUIs took to reach its composer when probed
/// cold on this machine, and shorter than a person's patience with a spawn
/// that appears to hang.
pub const READY_WAIT: Duration = Duration::from_secs(15);

/// How often the readiness poll captures the pane.
pub const READY_POLL: Duration = Duration::from_millis(250);

/// How often the runner looks for a pane that has stopped listing live after
/// its group was signalled.
const GONE_POLL: Duration = Duration::from_millis(50);

/// What the ring says once the composer marker was seen.
pub const RING_READY: &str = "composer ready in the pane";

/// What the ring says when the marker never showed inside [`READY_WAIT`].
///
/// Names the two things a person can act on — that the paste went ahead, and
/// that the pane may be showing a dialog only they can answer.
pub const RING_NOT_READY: &str = "composer marker not seen in the pane; pasting anyway — a \
     trust or login dialog may be waiting there for you";

/// What the ring says after one message was pasted and submitted.
pub const RING_DELIVERED: &str = "delivered to pane";

/// What the ring says after a delivery failed.
///
/// The text may be sitting in the composer unsubmitted; it is not pasted
/// again unseen (ruling 8(a)).
pub const RING_DELIVERY_FAILED: &str = "delivery to pane failed";

/// What the ring says after a message was pasted into a composer whose marker
/// never showed — and therefore **not** submitted.
///
/// The spawn saw no composer, which may mean a trust or login dialog is up, so
/// pressing Enter would answer that dialog unseen (HIGH-3, ruling F3's
/// reasoning): the text is left in the composer and the person, who is looking
/// at the pane, submits it. A spawn that did see its marker reports the
/// ordinary [`RING_DELIVERED`] instead.
pub const RING_PASTED_UNSUBMITTED: &str = "pasted into the pane, unsubmitted — no composer \
     marker was seen, so a person submits it";

/// The opening of a spawn refusal for a TUI that exited inside the readiness
/// window; the CLI's own last words follow it.
pub const REFUSED_DIED: &str = "exited before its composer was ready";

/// tmux's own last line on a pane kept after its process exited
/// (`remain-on-exit-format`, measured on next-3.8: `Pane is dead (status 1,
/// <time>)`). Not the CLI's words, so [`last_words`] leaves it out.
const PANE_IS_DEAD: &str = "Pane is dead";

/// What a per-CLI module answers for its native TUI, and the whole of it.
///
/// Two questions beside [`Driver`]'s: the words after the binary on the
/// pane's launch line, and the string a readiness poll looks for. Each
/// driver ships both as inherent items pinned against its probe recording;
/// the implementations below only delegate, so nothing here is a second
/// spelling of a vendor's flag.
pub trait TuiDriver: Driver {
    /// The launch arguments after the binary: the pinned floors, and nothing
    /// else — no prompt, no session, no identity flag.
    fn tui_argv(&self) -> Vec<OsString>;

    /// The string the composer shows once a paste has somewhere to land.
    fn ready_marker(&self) -> &'static str;
}

impl TuiDriver for Codex {
    fn tui_argv(&self) -> Vec<OsString> {
        Codex::tui_argv(self)
    }

    fn ready_marker(&self) -> &'static str {
        crate::teammate::codex::READY_MARKER
    }
}

impl TuiDriver for Grok {
    fn tui_argv(&self) -> Vec<OsString> {
        Grok::tui_argv(self)
    }

    fn ready_marker(&self) -> &'static str {
        crate::teammate::grok::READY_MARKER
    }
}

impl TuiDriver for Agy {
    fn tui_argv(&self) -> Vec<OsString> {
        Agy::tui_argv(self)
    }

    fn ready_marker(&self) -> &'static str {
        crate::teammate::agy::READY_MARKER
    }
}

/// The names a shim TUI pane's environment is composed from: the `ganja`
/// pane's own closed list (**D502**), then the driver's additions.
///
/// A tmux pane inherits the **server's** environment and `-e` only adds to
/// it, so what travels here is what the headless child would have been
/// *handed*: the config-home and XDG names every pane needs, and the CLI's
/// own home pointer — `CODEX_HOME` today — which the headless runner carries
/// through `additions()` and a pane would otherwise read off whatever shell
/// started tmux. Run through [`shim::admits`] as the headless enumeration is,
/// so a `GROK_*` name in an additions list travels on neither surface.
///
/// Pure over the additions, so the composition is a thing a test can hold.
#[must_use]
pub fn environment_names<'a>(additions: &'a [&'a str]) -> Vec<&'a str> {
    pane::CARRIED_ENV
        .into_iter()
        .chain(additions.iter().copied().filter(|name| shim::admits(name)))
        .collect()
}

/// The last thing a pane showed that was the program's own — the line a
/// refusal quotes.
///
/// The last non-empty line of a capture, leaving out tmux's own
/// `remain-on-exit` notice under it, since that is tmux talking and not the
/// CLI. [`None`] when the pane showed nothing at all.
#[must_use]
pub fn last_words(captured: &str) -> Option<String> {
    captured
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .rfind(|line| !line.starts_with(PANE_IS_DEAD))
        .map(str::to_owned)
}

/// The bytes actually pasted for one peer message: the `runner::envelope`,
/// with every control character but `\n` and `\t` stripped.
///
/// The body rides **inside** a bracketed-paste sequence (`ESC[200~ … ESC[201~`,
/// [`crate::teammate::tmux::Server::paste`]), and tmux does not filter the
/// buffer it pastes — so a literal `ESC[201~` in a peer's message closes the
/// paste early and lands everything after it (a `\r`, a slash command, control
/// bytes) as raw keystrokes in the foreign composer (measured live, tmux
/// next-3.8). Stripping the control characters disarms every such escape at the
/// one boundary where peer text is the untrusted thing: the `ESC` that arms
/// `[201~` is a control character and goes, leaving `[201~` as inert text a
/// composer shows rather than obeys. `\n` stays because a multi-line prompt is
/// the whole point of bracketed paste, and `\t` because it is indentation a
/// person typed, not a control sequence.
///
/// This is the invariant `ganja-tui`'s notifier already states for text
/// carried inside an OSC/BEL escape (`notify::body`), narrowed here to keep the
/// two whitespace characters a composer reads as content.
#[must_use]
pub fn paste_body(from: &str, text: &str) -> String {
    runner::envelope(from, text)
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

/// Whether the readiness window ended with a composer, without one, or with
/// a dead pane.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Ready {
    /// The marker showed.
    Seen,
    /// [`READY_WAIT`] passed without it; the pane is still running.
    TimedOut,
    /// The pane's process ended inside the window, with these last words.
    Died(Option<String>),
    /// The pane id now wears a different birth: not ours any more, and not
    /// ours to touch.
    Lost,
}

/// What a spawn's readiness poll concluded, carried on the handle so the
/// runner can put it on the ring the registry makes after the spawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// The composer marker showed inside [`READY_WAIT`].
    Seen,
    /// It did not; deliveries go ahead regardless.
    TimedOut,
}

/// What [`Handle::TuiPane`] holds: the pane's identity, the server it is
/// on, and the token that ends its runner.
///
/// The [`Server`] is held rather than re-read off `$TMUX` at kill time, for
/// two reasons that come to the same thing: a test spawns against a private
/// server it names, and a production kill must go to the server the pane was
/// split on rather than to whatever the environment says now.
pub struct TuiPane {
    cli: ShimCli,
    backend: MemberBackend,
    pane: Pane,
    server: Server,
    readiness: Readiness,
    cancel: CancellationToken,
    /// Set once the pane has been ended, so a second `end` — the runner's
    /// `shutdown_request` teardown followed by the lead's retire — looks and
    /// signals nothing.
    ended: AtomicBool,
}

impl std::fmt::Debug for TuiPane {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TuiPane")
            .field("cli", &self.cli)
            .field("pane_id", &self.pane.id)
            .field("birth", &self.pane.birth)
            .field("readiness", &self.readiness)
            .finish_non_exhaustive()
    }
}

impl TuiPane {
    /// A handle over a pane that is already running `cli`'s TUI on `server`.
    ///
    /// The one constructor, for the backend's `spawn` and for a test that
    /// wants to ask `end` about a pane it made itself.
    #[must_use]
    pub fn new(
        cli: ShimCli,
        backend: MemberBackend,
        pane: Pane,
        server: Server,
        readiness: Readiness,
    ) -> Self {
        Self {
            cli,
            backend,
            pane,
            server,
            readiness,
            cancel: CancellationToken::new(),
            ended: AtomicBool::new(false),
        }
    }

    /// Which CLI sits in the pane.
    #[must_use]
    pub fn cli(&self) -> ShimCli {
        self.cli
    }

    /// The recorded `(pane_id, birth)` pair.
    #[must_use]
    pub fn pane(&self) -> &Pane {
        &self.pane
    }

    /// The server the pane is on.
    #[must_use]
    pub fn server(&self) -> &Server {
        &self.server
    }

    /// What the spawn's readiness poll concluded.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        self.readiness
    }

    /// The token that ends this member's runner.
    #[must_use]
    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Ends the pane and the process in it, identity-checked, in the order
    /// the module doc gives: TERM the group while the pane pins the pid, wait,
    /// KILL, then close the dead pane. Idempotent.
    pub async fn end(&self) {
        self.cancel.cancel();
        if self.ended.swap(true, Ordering::AcqRel) {
            return;
        }
        let cli = backend_name(self.backend);
        let pane = &self.pane;

        let live = match self.server.panes().await {
            Ok(live) => live,
            Err(error) => {
                // Nothing can be identity-checked against a listing that will
                // not come; the pane outlives this process and is a person's
                // to close. Named rather than guessed at.
                tracing::warn!(cli, pane = pane.id, %error, "a TUI pane could not be listed, so it was left");
                return;
            }
        };
        match live.iter().find(|listed| listed.id == pane.id) {
            // Not running: dead and kept, or already closed. The corpse, if
            // there is one, is closed through the dead-only door.
            None => {
                self.close_corpse().await;
                return;
            }
            Some(listed) if !pane.is(listed) => {
                tracing::warn!(
                    cli,
                    pane = pane.id,
                    birth = pane.birth,
                    "a TUI pane's id now names somebody else's pane; left alone"
                );
                return;
            }
            Some(_) => {}
        }

        // The pair matched, so the birth pid is the pane's process right now,
        // and — tmux having `setsid` it — its own group's leader.
        let Ok(group) = pane.birth.parse::<i32>() else {
            // A birth the listing accepted is digits; this arm is the type
            // system's. The identity-checked kill is the honest fallback.
            self.kill_pane().await;
            return;
        };
        shim::signal_group(group, libc::SIGTERM);
        if !self.gone_within(SETTLE).await {
            tracing::warn!(
                cli,
                pane = pane.id,
                group,
                "a TUI teammate did not end on SIGTERM, so its process group was killed"
            );
            shim::signal_group(group, libc::SIGKILL);
            if !self.gone_within(SETTLE).await {
                // A group KILL cannot take is not a thing a user process sees
                // outside a wedged kernel; what is left is to end the pane
                // itself, identity-checked.
                self.kill_pane().await;
                return;
            }
        }
        tracing::info!(
            cli,
            pane = pane.id,
            "a TUI teammate's process group was ended"
        );
        self.close_corpse().await;
    }

    /// Whether the pane stops listing live inside `limit`.
    async fn gone_within(&self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            match self.server.panes().await {
                Ok(live) if !live.iter().any(|listed| listed.id == self.pane.id) => return true,
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(pane = self.pane.id, %error, "a liveness listing failed while waiting");
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(GONE_POLL).await;
        }
    }

    /// Closes the pane if its process has ended, through the dead-only door.
    async fn close_corpse(&self) {
        let cli = backend_name(self.backend);
        match self.server.close_dead(&self.pane.id).await {
            Ok(Closed::Yes) => {
                tracing::info!(cli, pane = self.pane.id, "a TUI teammate's pane was closed")
            }
            Ok(Closed::AlreadyGone) => {
                tracing::debug!(
                    cli,
                    pane = self.pane.id,
                    "a TUI teammate's pane was already gone"
                );
            }
            Ok(Closed::Alive) => {
                // Live again — respawned by a person into the same id, or the
                // process outlived a KILL. The identity check decides.
                self.kill_pane().await;
            }
            Ok(Closed::Refused) => {
                // Dead, the kill sent, and still there: a corpse tmux would
                // not take away, and not a member — left for a person to
                // close, and said out loud.
                tracing::warn!(
                    cli,
                    pane = self.pane.id,
                    "a TUI teammate's dead pane would not close; it is dead, and a person closes it"
                );
            }
            Err(error) => {
                tracing::warn!(cli, pane = self.pane.id, %error, "a TUI teammate's dead pane could not be closed");
            }
        }
    }

    /// The identity-checked `kill-pane`, for the paths where the group could
    /// not be signalled or the pane came back live.
    async fn kill_pane(&self) {
        let cli = backend_name(self.backend);
        match self.server.kill(&self.pane).await {
            Ok(Killed::Yes) => {
                tracing::info!(cli, pane = self.pane.id, "a TUI teammate's pane was ended")
            }
            Ok(Killed::AlreadyGone) => {
                tracing::debug!(
                    cli,
                    pane = self.pane.id,
                    "a TUI teammate's pane was already gone"
                );
            }
            Ok(Killed::Recycled) => tracing::warn!(
                cli,
                pane = self.pane.id,
                birth = self.pane.birth,
                "a TUI teammate's pane id now names somebody else's pane; left alone"
            ),
            Err(error) => {
                tracing::warn!(cli, pane = self.pane.id, %error, "a TUI teammate's pane could not be ended");
            }
        }
    }
}

/// One [`TuiDriver`] as a [`TeammateBackend`] that opens its CLI's native
/// TUI in a pane.
///
/// One value for all three CLIs, as [`shim::ShimBackend`] is for the headless
/// shape: what differs between codex, agy and grok is inside the driver, and
/// three copies of "split, keep, launch, wait, hand over" would be three
/// chances for one of them to set `remain-on-exit` after the launch line.
pub struct ShimTui {
    driver: Arc<dyn TuiDriver>,
    /// The server to split on. [`None`] is `$TMUX` at the moment of spawn —
    /// the production answer, and **D501**'s rule that the variable is a
    /// capability read when asked; a value is how a test points the backend
    /// at a private server without touching the process environment.
    server: Option<Server>,
    /// Where the binary is looked for. [`None`] is this process's `PATH`; a
    /// value is how a test points a spawn at a stub TUI.
    path: Option<OsString>,
}

impl std::fmt::Debug for ShimTui {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShimTui")
            .field("driver", &self.driver)
            .finish_non_exhaustive()
    }
}

impl ShimTui {
    /// The pane-mode backend for `driver`, splitting the server `$TMUX` names
    /// and resolving the binary on this process's own `PATH`.
    #[must_use]
    pub fn new(driver: Arc<dyn TuiDriver>) -> Self {
        Self {
            driver,
            server: None,
            path: None,
        }
    }

    /// The same backend against an explicit server.
    #[must_use]
    pub fn on(mut self, server: Server) -> Self {
        self.server = Some(server);

        self
    }

    /// The same backend against an explicit search path.
    #[must_use]
    pub fn searching(mut self, path: OsString) -> Self {
        self.path = Some(path);

        self
    }

    /// A tmux failure as the trait's refusal: this session cannot have the
    /// surface, and here is why. For [`TmuxError::NotHosted`] the reason is
    /// exactly [`tmux::REFUSED_NO_TMUX`], the **D501** sentence.
    fn refused(&self, error: &TmuxError) -> Unsupported {
        Unsupported {
            backend: self.driver.backend(),
            reason: error.to_string(),
        }
    }

    /// The server a spawn splits on.
    fn server(&self) -> Result<Server, Unsupported> {
        match &self.server {
            Some(server) => Ok(server.clone()),
            None => Server::current().map_err(|error| self.refused(&error)),
        }
    }

    /// Polls the pane for the composer marker, or for its death, until one
    /// shows or [`READY_WAIT`] passes.
    ///
    /// Liveness is asked twice against the marker, because a dead pane's
    /// capture still succeeds under `remain-on-exit` and a marker on a corpse
    /// is not a composer. It is asked *before* the marker on every pass — a
    /// pane already dead is [`Ready::Died`] before the capture is even read —
    /// and *again* the instant the marker is found, to catch a pane that
    /// printed its marker and then died between the two calls: only a pane the
    /// second listing still shows live answers [`Ready::Seen`].
    async fn wait_ready(&self, server: &Server, pane: &Pane) -> Ready {
        let marker = self.driver.ready_marker();
        let deadline = tokio::time::Instant::now() + READY_WAIT;
        loop {
            match server.panes().await {
                Ok(live) => match live.iter().find(|listed| listed.id == pane.id) {
                    None => {
                        // Dead and kept, or gone: the words first, while they
                        // are still there to read.
                        let words = match server.capture(&pane.id).await {
                            Ok(shown) => last_words(&shown),
                            Err(_) => None,
                        };
                        return Ready::Died(words);
                    }
                    Some(listed) if !pane.is(listed) => return Ready::Lost,
                    Some(_) => {}
                },
                Err(error) => {
                    // A listing that failed says nothing about the pane; the
                    // next pass asks again.
                    tracing::debug!(pane = pane.id, %error, "a liveness listing failed during readiness");
                }
            }
            match server.capture(&pane.id).await {
                Ok(shown) if shown.contains(marker) => {
                    // A marker on a corpse is not a ready composer (D512): a
                    // pane that printed its marker and then died still captures
                    // under `remain-on-exit`, so a `Seen` returned on the
                    // capture alone would hand back a live member over a dead
                    // process (grok's provisional banner is exactly this risk).
                    // One more liveness listing settles it — still live is
                    // `Seen`, gone is `Died` with the last words read now,
                    // while they are on the kept pane.
                    match server.panes().await {
                        Ok(live) if live.iter().any(|listed| listed.id == pane.id) => {
                            return Ready::Seen;
                        }
                        Ok(_) => {
                            let words = match server.capture(&pane.id).await {
                                Ok(fresh) => last_words(&fresh),
                                Err(_) => last_words(&shown),
                            };
                            return Ready::Died(words);
                        }
                        Err(error) => {
                            // The marker showed but liveness could not be
                            // confirmed; the next pass asks again rather than
                            // trust a marker it cannot vouch is live.
                            tracing::debug!(pane = pane.id, %error, "a liveness listing failed after a readiness marker");
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(pane = pane.id, %error, "a capture failed during readiness");
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ready::TimedOut;
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }

    /// Ends a pane this spawn made and cannot use: identity-checked, and the
    /// dead-only door afterwards in case the process already went.
    async fn unmake(&self, server: &Server, pane: &Pane) {
        match server.kill(pane).await {
            Ok(Killed::Yes | Killed::Recycled) => {}
            Ok(Killed::AlreadyGone) | Err(_) => {
                if let Err(error) = server.close_dead(&pane.id).await {
                    tracing::warn!(pane = pane.id, %error, "a refused spawn's pane could not be closed");
                }
            }
        }
    }
}

#[async_trait]
impl TeammateBackend for ShimTui {
    fn backend(&self) -> MemberBackend {
        self.driver.backend()
    }

    async fn spawn(&self, spec: &SpawnSpec) -> Result<Handle, Unsupported> {
        let backend = self.driver.backend();
        let cli = backend_name(backend);
        // D501's capability check, at the moment of asking rather than at
        // install: whether there is a server to put a pane in. First, because
        // it is the refusal a session outside tmux earns whatever else is
        // true of it.
        let server = self.server()?;
        // The two refusals every shim shares — bypass, and a binary that is
        // not there — and the binary resolved *now*, before any pane exists
        // that would then have to be unmade. The `Launch`'s environment is the
        // headless child's and goes unused: a pane's is composed below.
        let launch = shim::prepare(&*self.driver, spec, self.path.as_deref())?;
        // The launch line under the same rule: its one refusal — a word no
        // shell quoting can carry — makes no pane either.
        let line = launch_line(launch.binary.as_os_str(), &*self.driver)
            .map_err(|error| self.refused(&error))?;

        let environment = tmux::environment(environment_names(self.driver.additions()));
        let pane =
            pane::split_idle_shell(&server, spec, &environment, backend, "shim teammate").await?;

        // Kept on exit **before** the launch line: a CLI that refuses to
        // start says why and exits, and a pane that closed with it would take
        // the sentence with it.
        if let Err(error) = server.remain_on_exit(&pane.id, true).await {
            self.unmake(&server, &pane).await;
            return Err(self.refused(&error));
        }
        if let Err(error) = server.type_line(&pane.id, &line).await {
            self.unmake(&server, &pane).await;
            return Err(self.refused(&error));
        }
        tracing::info!(
            teammate = spec.name.as_str(),
            pane = pane.id,
            cli,
            "a TUI pane was launched"
        );

        let readiness = match self.wait_ready(&server, &pane).await {
            Ready::Seen => Readiness::Seen,
            Ready::TimedOut => {
                tracing::warn!(
                    teammate = spec.name.as_str(),
                    pane = pane.id,
                    cli,
                    "the composer marker was not seen within {READY_WAIT:?}; proceeding"
                );
                Readiness::TimedOut
            }
            Ready::Died(words) => {
                // The words were read first; now the corpse goes, so a
                // refusal does not halve the teammates' column (ruling 8(c)).
                if let Err(error) = server.close_dead(&pane.id).await {
                    tracing::warn!(pane = pane.id, %error, "a dead TUI pane could not be closed");
                }
                let said = match words {
                    Some(words) => format!("its pane said: {words}"),
                    None => "its pane showed nothing".to_owned(),
                };
                return Err(Unsupported {
                    backend,
                    reason: format!("{cli} {REFUSED_DIED}; {said}"),
                });
            }
            Ready::Lost => {
                return Err(Unsupported {
                    backend,
                    reason: format!(
                        "{cli}'s pane {} was reissued to somebody else before its composer was ready",
                        pane.id
                    ),
                });
            }
        };

        Ok(Handle::TuiPane(Arc::new(TuiPane::new(
            self.driver.cli(),
            backend,
            pane,
            server,
            readiness,
        ))))
    }

    async fn kill(&self, handle: &Handle) {
        let Some(tui) = handle.tui() else {
            // Not reachable through the registry, which hands back the handle
            // this backend's own `spawn` returned — but a handle of another
            // shape arriving here would mean a registry had crossed two
            // backends, and that is worth saying rather than ignoring.
            tracing::warn!(
                ?handle,
                backend = backend_name(self.driver.backend()),
                "a shim TUI backend was asked to end something it did not start"
            );

            return;
        };
        tui.end().await;
    }

    fn delivery(&self) -> Delivery {
        // A paste into a foreign composer is handed over and that is all
        // there is to see: the CLI reads it at its own pace, and nothing here
        // can watch it do so — so the lead retires its queue entry at write
        // time (**D503**).
        Delivery::FireAndForget
    }
}

/// What the registry lends one TUI member's loop.
///
/// [`shim::Lent`]'s shape, minus the orphan records a pane member never
/// writes, for that type's reason: every field is the registry's, and a loop
/// built from them one by one is a loop somebody could build with one
/// missing.
pub struct Lent {
    /// Where this member answers a shutdown.
    pub lead_inbox: PathBuf,
    /// **D503**'s ring, which this loop writes itself: there is no engine
    /// event stream to fold from.
    pub recent: Arc<Mutex<VecDeque<String>>>,
    /// Cleared when the loop ends, so a member that answered a shutdown stops
    /// being listed without the registry having to be told.
    pub alive: Arc<AtomicBool>,
    /// The registry's own cancellation, beside the handle's own.
    pub cancel: CancellationToken,
}

/// What one pass of [`TuiRunner`] did.
///
/// Returned rather than only logged so a test can drive a single pass and
/// assert the frame table, which is the part of this loop that is the
/// contract.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// The request id of a shutdown this pass answered, if it answered one.
    pub shutdown: Option<String>,
    /// How many messages were pasted and submitted.
    pub delivered: usize,
    /// How many deliveries failed and were noted rather than retried.
    pub failed: usize,
    /// The frames this pass dropped as information, by kind — `None` where
    /// the kind was one this build has never heard of and the sender gave no
    /// name for it.
    pub dropped: Vec<Option<String>>,
}

/// One TUI member's mailbox loop: read, classify, paste.
///
/// Mirrors [`shim::ShimRunner`]'s **shape** — the shutdown-first pass, the
/// frame table that is total by construction, the prune after — and shares
/// its one guard, because what the two deliver into is the same: a message
/// shaped like a frame this build cannot read is never composed into a
/// foreign CLI's prompt. What it does not share is everything after the
/// classification, because a paste has no answer to read: a turn here is one
/// `paste_submit`, and what the CLI does with it is for the person looking
/// at the pane.
pub struct TuiRunner {
    handle: Arc<TuiPane>,
    spec: SpawnSpec,
    lead_inbox: PathBuf,
    recent: Arc<Mutex<VecDeque<String>>>,
    alive: Arc<AtomicBool>,
    /// The registry's own token, beside the handle's — [`shim::ShimRunner`]
    /// says why there are two.
    registry: CancellationToken,
    poll: Duration,
}

impl std::fmt::Debug for TuiRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TuiRunner")
            .field("cli", &self.handle.cli())
            .field("member", &self.spec.name)
            .field("pane", &self.handle.pane().id)
            .finish_non_exhaustive()
    }
}

impl TuiRunner {
    /// Builds the loop for one TUI member.
    #[must_use]
    pub fn new(handle: Arc<TuiPane>, spec: SpawnSpec, lent: Lent) -> Self {
        Self {
            handle,
            spec,
            lead_inbox: lent.lead_inbox,
            recent: lent.recent,
            alive: lent.alive,
            registry: lent.cancel,
            poll: shim::POLL,
        }
    }

    /// Runs until the registry cancels it, a `shutdown_request` is answered,
    /// or the handle is ended.
    ///
    /// A delivery in flight is never cancelled mid-paste: cancellation is
    /// noticed between passes, so the one `paste_submit` a pass is inside
    /// completes — which is the fourth clause of ruling 8 by construction.
    pub async fn run(self) {
        // The spawn's own finding goes on the ring first, so a person opening
        // `/team` learns whether the composer was seen before the first
        // delivery was ever attempted.
        self.remember(match self.handle.readiness() {
            Readiness::Seen => RING_READY.to_owned(),
            Readiness::TimedOut => RING_NOT_READY.to_owned(),
        });
        let cancel = self.handle.cancel().clone();
        let registry = self.registry.clone();
        let mut poll = tokio::time::interval(self.poll);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = registry.cancelled() => break,
                _ = poll.tick() => {
                    if self.tick().await.shutdown.is_some() {
                        break;
                    }
                }
            }
        }

        // The pane itself is ended by whoever cancelled — the backend's kill,
        // or this loop's own shutdown teardown — so all that is left is to
        // stop being listed.
        self.alive.store(false, Ordering::Relaxed);
    }

    /// One pass: read, classify, paste whatever was really prompt material —
    /// one message at a time, in inbox order.
    pub async fn tick(&self) -> Tick {
        let mut tick = Tick::default();
        let Some(contents) = runner::read_inbox(self.inbox(), self.spec.name.as_str()).await else {
            return tick;
        };
        if contents.valid.is_empty() {
            return tick;
        }

        // A shutdown goes ahead of everything else, from any sender —
        // [`shim::ShimRunner`]'s rule, for its reason.
        let shutdown = contents
            .valid
            .iter()
            .enumerate()
            .find_map(|(position, message)| match message.frame() {
                Some(Frame::ShutdownRequest(request)) => Some((position, message, request)),
                _ => None,
            });
        if let Some((position, message, request)) = shutdown {
            tracing::info!(
                teammate = self.spec.name.as_str(),
                request = request.request_id,
                jumped = position,
                "a shutdown request goes ahead of everything else in the inbox"
            );
            self.tear_down(&request).await;
            self.prune(vec![mailbox::identity(message)]).await;
            tick.shutdown = Some(request.request_id);

            return tick;
        }

        let mut handled = Vec::new();
        for message in &contents.valid {
            handled.push(mailbox::identity(message));
            match Frame::classify(&message.text) {
                Tagged::NotAnObject | Tagged::Untagged => {
                    if self.deliver(&message.from, &message.text).await {
                        tick.delivered += 1;
                    } else {
                        tick.failed += 1;
                    }
                }
                Tagged::Reserved(kind) => {
                    tick.dropped.push(Some(kind.to_owned()));
                    self.drop_reserved(kind, &message.from).await;
                }
                Tagged::Unknown { name } => {
                    tick.dropped.push(name.clone());
                    self.drop_unknown(name.as_deref(), &message.from).await;
                }
            }
        }
        if !handled.is_empty() {
            self.prune(handled).await;
        }

        tick
    }

    /// This member's own inbox.
    fn inbox(&self) -> PathBuf {
        self.spec.inbox()
    }

    /// One message into the composer. Answers whether it landed; a failure is a
    /// ring note and a word to the sender, never a redelivery.
    ///
    /// The body is [`paste_body`], control-neutralized, so a peer's own text
    /// cannot forge the bracketed-paste framing carrying it. Whether the Enter
    /// follows is the spawn's readiness: a composer that showed its marker
    /// submits ([`Server::paste_submit`](crate::teammate::tmux::Server::paste_submit)),
    /// one that never did is pasted only ([`Server::paste`](crate::teammate::tmux::Server::paste)),
    /// because an Enter into a pane that may be holding a trust or login dialog
    /// answers it unseen (HIGH-3, ruling F3's reasoning).
    async fn deliver(&self, from: &str, text: &str) -> bool {
        let body = paste_body(from, text);
        let cli = backend_name(self.handle.backend);
        let pane = self.handle.pane().id.as_str();
        let server = self.handle.server();
        let submit = matches!(self.handle.readiness(), Readiness::Seen);
        let outcome = if submit {
            server.paste_submit(pane, &body).await
        } else {
            server.paste(pane, &body).await
        };
        match outcome {
            Ok(()) => {
                let ring = if submit {
                    RING_DELIVERED
                } else {
                    RING_PASTED_UNSUBMITTED
                };
                self.remember(format!("{ring} · {} bytes", body.len()));
                true
            }
            Err(error) => {
                tracing::warn!(
                    teammate = self.spec.name.as_str(),
                    pane,
                    cli,
                    %error,
                    "a message could not be delivered to a TUI pane; it is not being pasted again"
                );
                self.remember(format!("{RING_DELIVERY_FAILED} · {error}"));
                self.note_undelivered(from, &error).await;
                false
            }
        }
    }

    /// Tells the sender that a message did not land.
    ///
    /// [`drop_unknown`](Self::drop_unknown)'s courtesy for a different failure:
    /// under [`Delivery::FireAndForget`] the lead's model otherwise never
    /// learns its teammate went deaf, so a failed delivery is a word back — to
    /// the sender's own inbox, the same door an unknown frame is refused
    /// through. Notification, never a redelivery (ruling 8(a)): the text may be
    /// sitting pasted-but-unsubmitted in the composer, or it may not have
    /// landed at all, and pasting it again unseen is the one thing this must
    /// not do.
    async fn note_undelivered(&self, from: &str, error: &TmuxError) {
        let Some(inbox) = self.sender_inbox(from) else {
            return;
        };
        self.mail(
            inbox,
            format!(
                "That message was not delivered to {name}'s {cli} TUI pane ({error}). It is not \
                 being pasted again — it may be sitting unsubmitted in the composer, or it may \
                 not have landed at all. Send it again yourself if it still needs to reach that \
                 teammate.",
                name = self.spec.name.as_str(),
                cli = backend_name(self.handle.backend),
            ),
        )
        .await;
    }

    /// The inbox of the member named `from`, or [`None`] (logged) when the name
    /// cannot be addressed — the one door both a failed delivery and an unknown
    /// frame tell a sender through.
    fn sender_inbox(&self, from: &str) -> Option<PathBuf> {
        match MemberName::parse(from) {
            Ok(sender) => Some(self.spec.root.inbox_path(&self.spec.team, &sender)),
            Err(_) => {
                tracing::warn!(
                    teammate = self.spec.name.as_str(),
                    from,
                    "a sender's name cannot be addressed, so it could not be told"
                );

                None
            }
        }
    }

    /// A reserved frame, which a pane member has no engine to apply.
    async fn drop_reserved(&self, kind: &'static str, from: &str) {
        self.remember(format!(
            "dropped frame {kind} · a TUI member has no engine to apply it to"
        ));
        tracing::info!(
            teammate = self.spec.name.as_str(),
            from,
            kind,
            "a reserved frame reached a TUI member, which has no engine to apply it to"
        );
        if kind == "mode_set_request" {
            self.mail(
                self.lead_inbox.clone(),
                format!(
                    "{name} runs in the {cli} TUI, which has no ganja permission mode to set: the \
                     mode_set_request was read and dropped. Its posture is the one pinned at \
                     spawn, and that one holds for every turn.",
                    name = self.spec.name.as_str(),
                    cli = backend_name(self.handle.backend),
                ),
            )
            .await;
        }
    }

    /// A JSON object carrying a `type` this build has never heard of:
    /// dropped, and the sender told — [`shim::ShimRunner`]'s divergence from
    /// the in-process runner, for its reason.
    async fn drop_unknown(&self, name: Option<&str>, from: &str) {
        let named = name.unwrap_or("(unnamed)");
        self.remember(format!(
            "dropped frame-shaped message of unknown type {named}"
        ));
        tracing::warn!(
            teammate = self.spec.name.as_str(),
            from,
            kind = named,
            "a message shaped like a frame this build cannot read was dropped rather than \
             pasted into a foreign CLI's composer"
        );
        let Some(inbox) = self.sender_inbox(from) else {
            return;
        };
        self.mail(
            inbox,
            format!(
                "That message was not delivered. It is a JSON object carrying a \"type\" of \
                 {named:?}, which this build does not recognize as a frame, and {name} runs in \
                 the {cli} TUI — a message shaped like a frame is never pasted into that CLI's \
                 composer. Send prose, or a JSON document with no top-level \"type\" key.",
                name = self.spec.name.as_str(),
                cli = backend_name(self.handle.backend),
            ),
        )
        .await;
    }

    /// Ends this member and tells the lead it is done, as this member's own
    /// name.
    async fn tear_down(&self, request: &ShutdownRequest) {
        self.handle.end().await;
        let surface = Surface::Shim {
            cli: self.handle.cli(),
            pane: Some(self.handle.pane().id.clone()),
        };
        let approved = Frame::ShutdownApproved(ShutdownApproved {
            request_id: request.request_id.clone(),
            from: self.spec.name.as_str().to_owned(),
            timestamp: record::now_iso8601(),
            pane_id: Some(surface.tmux_pane_id().to_owned()),
            backend_type: Some(surface.backend_type().to_owned()),
        });
        runner::write_frame(
            self.lead_inbox.clone(),
            self.spec.name.as_str(),
            &approved,
            "a shutdown answer",
        )
        .await;
    }

    /// One line onto this member's ring.
    fn remember(&self, line: String) {
        shim::push_recent(&self.recent, line);
    }

    /// Writes one plain message into an inbox, as this member.
    async fn mail(&self, inbox: PathBuf, text: String) {
        let message = MailboxMessage::new(self.spec.name.as_str(), text, record::now_iso8601());
        if let Err(error) =
            crate::teammate::blocking_io(move || mailbox::write(&inbox, message)).await
        {
            tracing::error!(
                who = self.spec.name.as_str(),
                %error,
                "a TUI teammate's message could not be written, so nobody is being told"
            );
        }
    }

    /// Takes everything this pass finished out of the inbox, in one write.
    async fn prune(&self, handled: Vec<mailbox::Identity>) {
        runner::prune_inbox(self.inbox(), handled, self.spec.name.as_str()).await;
    }
}

/// The line typed into the pane's idle shell: `exec` the binary with the
/// driver's TUI words, each shell-quoted — [`tmux::launch_line`] over
/// [`TuiDriver::tui_argv`], spelled once so the spawn and the tests that pin
/// the quoting read the same composition.
///
/// # Errors
///
/// [`TmuxError::Unquotable`] for a word no shell quoting can carry.
pub fn launch_line(binary: &OsStr, driver: &dyn TuiDriver) -> Result<OsString, TmuxError> {
    tmux::launch_line(std::path::Path::new(binary), &driver.tui_argv())
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use super::{TuiDriver, environment_names, last_words, launch_line, paste_body};
    use crate::teammate::{
        agy::{self, Agy},
        codex::{self, Codex},
        grok::{self, Grok},
        pane::CARRIED_ENV,
        shim::Driver as _,
    };

    /// The companion trait says exactly what the inherent items say — it is
    /// a dispatch seam, not a second spelling (ruling 3).
    #[test]
    fn the_tui_driver_delegates_to_each_drivers_own_inherent_items() {
        let codex: &dyn TuiDriver = &Codex::new();
        assert_eq!(codex.tui_argv(), Codex::new().tui_argv());
        assert_eq!(codex.ready_marker(), codex::READY_MARKER);

        let grok: &dyn TuiDriver = &Grok::new();
        assert_eq!(grok.tui_argv(), Grok::new().tui_argv());
        assert_eq!(grok.ready_marker(), grok::READY_MARKER);

        let agy: &dyn TuiDriver = &Agy::new();
        assert_eq!(agy.tui_argv(), Agy::new().tui_argv());
        assert_eq!(agy.ready_marker(), agy::READY_MARKER);
    }

    /// Ruling 6, pinned: shlex single-quotes codex's `-c` values, the pane
    /// shell strips the quotes, and codex reads the TOML bytes exactly — so
    /// the composed line splits back into the very words the argv table
    /// holds, quotes inside the values included.
    #[test]
    fn the_codex_launch_line_round_trips_its_toml_values_through_the_shell() {
        let line = launch_line(OsStr::new("codex"), &Codex::new())
            .expect("no NUL rides these words")
            .into_string()
            .expect("ascii");
        assert_eq!(
            line,
            "exec codex -c 'sandbox_mode=\"read-only\"' -c 'approval_policy=\"never\"'"
        );

        let words = shlex::split(&line).expect("the line is a shell line");
        let mut expected = vec!["exec".to_owned(), "codex".to_owned()];
        expected.extend(
            Codex::new()
                .tui_argv()
                .into_iter()
                .map(|word| word.into_string().expect("ascii")),
        );
        assert_eq!(words, expected);
    }

    /// Every driver's line opens with `exec` and the binary and carries only
    /// that driver's own words after — no prompt, no identity flag.
    #[test]
    fn every_drivers_launch_line_is_exec_the_binary_and_its_floors() {
        let drivers: [(&dyn TuiDriver, &str); 3] = [
            (&Codex::new(), codex::BINARY),
            (&Grok::new(), grok::BINARY),
            (&Agy::new(), agy::BINARY),
        ];
        for (driver, binary) in drivers {
            let line = launch_line(OsStr::new(binary), driver)
                .expect("no NUL")
                .into_string()
                .expect("ascii");
            assert!(line.starts_with(&format!("exec {binary} ")), "{line}");
            for forbidden in [
                "--agent-id",
                "--parent-session-id",
                "--prompt",
                "exec resume",
            ] {
                assert!(!line.contains(forbidden), "{line} carries {forbidden}");
            }
        }
    }

    /// The pane's names are the `ganja` pane's closed list, then the driver's
    /// admitted additions — codex's `CODEX_HOME` travels, a `GROK_*` name
    /// never does, and nothing else is asked for.
    #[test]
    fn the_pane_environment_is_the_carried_list_then_the_admitted_additions() {
        let (codex, agy, grok) = (Codex::new(), Agy::new(), Grok::new());
        let names = environment_names(codex.additions());
        assert_eq!(&names[..CARRIED_ENV.len()], &CARRIED_ENV[..]);
        assert_eq!(&names[CARRIED_ENV.len()..], ["CODEX_HOME"]);

        let filtered = environment_names(&["CODEX_HOME", "GROK_SANDBOX", "GROK_HOME"]);
        assert_eq!(&filtered[CARRIED_ENV.len()..], ["CODEX_HOME"]);

        assert_eq!(environment_names(&[]), CARRIED_ENV.to_vec());
        for name in environment_names(agy.additions())
            .into_iter()
            .chain(environment_names(grok.additions()))
        {
            assert!(
                !name.contains("KEY") && !name.contains("PASSWORD") && !name.contains("TOKEN"),
                "{name} has no business on a pane's launch"
            );
        }
    }

    /// A refusal quotes the program's last line and never tmux's own notice
    /// under it; a pane that showed nothing quotes nothing.
    #[test]
    fn the_last_words_are_the_programs_last_line_and_never_tmuxs_dead_notice() {
        let captured = "\
warning: the sandbox profile could not be applied
error: could not apply the 'read-only' sandbox profile; see the warning above for the cause.

Pane is dead (status 1, Thu Aug 20 15:28:47 2026)
";
        assert_eq!(
            last_words(captured).as_deref(),
            Some(
                "error: could not apply the 'read-only' sandbox profile; see the warning above \
                 for the cause."
            )
        );
        assert_eq!(last_words("\n\nPane is dead (signal term, now)\n"), None);
        assert_eq!(last_words(""), None);
        assert_eq!(last_words("one line   \n"), Some("one line".to_owned()));
    }

    /// **HIGH-1.** A peer's own words cannot forge the bracketed-paste framing
    /// that carries them, from **either** field: the envelope is composed from
    /// `from` and `text` alike and the whole of it is neutralized, so a close
    /// sequence in a sender's *name* is as inert as one in the body.
    ///
    /// What goes is every control character — the `ESC` that arms a `[201~`
    /// into a paste terminator, the `\r` that would submit whatever it closed,
    /// and with them every character the pane's line discipline reads as a
    /// command rather than as text (`^C`, `^D`, `^Z`, `^U` are all controls, so
    /// none can reach the foreign CLI either). What stays is the two a composer
    /// reads as content, `\n` and `\t`, and every printable byte — the payload
    /// is defanged, not deleted, so a person looking at the pane sees what was
    /// sent to them.
    #[test]
    fn a_peers_words_cannot_forge_the_bracketed_paste_that_carries_them() {
        let hostile = "before\u{1b}[201~\rINJECTED\u{1b}[200~ /quit\u{7}\nafter\twith a tab";
        let body = paste_body("team-lead", hostile);
        assert_eq!(
            body, "A message from team-lead:\nbefore[201~INJECTED[200~ /quit\nafter\twith a tab",
            "the escapes are disarmed and the text is still readable"
        );

        // A hostile *sender name* is the same danger and takes the same route.
        let named = paste_body("w1\u{1b}[201~\rwhoami", "hello");
        assert_eq!(named, "A message from w1[201~whoami:\nhello");

        // Said as the invariant rather than as three examples: nothing that
        // survives is a control character, bar the two a composer reads.
        for composed in [&body, &named] {
            assert!(
                composed
                    .chars()
                    .all(|character| !character.is_control() || matches!(character, '\n' | '\t')),
                "{composed:?} still carries a control character"
            );
        }
        // Including the C1 forms, which are `Cc` too: a lone U+009B is the
        // single-character spelling of `ESC [`, and a filter that only knew
        // about `\u{1b}` would pass it straight through.
        assert_eq!(paste_body("w1", "\u{9b}201~x"), "A message from w1:\n201~x");
        // And a message that was never hostile is carried through untouched.
        assert_eq!(
            paste_body("w1", "hold the fort\nand report back"),
            "A message from w1:\nhold the fort\nand report back"
        );
    }

    /// The ring constants are sentences a person reads, not codes.
    #[test]
    fn the_ring_lines_say_what_happened_in_words() {
        assert!(super::RING_NOT_READY.contains("pasting anyway"));
        assert!(super::RING_DELIVERED.contains("pane"));
        assert!(super::RING_DELIVERY_FAILED.contains("failed"));
        let _: OsString = OsString::from(super::REFUSED_DIED);
    }
}
