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
//! `agentId` **and** the session id of the lead sweeping it. So the panes here
//! are shells started with the two flags a pane teammate carries
//! (`--agent-id <name>@<team> --parent-session-id <uuid>`), shells started
//! without them, and shells started with one lead's flags for another lead to
//! meet — which is exactly the distinction under test and nothing else. The
//! agent id is asked of [`MemberName::agent_id`] rather than spelled out, so it
//! cannot drift from what a record writes.
//!
//! # Why this binary may hold several tests
//!
//! Nothing here touches process-wide state: each test starts a tmux server of
//! its own on a socket in its own temporary directory, and the sweep is driven
//! through [`Server::at`] rather than `$TMUX`. Storage never enters it — a
//! [`TeammateRegistry`] over an explicit [`TeamsRoot`] is all a sweep reads.

use std::path::PathBuf;

use ganja_core::{
    team::{MemberName, Spawn, Surface, TeamFile, TeamName, TeamsRoot},
    teammate::{
        TeammateRegistry,
        reaper::{self, Fate},
        tmux::Server,
    },
};
use ganja_testkit::tmux::{PrivateServer, require_tmux};
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

/// A tmux server of this test's own ([`ganja_testkit::tmux`]'s, killed on
/// drop with its stand-in shells and their `sleep`s), holding a single pane
/// that outlives every test in it.
fn server() -> PrivateServer {
    require_tmux();

    PrivateServer::start(&["sleep", "3600"], &[], &[])
}

/// Whether a pane by that id is on the server right now.
fn holds(server: &PrivateServer, pane_id: &str) -> bool {
    server.panes().iter().any(|id| id == pane_id)
}

/// Ends a pane, for the test that wants a record with nothing behind it.
fn kill(server: &PrivateServer, pane_id: &str) {
    server.run(&["kill-pane", "-t", pane_id]);
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
        let home = ganja_testkit::temp_dir();
        let root = TeamsRoot::new(home.path().join("teams"));
        let name = TeamName::parse(team).expect("a team name");
        let cwd = home.path().to_string_lossy().into_owned();

        let members: Vec<(MemberName, Spawn)> = members
            .iter()
            .map(|(member, surface)| {
                (
                    MemberName::parse(member).expect("a member name"),
                    Spawn {
                        agent_type: "general".to_owned(),
                        model: "fake/fake".to_owned(),
                        color: "blue".to_owned(),
                        prompt: "watch the build".to_owned(),
                        plan_mode_required: false,
                        surface: surface.clone(),
                        cwd: cwd.clone(),
                    },
                )
            })
            .collect();
        let path = ganja_testkit::seed_team_file(&root, &name, lead_session, home.path(), &members);

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

/// Splits a pane that looks like the teammate `agent_id` names, launched by the
/// lead `parent_session` names: the two flags the witness reads, and nothing
/// else of a real one.
///
/// **Both** flags, because the witness wants both and
/// every real launch line carries both — `teammate::pane::arguments` and
/// `teammate::claude::arguments` put `--agent-id` and `--parent-session-id` on
/// §4.1's own five. A stand-in wearing one of them would be testing a pane this
/// build never spawns.
fn teammate_pane(server: &PrivateServer, agent_id: &str, parent_session: &str) -> String {
    server.split(
        None,
        &[],
        &[
            "/bin/sh",
            "-c",
            IDLE,
            "--agent-id",
            agent_id,
            "--parent-session-id",
            parent_session,
        ],
    )
}

/// Splits a pane belonging to nobody in this team — what a recycled `%N` is
/// wearing by the time a sweep meets it.
fn stranger_pane(server: &PrivateServer) -> String {
    server.split(None, &[], &["/bin/sh", "-c", IDLE])
}

/// A pane still running its teammate is an orphan of the lead that died: the
/// sweep ends it, and the record goes with it.
#[tokio::test]
async fn an_orphaned_pane_is_reaped_at_lead_startup() {
    let server = server();
    let pane = teammate_pane(&server, &agent_id("session-01998ad0", "worker"), SESSION);
    let team = Team::written("session-01998ad0", SESSION, &[("worker", on(&pane))]);
    assert!(holds(&server, &pane), "the stand-in pane is running");

    let swept = reaper::sweep_on(&team.registry(SESSION), &Server::at(server.socket(), None)).await;

    assert_eq!(
        swept.fate_of("worker"),
        Some(Fate::Reaped),
        "the orphan was ended: {swept:?}"
    );
    assert!(!holds(&server, &pane), "and its pane is gone: {pane}");
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
    let server = server();
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
        holds(&server, &pane),
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
    let server = server();
    let pane = teammate_pane(&server, &agent_id("session-01998ad0", "worker"), SESSION);
    kill(&server, &pane);
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
    let server = server();
    let pane = teammate_pane(&server, &agent_id("default", "worker"), OTHER_SESSION);
    let team = Team::written("default", OTHER_SESSION, &[("worker", on(&pane))]);

    let swept = reaper::sweep_on(&team.registry(SESSION), &Server::at(server.socket(), None)).await;

    assert!(swept.is_empty(), "nothing was even looked at: {swept:?}");
    assert!(
        holds(&server, &pane),
        "the other lead's teammate is still running"
    );
    assert_eq!(
        team.members(),
        vec!["team-lead".to_owned(), "worker".to_owned()],
        "and its record is untouched"
    );
}

/// The suffix-collision case: a member whose name is a *suffix* of a
/// sibling's — the
/// sibling's live pane must survive a sweep looking for the dead one.
///
/// `build@session-01998ad0` is a substring of `rebuild@session-01998ad0`, so the
/// witness this replaced (`argv.contains(agent_id)`) read `rebuild`'s pane as
/// `build`'s the moment tmux reissued `build`'s dead `%N` to it — and killed a
/// teammate that was working. Nothing about the two names is unusual; a team
/// with `build` and `rebuild` in it is a team somebody would write.
#[tokio::test]
async fn a_siblings_pane_is_not_killed_because_its_name_ends_with_the_dead_ones() {
    let server = server();
    // The live sibling, wearing its own agent id — and now wearing the pane id
    // the record names, which is what a recycled `%N` looks like from here.
    let pane = teammate_pane(&server, &agent_id("session-01998ad0", "rebuild"), SESSION);
    let team = Team::written("session-01998ad0", SESSION, &[("build", on(&pane))]);

    let swept = reaper::sweep_on(&team.registry(SESSION), &Server::at(server.socket(), None)).await;

    assert_eq!(
        swept.fate_of("build"),
        Some(Fate::Recycled),
        "`rebuild`'s pane is not `build`'s, however the two names read: {swept:?}"
    );
    assert!(
        holds(&server, &pane),
        "and the sibling is still working: {pane} on {:?}",
        server.socket()
    );
}

/// The co-tenant case: two leads inside one 65.536-second team-name bucket share a
/// team *file*, and the document keeps naming whichever of them wrote it first.
/// A sweep by that lead must leave the co-tenant's **live** panes alone.
///
/// The file is stamped [`SESSION`], so the `leadSessionId` guard passes — this
/// is that lead's own team by every fact on disk. What says otherwise is the
/// pane itself: it carries [`OTHER_SESSION`] as its `--parent-session-id`,
/// because lead B launched it and B is still running. Before the witness read
/// that flag, this sweep killed B's teammates.
///
/// The record still goes, and that is the documented trade rather than an
/// oversight: a `Recycled` verdict drops a row over a pane that is not this
/// teammate's, and telling "somebody else's window" from "another lead's
/// teammate" would need a fate this module does not have. The kill is the harm
/// that had to stop.
#[tokio::test]
async fn a_co_tenant_leads_live_panes_survive_a_sweep_of_the_team_file_they_share() {
    let server = server();
    let pane = teammate_pane(
        &server,
        &agent_id("session-01998ad0", "worker"),
        OTHER_SESSION,
    );
    let team = Team::written("session-01998ad0", SESSION, &[("worker", on(&pane))]);

    let swept = reaper::sweep_on(&team.registry(SESSION), &Server::at(server.socket(), None)).await;

    assert_eq!(
        swept.fate_of("worker"),
        Some(Fate::Recycled),
        "a pane another lead launched is not this lead's to end: {swept:?}"
    );
    assert!(
        holds(&server, &pane),
        "and the other lead's teammate is still working: {pane} on {:?}",
        server.socket()
    );
}

/// A team of in-process teammates has no panes to sweep, so a sweep reads the
/// file, decides there is nothing to do, and writes nothing.
#[tokio::test]
async fn a_team_with_no_panes_is_swept_silently() {
    let server = server();
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
