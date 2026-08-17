//! **AC-12** — the orphan reaper, against a real tmux server (**D506**).
//!
//! What a lead does at startup to the panes a *previous* lead left running:
//! ends the ones that are still its teammates', drops the records of the ones
//! that are gone, and — the half this binary exists to prove — leaves a pane
//! that wears a recorded id while running somebody else's work completely
//! alone. `%N` recycles (§10.10), so that near-miss is the failure the whole
//! module is shaped around, and a test that only watched a reaping succeed
//! would be watching the easy half.
//!
//! # The panes are stand-ins, and that is the point
//!
//! A real teammate's pane is `crates/ganja-core/tests/teammate_pane_lifecycle.rs`'s
//! business — a whole spawn, a whole engine, a whole `ganja` in the window.
//! What the reaper reads of a pane is narrower than any of that: whether it is
//! live, and whether its first process's command line carries the member's
//! `agentId`. So the panes here are shells started with the same flag a pane
//! teammate carries (`--agent-id <name>@<team>`) and shells started without
//! one, which is exactly the distinction under test and nothing else. The
//! agent id is asked of [`MemberName::agent_id`] rather than spelled out, so it
//! cannot drift from what a record writes.
//!
//! # Why this binary may hold several tests
//!
//! Nothing here touches process-wide state: each test starts a tmux server of
//! its own on a socket in its own temporary directory, and the sweep is driven
//! through [`Server::at`] rather than `$TMUX`. Storage never enters it — a
//! [`TeammateRegistry`] over an explicit [`TeamsRoot`] is all a sweep reads.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use ganja_core::{
    team::{MemberName, MemberRecord, Spawn, Surface, TeamFile, TeamName, TeamsRoot, record},
    teammate::{
        TeammateRegistry,
        reaper::{self, Fate},
        tmux::Server,
    },
};
use tempfile::TempDir;

/// The lead's session, and therefore §2.1's team name: `session-01998ad0`.
const SESSION: &str = "01998ad0-0000-7000-8000-000000000000";

/// Some other lead's session, for the team a sweep must not touch.
const OTHER_SESSION: &str = "01998ad0-1111-7000-8000-000000000000";

/// What a stand-in pane runs: long enough that no test races its exit, and a
/// **list** rather than one simple command.
///
/// The trailing `; :` is load-bearing, and what it works around is the same
/// mechanism production depends on. `sh -c 'sleep 300'` is optimized into an
/// `exec`, so the shell's own argv — the flags after the command string among
/// them — is replaced by `sleep 300` a moment after the split, and a witness
/// reading `#{pane_pid}`'s command line would find nothing of the teammate.
/// That is exactly how a real pane teammate's argv *becomes* `ganja
/// --agent-id …` — `teammate::pane` types `exec` into the shell — and here the
/// point is the opposite, so the list keeps the shell alive wearing the flags
/// it was given.
const IDLE: &str = "sleep 300; :";

/// A tmux server of this test's own, on a socket nothing else knows.
struct PrivateServer {
    socket: PathBuf,
    _dir: TempDir,
}

impl PrivateServer {
    /// Starts one, holding a single pane that outlives every test in it.
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory for the socket");
        let socket = dir.path().join("tmux.sock");
        let started = Command::new("tmux")
            .arg("-S")
            .arg(&socket)
            .args([
                "new-session",
                "-d",
                "-s",
                "ganja-reaper",
                "-x",
                "200",
                "-y",
                "50",
            ])
            .args(["sleep", "3600"])
            .output()
            .expect("tmux starts a private server");
        assert!(
            started.status.success(),
            "the private tmux server did not start: {}",
            String::from_utf8_lossy(&started.stderr)
        );

        Self { socket, _dir: dir }
    }

    /// The socket, for [`Server::at`].
    fn socket(&self) -> &Path {
        &self.socket
    }

    /// Splits a pane running `argv` and answers its id.
    fn split(&self, argv: &[&str]) -> String {
        let id = self
            .tmux(
                &["split-window", "-d", "-P", "-F", "#{pane_id}", "--"],
                argv,
            )
            .trim()
            .to_owned();
        assert!(id.starts_with('%'), "a split answers a pane id: {id:?}");

        id
    }

    /// Whether a pane by that id is on the server right now.
    fn holds(&self, pane_id: &str) -> bool {
        self.tmux(&["list-panes", "-a", "-F", "#{pane_id}"], &[])
            .lines()
            .any(|line| line.trim() == pane_id)
    }

    /// Ends a pane, for the test that wants a record with nothing behind it.
    fn kill(&self, pane_id: &str) {
        self.tmux(&["kill-pane", "-t", pane_id], &[]);
    }

    /// One client call against this server, and its stdout.
    fn tmux(&self, args: &[&str], rest: &[&str]) -> String {
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .args(rest)
            .output()
            .expect("the tmux client runs");
        assert!(
            output.status.success(),
            "tmux {args:?} {rest:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

impl Drop for PrivateServer {
    fn drop(&mut self) {
        // Takes the stand-in shells and their `sleep`s with it, however a test
        // ended.
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

/// A team on disk: the root, the name, and the file a sweep reads.
struct Team {
    _home: TempDir,
    root: TeamsRoot,
    name: TeamName,
    path: PathBuf,
}

impl Team {
    /// Writes a team file led by `lead_session`, holding one member per
    /// `(name, surface)` pair.
    fn written(team: &str, lead_session: &str, members: &[(&str, Surface)]) -> Self {
        let home = tempfile::tempdir().expect("a temporary config home");
        let root = TeamsRoot::new(home.path().join("teams"));
        let name = TeamName::parse(team).expect("a team name");
        let cwd = home.path().to_string_lossy().into_owned();

        let mut file = TeamFile::new(&name, lead_session, cwd.clone(), record::now_millis());
        for (member, surface) in members {
            file.members.push(MemberRecord::teammate(
                &MemberName::parse(member).expect("a member name"),
                &name,
                Spawn {
                    agent_type: "general".to_owned(),
                    model: "fake/fake".to_owned(),
                    color: "blue".to_owned(),
                    prompt: "watch the build".to_owned(),
                    plan_mode_required: false,
                    surface: surface.clone(),
                    cwd: cwd.clone(),
                },
                record::now_millis(),
            ));
        }

        let path = root.config_path(&name);
        std::fs::create_dir_all(path.parent().expect("a team directory"))
            .expect("the team directory is made");
        std::fs::write(
            &path,
            record::document(&file).expect("the team file encodes"),
        )
        .expect("the team file is written");

        Self {
            _home: home,
            root,
            name,
            path,
        }
    }

    /// The registry a lead leading `session` would sweep with.
    fn registry(&self, session: &str) -> TeammateRegistry {
        TeammateRegistry::new(
            self.root.clone(),
            self.name.clone(),
            session,
            self.path.parent().expect("a team directory"),
        )
    }

    /// Every member the file names now, in its own order.
    fn members(&self) -> Vec<String> {
        let file: TeamFile =
            serde_json::from_str(&std::fs::read_to_string(&self.path).expect("the file is read"))
                .expect("the team file decodes");

        file.members.into_iter().map(|member| member.name).collect()
    }
}

/// The pane surface a record names.
fn on(pane_id: &str) -> Surface {
    Surface::Pane {
        id: pane_id.to_owned(),
    }
}

/// `<name>@<team>`, built the way a member record builds it — so a test can
/// never hand a pane an agent id the record would not have written.
fn agent_id(team: &str, member: &str) -> String {
    MemberName::parse(member)
        .expect("a member name")
        .agent_id(&TeamName::parse(team).expect("a team name"))
}

/// Splits a pane that looks like the teammate `agent_id` names: the flag a real
/// pane teammate is launched with, and nothing else of one.
fn teammate_pane(server: &PrivateServer, agent_id: &str) -> String {
    server.split(&["/bin/sh", "-c", IDLE, "--agent-id", agent_id])
}

/// Splits a pane belonging to nobody in this team — what a recycled `%N` is
/// wearing by the time a sweep meets it.
fn stranger_pane(server: &PrivateServer) -> String {
    server.split(&["/bin/sh", "-c", IDLE])
}

/// A pane still running its teammate is an orphan of the lead that died: the
/// sweep ends it, and the record goes with it.
#[tokio::test]
async fn an_orphaned_pane_is_reaped_at_lead_startup() {
    let server = PrivateServer::start();
    let pane = teammate_pane(&server, &agent_id("session-01998ad0", "worker"));
    let team = Team::written("session-01998ad0", SESSION, &[("worker", on(&pane))]);
    assert!(server.holds(&pane), "the stand-in pane is running");

    let swept = reaper::sweep_on(&team.registry(SESSION), &Server::at(server.socket(), None)).await;

    assert_eq!(
        swept.fate_of("worker"),
        Some(Fate::Reaped),
        "the orphan was ended: {swept:?}"
    );
    assert!(!server.holds(&pane), "and its pane is gone: {pane}");
    assert_eq!(
        team.members(),
        vec!["team-lead".to_owned()],
        "and the team file stopped naming it"
    );
}

/// The near-miss: a live pane wearing a recorded id, running somebody else's
/// work. It must survive — the record is what is stale, not the window.
#[tokio::test]
async fn a_recycled_pane_id_is_not_killed() {
    let server = PrivateServer::start();
    // No `--agent-id` on it at all: tmux reissued `%N` to a stranger's shell.
    let pane = stranger_pane(&server);
    let team = Team::written("session-01998ad0", SESSION, &[("worker", on(&pane))]);

    let swept = reaper::sweep_on(&team.registry(SESSION), &Server::at(server.socket(), None)).await;

    assert_eq!(
        swept.fate_of("worker"),
        Some(Fate::Recycled),
        "a pane that cannot show the agent id is not the teammate's: {swept:?}"
    );
    assert!(
        server.holds(&pane),
        "and it is still running: {pane} on {:?}",
        server.socket()
    );
    assert_eq!(
        team.members(),
        vec!["team-lead".to_owned()],
        "the stale record still went, because the teammate it named runs nowhere"
    );
}

/// A record over a pane that is simply gone: nothing to kill, and a row the
/// next lead should not have to look at again.
#[tokio::test]
async fn a_record_whose_pane_is_gone_is_dropped_without_a_kill() {
    let server = PrivateServer::start();
    let pane = teammate_pane(&server, &agent_id("session-01998ad0", "worker"));
    server.kill(&pane);
    let team = Team::written("session-01998ad0", SESSION, &[("worker", on(&pane))]);

    let swept = reaper::sweep_on(&team.registry(SESSION), &Server::at(server.socket(), None)).await;

    assert_eq!(swept.fate_of("worker"), Some(Fate::Vanished), "{swept:?}");
    assert_eq!(team.members(), vec!["team-lead".to_owned()]);
}

/// The shared fallback team, where two leads really can meet: a document whose
/// `leadSessionId` is not this lead's is another lead's team, and its panes are
/// its own lead's to end.
#[tokio::test]
async fn a_team_led_by_another_session_is_left_whole() {
    let server = PrivateServer::start();
    let pane = teammate_pane(&server, &agent_id("default", "worker"));
    let team = Team::written("default", OTHER_SESSION, &[("worker", on(&pane))]);

    let swept = reaper::sweep_on(&team.registry(SESSION), &Server::at(server.socket(), None)).await;

    assert!(swept.is_empty(), "nothing was even looked at: {swept:?}");
    assert!(
        server.holds(&pane),
        "the other lead's teammate is still running"
    );
    assert_eq!(
        team.members(),
        vec!["team-lead".to_owned(), "worker".to_owned()],
        "and its record is untouched"
    );
}

/// A team of in-process teammates has no panes to sweep, so a sweep reads the
/// file, decides there is nothing to do, and writes nothing.
#[tokio::test]
async fn a_team_with_no_panes_is_swept_silently() {
    let server = PrivateServer::start();
    let team = Team::written(
        "session-01998ad0",
        SESSION,
        &[("worker", Surface::InProcess)],
    );

    let swept = reaper::sweep_on(&team.registry(SESSION), &Server::at(server.socket(), None)).await;

    assert!(swept.is_empty(), "{swept:?}");
    assert_eq!(
        team.members(),
        vec!["team-lead".to_owned(), "worker".to_owned()],
        "an in-process member's record is not this sweep's to judge"
    );
}
