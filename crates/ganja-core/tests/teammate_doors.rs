//! The `task` tool's teammate door, end to end (**AC-14**, the `task` half).
//!
//! Spec: Claude Code's teammates — §4.1's spawn sequence. Upstream opencode
//! has no teammates and no counterpart to any of it.
//!
//! AC-14's claim is "both doors, one sequence", and it is pinned in three
//! places because no one binary can see all three: this file holds the `task`
//! door's engine-side half, `crates/ganja-tui/src/component/team.rs` holds the
//! `/team spawn` dialog's half, and `teammate_backends.rs` holds the
//! per-backend refusals on their own. What this file has to show is that the
//! **door** reaches [`Teammates::start`] and that the two pane values refuse
//! through it in exactly the sentence the other door refuses in — one door must
//! not spawn where the other refuses.
//!
//! One test, because it redirects `XDG_DATA_HOME` and **withdraws `TMUX`**, and
//! a binary that mutates process-wide state holds exactly one — a plain `cargo
//! test` runs a binary's tests on threads of one process. `TMUX` goes because
//! since P25b the pane backends are real: with it set, the `pane` request below
//! would split a pane of this test harness into whatever tmux the developer is
//! running the suite in, and the claim here is about the *door*, not about a
//! window.
//!
//! # What sits behind the seam here, and why
//!
//! The context this drives carries a test-local [`Subagents`] whose
//! `spawn_teammate` is the engine's own [`Teammates`] and whose `delegate` is
//! not exercised. That is one forwarding call short of what a session installs
//! — `ganja_core`'s `subagent::Spawn` reads the same value off its `Host` —
//! and it is short of it because a `Host` needs a whole engine, a provider, an
//! agent roster and a live parent turn to exist, none of which this claim is
//! about. Everything past the seam is the real thing: the real registry, the
//! real backends, the real team file.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ganja_core::{
    Backends, Caller, SpawnAsk, SpawnAsker, Storage, Teammates,
    permission::Permissions,
    protocol::PermissionReply,
    provider::FakeProvider,
    teammate::{
        InProcess, TeammateRegistry,
        tmux::{self, REFUSED_NO_TMUX},
    },
    tool::{
        Credentials, FileTimes, Registry, Tool as _, ToolCtx, ToolError,
        task::{
            Delegated, Delegation, NotSpawned, Offered, Subagents, TaskTool, TeammateSpawn,
            Teammated, Unanswered,
        },
    },
};
use ganja_team::{TeamFile, TeamName, TeamsRoot};
use tokio_util::sync::CancellationToken;

/// How long the teammate's own provider takes to answer.
///
/// Long enough that a door which *waited* for the teammate could not possibly
/// come back inside [`AT_ONCE`]: the runner's first pass alone is half a second
/// behind the spawn, and the turn it starts is this much more.
const TEAMMATE_TURN: Duration = Duration::from_secs(2);

/// The most a door that answers without waiting may take.
///
/// A generous multiple of what the work actually is — writing two small files
/// — and a small fraction of [`TEAMMATE_TURN`], which is what makes the bound
/// a claim about *whether* it waited rather than about how fast this machine
/// is.
const AT_ONCE: Duration = Duration::from_secs(1);

/// The team every member here joins.
const TEAM: &str = "session-abcd1234";

/// The engine-side seam a `task` call reaches, with only the teammate half
/// wired: what a delegation does is `task_tool.rs`'s claim, not this one.
#[derive(Debug)]
struct Door {
    teammates: Teammates,
    caller: Caller,
    /// Every spawn a person was asked about, which for a teammate working
    /// inside the project should be none at all.
    asked: Arc<Mutex<Vec<SpawnAsk>>>,
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
        self.teammates.start(request, &self.caller, self).await
    }
}

#[async_trait]
impl SpawnAsker for Door {
    async fn ask(&self, request: SpawnAsk) -> PermissionReply {
        self.asked.lock().expect("no panic").push(request);

        PermissionReply::Once
    }
}

/// What the model calls the tool with to start a teammate on `backend`.
fn args(backend: &str) -> serde_json::Value {
    serde_json::json!({
        "description": "hold the fort",
        "prompt": "watch the build and say what breaks",
        "subagent_type": "general",
        "name": "worker",
        "backend": backend,
    })
}

/// The team file as it stands on disk.
fn team_file(root: &TeamsRoot, team: &TeamName) -> Option<TeamFile> {
    let text = std::fs::read_to_string(root.config_path(team)).ok()?;

    Some(serde_json::from_str(&text).expect("the team file this build wrote decodes"))
}

/// Both halves of AC-14's `task` leg: the door starts a teammate and answers
/// before the teammate has done anything, and the two pane surfaces refuse
/// through it in the sentence they refuse in everywhere else.
#[tokio::test]
async fn the_task_door_starts_a_teammate_at_once_and_refuses_a_pane_as_the_other_door_does() {
    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written.
    let data = unsafe { ganja_testkit::redirect_xdg_data_home() };
    // SAFETY: the same invariant. Outside tmux, both pane values refuse (D501)
    // instead of splitting a pane of this binary into the developer's session.
    unsafe {
        std::env::remove_var(tmux::TMUX);
        std::env::remove_var(tmux::TMUX_PANE);
    }
    let home = ganja_testkit::temp_dir();
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse(TEAM).expect("a team name");
    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        "01998ad0-0000-7000-8000-000000000000",
        home.path(),
    ));
    let asked: Arc<Mutex<Vec<SpawnAsk>>> = Arc::default();
    let door = Door {
        asked: Arc::clone(&asked),
        teammates: Teammates::new(
            Arc::clone(&registry),
            Backends {
                in_process: Arc::new(InProcess::new(
                    Arc::new(FakeProvider::new("on it", TEAMMATE_TURN)),
                    Arc::new(Registry::new(Vec::new())),
                    Storage::open(home.path().join("storage")),
                    |_| Permissions::default(),
                )),
                pane: Arc::new(ganja_core::teammate::pane::GanjaPane),
                claude: Arc::new(ganja_core::teammate::claude::ClaudePane),
            },
        ),
        caller: Caller {
            model: "recorder-model".to_owned(),
            cwd: home.path().to_path_buf(),
            permissions: Arc::new(Mutex::new(Permissions::default())),
            // The teammate works where the calling turn works, so the spawn
            // gate has nothing to disclose and nobody to ask.
            project_root: home.path().to_path_buf(),
        },
    };
    let tool = TaskTool::new(&[Offered {
        name: "general".to_owned(),
        description: None,
    }]);
    let ctx = ToolCtx {
        cwd: home.path().to_path_buf(),
        cancel: CancellationToken::new(),
        call_id: "call_1".to_owned(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: Some(Arc::new(door) as Arc<dyn Subagents>),
        postbox: None,
        ask: None,
        switch: None,
        jobs: None,
    };

    assert!(
        team_file(&root, &team).is_none(),
        "a session that has not spawned anything leaves no team on disk"
    );

    let started = std::time::Instant::now();
    let output = tool
        .run(args("in-process"), &ctx)
        .await
        .expect("the door starts an in-process teammate");
    let took = started.elapsed();

    // The door answered, and it answered without the teammate having done its
    // work — which is the whole difference between this door and the other.
    assert!(
        took < AT_ONCE,
        "the door waited for the teammate: {took:?} of a {TEAMMATE_TURN:?} turn"
    );
    assert_eq!(
        output.metadata.get("teammate").and_then(|to| to.as_str()),
        Some("worker"),
        "the result names the teammate a later message has to address: {output:?}"
    );
    assert_eq!(
        output.metadata.get("agent_id").and_then(|id| id.as_str()),
        Some(format!("worker@{TEAM}").as_str()),
        "and its derived identity: {output:?}"
    );
    assert_eq!(
        output.metadata.get("backend").and_then(|on| on.as_str()),
        Some("in-process"),
        "and the surface it really runs on: {output:?}"
    );

    // §4.1's member record, written by the door rather than by anything the
    // teammate itself later did.
    let file = team_file(&root, &team).expect("a spawn writes the team file");
    let member = file
        .members
        .iter()
        .find(|member| member.name == "worker")
        .unwrap_or_else(|| panic!("the teammate joined the team: {file:?}"));
    assert_eq!(
        member.agent_type, "general",
        "the record carries the agent kind the call named: {member:?}"
    );
    assert_eq!(
        member.model.as_deref(),
        Some("recorder-model"),
        "and the model the calling turn is asking: {member:?}"
    );
    assert_eq!(
        member.agent_id,
        format!("worker@{TEAM}"),
        "and the identity the tool reported: {member:?}"
    );
    assert_eq!(registry.running(), 1, "and the teammate is running");
    assert!(
        asked.lock().expect("no panic").is_empty(),
        "a teammate working inside the project asks nobody: {:?}",
        asked.lock().expect("no panic")
    );

    // The two pane values refuse through this door — a door that spawned where
    // the other refused would be two behaviours wearing one argument — and both
    // refuse in the sentence `teammate_no_tmux.rs` pins: the session, not the
    // build, is what is missing. Since W5b both bodies are real, so this says
    // the same thing about `claude` it always said about `pane`.
    for backend in ["pane", "claude"] {
        let refused = match tool.run(args(backend), &ctx).await {
            Err(ToolError::Failed(message)) => message,
            other => panic!("expected {backend} to be refused, got {other:?}"),
        };
        assert!(
            refused.contains(REFUSED_NO_TMUX),
            "{backend} refuses in the sentence teammate_no_tmux.rs pins: {refused}"
        );
        assert!(
            refused.contains(backend),
            "and names the surface that was asked for: {refused}"
        );
    }
    let file = team_file(&root, &team).expect("the team file is still there");
    assert_eq!(
        file.members
            .iter()
            .map(|member| member.name.as_str())
            .collect::<Vec<_>>(),
        vec!["team-lead", "worker"],
        "a refused spawn joined nobody to the team: {file:?}"
    );

    registry.shutdown().await;
    // Held to the end so the redirected data home outlives every write a
    // teammate's turn might make on its way out.
    drop(data);
}
