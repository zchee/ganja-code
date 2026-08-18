//! What the two pane binaries share: the pane child, the private tmux server,
//! and the `task` door.
//!
//! Not a test binary of its own — cargo does not discover `tests/*/mod.rs` as
//! one — but a module both `teammate_pane_lifecycle.rs` and
//! `teammate_pane_env.rs` declare, so the child a pane runs is written once.
//!
//! # The pane child is this very binary
//!
//! `pane.rs` launches `current_exe()`, and inside a test binary that is the
//! test binary itself, so it is the test binary that gets started in the pane,
//! carrying `--agent-id` and the other four flags on its command line. libtest
//! would refuse those flags on sight and the pane would close in milliseconds,
//! which is why both binaries are `harness = false` in `Cargo.toml` and open by
//! asking [`pane_child_if_asked`] whether they are the child. The child does
//! what a `ganja` pane's first breath does: it finds its team through
//! `GANJA_CONFIG_HOME` — the D502 variable — and writes to the lead's inbox.
//! Then it waits to be killed, because a pane that exits on its own is a pane
//! `kill-pane` never gets to prove anything about.
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

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use ganja_core::{
    Backends, Caller, SpawnAsk, SpawnAsker, Storage, Teammates,
    permission::Permissions,
    protocol::PermissionReply,
    provider::FakeProvider,
    teammate::{
        InProcess, TeammateRegistry,
        claude::ClaudePane,
        pane::{AGENT_ID, AGENT_NAME, GanjaPane, TEAM_NAME},
    },
    tool::{
        Credentials, FileTimes, Registry, ToolCtx,
        task::{
            Delegated, Delegation, NotSpawned, Subagents, TeammateSpawn, Teammated, Unanswered,
        },
    },
};
use ganja_team::{MailboxMessage, MemberName, TeamName, TeamsRoot, mailbox, record};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// The lead's session, and therefore the team's name: `session-01998ad0`.
pub const SESSION_ID: &str = "01998ad0-0000-7000-8000-000000000000";

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
    let argv: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
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
    let mut env_names: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    env_names.sort();
    let report = Report {
        argv,
        config_home,
        env_names,
    };
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

    // Wait to be killed. A pane teammate lives until its lead ends it.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
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
    require_tmux();
    println!("running 1 test");
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(test);
    println!("test {name} ... ok");
    println!("test result: ok. 1 passed; 0 failed; 0 ignored");
}

/// Refuses to run without tmux, by name: a green pane test that spawned no
/// pane would be worth nothing.
pub fn require_tmux() {
    let version = Command::new("tmux").arg("-V").output();
    assert!(
        version.as_ref().is_ok_and(|output| output.status.success()),
        "the pane tests need tmux on PATH and there is none: {version:?}"
    );
}

/// A tmux server of this test's own, on a socket nobody else knows.
///
/// Killed when dropped, panics included, so a failing test leaves no server
/// behind holding a pane of this binary open.
pub struct PrivateServer {
    socket: PathBuf,
    /// The pane the server was born with — what the lead "runs in".
    first_pane: String,
    _dir: TempDir,
}

impl PrivateServer {
    /// Starts a detached server whose first pane sleeps, with `withheld` taken
    /// **out** of the environment the server is born with — which is how a
    /// test stages "the server predates the export".
    pub fn start(withheld: &[&str]) -> Self {
        let dir = ganja_testkit::temp_dir();
        let socket = dir.path().join("tmux.sock");
        let mut command = Command::new("tmux");
        command
            .arg("-S")
            .arg(&socket)
            .arg("-f")
            .arg("/dev/null")
            .args([
                "new-session",
                "-d",
                "-s",
                "ganja-test",
                "-x",
                "200",
                "-y",
                "50",
            ])
            .args(["sleep", "3600"]);
        for name in withheld {
            command.env_remove(name);
        }
        let started = command.output().expect("tmux starts a private server");
        assert!(
            started.status.success(),
            "the private tmux server did not start: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        let listing = tmux(&socket, &["list-panes", "-a", "-F", "#{pane_id}"]);
        let first_pane = listing.trim().to_owned();
        assert!(
            first_pane.starts_with('%'),
            "the private server has a first pane: {listing:?}"
        );

        Self {
            socket,
            first_pane,
            _dir: dir,
        }
    }

    /// The socket, for `Server::at` and for `$TMUX`.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Points this process at the private server the way tmux would have:
    /// `$TMUX` and `$TMUX_PANE`.
    ///
    /// # Safety
    ///
    /// Mutates process-wide environment; the calling binary holds one test.
    pub unsafe fn enter(&self) {
        // SAFETY: the caller's binary holds exactly one test.
        unsafe {
            std::env::set_var("TMUX", format!("{},0,0", self.socket.display()));
            std::env::set_var("TMUX_PANE", &self.first_pane);
        }
    }

    /// Whether the server's **global** environment — what every pane it makes
    /// inherits — holds `name`.
    pub fn global_has(&self, name: &str) -> bool {
        Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(["show-environment", "-g", name])
            .output()
            .expect("tmux runs")
            .status
            .success()
    }

    /// The pane's title, as `select-pane -T` set it.
    pub fn title(&self, pane_id: &str) -> String {
        tmux(
            &self.socket,
            &["display-message", "-p", "-t", pane_id, "#{pane_title}"],
        )
        .trim()
        .to_owned()
    }

    /// The command a pane was started with, as tmux itself records it.
    pub fn start_command(&self, pane_id: &str) -> String {
        tmux(
            &self.socket,
            &[
                "display-message",
                "-p",
                "-t",
                pane_id,
                "#{pane_start_command}",
            ],
        )
    }
}

impl Drop for PrivateServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

/// One tmux client call against `socket`, or a panic in tmux's own words.
fn tmux(socket: &Path, args: &[&str]) -> String {
    let output = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(args)
        .output()
        .expect("tmux runs");
    assert!(
        output.status.success(),
        "tmux {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Says yes to everything, and is asked nothing: every spawn here works
/// inside its own project and asks for no bypass.
#[derive(Debug)]
pub struct Yes;

#[async_trait]
impl SpawnAsker for Yes {
    async fn ask(&self, _request: SpawnAsk) -> PermissionReply {
        PermissionReply::Once
    }
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
        self.teammates.start(request, &self.caller, &Yes).await
    }
}

/// The lead's team over `config_home`, exactly as a frontend installs it —
/// [`TeammateRegistry::for_session`] — with production's three backends.
pub fn lead(config_home: &Path, project: &Path) -> (Arc<TeammateRegistry>, Door) {
    let registry = Arc::new(TeammateRegistry::for_session(
        config_home,
        SESSION_ID,
        project,
    ));
    let door = Door {
        teammates: Teammates::new(
            Arc::clone(&registry),
            Backends {
                in_process: Arc::new(InProcess::new(
                    Arc::new(FakeProvider::new("on it", Duration::ZERO)),
                    Arc::new(Registry::new(Vec::new())),
                    Storage::open(project.join("storage")),
                    |_| Permissions::default(),
                )),
                pane: Arc::new(GanjaPane),
                claude: Arc::new(ClaudePane),
            },
        ),
        caller: Caller {
            model: "recorder-model".to_owned(),
            cwd: project.to_path_buf(),
            permissions: Arc::new(Mutex::new(Permissions::default())),
            project_root: project.to_path_buf(),
        },
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

/// Polls `read` every 100ms until it answers, or panics with `what` after
/// `limit`.
pub async fn wait_for<T>(
    limit: Duration,
    what: &str,
    mut read: impl AsyncFnMut() -> Option<T>,
) -> T {
    let started = Instant::now();
    loop {
        if let Some(found) = read().await {
            return found;
        }
        assert!(
            started.elapsed() < limit,
            "waited {limit:?} for {what} and it did not happen"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The lead's team, its name and where its documents are, off the same
/// config home the child resolves.
pub fn team_of(registry: &TeammateRegistry) -> (TeamsRoot, TeamName) {
    (registry.root().clone(), registry.team().clone())
}
