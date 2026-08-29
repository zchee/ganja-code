//! What the three pane binaries share: the pane child, the spawn-and-report
//! spine, and the `task` door.
//!
//! Not a test binary of its own — cargo does not discover `tests/*/mod.rs` as
//! one — but a module `teammate_pane_lifecycle.rs`, `teammate_pane_env.rs` and
//! `teammate_pane_exit.rs` all declare, so the child a pane runs is written
//! once.
//! The private tmux server itself is [`ganja_testkit::tmux::PrivateServer`].
//!
//! # The pane child is this very binary
//!
//! `pane.rs` launches `current_exe()`, and inside a test binary that is the
//! test binary itself, so it is the test binary that gets started in the pane,
//! carrying `--agent-id` and the other four flags on its command line. libtest
//! would refuse those flags on sight and the pane would close in milliseconds,
//! which is why all three binaries are `harness = false` in `Cargo.toml` and
//! open by asking [`pane_child_if_asked`] whether they are the child. The child does
//! what a `ganja` pane's first breath does: it finds its team through
//! `GANJA_CONFIG_HOME` — the D502 variable — and writes to the lead's inbox.
//! Then it waits to be killed, because a pane that exits on its own is a pane
//! `kill-pane` never gets to prove anything about — for [`CHILD_LIFE`], which
//! is long past any run of these tests and is what keeps a server orphaned by
//! a signal from outliving the day.
//!
//! What the child writes is a report a test can read back: its argv as it
//! received it, the config home it resolved, and the **names** — never the
//! values — of every variable in its environment. That is the argv-secrets
//! and D502 evidence in one message: the flags are what `pane.rs` composed, the
//! home is the lead's, and a credential planted in the lead's environment is a
//! name that must not appear.

// Each pane binary compiles this module separately and uses only part of it
// (`title` is lifecycle's alone, `global_has`/`start_command` env's), so the
// unused half differs per binary and a targeted allow cannot name it.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ganja_core::teammate::TeammateRegistry;
use ganja_core::teammate::lead_inbox::LeadInbox;
use ganja_core::tool::task::{
    Delegated, Delegation, NotSpawned, Offered, Subagents, TaskTool, TeammateSpawn, Teammated,
    Unanswered,
};
use ganja_core::tool::{Credentials, FileTimes, Tool as _, ToolCtx};
use ganja_core::{Caller, Storage, Teammates};
use ganja_team::{MailboxMessage, MemberName, MemberRecord, TeamName, TeamsRoot, mailbox, record};
use ganja_teammate_local::pane::{AGENT_COLOR, AGENT_ID, AGENT_NAME, PARENT_SESSION_ID, TEAM_NAME};
use ganja_testkit::AllowSpawn;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// The lead's session, and therefore the team's name: `session-01998ad0`.
pub const SESSION_ID: &str = "01998ad0-0000-7000-8000-000000000000";

/// How long the pane's process gets to start and report. Generous: it is a
/// debug test binary being exec'd cold on a machine running the rest of the
/// suite.
pub const CHILD_STARTS: Duration = Duration::from_secs(30);

/// How long the pane child waits to be killed before ending on its own, and
/// the same bound spelled the way tmux takes it, for the first window every
/// pane binary's private server is born with.
///
/// Both are hygiene rather than behaviour, and the two together are the whole
/// of it: [`ganja_testkit::tmux::PrivateServer`] kills its server when it is
/// dropped, which covers every road out of a test that *unwinds* — a pass, a
/// failed assertion, a panic — but no `Drop` runs for a process killed by a
/// signal, which is what nextest's `terminate-after` cap does to a wedged
/// test and what a harness does to a lane it tears down. Such a server was
/// then immortal: tmux exits when its last pane does, and a first window
/// sleeping an hour beside a child sleeping forever meant it never did, so
/// the orphans had to be found and killed by hand. Bounded, the same orphan
/// empties itself and the server goes with it.
///
/// Five minutes is picked from both sides: far past any run of these tests
/// (seconds, with the longest wait in one 30 seconds) and past nextest's own
/// four-minute kill, so it can never end a pane a live test still wants;
/// short enough that an orphan is a nuisance for one coffee rather than
/// until the machine reboots.
pub const CHILD_LIFE: Duration = Duration::from_secs(300);

/// What the first window runs — see [`CHILD_LIFE`], whose number this is.
pub const IDLE_WINDOW: [&str; 2] = ["sleep", "300"];

/// What the pane child tells the lead about itself.
#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    /// The child's argv, after the binary.
    pub argv: Vec<String>,
    /// `GANJA_CONFIG_HOME` as the child resolved it — the D502 fact.
    pub config_home: Option<String>,
    /// Every environment variable the child was started with, by name only.
    pub env_names: Vec<String>,
}

/// If this process was started as a pane child, be one and never return.
///
/// The child branch of `main`: called before anything else, on the argv alone.
pub fn pane_child_if_asked() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if !args.iter().any(|arg| arg == AGENT_ID) {
        return;
    }
    let argv: Vec<String> = args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
    let value_of = |flag: &str| -> String {
        argv.iter()
            .position(|arg| arg == flag)
            .and_then(|at| argv.get(at + 1))
            .cloned()
            .unwrap_or_else(|| panic!("the pane child was started without {flag}: {argv:?}"))
    };
    let name =
        MemberName::parse(&value_of(AGENT_NAME)).expect("the pane's own name is a member name");
    let team = TeamName::parse(&value_of(TEAM_NAME)).expect("the pane's team is a team name");

    // The way a real pane finds its team: through the config home this
    // process sees, which is the whole of what D502 carries.
    let config_home = std::env::var(ganja_core::config::CONFIG_HOME_ENV).ok();
    let home = ganja_core::config::config_home().expect("a pane child resolves a config home");
    let root = TeammateRegistry::for_session(&home, SESSION_ID, std::env::current_dir().unwrap())
        .root()
        .clone();
    let mut env_names: Vec<String> =
        std::env::vars_os().map(|(name, _)| name.to_string_lossy().into_owned()).collect();
    env_names.sort();
    let report = Report { argv, config_home, env_names };
    let inbox = root.inbox_path(&team, &MemberName::lead());
    mailbox::seed(&inbox).expect("the lead's inbox seeds");
    mailbox::write(
        &inbox,
        MailboxMessage::new(
            name.as_str(),
            serde_json::to_string(&report).expect("a report encodes"),
            record::now_iso8601(),
        ),
    )
    .expect("the pane child reports to its lead");

    // Wait to be killed. A pane teammate lives until its lead ends it — and
    // this one until [`CHILD_LIFE`] is up, so that a server whose test was
    // killed by a signal rather than dropped is left holding a pane that ends
    // on its own. The exit is explicit because returning from here would fall
    // into `run_one` and run the test a second time, in the pane.
    std::thread::sleep(CHILD_LIFE);
    std::process::exit(0);
}

/// Runs this binary's one test under the libtest-shaped protocol nextest
/// speaks to a `harness = false` binary: `--list --format terse` names the
/// test (and names nothing under `--ignored`, since it is not), and a run
/// carrying a filter runs it only when the filter is its name.
///
/// Called after [`pane_child_if_asked`], so a pane child never gets here.
pub fn run_one(name: &str, test: impl std::future::Future<Output = ()>) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--list") {
        if !args.iter().any(|arg| arg == "--ignored") {
            println!("{name}: test");
        }
        return;
    }
    if let Some(filter) = args.iter().find(|arg| !arg.starts_with('-'))
        && filter != name
    {
        println!("running 0 tests");
        return;
    }
    ganja_testkit::tmux::require_tmux();
    println!("running 1 test");
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(test);
    println!("test {name} ... ok");
    println!("test result: ok. 1 passed; 0 failed; 0 ignored");
}

/// The engine-side seam a `task` call reaches, with only the teammate half
/// wired — `teammate_doors.rs`'s shape, for its reason.
#[derive(Debug)]
pub struct Door {
    pub teammates: Teammates,
    pub caller: Caller,
}

#[async_trait]
impl Subagents for Door {
    async fn delegate(
        &self,
        _request: Delegation,
        _cancel: CancellationToken,
    ) -> Result<Delegated, Unanswered> {
        Err(Unanswered::Unknown)
    }

    async fn spawn_teammate(&self, request: TeammateSpawn) -> Result<Teammated, NotSpawned> {
        self.teammates.start(request, &self.caller, &AllowSpawn).await
    }
}

/// The lead's team over `config_home`, exactly as a frontend installs it —
/// [`TeammateRegistry::for_session`] — with production's three backends.
pub fn lead(config_home: &Path, project: &Path) -> (Arc<TeammateRegistry>, Door) {
    let registry = Arc::new(TeammateRegistry::for_session(config_home, SESSION_ID, project));
    let door = Door {
        teammates: Teammates::new(
            Arc::clone(&registry),
            ganja_testkit::backends(Storage::open(project.join("storage"))),
        ),
        caller: ganja_testkit::caller(project),
    };

    (registry, door)
}

/// A `ToolCtx` for the task door, over `project`.
pub fn ctx(project: &Path, door: Arc<Door>) -> ToolCtx {
    ToolCtx {
        cwd: project.to_path_buf(),
        cancel: CancellationToken::new(),
        call_id: "call_1".to_owned(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: Some(door as Arc<dyn Subagents>),
        postbox: None,
        ask: None,
        switch: None,
        jobs: None,
    }
}

/// What the model calls `task` with to start `name` on `backend`.
pub fn task_args(name: &str, backend: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "description": "hold the fort",
        "prompt": prompt,
        "subagent_type": "general",
        "name": name,
        "backend": backend,
    })
}

/// The lead's team, its name and where its documents are, off the same
/// config home the child resolves.
pub fn team_of(registry: &TeammateRegistry) -> (TeamsRoot, TeamName) {
    (registry.root().clone(), registry.team().clone())
}

/// What [`spawn_pane_worker`] hands back: the lead's side of the team, the
/// member record the spawn wrote, and the report the pane child sent.
pub struct Spawned {
    pub registry: Arc<TeammateRegistry>,
    pub root: TeamsRoot,
    pub team: TeamName,
    pub member: MemberRecord,
    pub pane_id: String,
    pub report: Report,
    pub inbox: LeadInbox,
}

/// The spine every pane binary walks: a lead over `config_home`, a `worker`
/// spawned on the `pane` surface through the `task` door with `prompt`, the
/// member record it wrote, and the child's report read back through the
/// lead's own §6.2 pass. Each binary keeps only what it asserts beyond that.
pub async fn spawn_pane_worker(config_home: &Path, project: &Path, prompt: &str) -> Spawned {
    let (registry, door) = lead(config_home, project);
    let (root, team) = team_of(&registry);
    let door = Arc::new(door);
    let tool = TaskTool::new(&[Offered { name: "general".to_owned(), description: None }]);
    let ctx = ctx(project, Arc::clone(&door));

    let output = tool
        .run(task_args("worker", "ganja", prompt), &ctx)
        .await
        .expect("the door spawns a pane teammate inside tmux");
    assert_eq!(
        output.metadata.get("backend").and_then(|on| on.as_str()),
        Some("ganja"),
        "the surface it really runs on: {output:?}"
    );

    let file = ganja_testkit::team_file(&root, &team).expect("the team file is written");
    let member = file
        .member("worker")
        .unwrap_or_else(|| panic!("the pane teammate joined the team: {file:?}"))
        .clone();
    assert!(
        member.tmux_pane_id.starts_with('%'),
        "§2.2's tmuxPaneId is the pane's own id: {member:?}"
    );
    let pane_id = member.tmux_pane_id.clone();

    // The pane's process is the child branch of this binary, running as the
    // teammate: it finds the team through the carried config home and reports
    // to the lead — read through the lead's own inbox pass, the way a real
    // lead reads it.
    let inbox = LeadInbox::new(Arc::clone(&registry));
    let report = ganja_testkit::eventually(
        CHILD_STARTS,
        "the pane's report to reach the lead",
        async || {
            let pass = inbox.poll().await;
            pass.messages.into_iter().find(|message| message.from == "worker").map(|message| {
                serde_json::from_str::<Report>(&message.body).unwrap_or_else(|error| {
                    panic!("the pane wrote a report: {error} in {message:?}")
                })
            })
        },
    )
    .await;

    Spawned { registry, root, team, member, pane_id, report, inbox }
}

/// The five spawn flags and their values, in `pane.rs`'s order — what the
/// child's argv must be, whole.
pub fn expected_argv(team: &TeamName, member: &MemberRecord) -> [String; 10] {
    [
        AGENT_ID.to_owned(),
        format!("{}@{}", member.name, team.as_str()),
        AGENT_NAME.to_owned(),
        member.name.clone(),
        TEAM_NAME.to_owned(),
        team.as_str().to_owned(),
        AGENT_COLOR.to_owned(),
        member.color.as_deref().expect("a spawn assigns a colour").to_owned(),
        PARENT_SESSION_ID.to_owned(),
        SESSION_ID.to_owned(),
    ]
}
